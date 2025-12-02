use std::fs;
use std::io::Read;
use std::str::FromStr;
use std::sync::{Arc, mpsc::channel};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, Version};
use bytes::Bytes;
use chrono::{TimeZone, Utc};
use parking_lot::Mutex;
use tempfile::NamedTempFile;
use tower::{Layer, Service, ServiceExt};

use crate::axum_middleware::{
    WelogContext, extract_client_ip, header_to_string, log_axum, log_axum_client, version_to_string,
};
use crate::logger::{
    run_worker_with_path, test_build_fallback_bytes, test_logger_with_sender,
    test_send_to_elastic_without_config, test_trim_oldest_lines, test_write_fallback,
};
use crate::model::{TargetRequest, TargetResponse};
use crate::util::{LogFields, build_target_log_fields, header_to_map};
use crate::{Config, set_config};

#[test]
fn header_to_map_converts_values_to_strings() {
    let mut headers = HeaderMap::new();
    headers.insert("X-Test", HeaderValue::from_static("123"));
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));

    let map = header_to_map(&headers);
    assert_eq!(
        map.get("x-test"),
        Some(&serde_json::Value::String("123".into()))
    );
    assert_eq!(
        map.get("content-type"),
        Some(&serde_json::Value::String("application/json".into()))
    );
}

