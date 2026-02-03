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

use std::{net::SocketAddr, sync::Arc};

use axum::{Json, Router, extract::Extension, routing::get};
use serde_json::{Value, json};

use welog_rs::{Config, WelogContext, WelogLayer, set_config};

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

    // Axum router with one endpoint:
    // - GET / -> basic hello + request_id
    let _app: Router = Router::new()
        .route("/", get(root_handler))
        // Attach WelogLayer at the outermost level
        .layer(WelogLayer);

    let addr: SocketAddr = "0.0.0.0:3000".parse().expect("alamat tidak valid");
    println!("Server running on http://{}", addr);
    println!("Try:");
    println!("  curl http://{}/", addr);
    println!("Logs will be sent to Elasticsearch (if configured) or to logs.txt");

    #[cfg(not(coverage))]
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
