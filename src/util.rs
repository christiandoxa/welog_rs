use axum::http::HeaderMap;
use chrono::{Duration as ChronoDuration, SecondsFormat};
use serde_json::{Map, Number, Value};

use crate::model::{HeaderMapJson, TargetRequest, TargetResponse};

/// Alias for log fields, counterpart of `logrus.Fields` (`map[string]interface{}`) in Go.
pub type LogFields = Map<String, Value>;

/// Counterpart of `HeaderToMap` in Go for Hyper/Axum headers.
pub fn header_to_map(headers: &HeaderMap) -> HeaderMapJson {
    let mut map = HeaderMapJson::new();

    for (name, value) in headers.iter() {
        let key = name.to_string();
        let val_str = value.to_str().unwrap_or_default().to_string();
        map.insert(key, Value::String(val_str));
    }

    map
}

/// Counterpart of `BuildTargetLogFields` in Go.
///
/// Builds structured log fields for target (HTTP client) logs.
pub fn build_target_log_fields(req: &TargetRequest, res: &TargetResponse) -> LogFields {
    let mut request_field = LogFields::new();
    let mut response_field = LogFields::new();

    if let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(&req.body) {
        request_field = obj;
    }

    if let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(&res.body) {
        response_field = obj;
    }

    let req_ts = req.timestamp.to_rfc3339_opts(SecondsFormat::Nanos, true);

    let resp_ts = (req.timestamp
        + ChronoDuration::from_std(res.latency).unwrap_or_else(|_| ChronoDuration::zero()))
    .to_rfc3339_opts(SecondsFormat::Nanos, true);

    let mut fields = LogFields::new();

    fields.insert(
        "targetRequestBody".to_string(),
        Value::Object(request_field),
    );
    fields.insert(
        "targetRequestBodyString".to_string(),
        Value::String(String::from_utf8_lossy(&req.body).to_string()),
    );
    fields.insert(
        "targetRequestContentType".to_string(),
        Value::String(req.content_type.clone()),
    );
    fields.insert(
        "targetRequestHeader".to_string(),
        Value::Object(req.header.clone()),
    );
    fields.insert(
        "targetRequestMethod".to_string(),
        Value::String(req.method.clone()),
    );
    fields.insert("targetRequestTimestamp".to_string(), Value::String(req_ts));
    fields.insert(
        "targetRequestURL".to_string(),
        Value::String(req.url.clone()),
    );
    fields.insert(
        "targetResponseBody".to_string(),
        Value::Object(response_field),
    );
    fields.insert(
        "targetResponseBodyString".to_string(),
        Value::String(String::from_utf8_lossy(&res.body).to_string()),
    );
    fields.insert(
        "targetResponseHeader".to_string(),
        Value::Object(res.header.clone()),
    );
    fields.insert(
        "targetResponseLatency".to_string(),
        Value::String(format!("{:?}", res.latency)),
    );
    fields.insert(
        "targetResponseStatus".to_string(),
        Value::Number(Number::from(res.status)),
    );
    fields.insert(
        "targetResponseTimestamp".to_string(),
        Value::String(resp_ts),
    );

    fields
}