#[test]
fn build_target_log_fields_maps_request_and_response() {
    let timestamp = Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).single().unwrap();

    let req = TargetRequest {
        url: "https://example.com/api".into(),
        method: "POST".into(),
        content_type: "application/json".into(),
        header: Default::default(),
        body: br#"{"hello":"world"}"#.to_vec(),
        timestamp,
    };

    let res = TargetResponse {
        header: Default::default(),
        body: br#"{"status":"ok"}"#.to_vec(),
        status: 200,
        latency: Duration::from_millis(150),
    };

    let fields = build_target_log_fields(&req, &res);

    assert_eq!(
        fields
            .get("targetRequestBody")
            .and_then(|v| v.as_object())
            .and_then(|o| o.get("hello"))
            .and_then(|v| v.as_str()),
        Some("world")
    );
    assert_eq!(
        fields.get("targetRequestBodyString"),
        Some(&serde_json::Value::String(r#"{"hello":"world"}"#.into()))
    );
    assert_eq!(
        fields.get("targetResponseBodyString"),
        Some(&serde_json::Value::String(r#"{"status":"ok"}"#.into()))
    );
    assert_eq!(
        fields.get("targetResponseStatus"),
        Some(&serde_json::Value::Number(serde_json::Number::from(200)))
    );

    let expected_request_ts = "2024-01-02T03:04:05.000000000Z".to_string();
    let expected_response_ts = "2024-01-02T03:04:05.150000000Z".to_string();

    assert_eq!(
        fields.get("targetRequestTimestamp"),
        Some(&serde_json::Value::String(expected_request_ts))
    );
    assert_eq!(
        fields.get("targetResponseTimestamp"),
        Some(&serde_json::Value::String(expected_response_ts))
    );
    assert_eq!(
        fields.get("targetResponseLatency"),
        Some(&serde_json::Value::String("150ms".into()))
    );
}

#[test]
fn build_target_log_fields_handles_non_json_body() {
    let timestamp = Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).single().unwrap();
    let req = TargetRequest {
        url: "https://example.com/api".into(),
        method: "GET".into(),
        content_type: "text/plain".into(),
        header: Default::default(),
        body: b"not-json".to_vec(),
        timestamp,
    };
    let res = TargetResponse {
        header: Default::default(),
        body: b"also-not-json".to_vec(),
        status: 404,
        latency: Duration::from_millis(10),
    };

    let fields = build_target_log_fields(&req, &res);

    let request_body_obj = fields
        .get("targetRequestBody")
        .and_then(|v| v.as_object())
        .unwrap();
    let response_body_obj = fields
        .get("targetResponseBody")
        .and_then(|v| v.as_object())
        .unwrap();
    assert!(request_body_obj.is_empty());
    assert!(response_body_obj.is_empty());
    assert_eq!(
        fields.get("targetResponseStatus"),
        Some(&serde_json::Value::Number(serde_json::Number::from(404)))
    );
    assert_eq!(
        fields.get("targetRequestBodyString"),
        Some(&serde_json::Value::String("not-json".into()))
    );
    assert_eq!(
        fields.get("targetResponseBodyString"),
        Some(&serde_json::Value::String("also-not-json".into()))
    );
}

#[test]
fn header_to_string_is_case_insensitive() {
    let mut headers = HeaderMap::new();
    headers.insert("User-Agent", HeaderValue::from_static("agent-1"));
    assert_eq!(
        header_to_string(&headers, "user-agent"),
        "agent-1".to_string()
    );
    assert_eq!(header_to_string(&headers, "missing"), "");
}

#[test]
fn extract_client_ip_prefers_known_headers() {
    let mut headers = HeaderMap::new();
    headers.insert("X-Forwarded-For", HeaderValue::from_static("203.0.113.10"));
    headers.insert("X-Real-IP", HeaderValue::from_static("203.0.113.11"));
    assert_eq!(extract_client_ip(&headers), "203.0.113.11".to_string());

    let empty = HeaderMap::new();
    assert_eq!(extract_client_ip(&empty), "");
}

#[test]
fn version_to_string_maps_http_versions() {
    assert_eq!(version_to_string(Version::HTTP_2), "HTTP/2.0");
    assert_eq!(version_to_string(Version::HTTP_10), "HTTP/1.0");
}

#[test]
fn log_axum_client_pushes_target_logs() {
    let ctx = WelogContext {
        request_id: "req-1".into(),
        logger: Arc::new(crate::logger::logger().clone()),
        client_log: Arc::new(Mutex::new(Vec::new())),
    };

    let req = TargetRequest {
        url: "https://example.com/api".into(),
        method: "GET".into(),
        content_type: "application/json".into(),
        header: Default::default(),
        body: br#"{"a":1}"#.to_vec(),
        timestamp: Utc::now(),
    };
    let res = TargetResponse {
        header: Default::default(),
        body: br#"{"ok":true}"#.to_vec(),
        status: 200,
        latency: Duration::from_millis(20),
    };

    log_axum_client(&ctx, req.clone(), res.clone());

    let guard = ctx.client_log.lock();
    assert_eq!(guard.len(), 1);
    assert_eq!(guard[0], build_target_log_fields(&req, &res));
}

#[tokio::test]
async fn log_axum_builds_structured_fields() {
    let (sender, receiver) = channel();
    let logger = Arc::new(test_logger_with_sender(sender));
    let client_log = Arc::new(Mutex::new(Vec::<LogFields>::new()));

    let target_req = TargetRequest {
        url: "https://example.com/target".into(),
        method: "POST".into(),
        content_type: "application/json".into(),
        header: Default::default(),
        body: br#"{"ping":"pong"}"#.to_vec(),
        timestamp: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).single().unwrap(),
    };
    let target_res = TargetResponse {
        header: Default::default(),
        body: br#"{"status":"ok"}"#.to_vec(),
        status: 201,
        latency: Duration::from_millis(50),
    };
    client_log
        .lock()
        .push(build_target_log_fields(&target_req, &target_res));

    let mut request_headers = HeaderMap::new();
    request_headers.insert("user-agent", HeaderValue::from_static("demo-agent"));
    request_headers.insert("content-type", HeaderValue::from_static("application/json"));
    request_headers.insert("host", HeaderValue::from_static("example.com"));
    request_headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.10"));
    request_headers.insert(
        HeaderName::from_static("x-request-id"),
        HeaderValue::from_static("req-123"),
    );

    let mut response_headers = HeaderMap::new();
    response_headers.insert("etag", HeaderValue::from_static("abc123"));

    let ctx = WelogContext {
        request_id: "req-123".into(),
        logger,
        client_log,
    };

    let request_time = Utc.with_ymd_and_hms(2024, 6, 3, 12, 0, 0).single().unwrap();
    let latency = Duration::from_millis(200);
    let method = Method::POST;
    let uri = Uri::from_str("http://example.com/demo?x=1").unwrap();
    let version = Version::HTTP_11;
    let request_body = Bytes::from_static(br#"{"hello":"world"}"#);
    let response_body = Bytes::from_static(br#"{"ok":true}"#);
    let status = StatusCode::OK;
    let client_ip = extract_client_ip(&request_headers);

    log_axum(
        &ctx,
        request_time,
        latency,
        &method,
        &uri,
        version,
        &request_headers,
        &request_body,
        &response_headers,
        &response_body,
        status,
        &client_ip,
    );

    let fields = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("log should be sent");

    assert_eq!(
        fields.get("requestId"),
        Some(&serde_json::Value::String("req-123".into()))
    );
    assert_eq!(
        fields.get("requestAgent"),
        Some(&serde_json::Value::String("demo-agent".into()))
    );
    assert_eq!(
        fields.get("requestBodyString"),
        Some(&serde_json::Value::String(r#"{"hello":"world"}"#.into()))
    );
    assert_eq!(
        fields.get("responseBodyString"),
        Some(&serde_json::Value::String(r#"{"ok":true}"#.into()))
    );
    assert_eq!(
        fields.get("requestIp"),
        Some(&serde_json::Value::String("203.0.113.10".into()))
    );
    assert_eq!(
        fields.get("requestUrl"),
        Some(&serde_json::Value::String(
            "http://example.com/demo?x=1".into()
        ))
    );
    assert_eq!(
        fields.get("responseStatus"),
        Some(&serde_json::Value::Number(serde_json::Number::from(200)))
    );
    assert_eq!(
        fields
            .get("target")
            .and_then(|v| v.as_array())
            .map(|a| a.len()),
        Some(1)
    );

    let expected_request_ts = "2024-06-03T12:00:00.000000000Z".to_string();
    let expected_response_ts = "2024-06-03T12:00:00.200000000Z".to_string();
    assert_eq!(
        fields.get("requestTimestamp"),
        Some(&serde_json::Value::String(expected_request_ts))
    );
    assert_eq!(
        fields.get("responseTimestamp"),
        Some(&serde_json::Value::String(expected_response_ts))
    );
}

#[test]
fn send_to_elastic_returns_error_when_not_configured() {
    let err = test_send_to_elastic_without_config(&LogFields::new())
        .expect_err("expected missing config error");
    assert!(err.contains("ELASTIC_URL__"));
}

#[test]
fn build_fallback_bytes_appends_newline_and_hook_error() {
    let mut fields = LogFields::new();
    fields.insert("message".into(), serde_json::Value::String("ok".into()));

    let bytes = test_build_fallback_bytes(&fields, Some("boom"));

    let as_str = String::from_utf8(bytes.clone()).unwrap();
    assert!(as_str.ends_with('\n'));

    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let obj = parsed.as_object().unwrap();
    assert_eq!(
        obj.get("message"),
        Some(&serde_json::Value::String("ok".into()))
    );
    assert_eq!(
        obj.get("hook_error"),
        Some(&serde_json::Value::String("boom".into()))
    );
}

#[test]
fn trim_oldest_lines_removes_prefix_lines() {
    let tmp = NamedTempFile::new().unwrap();
    fs::write(tmp.path(), b"line1\nline2\nline3\n").unwrap();

    test_trim_oldest_lines(tmp.path(), 7).unwrap();

    let mut contents = String::new();
    fs::File::open(tmp.path())
        .unwrap()
        .read_to_string(&mut contents)
        .unwrap();

    assert_eq!(contents, "line2\nline3\n");
}

#[test]
fn log_sends_fields_through_channel() {
    let (sender, receiver) = channel();
    let logger = test_logger_with_sender(sender);
    let mut fields = LogFields::new();
    fields.insert("hello".into(), serde_json::Value::String("world".into()));

    logger.log(fields.clone());

    let received = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("expected log to be sent");
    assert_eq!(received, fields);
}

#[test]
fn write_fallback_persists_line() {
    let tmp = NamedTempFile::new().unwrap();
    let mut fields = LogFields::new();
    fields.insert("message".into(), serde_json::Value::String("ok".into()));

    test_write_fallback(tmp.path(), &fields, Some("boom")).unwrap();

    let contents = std::fs::read_to_string(tmp.path()).unwrap();
    assert!(contents.contains("\"message\":\"ok\""));
    assert!(contents.contains("\"hook_error\":\"boom\""));
    assert!(contents.ends_with('\n'));
}

#[test]
fn set_config_sets_env_vars() {
    let cfg = Config {
        elastic_index: "idx".into(),
        elastic_url: "http://localhost:9200".into(),
        elastic_username: "user1".into(),
        elastic_password: "pass1".into(),
    };

    set_config(cfg);

    assert_eq!(std::env::var("ELASTIC_INDEX__").unwrap(), "idx");
    assert_eq!(
        std::env::var("ELASTIC_URL__").unwrap(),
        "http://localhost:9200"
    );
    assert_eq!(std::env::var("ELASTIC_USERNAME__").unwrap(), "user1");
    assert_eq!(std::env::var("ELASTIC_PASSWORD__").unwrap(), "pass1");
}

#[tokio::test]
async fn welog_middleware_wraps_request_and_sets_request_id() {
    let inner = tower::service_fn(|req: axum::http::Request<Body>| async move {
        assert_eq!(req.method(), Method::POST);
        assert_eq!(req.uri(), &Uri::from_static("/demo"));
        let req_id = req
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(!req_id.is_empty());
        let ctx = req
            .extensions()
            .get::<Arc<WelogContext>>()
            .expect("context should be set");
        assert!(!ctx.request_id().is_empty());
        Ok::<_, std::convert::Infallible>(
            axum::http::Response::builder()
                .status(StatusCode::CREATED)
                .body(Body::from(r#"{"ok":true}"#))
                .unwrap(),
        )
    });

    let mut svc = crate::axum_middleware::WelogLayer.layer(inner);

    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri("/demo")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"input":1}"#))
        .unwrap();

    let resp = svc.ready().await.unwrap().call(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    if let Some(request_id) = resp
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
    {
        assert!(!request_id.is_empty());
    }
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body, Bytes::from_static(br#"{"ok":true}"#));
}

#[test]
fn worker_loop_writes_fallback_on_error() {
    let tmp = NamedTempFile::new().unwrap();
    let (sender, receiver) = channel();

    let path = tmp.path().to_path_buf();
    let handle = std::thread::spawn(move || run_worker_with_path(&path, receiver));

    let mut fields = LogFields::new();
    fields.insert("msg".into(), serde_json::Value::String("hello".into()));
    sender.send(fields).unwrap();
    drop(sender);
    handle.join().unwrap();

    let contents = fs::read_to_string(tmp.path()).unwrap();
    assert!(contents.contains("\"msg\":\"hello\""));
}
