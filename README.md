# welog_rs [![Rust Test](https://github.com/christiandoxa/welog_rs/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/christiandoxa/welog_rs/actions/workflows/ci.yml)

Rust port of the Go `welog` library. Provides structured logging to Elasticsearch with a file fallback plus Axum
middleware that logs requests/responses and target (HTTP client) calls.

## Features

- Structured logger that ships JSON logs to Elasticsearch; falls back to `logs.txt` (trimmed to 1GB).
- Background worker thread for non-blocking log enqueueing.
- Axum middleware (`WelogLayer`) that captures request/response payloads, headers, latency, client IP, and attaches a
  request ID.
- Helper to record outbound/target HTTP calls (`log_axum_client`) and include them in the same log entry.
- Tonic gRPC helpers: interceptor + unary/stream wrappers + `log_grpc_client` (mirrors the Go `NewGRPCUnary` /
  `NewGRPCStream`).
- Environment-based configuration to match the Go library’s behavior.

## Environment variables

The logger reads the following variables:

- `ELASTIC_INDEX__` – index prefix (e.g., `welog`)
- `ELASTIC_URL__` – Elasticsearch base URL (e.g., `http://localhost:9200`)
- `ELASTIC_USERNAME__` – Elasticsearch username
- `ELASTIC_PASSWORD__` – Elasticsearch password

If any are missing/empty, logs are written to `logs.txt` in the working directory.

## Usage

### Add the dependency

Add directly from this repo:

```toml
[dependencies]
welog_rs = { git = "https://github.com/christiandoxa/welog_rs.git" }
tokio = { version = "1", features = ["full"] }
axum = { version = "0.8", features = ["macros", "json"] }
# only for gRPC
tonic = { version = "0.12", features = ["transport"] }
```

Or if you vendored the crate locally:

```toml
[dependencies]
welog_rs = { path = "." } # or use your crate source path
tokio = { version = "1", features = ["full"] }
axum = { version = "0.8", features = ["macros", "json"] }
```

### Configure and start Axum

```rust
use std::{sync::Arc, time::Duration};
use axum::{routing::get, Extension, Json, Router};
use serde_json::json;
use welog_rs::{Config, WelogContext, WelogLayer, log_axum_client, set_config};
use welog_rs::model::{TargetRequest, TargetResponse};
use chrono::Utc;

#[tokio::main]
async fn main() {
    // Configure via code (or set env vars directly before startup).
    set_config(Config {
        elastic_index: "welog".into(),
        elastic_url: "http://localhost:9200".into(),
        elastic_username: "elastic".into(),
        elastic_password: "changeme".into(),
    });

    let app = Router::new()
        .route("/", get(root_handler))
        .route("/test-target", get(test_target_handler))
        .layer(WelogLayer); // install middleware

    axum::serve(
        tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap(),
        app,
    ).await.unwrap();
}

async fn root_handler(Extension(ctx): Extension<Arc<WelogContext>>) -> Json<serde_json::Value> {
    Json(json!({ "message": "hello", "request_id": ctx.request_id() }))
}

async fn test_target_handler(Extension(ctx): Extension<Arc<WelogContext>>) -> Json<serde_json::Value> {
    let target_req = TargetRequest {
        url: "https://example.com/api/demo".into(),
        method: "POST".into(),
        content_type: "application/json".into(),
        header: Default::default(),
        body: br#"{"ping":"test"}"#.to_vec(),
        timestamp: Utc::now(),
    };
    let target_res = TargetResponse {
        header: Default::default(),
        body: br#"{"status":"ok"}"#.to_vec(),
        status: 200,
        latency: Duration::from_millis(123),
    };

    // Attach target log to the current request log entry.
    log_axum_client(&ctx, target_req, target_res);

    Json(json!({ "message": "target logged", "request_id": ctx.request_id() }))
}
```

### Log arbitrary events (non-Axum/gRPC)

`Logger::log` mirrors `logrus.WithFields(...).Info()` in Go. The helper `logger()` gives you the global instance (same
shape as Go’s `logger.Logger()`), so you can emit structured logs from anywhere:

