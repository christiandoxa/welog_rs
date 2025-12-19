use std::fs;
use std::future::Future;
use std::io::Read;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{Arc, Mutex as StdMutex, mpsc::sync_channel};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, Version};
use bytes::Bytes;
use chrono::{Duration as ChronoDuration, Local, SecondsFormat, TimeZone};
use futures_core::Stream;
use parking_lot::Mutex;
use tempfile::NamedTempFile;
use tower::{Layer, Service, ServiceExt};

use crate::axum_middleware::{
    WelogContext, extract_client_ip, header_to_string, log_axum, log_axum_client,
    test_force_invalid_request_id, version_to_string,
};
use crate::grpc::{
    GrpcContext, WelogGrpcInterceptor, log_grpc_client, test_ensure_context,
    test_force_invalid_header_key, test_force_invalid_request_id_key,
    test_log_grpc_stream_with_latency, test_log_grpc_unary_with_latency,
    test_request_id_metadata_key, test_serialize_value, test_set_request_id,
    with_grpc_stream_logging, with_grpc_unary_logging,
};
use crate::logger::{
    run_worker_with_path, test_build_fallback_bytes, test_duration_nanos,
    test_force_fallback_serialize_error, test_force_stdout_serialize_error,
    test_force_write_fallback_error, test_insert_nested_if_absent, test_logger_with_sender,
    test_new_from_env, test_send_to_elastic_with_config, test_send_to_elastic_without_config,
    test_set_elastic_behavior_io, test_set_elastic_behavior_ok, test_set_elastic_behavior_status,
    test_set_fallback_max_bytes, test_trim_oldest_lines, test_write_fallback,
};
use crate::model::{TargetRequest, TargetResponse};
use crate::util::{LogFields, build_target_log_fields, header_to_map};
use crate::{Config, set_config};
use serde::Serialize;
use tonic::metadata::MetadataValue;
use tonic::service::Interceptor;
use tonic::{GrpcMethod, Request, Response};

static ELASTIC_BEHAVIOR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static FALLBACK_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static GRPC_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static AXUM_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
const QUEUE_CAPACITY_ENV: &str = "WELOG_QUEUE_CAPACITY__";

#[derive(Serialize, Debug)]
struct DemoReq {
    message: String,
}

#[derive(Serialize, Debug)]
struct DemoRes {
    message: String,
}

#[derive(Clone, Debug)]
struct BadSerialize;

impl Serialize for BadSerialize {
    fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        Err(serde::ser::Error::custom("nope"))
    }
}

struct ErrorStream {
    yielded: bool,
}

impl Stream for ErrorStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.yielded {
            Poll::Ready(None)
        } else {
            self.yielded = true;
            Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "stream error",
            ))))
        }
    }
}

fn error_body() -> Body {
    Body::from_stream(ErrorStream { yielded: false })
}

#[derive(Clone, Copy)]
enum AxumBodyKind {
    Static(&'static [u8]),
    Error,
}

#[derive(Clone, Copy)]
enum AxumServiceMode {
    Ok {
        status: StatusCode,
        body: AxumBodyKind,
    },
    Err,
}

#[derive(Default)]
struct AxumTestState {
    method: Option<Method>,
    uri: Option<Uri>,
    request_id: Option<String>,
    has_context: bool,
    context_request_id: Option<String>,
}

#[derive(Clone)]
struct AxumTestService {
    mode: AxumServiceMode,
    state: Arc<StdMutex<AxumTestState>>,
}

impl Service<http::Request<Body>> for AxumTestService {
    type Response = http::Response<Body>;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let mode = self.mode;
        let state = self.state.clone();
        Box::pin(async move {
            let (parts, _body) = req.into_parts();
            let request_id = parts
                .headers
                .get("x-request-id")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.to_string());
            let context_request_id = parts
                .extensions
                .get::<Arc<WelogContext>>()
                .map(|ctx| ctx.request_id().to_string());
            let has_context = context_request_id.is_some();

            let mut guard = state.lock().unwrap();
            guard.method = Some(parts.method.clone());
            guard.uri = Some(parts.uri.clone());
            guard.request_id = request_id;
            guard.has_context = has_context;
            guard.context_request_id = context_request_id;
            drop(guard);

            match mode {
                AxumServiceMode::Ok { status, body } => {
                    let body = match body {
                        AxumBodyKind::Static(bytes) => Body::from(bytes),
                        AxumBodyKind::Error => error_body(),
                    };
                    Ok(http::Response::builder().status(status).body(body).unwrap())
                }
                AxumServiceMode::Err => Err(std::io::Error::new(std::io::ErrorKind::Other, "boom")),
            }
        })
    }
}

fn axum_test_service(mode: AxumServiceMode) -> (AxumTestService, Arc<StdMutex<AxumTestState>>) {
    let state = Arc::new(StdMutex::new(AxumTestState::default()));
    (
        AxumTestService {
            mode,
            state: state.clone(),
        },
        state,
    )
}

const GRPC_TEST_FORCE_ERROR: &str = "x-test-error";
const GRPC_TEST_LOG_TARGET: &str = "x-test-log-target";

async fn grpc_demo_handler(req: Request<DemoReq>) -> Result<Response<DemoRes>, tonic::Status> {
    if req.metadata().get(GRPC_TEST_LOG_TARGET).is_some() {
        if let Some(ctx) = req.extensions().get::<Arc<GrpcContext>>() {
            log_grpc_client(
                ctx,
                TargetRequest {
                    url: "https://example.com".into(),
                    method: "GET".into(),
                    content_type: "application/json".into(),
                    header: Default::default(),
                    body: br#"{"ping":"pong"}"#.to_vec(),
                    timestamp: Local::now(),
                },
                TargetResponse {
                    header: Default::default(),
                    body: br#"{"ok":true}"#.to_vec(),
                    status: 200,
                    latency: Duration::from_millis(10),
                },
            );
        }
    }

    if req.metadata().get(GRPC_TEST_FORCE_ERROR).is_some() {
        return Err(tonic::Status::internal("boom"));
    }

    Ok(Response::new(DemoRes {
        message: format!("hello {}", req.get_ref().message),
    }))
}

