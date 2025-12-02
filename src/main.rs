// src/main.rs
//
// Simple entrypoint example to test welog_rs with Axum.
// Ensure Cargo.toml has these dependencies:
//
// [dependencies]
// tokio = { version = "1", features = ["full"] }
// axum = { version = "0.7", features = ["macros", "json"] }
// welog_rs = { path = "." }   # kalau main.rs dan lib.rs ada di crate yang sama
//
// Rust edition in Cargo.toml: edition = "2024"

use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{Json, Router, extract::Extension, routing::get};
use chrono::Utc;
use serde_json::{Map, Value, json};

use welog_rs::model::{TargetRequest, TargetResponse};
use welog_rs::{Config, WelogContext, WelogLayer, log_axum_client, set_config};

#[tokio::main]
async fn main() {
    // Optional: set Elasticsearch config via code (can skip if using ENV directly)
    //
    // If Elasticsearch is down / unreachable,
    // logs will automatically fall back to the `logs.txt` file.
    set_config(Config {
        elastic_index: "welog".into(),
        elastic_url: "http://localhost:9200".into(),
        elastic_username: "elastic".into(),
        elastic_password: "changeme".into(),
    });

    // Axum router with two endpoints:
    // - GET /              -> basic hello + request_id
    // - GET /test-target   -> example target log (fake HTTP client)
    let app = Router::new()
        .route("/", get(root_handler))
        .route("/test-target", get(test_target_handler))
        // Attach WelogLayer at the outermost level
        .layer(WelogLayer);

    let addr: SocketAddr = "0.0.0.0:3000".parse().expect("alamat tidak valid");
    println!("Server running on http://{}", addr);
    println!("Try:");
    println!("  curl http://{}/", addr);
    println!("  curl http://{}/test-target", addr);
    println!("Logs will be sent to Elasticsearch (if configured) or to logs.txt");

    axum::serve(tokio::net::TcpListener::bind(addr).await.unwrap(), app)
        .await
        .unwrap();
}

/// Root handler: return simple JSON plus request_id from WelogContext.
async fn root_handler(Extension(ctx): Extension<Arc<WelogContext>>) -> Json<Value> {
    let payload = json!({
        "message": "hello from welog_rs",
        "request_id": ctx.request_id(),
    });

    Json(payload)
}

/// Handler to test target logging (HTTP client).
///
/// We do *not* really call another service here;
/// we build dummy `TargetRequest` & `TargetResponse` so the
/// `target` field appears in the log (similar to LogFiberClient/LogGinClient).
async fn test_target_handler(Extension(ctx): Extension<Arc<WelogContext>>) -> Json<Value> {
    // Dummy request to a “different service”
    let target_req = TargetRequest {
        url: "https://example.com/api/demo".to_string(),
        method: "POST".to_string(),
        content_type: "application/json".to_string(),
        header: {
            let mut h = Map::new();
            h.insert(
                "X-Dummy-Header".into(),
                Value::String("dummy-header-value".into()),
            );
            h
        },
        body: br#"{"ping":"test"}"#.to_vec(),
        timestamp: Utc::now(),
    };

    // Dummy response from that “service”
    let target_res = TargetResponse {
        header: {
            let mut h = Map::new();
            h.insert(
                "Content-Type".into(),
                Value::String("application/json".into()),
            );
            h
        },
        body: br#"{"status":"ok","source":"fake-target"}"#.to_vec(),
        status: 200,
        latency: Duration::from_millis(123),
    };

    // Record the target log into WelogContext
    log_axum_client(&ctx, target_req, target_res);

    let payload = json!({
        "message": "test target logged. Check Elasticsearch or logs.txt.",
        "request_id": ctx.request_id(),
    });

    Json(payload)
}
