use crate::app::{new_app, AppOptions};
use crate::endpoint::Endpoint;
use actix_cors::Cors;
use actix_tls::{accept::openssl::reexports::SslAcceptor, connect::openssl::reexports::SslMethod};
use actix_web::{
    web::{self, JsonConfig, ServiceConfig},
    HttpServer,
};
use actix_web_prom::{PrometheusMetrics, PrometheusMetricsBuilder};
use anyhow::{anyhow, Context};
use bytesize::ByteSize;
use openssl::ssl::SslFiletype;
use prometheus::Registry;
use std::fmt::{Debug, Display, Formatter};
use std::marker::PhantomData;
use std::net::{IpAddr, Ipv6Addr, SocketAddr, TcpListener};
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use trustification_auth::{authenticator::Authenticator, authorizer::Authorizer};

const DEFAULT_ADDR: SocketAddr = SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0x1)), 8080);

#[derive(Copy, Clone, Debug, PartialEq, PartialOrd, Eq, Ord, Hash, Default)]
pub struct BinaryByteSize(pub ByteSize);

impl Deref for BinaryByteSize {
    type Target = ByteSize;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for BinaryByteSize {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Display for BinaryByteSize {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.to_string_as(true))
    }
}

impl FromStr for BinaryByteSize {
    type Err = <ByteSize as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ByteSize::from_str(s).map(BinaryByteSize)
    }
}

#[derive(Clone, Debug, clap::Args)]
#[command(rename_all_env = "SCREAMING_SNAKE_CASE", next_help_heading = "HTTP endpoint")]
pub struct HttpServerConfig<E>
where
    E: Endpoint + Send + Sync,
{
    /// The number of worker threads, defaults to zero, which falls back to the number of cores.
    #[arg(id = "http-server-workers", long, env = "HTTP_SERVER_WORKERS", default_value_t = 0)]
    pub workers: usize,

    /// The address to listen on
    #[arg(
        id = "http-server-bind-address",
        long,
        default_value_t = default::bind_addr(),
        env = "HTTP_SERVER_BIND_ADDR"
    )]
    pub bind_addr: String,

    /// The port to listen on
    #[arg(
        id = "http-server-bind-port",
        long,
        env = "HTTP_SERVER_BIND_PORT",
        default_value_t = E::PORT
    )]
    pub bind_port: u16,

    /// The overall request limit
    #[arg(
        id = "http-server-request-limit",
        long,
        default_value_t = default::request_limit(),
        env = "HTTP_SERVER_REQUEST_LIMIT"
    )]
    pub request_limit: BinaryByteSize,

    /// The JSON request limit
    #[arg(
        id = "http-server-json-limit",
        long,
        default_value_t = default::json_limit(),
        env = "HTTP_SERVER_JSON_LIMIT"
    )]
    pub json_limit: BinaryByteSize,

    /// Enable TLS
    #[arg(
        id = "http-server-tls-enabled",
        long,
        env = "HTTP_SERVER_TLS_ENABLED",
        default_value_t = false,
        action = clap::ArgAction::Set
    )]
    pub tls_enabled: bool,

    /// The path to the TLS key file in PEM format
    #[arg(id = "http-server-tls-key-file", long, env = "HTTP_SERVER_TLS_KEY_FILE")]
    pub tls_key_file: Option<PathBuf>,

    /// The path to the TLS certificate in PEM format
    #[arg(
        id = "http-server-tls-certificate-file",
        long,
        env = "HTTP_SERVER_TLS_CERTIFICATE_FILE"
    )]
    pub tls_certificate_file: Option<PathBuf>,

    #[arg(skip)]
    _marker: Marker<E>,
}

mod default {
    use super::*;

    pub fn bind_addr() -> String {
        "[::1]".to_string()
    }

    pub const fn request_limit() -> BinaryByteSize {
        BinaryByteSize(ByteSize::kib(256))
    }

