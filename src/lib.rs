//! welog_rs
//!
//! Port of the Go `welog` library to Rust with Axum middleware equivalents.
//!
//! Key features:
//! - Env-based config for Elasticsearch
//! - Structured logger that sends to Elasticsearch with `logs.txt` fallback
//! - Axum middleware similar to Fiber/Gin middleware: request/response logging + target (client log)
//! - Helper for HTTP client (target) logging similar to `LogFiberClient` / `LogGinClient`

pub mod app;
pub mod axum_middleware;
pub mod config;
pub mod envkey;
pub mod generalkey;
pub mod grpc;
pub mod logger;
pub mod model;
pub mod util;

pub use crate::axum_middleware::{WelogContext, WelogLayer, log_axum_client};
pub use crate::config::Config;
pub use crate::grpc::{
    GrpcContext, WelogGrpcInterceptor, log_grpc_client, with_grpc_stream_logging,
    with_grpc_unary_logging,
};

/// Set logger configuration to environment variables, similar to `SetConfig` in Go.
///
/// Must be called **before** the logger is used for the first time.
pub fn set_config(cfg: Config) {
    use crate::envkey;
    use std::env;

    // `set_var` adalah fungsi unsafe di Rust 1.91+; pastikan dipanggil sebelum ada thread lain.
    unsafe {
        env::set_var(envkey::ELASTIC_INDEX, cfg.elastic_index);
        env::set_var(envkey::ELASTIC_URL, cfg.elastic_url);
        env::set_var(envkey::ELASTIC_USERNAME, cfg.elastic_username);
        env::set_var(envkey::ELASTIC_PASSWORD, cfg.elastic_password);
    }
}