```rust
use serde_json::json;
use welog_rs::logger::logger;
use welog_rs::util::LogFields;

fn main() {
    // Optionally set config first (or rely on env vars):
    // welog_rs::set_config(...);

    let mut fields = LogFields::new();
    fields.insert("message".into(), json!("user logged in"));
    fields.insert("userId".into(), json!(42));
    fields.insert("roles".into(), json!(["admin", "editor"]));

    // Prints JSON to stdout, enqueues to background worker, and falls back to logs.txt on error.
    logger().log(fields);
}
```

### How it works

- `WelogLayer` clones the request body, response body, headers, status, latency, and client IP, then sends a structured
  log to the background worker.
- Logs are sent to Elasticsearch using `ureq` with a 5s global timeout. Non-2xx/3xx responses are treated as errors.
- On error or missing config, logs are appended to `logs.txt`, trimming the oldest lines when the file would exceed 1GB.
- Request IDs are preserved from the `X-Request-ID` header or generated if missing; the value is added back to the
  response headers.

### gRPC (tonic)

`welog_rs` now mirrors the Go gRPC interceptors via Tonic helpers:

- `WelogGrpcInterceptor` injects request ID + logger + client log into request extensions.
- `with_grpc_unary_logging` wraps unary handlers and emits the same fields as `logGRPCUnary` in Go.
- `with_grpc_stream_logging` wraps streaming handlers (logs when the handler future completes) and mirrors
  `logGRPCStream`.
- `log_grpc_client` appends outbound/target logs to the current context.

Integration example (per-service interceptor + handler wrapping):

```rust
use std::sync::Arc;
use tonic::{transport::Server, Request, Response, Status};
use welog_rs::{
    GrpcContext, WelogGrpcInterceptor, log_grpc_client,
    with_grpc_stream_logging, with_grpc_unary_logging,
};
use welog_rs::model::{TargetRequest, TargetResponse};

// Generated by tonic from your proto
use my_proto::my_service_server::{MyService, MyServiceServer};
use my_proto::{HelloReply, HelloRequest};

#[derive(Default)]
struct MyServiceImpl;

#[tonic::async_trait]
impl MyService for MyServiceImpl {
    async fn say_hello(
        &self,
        request: Request<HelloRequest>,
    ) -> Result<Response<HelloReply>, Status> {
        // Wrap the handler to emit Welog logs
        with_grpc_unary_logging(request, |req| async move {
            let ctx: Arc<GrpcContext> = req
                .extensions()
                .get::<Arc<GrpcContext>>()
                .cloned()
                .ok_or_else(|| Status::internal("missing welog context"))?;

            // Optional: record outbound HTTP/gRPC call into the same log entry
            log_grpc_client(&ctx, TargetRequest {
                url: "https://example.com".into(),
                method: "GET".into(),
                content_type: "application/json".into(),
                header: Default::default(),
                body: b"{}".to_vec(),
                timestamp: chrono::Utc::now(),
            }, TargetResponse {
                header: Default::default(),
                body: b"{}".to_vec(),
                status: 200,
                latency: std::time::Duration::from_millis(20),
            });

            Ok(Response::new(HelloReply {
                message: format!("hello {}", req.get_ref().name),
            }))
        })
            .await
    }

    type BidiStream = tonic::codec::Streaming<HelloReply>;
    async fn bidi_example(
        &self,
        request: Request<tonic::Streaming<HelloRequest>>,
    ) -> Result<Response<Self::BidiStream>, Status> {
        // Mirrors logGRPCStream: logs when handler future finishes
        with_grpc_stream_logging(request, |req| async move {
            // your streaming logic here
            Ok(Response::new(req.into_inner()))
        }, true, true)
            .await
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Attach interceptor at service-level so every RPC gets Welog context
    let svc = MyServiceServer::with_interceptor(MyServiceImpl::default(), WelogGrpcInterceptor);

    Server::builder()
        .add_service(svc)
        .serve("0.0.0.0:50051".parse()?)
        .await?;
    Ok(())
}
```

## Development

- Build: `cargo build`
- Tests: `cargo test`
- Rust edition: 2024 (requires Rust 1.91+)

## Notes

- `set_config` uses `std::env::set_var` and should be called before other threads start (mirrors Rust’s safety note for
  environment mutation).
- Body capture uses `usize::MAX` limit; adjust `BODY_READ_LIMIT` in `axum_middleware.rs` if you need to cap memory for
  very large payloads.
