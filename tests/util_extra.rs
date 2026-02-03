use std::time::Duration;

use serde_json::Value;

use welog_rs::model::{TargetRequest, TargetResponse};
use welog_rs::util::build_target_log_fields;

#[test]
fn build_target_log_fields_handles_invalid_json() {
    let req = TargetRequest {
        url: "https://example.com".into(),
        method: "GET".into(),
        content_type: "application/json".into(),
        header: Default::default(),
        body: br#"{invalid"#.to_vec(),
        timestamp: chrono::Local::now(),
    };
    let res = TargetResponse {
        header: Default::default(),
        body: br#"{invalid"#.to_vec(),
        status: 200,
        latency: Duration::from_millis(5),
    };

    let fields = build_target_log_fields(&req, &res);
    assert_eq!(
        fields.get("targetRequestBody"),
        Some(&Value::Object(Default::default()))
    );
    assert_eq!(
        fields.get("targetResponseBody"),
        Some(&Value::Object(Default::default()))
    );
}

#[test]
fn build_target_log_fields_handles_latency_overflow() {
    let req = TargetRequest {
        url: "https://example.com".into(),
        method: "GET".into(),
        content_type: "application/json".into(),
        header: Default::default(),
        body: br#"{"ok":true}"#.to_vec(),
        timestamp: chrono::Local::now(),
    };
    let res = TargetResponse {
        header: Default::default(),
        body: br#"{"ok":true}"#.to_vec(),
        status: 200,
        latency: Duration::from_secs(u64::MAX),
    };

    let _ = build_target_log_fields(&req, &res);
}