    pub const fn json_limit() -> BinaryByteSize {
        BinaryByteSize(ByteSize::mib(2))
    }
}

impl<E: Endpoint> Default for HttpServerConfig<E>
where
    E: Endpoint + Send + Sync,
{
    fn default() -> Self {
        Self {
            workers: 0,
            bind_addr: default::bind_addr().to_string(),
            bind_port: E::PORT,
            request_limit: default::request_limit(),
            json_limit: default::json_limit(),
            tls_enabled: false,
            tls_key_file: None,
            tls_certificate_file: None,
            _marker: Default::default(),
        }
    }
}

#[derive(Debug)]
struct Marker<E>(PhantomData<E>);

impl<E> Default for Marker<E> {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<E> Clone for Marker<E> {
    fn clone(&self) -> Self {
        Default::default()
    }
}

impl<E> TryFrom<HttpServerConfig<E>> for HttpServerBuilder
where
    E: Endpoint + Send + Sync,
{
    type Error = anyhow::Error;

    fn try_from(value: HttpServerConfig<E>) -> Result<Self, Self::Error> {
        let mut result = HttpServerBuilder::new()
            .workers(value.workers)
            .bind(SocketAddr::from_str(&value.bind_addr).context("parse bind address")?)
            .request_limit(value.request_limit.0 .0 as _)
            .json_limit(value.json_limit.0 .0 as _);

        if value.tls_enabled {
            result = result.tls(TlsConfiguration {
                key: value.tls_key_file.ok_or_else(|| {
                    anyhow!("TLS enabled but no key file configured (use --http-server-tls-key-file)")
                })?,
                certificate: value.tls_certificate_file.ok_or_else(|| {
                    anyhow!("TLS enabled but no certificate file configured (use --http-server-tls-certificate-file)")
                })?,
            });
        }

        Ok(result)
    }
}

pub type ConfiguratorFn = dyn Fn(&mut ServiceConfig) + Send + Sync;

pub struct HttpServerBuilder {
    configurator: Option<Arc<ConfiguratorFn>>,
    bind: Bind,
    tls: Option<TlsConfiguration>,

    metrics_factory: Option<Arc<dyn Fn() -> anyhow::Result<PrometheusMetrics> + Send + Sync>>,
    cors_factory: Option<Arc<dyn Fn() -> Cors + Send + Sync>>,
    authenticator: Option<Arc<Authenticator>>,
    authorizer: Option<Authorizer>,

    workers: usize,
    json_limit: Option<usize>,
    request_limit: Option<usize>,
}

pub struct TlsConfiguration {
    certificate: PathBuf,
    key: PathBuf,
}

pub enum Bind {
    /// Use the provided listener
    Listener(TcpListener),
    /// Bind to the provided address and port
    Address(SocketAddr),
}

impl Default for HttpServerBuilder {
    fn default() -> Self {
        HttpServerBuilder::new()
    }
}

impl HttpServerBuilder {
    pub fn new() -> Self {
        Self {
            configurator: None,
            bind: Bind::Address(DEFAULT_ADDR),
            tls: None,
            metrics_factory: None,
            cors_factory: Some(Arc::new(Cors::permissive)),
            authenticator: None,
            authorizer: None,
            workers: 0,
            json_limit: None,
            request_limit: None,
        }
    }

    /// Set a custom CORS factory.
    ///
    /// The default is [`Cors::permissive`].
    pub fn cors<F>(mut self, cors_factory: F) -> Self
    where
        F: Fn() -> Cors + Send + Sync + 'static,
    {
        self.cors_factory = Some(Arc::new(cors_factory));
        self
    }

    pub fn cors_disabled(mut self) -> Self {
        self.cors_factory = None;
        self
    }

    pub fn default_authenticator(mut self, authenticator: Option<Arc<Authenticator>>) -> Self {
        self.authenticator = authenticator;
        self
    }

