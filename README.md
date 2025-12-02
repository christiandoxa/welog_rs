# welog_rs [![CI](https://github.com/christiandoxa/welog_rs/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/christiandoxa/welog_rs/actions/workflows/ci.yml)

Rust port of the Go `welog` library. Provides structured logging to Elasticsearch with a file fallback plus Axum
middleware that logs requests/responses and target (HTTP client) calls.

## Features

- Structured logger that ships JSON logs to Elasticsearch; falls back to `logs.txt` (trimmed to 1GB).
- Background worker thread for non-blocking log enqueueing.
- Axum middleware (`WelogLayer`) that captures request/response payloads, headers, latency, client IP, and attaches a
  request ID.
- Helper to record outbound/target HTTP calls (`log_axum_client`) and include them in the same log entry.
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

### How it works

- `WelogLayer` clones the request body, response body, headers, status, latency, and client IP, then sends a structured
  log to the background worker.
- Logs are sent to Elasticsearch using `ureq` with a 5s global timeout. Non-2xx/3xx responses are treated as errors.
- On error or missing config, logs are appended to `logs.txt`, trimming the oldest lines when the file would exceed 1GB.
- Request IDs are preserved from the `X-Request-ID` header or generated if missing; the value is added back to the
  response headers.

## Development

- Build: `cargo build`
- Tests: `cargo test`
- Rust edition: 2024 (requires Rust 1.91+)

## Notes

- `set_config` uses `std::env::set_var` and should be called before other threads start (mirrors Rust’s safety note for
  environment mutation).
- Body capture uses `usize::MAX` limit; adjust `BODY_READ_LIMIT` in `axum_middleware.rs` if you need to cap memory for
  very large payloads.