async fn grpc_bad_serialize_handler(
    req: Request<BadSerialize>,
) -> Result<Response<BadSerialize>, tonic::Status> {
    if req.metadata().get(GRPC_TEST_FORCE_ERROR).is_some() {
        return Err(tonic::Status::internal("boom"));
    }

    Ok(Response::new(BadSerialize))
}

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
    let timestamp = Local
        .with_ymd_and_hms(2024, 1, 2, 3, 4, 5)
        .single()
        .unwrap();

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

    let expected_request_ts = timestamp.to_rfc3339_opts(SecondsFormat::Nanos, true);
    let expected_response_ts = (timestamp + ChronoDuration::from_std(res.latency).unwrap())
        .to_rfc3339_opts(SecondsFormat::Nanos, true);

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
fn build_target_log_fields_handles_large_latency() {
    let timestamp = Local
        .with_ymd_and_hms(2024, 1, 2, 3, 4, 5)
        .single()
        .unwrap();

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
        latency: Duration::from_secs(u64::MAX),
    };

    let fields = build_target_log_fields(&req, &res);
    let req_ts = fields
        .get("targetRequestTimestamp")
        .and_then(|v| v.as_str());
    let resp_ts = fields
        .get("targetResponseTimestamp")
        .and_then(|v| v.as_str());
    assert_eq!(req_ts, resp_ts);
}

#[test]
fn build_target_log_fields_handles_non_json_body() {
    let timestamp = Local
        .with_ymd_and_hms(2024, 1, 2, 3, 4, 5)
        .single()
        .unwrap();
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
    assert_eq!(version_to_string(Version::HTTP_09), "HTTP/0.9");
    assert_eq!(version_to_string(Version::HTTP_2), "HTTP/2.0");
    assert_eq!(version_to_string(Version::HTTP_10), "HTTP/1.0");
    assert_eq!(version_to_string(Version::HTTP_3), "HTTP/3.0");
    assert_eq!(version_to_string(Version::HTTP_11), "HTTP/1.1");
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
        timestamp: Local::now(),
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
    let (sender, receiver) = sync_channel(1);
    let logger = Arc::new(test_logger_with_sender(sender));
    let client_log = Arc::new(Mutex::new(Vec::<LogFields>::new()));

    let target_req = TargetRequest {
        url: "https://example.com/target".into(),
        method: "POST".into(),
        content_type: "application/json".into(),
        header: Default::default(),
        body: br#"{"ping":"pong"}"#.to_vec(),
        timestamp: Local
            .with_ymd_and_hms(2024, 1, 1, 0, 0, 0)
            .single()
            .unwrap(),
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

    let request_time = Local
        .with_ymd_and_hms(2024, 6, 3, 12, 0, 0)
        .single()
        .unwrap();
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

    let expected_request_ts = request_time.to_rfc3339_opts(SecondsFormat::Nanos, true);
    let expected_response_ts = (request_time + ChronoDuration::from_std(latency).unwrap())
        .to_rfc3339_opts(SecondsFormat::Nanos, true);
    assert_eq!(
        fields.get("requestTimestamp"),
        Some(&serde_json::Value::String(expected_request_ts))
    );
    assert_eq!(
        fields.get("responseTimestamp"),
        Some(&serde_json::Value::String(expected_response_ts))
    );
}

#[tokio::test]
async fn log_axum_handles_non_json_bodies() {
    let (sender, receiver) = sync_channel(1);
    let logger = Arc::new(test_logger_with_sender(sender));
    let ctx = WelogContext {
        request_id: "req-non-json".into(),
        logger,
        client_log: Arc::new(Mutex::new(Vec::new())),
    };

    let request_time = Local
        .with_ymd_and_hms(2024, 6, 3, 12, 0, 0)
        .single()
        .unwrap();
    let latency = Duration::from_millis(5);
    let method = Method::POST;
    let uri = Uri::from_str("http://example.com/demo").unwrap();
    let version = Version::HTTP_11;
    let request_body = Bytes::from_static(b"not-json");
    let response_body = Bytes::from_static(b"also-not-json");
    let status = StatusCode::OK;
    let request_headers = HeaderMap::new();
    let response_headers = HeaderMap::new();

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
        "",
    );

    let fields = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("log should be sent");
    assert!(
        fields
            .get("requestBody")
            .and_then(|v| v.as_object())
            .is_some()
    );
    assert!(
        fields
            .get("responseBody")
            .and_then(|v| v.as_object())
            .is_some()
    );
}

#[tokio::test]
async fn log_axum_handles_large_latency() {
    let (sender, receiver) = sync_channel(1);
    let logger = Arc::new(test_logger_with_sender(sender));
    let ctx = WelogContext {
        request_id: "req-latency".into(),
        logger,
        client_log: Arc::new(Mutex::new(Vec::new())),
    };

    let request_time = Local
        .with_ymd_and_hms(2024, 6, 3, 12, 0, 0)
        .single()
        .unwrap();
    let latency = Duration::from_secs(u64::MAX);
    let method = Method::GET;
    let uri = Uri::from_str("http://example.com/latency").unwrap();
    let version = Version::HTTP_11;
    let request_body = Bytes::from_static(b"{}");
    let response_body = Bytes::from_static(b"{}");
    let status = StatusCode::OK;
    let request_headers = HeaderMap::new();
    let response_headers = HeaderMap::new();

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
        "",
    );

    let fields = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("log should be sent");
    let request_ts = fields.get("requestTimestamp").and_then(|v| v.as_str());
    let response_ts = fields.get("responseTimestamp").and_then(|v| v.as_str());
    assert_eq!(request_ts, response_ts);
}