    pub fn authorizer(mut self, authorizer: Authorizer) -> Self {
        self.authorizer = Some(authorizer);
        self
    }

    pub fn configure<F>(mut self, configurator: F) -> Self
    where
        F: Fn(&mut ServiceConfig) + Send + Sync + 'static,
    {
        self.configurator = Some(Arc::new(configurator));
        self
    }

    pub fn metrics(mut self, registry: impl Into<Registry>, namespace: impl AsRef<str>) -> Self {
        let metrics = PrometheusMetricsBuilder::new(namespace.as_ref())
            .registry(registry.into())
            .build();

        self.metrics_factory = Some(Arc::new(move || {
            metrics.as_ref().map(|r| r.clone()).map_err(|err| anyhow!("{err}"))
        }));

        self
    }

    pub fn metrics_factory<F>(mut self, metrics_factory: F) -> Self
    where
        F: Fn() -> Result<PrometheusMetrics, Box<dyn std::error::Error + Send + Sync>> + Send + Sync + 'static,
    {
        self.metrics_factory = Some(Arc::new(move || {
            (metrics_factory)().map_err(|err| anyhow!("Failed to create prometheus registry: {err}"))
        }));
        self
    }

    pub fn listen(mut self, listener: TcpListener) -> Self {
        self.bind = Bind::Listener(listener);
        self
    }

    pub fn bind(mut self, addr: impl Into<SocketAddr>) -> Self {
        self.bind = Bind::Address(addr.into());
        self
    }

    pub fn tls(mut self, tls: impl Into<Option<TlsConfiguration>>) -> Self {
        self.tls = tls.into();
        self
    }

    pub fn workers(mut self, workers: usize) -> Self {
        self.workers = workers;
        self
    }

    pub fn json_limit(mut self, json_limit: usize) -> Self {
        self.json_limit = Some(json_limit);
        self
    }

    pub fn request_limit(mut self, request_limit: usize) -> Self {
        self.request_limit = Some(request_limit);
        self
    }

    pub async fn run(self) -> anyhow::Result<()> {
        let metrics = self.metrics_factory.as_ref().map(|factory| (factory)()).transpose()?;

        let mut http = HttpServer::new(move || {
            let config = self.configurator.clone();

            let cors = self.cors_factory.as_ref().map(|factory| (factory)());

            let mut json = JsonConfig::default();
            if let Some(limit) = self.json_limit {
                json = json.limit(limit);
            }

            let mut app = new_app(AppOptions {
                cors,
                metrics: metrics.clone(),
                authenticator: self.authenticator.clone(),
                authorizer: self.authorizer.clone().unwrap_or_else(|| Authorizer::new(None)),
            });

            // configure payload limit

            if let Some(limit) = self.request_limit {
                app = app.app_data(web::PayloadConfig::new(limit));
            }

            app.app_data(json).configure(|svc| {
                if let Some(config) = config {
                    config(svc);
                }
            })
        });

        if self.workers > 0 {
            http = http.workers(self.workers);
        }

        let tls = match self.tls {
            Some(tls) => {
                let mut acceptor = SslAcceptor::mozilla_modern_v5(SslMethod::tls_server())?;
                acceptor.set_certificate_chain_file(tls.certificate)?;
                acceptor.set_private_key_file(tls.key, SslFiletype::PEM)?;
                Some(acceptor)
            }
            None => None,
        };

        match self.bind {
            Bind::Listener(listener) => {
                log::info!("Binding to provided listener: {listener:?}");
                http = match tls {
                    Some(tls) => http.listen_openssl(listener, tls)?,
                    None => http.listen(listener)?,
                };
            }
            Bind::Address(addr) => {
                log::info!("Binding to: {addr}");
                http = match tls {
                    Some(tls) => http.bind_openssl(addr, tls)?,
                    None => http.bind(addr)?,
                };
            }
        }

        Ok(http.run().await?)
    }
}
