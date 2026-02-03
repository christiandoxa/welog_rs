use std::sync::Arc;

use axum::{Json, Router, extract::Extension, routing::get};
use serde_json::{Value, json};

use crate::{WelogContext, WelogLayer};

pub fn build_app() -> Router {
    Router::new()
        .route("/", get(root_handler))
        .layer(WelogLayer)
}

/// Root handler: return simple JSON plus request_id from WelogContext.
pub async fn root_handler(Extension(ctx): Extension<Arc<WelogContext>>) -> Json<Value> {
    let payload = json!({
        "message": "hello from welog_rs",
        "request_id": ctx.request_id(),
    });

    Json(payload)
}
