use actix_cors::Cors;
use actix_web::{
    body::MessageBody,
    dev::{ServiceFactory, ServiceRequest, ServiceResponse},
    middleware::Logger,
    App, Error,
};
use actix_web_extras::middleware::Condition;
use actix_web_prom::PrometheusMetrics;
use std::sync::Arc;
use trustification_auth::authenticator::Authenticator;

#[derive(Default)]
pub struct AppOptions {
    pub cors: Option<Cors>,
    pub metrics: Option<PrometheusMetrics>,
    pub authenticator: Option<Arc<Authenticator>>,
}

#[macro_export]
macro_rules! new_auth {
    ($auth:expr) => {
        actix_web_extras::middleware::Condition::from_option($auth.map(move |authenticator| {
            actix_web_httpauth::middleware::HttpAuthentication::bearer(move |req, auth| {
                trustification_auth::authenticator::actix::openid_validator(req, auth, authenticator.clone())
            })
        }))
    };
}

/// Build a new HTTP app in a consistent way.
///
/// Adding middleware to an HTTP app is tricky, as it requires to think about the order of adding.
/// This function should capture all the logic requires to properly set up a common application,
/// allowing some choices in the process.
pub fn new_app(
    options: AppOptions,
) -> App<
    impl ServiceFactory<
        ServiceRequest,
        Config = (),
        Response = ServiceResponse<impl MessageBody>,
        Error = Error,
        InitError = (),
    >,
> {
    // The order of execution is last added becomes first to be executed. So if you read the
    // following lines, read them from end to start!
    App::new()
        // Handle authentication, might fail and return early
        .wrap(new_auth!(options.authenticator))
        // Handle CORS requests, this might finish early and not pass requests to the next entry
        .wrap(Condition::from_option(options.cors))
        // Next, record metrics for the request (should never fail)
        .wrap(Condition::from_option(options.metrics))
        // First log the request, so that we know what happens (can't fail)
        .wrap(Logger::default())
}