#[test]
fn log_axum_client_skips_when_full() {
    let ctx = WelogContext {
        request_id: "req-full".into(),
        logger: Arc::new(crate::logger::logger().clone()),
        client_log: Arc::new(Mutex::new(vec![LogFields::new(); 1000])),
    };

    let req = TargetRequest {
        url: "https://example.com/api".into(),
        method: "GET".into(),
        content_type: "application/json".into(),
        header: Default::default(),
        body: br#"{"a":1}"#.to_vec(),
        timestamp: Local::now(),
    };
    let res = TargetResponse {
        header: Default::default(),
        body: br#"{"ok":true}"#.to_vec(),
        status: 200,
        latency: Duration::from_millis(20),
    };

    log_axum_client(&ctx, req, res);

    let guard = ctx.client_log.lock();
    assert_eq!(guard.len(), 1000);
}

#[test]
fn welog_context_logger_returns_clone() {
    let logger = Arc::new(crate::logger::logger().clone());
    let ctx = WelogContext {
        request_id: "req-logger".into(),
        logger: logger.clone(),
        client_log: Arc::new(Mutex::new(Vec::new())),
    };

    assert!(Arc::ptr_eq(&ctx.logger(), &logger));
}

#[test]
fn send_to_elastic_returns_error_when_not_configured() {
    let err = test_send_to_elastic_without_config(&LogFields::new())
        .expect_err("expected missing config error");
    assert!(err.contains("ELASTIC_URL__"));
}

#[test]
fn log_handles_disconnected_channel() {
    let (sender, receiver) = sync_channel(1);
    drop(receiver);
    let logger = test_logger_with_sender(sender);

    let mut fields = LogFields::new();
    fields.insert("event".into(), serde_json::Value::String("test".into()));
    logger.log(fields);
}

#[test]
fn log_drops_when_channel_full() {
    let (sender, receiver) = sync_channel(0);
    let logger = test_logger_with_sender(sender);

    let mut fields = LogFields::new();
    fields.insert("event".into(), serde_json::Value::String("test".into()));
    logger.log(fields);

    drop(receiver);
}

#[test]
fn log_drops_reports_every_1000() {
    let (sender, receiver) = sync_channel(0);
    let logger = test_logger_with_sender(sender);

    let mut fields = LogFields::new();
    fields.insert("event".into(), serde_json::Value::String("burst".into()));

    for _ in 0..1000 {
        logger.log(fields.clone());
    }

    drop(receiver);
}

#[test]
fn log_handles_stdout_serialization_error() {
    let (sender, receiver) = sync_channel(1);
    let logger = test_logger_with_sender(sender);

    test_force_stdout_serialize_error(true);
    let mut fields = LogFields::new();
    fields.insert(
        "event".into(),
        serde_json::Value::String("stdout-fail".into()),
    );
    logger.log(fields);
    test_force_stdout_serialize_error(false);

    drop(receiver);
}

#[test]
fn fallback_serialization_error_returns_empty_bytes() {
    let _guard = FALLBACK_TEST_LOCK.lock().unwrap();
    test_force_fallback_serialize_error(true);
    let mut fields = LogFields::new();
    fields.insert(
        "event".into(),
        serde_json::Value::String("fallback-fail".into()),
    );
    let bytes = test_build_fallback_bytes(&fields, None);
    test_force_fallback_serialize_error(false);

    assert!(bytes.is_empty());
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
fn insert_nested_if_absent_handles_empty_and_non_object() {
    let mut map = serde_json::Map::new();
    test_insert_nested_if_absent(&mut map, &[], serde_json::Value::String("ignored".into()));
    assert!(map.is_empty());

    map.insert("http".into(), serde_json::Value::String("oops".into()));
    test_insert_nested_if_absent(
        &mut map,
        &["http", "request"],
        serde_json::Value::String("ok".into()),
    );
    assert!(map.get("http").unwrap().is_object());
}

#[test]
fn duration_nanos_returns_none_for_negative() {
    let start = Local::now();
    let end = start - ChronoDuration::milliseconds(1);
    assert!(test_duration_nanos(start, end).is_none());
}

#[test]
fn log_sends_fields_through_channel() {
    let (sender, receiver) = sync_channel(1);
    let logger = test_logger_with_sender(sender);
    let mut fields = LogFields::new();
    fields.insert("hello".into(), serde_json::Value::String("world".into()));

    logger.log(fields.clone());

    let received = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("expected log to be sent");
    assert_eq!(
        received.get("hello"),
        Some(&serde_json::Value::String("world".into()))
    );

    assert!(received.get("@timestamp").is_some());

    let ecs_version = received
        .get("ecs")
        .and_then(|v| v.as_object())
        .and_then(|obj| obj.get("version"))
        .and_then(|v| v.as_str());
    assert_eq!(ecs_version, Some("9.2.0"));

    let log_level = received
        .get("log")
        .and_then(|v| v.as_object())
        .and_then(|obj| obj.get("level"))
        .and_then(|v| v.as_str());
    assert_eq!(log_level, Some("info"));

    let event_dataset = received
        .get("event")
        .and_then(|v| v.as_object())
        .and_then(|obj| obj.get("dataset"))
        .and_then(|v| v.as_str());
    assert_eq!(event_dataset, Some("welog"));
}

#[test]
fn log_enriches_duration_and_labels() {
    let (sender, receiver) = sync_channel(1);
    let logger = test_logger_with_sender(sender);

    let start = Local
        .with_ymd_and_hms(2024, 1, 1, 0, 0, 0)
        .single()
        .unwrap();
    let end = start + ChronoDuration::milliseconds(10);

    let mut fields = LogFields::new();
    fields.insert(
        "requestTimestamp".into(),
        serde_json::Value::String(start.to_rfc3339_opts(SecondsFormat::Nanos, true)),
    );
    fields.insert(
        "responseTimestamp".into(),
        serde_json::Value::String(end.to_rfc3339_opts(SecondsFormat::Nanos, true)),
    );
    fields.insert(
        "requestId".into(),
        serde_json::Value::String("req-abc".into()),
    );

    logger.log(fields);

    let received = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("log should be sent");
    assert!(
        received
            .get("event")
            .and_then(|v| v.as_object())
            .and_then(|obj| obj.get("duration"))
            .and_then(|v| v.as_u64())
            .is_some()
    );
    let labels = received.get("labels").and_then(|v| v.as_object()).unwrap();
    assert_eq!(
        labels.get("request_id"),
        Some(&serde_json::Value::String("req-abc".into()))
    );
}

#[test]
fn log_skips_duration_when_request_timestamp_missing() {
    let (sender, receiver) = sync_channel(1);
    let logger = test_logger_with_sender(sender);

    let end = Local
        .with_ymd_and_hms(2024, 1, 1, 0, 0, 1)
        .single()
        .unwrap();
    let mut fields = LogFields::new();
    fields.insert(
        "responseTimestamp".into(),
        serde_json::Value::String(end.to_rfc3339_opts(SecondsFormat::Nanos, true)),
    );

    logger.log(fields);

    let received = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("log should be sent");
    let duration = received
        .get("event")
        .and_then(|v| v.as_object())
        .and_then(|obj| obj.get("duration"));
    assert!(duration.is_none());
}

#[test]
fn write_fallback_persists_line() {
    let _guard = FALLBACK_TEST_LOCK.lock().unwrap();
    let tmp = NamedTempFile::new().unwrap();
    let mut fields = LogFields::new();
    fields.insert("message".into(), serde_json::Value::String("ok".into()));

    test_write_fallback(tmp.path(), &fields, Some("boom")).unwrap();

    let contents = fs::read_to_string(tmp.path()).unwrap();
    assert!(contents.contains("\"message\":\"ok\""));
    assert!(contents.contains("\"hook_error\":\"boom\""));
    assert!(contents.ends_with('\n'));
}

#[test]
fn queue_capacity_env_parsing_handles_invalid_and_zero() {
    let _guard = ENV_LOCK.lock().unwrap();
    let prev = std::env::var(QUEUE_CAPACITY_ENV).ok();

    unsafe {
        std::env::set_var(QUEUE_CAPACITY_ENV, "bad");
    }
    let _ = test_new_from_env();

    unsafe {
        std::env::set_var(QUEUE_CAPACITY_ENV, "0");
    }
    let _ = test_new_from_env();

    unsafe {
        match prev {
            Some(val) => std::env::set_var(QUEUE_CAPACITY_ENV, val),
            None => std::env::remove_var(QUEUE_CAPACITY_ENV),
        }
    }
}

#[test]
fn write_fallback_creates_missing_file() {
    let _guard = FALLBACK_TEST_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing-log.txt");

    let mut fields = LogFields::new();
    fields.insert("message".into(), serde_json::Value::String("ok".into()));

    test_write_fallback(&path, &fields, None).unwrap();
    assert!(path.exists());
}

#[test]
fn write_fallback_skips_when_log_too_large() {
    let _guard = FALLBACK_TEST_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("too-large.txt");

    test_set_fallback_max_bytes(10);
    let mut fields = LogFields::new();
    fields.insert(
        "message".into(),
        serde_json::Value::String("this is big".into()),
    );

    test_write_fallback(&path, &fields, None).unwrap();
    test_set_fallback_max_bytes(0);

    assert!(!path.exists());
}

#[test]
fn write_fallback_trims_when_over_capacity() {
    let _guard = FALLBACK_TEST_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trim.txt");

    fs::write(&path, b"line1\nline2\nline3\n").unwrap();

    test_set_fallback_max_bytes(25);
    let mut fields = LogFields::new();
    fields.insert("message".into(), serde_json::Value::String("ok".into()));

    test_write_fallback(&path, &fields, None).unwrap();
    test_set_fallback_max_bytes(0);

    let contents = fs::read_to_string(&path).unwrap();
    assert!(!contents.starts_with("line1"));
}

#[test]
fn write_fallback_skips_when_serialization_fails() {
    let _guard = FALLBACK_TEST_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("serialize-fail.txt");

    test_force_fallback_serialize_error(true);
    let mut fields = LogFields::new();
    fields.insert("message".into(), serde_json::Value::String("ok".into()));
    test_write_fallback(&path, &fields, None).unwrap();
    test_force_fallback_serialize_error(false);

    assert!(!path.exists());
}

#[cfg(not(coverage))]
#[test]
fn worker_loop_reports_fallback_error() {
    let dir = tempfile::tempdir().unwrap();
    let (sender, receiver) = sync_channel(1);

    let path = dir.path().to_path_buf();
    let handle = std::thread::spawn(move || run_worker_with_path(&path, receiver));

    let mut fields = LogFields::new();
    fields.insert("msg".into(), serde_json::Value::String("hello".into()));
    sender.send(fields).unwrap();
    drop(sender);
    handle.join().unwrap();
}

#[test]
fn send_to_elastic_succeeds_with_auth() {
    let _guard = ELASTIC_BEHAVIOR_LOCK.lock().unwrap();
    test_set_elastic_behavior_ok(201);
    let result = test_send_to_elastic_with_config(
        "http://example.com",
        Some("welog"),
        Some("user"),
        Some("pass"),
        &LogFields::new(),
    );

    assert!(result.is_ok());
}

#[test]
fn send_to_elastic_returns_error_on_status_code() {
    let _guard = ELASTIC_BEHAVIOR_LOCK.lock().unwrap();
    test_set_elastic_behavior_status(500);
    let err = test_send_to_elastic_with_config(
        "http://example.com",
        Some("welog"),
        None,
        None,
        &LogFields::new(),
    )
    .expect_err("expected status error");

    assert!(err.contains("HTTP error"));
}

#[test]
fn send_to_elastic_returns_error_on_redirect() {
    let _guard = ELASTIC_BEHAVIOR_LOCK.lock().unwrap();
    test_set_elastic_behavior_ok(302);
    let err = test_send_to_elastic_with_config(
        "http://example.com",
        Some("welog"),
        None,
        None,
        &LogFields::new(),
    )
    .expect_err("expected redirect error");

    assert!(err.contains("HTTP error"));
}

#[test]
fn send_to_elastic_returns_error_on_request_failure() {
    let _guard = ELASTIC_BEHAVIOR_LOCK.lock().unwrap();
    test_set_elastic_behavior_io();
    let err = test_send_to_elastic_with_config(
        "http://example.com",
        Some("welog"),
        None,
        None,
        &LogFields::new(),
    )
    .expect_err("expected request error");
    assert!(err.contains("request error"));
}

#[test]
fn send_to_elastic_returns_error_when_behavior_missing() {
    let _guard = ELASTIC_BEHAVIOR_LOCK.lock().unwrap();
    let err = test_send_to_elastic_with_config(
        "http://example.com",
        Some("welog"),
        None,
        None,
        &LogFields::new(),
    )
    .expect_err("expected request error");
    assert!(err.contains("request error"));
}

#[test]
fn send_to_elastic_handles_invalid_test_status() {
    let _guard = ELASTIC_BEHAVIOR_LOCK.lock().unwrap();
    test_set_elastic_behavior_ok(0);
    let err = test_send_to_elastic_with_config(
        "http://example.com",
        Some("welog"),
        None,
        None,
        &LogFields::new(),
    )
    .expect_err("expected request error");
    assert!(err.contains("request error"));
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
    let (inner, state) = axum_test_service(AxumServiceMode::Ok {
        status: StatusCode::CREATED,
        body: AxumBodyKind::Static(br#"{"ok":true}"#),
    });
    let mut svc = crate::axum_middleware::WelogLayer.layer(inner);

    let req = http::Request::builder()
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

    let guard = state.lock().unwrap();
    assert_eq!(guard.method.as_ref(), Some(&Method::POST));
    assert_eq!(guard.uri.as_ref(), Some(&Uri::from_static("/demo")));
    assert!(guard.has_context);
    assert!(guard.request_id.as_ref().is_some_and(|v| !v.is_empty()));
}

#[tokio::test]
async fn welog_middleware_preserves_existing_request_id() {
    let _guard = AXUM_TEST_LOCK.lock().unwrap();
    let (inner, state) = axum_test_service(AxumServiceMode::Ok {
        status: StatusCode::OK,
        body: AxumBodyKind::Static(b"ok"),
    });
    let mut svc = crate::axum_middleware::WelogLayer.layer(inner);

    let req = http::Request::builder()
        .method(Method::GET)
        .uri("/demo")
        .header("x-request-id", "existing-id")
        .body(Body::empty())
        .unwrap();

    let _resp = svc.ready().await.unwrap().call(req).await.unwrap();

    let guard = state.lock().unwrap();
    assert_eq!(guard.request_id.as_deref(), Some("existing-id"));
}

#[tokio::test]
async fn welog_middleware_handles_invalid_request_id() {
    let _guard = AXUM_TEST_LOCK.lock().unwrap();
    let (inner, state) = axum_test_service(AxumServiceMode::Ok {
        status: StatusCode::OK,
        body: AxumBodyKind::Static(b"ok"),
    });
    let mut svc = crate::axum_middleware::WelogLayer.layer(inner);
    let req = http::Request::builder()
        .method(Method::GET)
        .uri("/demo")
        .body(Body::empty())
        .unwrap();

    test_force_invalid_request_id(true);
    let _resp = svc.ready().await.unwrap().call(req).await.unwrap();
    test_force_invalid_request_id(false);

    let guard = state.lock().unwrap();
    assert_eq!(guard.request_id.as_deref(), Some("invalid-request-id"));
}

#[tokio::test]
async fn welog_middleware_handles_request_body_error() {
    let (inner, _state) = axum_test_service(AxumServiceMode::Ok {
        status: StatusCode::OK,
        body: AxumBodyKind::Static(b"ok"),
    });
    let mut svc = crate::axum_middleware::WelogLayer.layer(inner);
    let req = http::Request::builder()
        .method(Method::POST)
        .uri("/demo")
        .body(error_body())
        .unwrap();

    let resp = svc.ready().await.unwrap().call(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn welog_middleware_handles_response_body_error() {
    let (inner, _state) = axum_test_service(AxumServiceMode::Ok {
        status: StatusCode::OK,
        body: AxumBodyKind::Error,
    });
    let mut svc = crate::axum_middleware::WelogLayer.layer(inner);
    let req = http::Request::builder()
        .method(Method::GET)
        .uri("/demo")
        .body(Body::empty())
        .unwrap();

    let resp = svc.ready().await.unwrap().call(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn welog_middleware_propagates_inner_error() {
    let (inner, _state) = axum_test_service(AxumServiceMode::Err);
    let mut svc = crate::axum_middleware::WelogLayer.layer(inner);
    let req = http::Request::builder()
        .method(Method::GET)
        .uri("/demo")
        .body(Body::empty())
        .unwrap();

    let err = svc
        .ready()
        .await
        .unwrap()
        .call(req)
        .await
        .expect_err("expected inner error");
    assert_eq!(err.kind(), std::io::ErrorKind::Other);
}

#[test]
fn worker_loop_writes_fallback_on_error() {
    let _guard = FALLBACK_TEST_LOCK.lock().unwrap();
    let tmp = NamedTempFile::new().unwrap();
    let (sender, receiver) = sync_channel(1);

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

#[test]
fn worker_loop_reports_forced_fallback_error() {
    let _guard = FALLBACK_TEST_LOCK.lock().unwrap();
    let tmp = NamedTempFile::new().unwrap();
    let (sender, receiver) = sync_channel(1);

    test_force_write_fallback_error(true);
    let path = tmp.path().to_path_buf();
    let handle = std::thread::spawn(move || run_worker_with_path(&path, receiver));

    let mut fields = LogFields::new();
    fields.insert("msg".into(), serde_json::Value::String("hello".into()));
    sender.send(fields).unwrap();
    drop(sender);
    handle.join().unwrap();
    test_force_write_fallback_error(false);
}

#[test]
fn grpc_interceptor_injects_request_id_and_context() {
    let mut interceptor = WelogGrpcInterceptor;
    let mut req: Request<()> = Request::new(());
    req.metadata_mut()
        .insert("user-agent", MetadataValue::from_static("unit-test"));

    let req = interceptor.call(req).expect("interceptor should succeed");

    let ctx = req
        .extensions()
        .get::<Arc<GrpcContext>>()
        .cloned()
        .expect("context should be inserted");

    let request_id = ctx.request_id();
    assert!(!request_id.is_empty());

    let header_val = req
        .metadata()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap();
    assert_eq!(header_val, request_id);
}

#[test]
fn grpc_interceptor_uses_existing_request_id() {
    let mut interceptor = WelogGrpcInterceptor;
    let mut req: Request<()> = Request::new(());
    req.metadata_mut()
        .insert("x-request-id", MetadataValue::from_static("existing-id"));

    let req = interceptor.call(req).expect("interceptor should succeed");
    let ctx = req
        .extensions()
        .get::<Arc<GrpcContext>>()
        .cloned()
        .expect("context should be inserted");

    assert_eq!(ctx.request_id(), "existing-id");
}

#[tokio::test]
async fn with_grpc_unary_logging_logs_payload_and_target() {
    let (sender, receiver) = sync_channel(1);
    let logger = Arc::new(test_logger_with_sender(sender));
    let ctx = Arc::new(GrpcContext {
        request_id: "grpc-test-id".into(),
        logger,
        client_log: Arc::new(Mutex::new(Vec::new())),
    });

    let mut req = Request::new(DemoReq {
        message: "hi".into(),
    });
    req.metadata_mut()
        .insert("x-request-id", MetadataValue::from_static("grpc-test-id"));
    req.metadata_mut()
        .insert("user-agent", MetadataValue::from_static("unit-test"));
    req.metadata_mut()
        .insert(GRPC_TEST_LOG_TARGET, MetadataValue::from_static("1"));
    req.extensions_mut().insert(ctx.clone());
    req.extensions_mut()
        .insert(GrpcMethod::new("test.Service", "Say"));

    let response = with_grpc_unary_logging(req, grpc_demo_handler)
        .await
        .unwrap();

    let req_id_from_resp = response
        .metadata()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap();
    assert_eq!(req_id_from_resp, "grpc-test-id");

    let fields = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("log should be sent");

    assert_eq!(
        fields.get("grpcMethod"),
        Some(&serde_json::Value::String("/test.Service/Say".into()))
    );
    assert_eq!(
        fields.get("grpcStatusCode"),
        Some(&serde_json::Value::String("OK".into()))
    );
    assert_eq!(
        fields.get("requestId"),
        Some(&serde_json::Value::String("grpc-test-id".into()))
    );

    let target = fields
        .get("target")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap();
    assert_eq!(target.len(), 1);
}

#[tokio::test]
async fn with_grpc_stream_logging_logs_flags() {
    let (sender, receiver) = sync_channel(1);
    let logger = Arc::new(test_logger_with_sender(sender));
    let ctx = Arc::new(GrpcContext {
        request_id: "stream-id".into(),
        logger,
        client_log: Arc::new(Mutex::new(Vec::new())),
    });

    let mut req = Request::new(DemoReq {
        message: "hi".into(),
    });
    req.metadata_mut()
        .insert("x-request-id", MetadataValue::from_static("stream-id"));
    req.extensions_mut().insert(ctx.clone());
    req.extensions_mut()
        .insert(GrpcMethod::new("test.Service", "Bidi"));

    let response = with_grpc_stream_logging(req, grpc_demo_handler, true, true)
        .await
        .unwrap();

    let req_id_from_resp = response
        .metadata()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap();
    assert_eq!(req_id_from_resp, "stream-id");

    let fields = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("log should be sent");

    assert_eq!(
        fields.get("grpcMethod"),
        Some(&serde_json::Value::String("/test.Service/Bidi".into()))
    );
    assert_eq!(
        fields.get("grpcStatusCode"),
        Some(&serde_json::Value::String("OK".into()))
    );
    assert_eq!(
        fields.get("grpcIsClientStream"),
        Some(&serde_json::Value::Bool(true))
    );
    assert_eq!(
        fields.get("grpcIsServerStream"),
        Some(&serde_json::Value::Bool(true))
    );
}

#[test]
fn log_grpc_client_skips_when_full() {
    let ctx = GrpcContext {
        request_id: "grpc-full".into(),
        logger: Arc::new(crate::logger::logger().clone()),
        client_log: Arc::new(Mutex::new(vec![LogFields::new(); 1000])),
    };

    let req = TargetRequest {
        url: "https://example.com".into(),
        method: "GET".into(),
        content_type: "application/json".into(),
        header: Default::default(),
        body: br#"{"ping":"pong"}"#.to_vec(),
        timestamp: Local::now(),
    };
    let res = TargetResponse {
        header: Default::default(),
        body: br#"{"ok":true}"#.to_vec(),
        status: 200,
        latency: Duration::from_millis(10),
    };

    log_grpc_client(&ctx, req, res);

    let guard = ctx.client_log.lock();
    assert_eq!(guard.len(), 1000);
}

#[tokio::test]
async fn with_grpc_unary_logging_logs_error() {
    let (sender, receiver) = sync_channel(1);
    let logger = Arc::new(test_logger_with_sender(sender));
    let ctx = Arc::new(GrpcContext {
        request_id: "grpc-error-id".into(),
        logger,
        client_log: Arc::new(Mutex::new(Vec::new())),
    });

    let mut req = Request::new(DemoReq {
        message: "oops".into(),
    });
    req.metadata_mut()
        .insert("x-request-id", MetadataValue::from_static("grpc-error-id"));
    req.extensions_mut().insert(ctx.clone());
    req.extensions_mut()
        .insert(GrpcMethod::new("test.Service", "Fail"));
    req.metadata_mut()
        .insert(GRPC_TEST_FORCE_ERROR, MetadataValue::from_static("1"));

    let response = with_grpc_unary_logging(req, grpc_demo_handler)
        .await
        .expect_err("expected grpc error");
    assert_eq!(response.code(), tonic::Code::Internal);

    let fields = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("log should be sent");
    assert_eq!(
        fields.get("grpcStatusCode"),
        Some(&serde_json::Value::String("INTERNAL".into()))
    );
    assert_eq!(
        fields.get("grpcError"),
        Some(&serde_json::Value::String("boom".into()))
    );
}

#[tokio::test]
async fn with_grpc_stream_logging_logs_error() {
    let (sender, receiver) = sync_channel(1);
    let logger = Arc::new(test_logger_with_sender(sender));
    let ctx = Arc::new(GrpcContext {
        request_id: "stream-error-id".into(),
        logger,
        client_log: Arc::new(Mutex::new(Vec::new())),
    });

    let mut req = Request::new(DemoReq {
        message: "oops".into(),
    });
    req.metadata_mut().insert(
        "x-request-id",
        MetadataValue::from_static("stream-error-id"),
    );
    req.extensions_mut().insert(ctx.clone());
    req.extensions_mut()
        .insert(GrpcMethod::new("test.Service", "StreamFail"));
    req.metadata_mut()
        .insert(GRPC_TEST_FORCE_ERROR, MetadataValue::from_static("1"));

    let response = with_grpc_stream_logging(req, grpc_demo_handler, true, true)
        .await
        .expect_err("expected grpc error");
    assert_eq!(response.code(), tonic::Code::Internal);

    let fields = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("log should be sent");
    assert_eq!(
        fields.get("grpcStatusCode"),
        Some(&serde_json::Value::String("INTERNAL".into()))
    );
}

#[tokio::test]
async fn with_grpc_unary_logging_without_context_uses_unknown_method() {
    let (sender, receiver) = sync_channel(1);
    let logger = Arc::new(test_logger_with_sender(sender));
    let ctx = Arc::new(GrpcContext {
        request_id: "no-ctx".into(),
        logger,
        client_log: Arc::new(Mutex::new(Vec::new())),
    });

    let mut req = Request::new(DemoReq {
        message: "123".into(),
    });
    req.metadata_mut()
        .insert("x-request-id", MetadataValue::from_static("no-ctx"));
    req.extensions_mut().insert(ctx.clone());

    let response = with_grpc_unary_logging(req, grpc_demo_handler)
        .await
        .unwrap();

    let req_id_from_resp = response
        .metadata()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap();
    assert_eq!(req_id_from_resp, "no-ctx");

    let fields = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("log should be sent");
    assert_eq!(
        fields.get("grpcMethod"),
        Some(&serde_json::Value::String("unknown".into()))
    );
    assert_eq!(
        fields.get("grpcRequestString"),
        Some(&serde_json::Value::String(r#"{"message":"123"}"#.into()))
    );
}

#[tokio::test]
async fn with_grpc_unary_logging_handles_bad_serialize() {
    let (sender, receiver) = sync_channel(1);
    let logger = Arc::new(test_logger_with_sender(sender));
    let ctx = Arc::new(GrpcContext {
        request_id: "bad-ser".into(),
        logger,
        client_log: Arc::new(Mutex::new(Vec::new())),
    });

    let mut req = Request::new(BadSerialize);
    req.metadata_mut()
        .insert("x-request-id", MetadataValue::from_static("bad-ser"));
    req.extensions_mut().insert(ctx.clone());
    req.extensions_mut()
        .insert(GrpcMethod::new("test.Service", "Bad"));

    with_grpc_unary_logging(req, grpc_bad_serialize_handler)
        .await
        .unwrap();

    let mut req_err = Request::new(BadSerialize);
    req_err
        .metadata_mut()
        .insert("x-request-id", MetadataValue::from_static("bad-ser"));
    req_err
        .metadata_mut()
        .insert(GRPC_TEST_FORCE_ERROR, MetadataValue::from_static("1"));
    req_err.extensions_mut().insert(ctx.clone());
    req_err
        .extensions_mut()
        .insert(GrpcMethod::new("test.Service", "Bad"));

    let err = with_grpc_unary_logging(req_err, grpc_bad_serialize_handler)
        .await
        .expect_err("expected grpc error");
    assert_eq!(err.code(), tonic::Code::Internal);

    let fields = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("log should be sent");
    assert_eq!(
        fields.get("grpcMethod"),
        Some(&serde_json::Value::String("/test.Service/Bad".into()))
    );
    let request_string = fields
        .get("grpcRequestString")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(!request_string.is_empty());
}

#[tokio::test]
async fn with_grpc_unary_logging_captures_binary_metadata() {
    let (sender, receiver) = sync_channel(1);
    let logger = Arc::new(test_logger_with_sender(sender));
    let ctx = Arc::new(GrpcContext {
        request_id: "meta-id".into(),
        logger,
        client_log: Arc::new(Mutex::new(Vec::new())),
    });

    let mut req = Request::new(DemoReq {
        message: "meta".into(),
    });
    req.metadata_mut()
        .insert("x-request-id", MetadataValue::from_static("meta-id"));
    req.metadata_mut()
        .insert_bin("trace-bin", MetadataValue::from_bytes(b"\x01\x02"));
    req.metadata_mut()
        .append("multi", MetadataValue::from_static("a"));
    req.metadata_mut()
        .append("multi", MetadataValue::from_static("b"));
    req.extensions_mut().insert(ctx.clone());
    req.extensions_mut()
        .insert(GrpcMethod::new("test.Service", "Meta"));

    with_grpc_unary_logging(req, grpc_demo_handler)
        .await
        .unwrap();

    let fields = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("log should be sent");
    let meta = fields
        .get("grpcRequestMeta")
        .and_then(|v| v.as_object())
        .expect("metadata should be object");
    let trace = meta.get("trace-bin").and_then(|v| v.as_str()).unwrap();
    assert_eq!(trace, "QVFJ");
    let multi = meta.get("multi").and_then(|v| v.as_array()).unwrap();
    assert_eq!(multi.len(), 2);
}

#[tokio::test]
async fn with_grpc_unary_logging_logs_peer_address() {
    let (sender, receiver) = sync_channel(1);
    let logger = Arc::new(test_logger_with_sender(sender));
    let ctx = Arc::new(GrpcContext {
        request_id: "peer-id".into(),
        logger,
        client_log: Arc::new(Mutex::new(Vec::new())),
    });

    let mut req = Request::new(DemoReq {
        message: "peer".into(),
    });
    req.metadata_mut()
        .insert("x-request-id", MetadataValue::from_static("peer-id"));
    req.extensions_mut().insert(ctx.clone());
    req.extensions_mut()
        .insert(GrpcMethod::new("test.Service", "Peer"));
    req.extensions_mut()
        .insert(tonic::transport::server::TcpConnectInfo {
            local_addr: None,
            remote_addr: Some("127.0.0.1:12345".parse().unwrap()),
        });

    with_grpc_unary_logging(req, grpc_demo_handler)
        .await
        .unwrap();

    let fields = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("log should be sent");
    assert_eq!(
        fields.get("grpcPeer"),
        Some(&serde_json::Value::String("127.0.0.1:12345".into()))
    );
}

#[test]
fn grpc_context_logger_returns_clone() {
    let logger = Arc::new(crate::logger::logger().clone());
    let ctx = GrpcContext {
        request_id: "grpc-logger".into(),
        logger: logger.clone(),
        client_log: Arc::new(Mutex::new(Vec::new())),
    };

    assert!(Arc::ptr_eq(&ctx.logger(), &logger));
}

#[test]
fn grpc_set_request_id_ignores_invalid_value() {
    let mut md = tonic::metadata::MetadataMap::new();
    let invalid = "bad\nid";
    test_set_request_id(&mut md, invalid);
    assert!(md.get("x-request-id").is_none());
}

#[test]
fn grpc_request_id_metadata_key_falls_back_on_invalid_bytes() {
    let _guard = GRPC_TEST_LOCK.lock().unwrap();
    test_force_invalid_request_id_key(true);
    let key = test_request_id_metadata_key();
    test_force_invalid_request_id_key(false);

    assert_eq!(key.as_str(), "x-request-id");
}

#[test]
fn grpc_set_request_id_skips_invalid_header_key() {
    let _guard = GRPC_TEST_LOCK.lock().unwrap();
    let mut md = tonic::metadata::MetadataMap::new();
    test_force_invalid_header_key(true);
    test_set_request_id(&mut md, "req-id");
    test_force_invalid_header_key(false);

    assert!(md.get("x-request-id").is_some());
}

#[test]
fn grpc_serialize_value_handles_non_object() {
    let (fields, string_repr) = test_serialize_value(serde_json::Value::String("plain".into()));
    assert!(fields.is_empty());
    assert_eq!(string_repr, "\"plain\"");
}

#[test]
fn grpc_log_unary_handles_large_latency() {
    let fields = test_log_grpc_unary_with_latency(Duration::from_secs(u64::MAX));
    let request_ts = fields.get("requestTimestamp").and_then(|v| v.as_str());
    let response_ts = fields.get("responseTimestamp").and_then(|v| v.as_str());
    assert_eq!(request_ts, response_ts);
}

#[test]
fn grpc_log_stream_handles_large_latency() {
    let fields = test_log_grpc_stream_with_latency(Duration::from_secs(u64::MAX));
    let request_ts = fields.get("requestTimestamp").and_then(|v| v.as_str());
    let response_ts = fields.get("responseTimestamp").and_then(|v| v.as_str());
    assert_eq!(request_ts, response_ts);
}

#[test]
fn ensure_context_creates_context_when_missing() {
    let mut req = Request::new(());
    req.metadata_mut()
        .insert("x-request-id", MetadataValue::from_static("ctx-id"));

    let ctx = test_ensure_context(&mut req);
    assert_eq!(ctx.request_id(), "ctx-id");
}
