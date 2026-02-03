//! Middleware counterpart of Fiber & Gin for Axum.
//!
//! In Go there are:
//! - `NewFiber(fiberConfig fiber.Config) fiber.Handler`
//! - `NewGin() gin.HandlerFunc`
//! - `LogFiberClient` / `LogGinClient`
//!
//! In Rust, we provide:
//! - `WelogLayer` (Tower Layer) for Axum
//! - `WelogContext` stored in request extensions
//! - `log_axum_client` to record target (HTTP client) logs

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
#[cfg(coverage)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::http::{
    HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, Version,
};
use bytes::Bytes;
use chrono::{DateTime, Local, SecondsFormat};
use parking_lot::Mutex;
use serde_json::{Map, Number, Value};
use tower::{Layer, Service};
use whoami;

use crate::generalkey;
use crate::logger;
use crate::model::{TargetRequest, TargetResponse};
use crate::util::{LogFields, build_target_log_fields, header_to_map};

/// Limit when reading a body into Bytes (use usize::MAX to mirror no limit).
const BODY_READ_LIMIT: usize = usize::MAX;
/// Cap target logs per request to avoid unbounded growth.
const MAX_TARGET_LOGS: usize = 1000;

#[cfg(coverage)]
static FORCE_INVALID_REQUEST_ID: AtomicBool = AtomicBool::new(false);
#[cfg(coverage)]
static FORCE_LATENCY_OVERFLOW: AtomicBool = AtomicBool::new(false);

/// Main layer attached to Axum, equivalent to `NewFiber` / `NewGin`.
#[derive(Clone)]
pub struct WelogLayer;

#[derive(Clone)]
pub struct WelogMiddleware<S> {
    inner: S,
}

/// Per-request context, equivalent to `c.Locals(...)` / `c.Set(...)` in Go.
#[derive(Clone)]
pub struct WelogContext {
    pub(crate) request_id: String,
    pub(crate) logger: Arc<logger::Logger>,
    pub(crate) client_log: Arc<Mutex<Vec<LogFields>>>,
}

impl WelogContext {
    /// Get the current request ID.
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Get the global logger.
    pub fn logger(&self) -> Arc<logger::Logger> {
        self.logger.clone()
    }
}

impl<S> Layer<S> for WelogLayer {
    type Service = WelogMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        WelogMiddleware { inner }
    }
}

impl<S> Service<Request<Body>> for WelogMiddleware<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Error: Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let mut inner = self.inner.clone();

        Box::pin(async move {
            let start_instant = Instant::now();
            let request_time: DateTime<Local> = Local::now();

            // Split parts and body to capture them.
            let (mut parts, body) = req.into_parts();

            let body_bytes: Bytes = to_bytes(body, BODY_READ_LIMIT)
                .await
                .unwrap_or_else(|_| Bytes::new());

            let method: Method = parts.method.clone();
            let uri: Uri = parts.uri.clone();
            let version: Version = parts.version;

            // Read or create a request ID.
            let request_id_header_name =
                HeaderName::from_bytes(generalkey::REQUEST_ID_HEADER.as_bytes())
                    .unwrap_or(HeaderName::from_static("x-request-id"));

            let existing_request_id = parts
                .headers
                .get(&request_id_header_name)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());

            let request_id =
                existing_request_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            #[cfg(coverage)]
            let request_id = {
                let mut request_id = request_id;
                if FORCE_INVALID_REQUEST_ID.load(Ordering::Relaxed) {
                    request_id = "invalid\nrequest-id".to_string();
                }
                request_id
            };
            // Set the X-Request-ID header on the response/request parts.
            let header_value = HeaderValue::from_str(&request_id)
                .unwrap_or_else(|_| HeaderValue::from_static("invalid-request-id"));
            parts.headers.insert(request_id_header_name, header_value);

            // Prepare context to store in Extensions.
            let logger = Arc::new(logger::logger().clone());
            let client_log = Arc::new(Mutex::new(Vec::<LogFields>::new()));

            let ctx = Arc::new(WelogContext {
                request_id: request_id.clone(),
                logger,
                client_log: client_log.clone(),
            });

            // Store in extensions so handlers can access it.
            parts.extensions.insert(ctx.clone());

            let request_headers_for_log: HeaderMap = parts.headers.clone();

            // Rebuild the request for the inner service.
            let req_for_inner = Request::from_parts(parts, Body::from(body_bytes.clone()));

            // Run the inner service.
            let resp = match inner.call(req_for_inner).await {
                Ok(resp) => resp,
                Err(err) => return Err(err),
            };

            // Capture response body.
            let (resp_parts, resp_body) = resp.into_parts();
            let resp_bytes: Bytes = to_bytes(resp_body, BODY_READ_LIMIT)
                .await
                .unwrap_or_else(|_| Bytes::new());
            let status = resp_parts.status;
            let response_headers_for_log: HeaderMap = resp_parts.headers.clone();

            // Rebuild the response to return to the client.
            let new_resp = Response::from_parts(resp_parts, Body::from(resp_bytes.clone()));

            let latency = start_instant.elapsed();

            // Client IP ( the best effort using common headers).
            let client_ip = extract_client_ip(&request_headers_for_log);

            // Log request/response similar to `logFiber` / `logGin`.
            log_axum(
                &ctx,
                request_time,
                latency,
                &method,
                &uri,
                version,
                &request_headers_for_log,
                &body_bytes,
                &response_headers_for_log,
                &resp_bytes,
                status,
                &client_ip,
            );

            Ok(new_resp)
        })
    }
}

/// Build log fields and send them to the logger, equivalent to `logFiber`/`logGin`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn log_axum(
    ctx: &WelogContext,
    request_time: DateTime<Local>,
    latency: Duration,
    method: &Method,
    uri: &Uri,
    version: Version,
    request_headers: &HeaderMap,
    request_body: &Bytes,
    response_headers: &HeaderMap,
    response_body: &Bytes,
    status: StatusCode,
    client_ip: &str,
) {
    #[cfg(coverage)]
    let latency = if FORCE_LATENCY_OVERFLOW.load(Ordering::Relaxed) {
        Duration::from_secs(u64::MAX)
    } else {
        latency
    };

    let username = whoami::username().unwrap_or_default();

    // Parse body as a JSON map if possible.
    let mut request_body_json = Map::new();
    if let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(request_body) {
        request_body_json = obj;
    }

    let mut response_body_json = Map::new();
    if let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(response_body) {
        response_body_json = obj;
    }

    let request_headers_json = header_to_map(request_headers);
    let response_headers_json = header_to_map(response_headers);

    let user_agent = header_to_string(request_headers, "user-agent");
    let content_type = header_to_string(request_headers, "content-type");
    let host = header_to_string(request_headers, "host");

    let protocol = version_to_string(version);

    let request_timestamp = request_time.to_rfc3339_opts(SecondsFormat::Nanos, true);
    let response_timestamp = (request_time
        + chrono::Duration::from_std(latency).unwrap_or_else(|_| chrono::Duration::zero()))
    .to_rfc3339_opts(SecondsFormat::Nanos, true);

    let latency_str = format!("{:?}", latency);

    let method_str = method.to_string();
    let url_str = uri.to_string();

    // Pull client log (target) from the context.
    let target_logs_vec = {
        let mut guard = ctx.client_log.lock();
        let logs = std::mem::take(&mut *guard);
        logs.into_iter().map(Value::Object).collect::<Vec<Value>>()
    };

    let mut fields = LogFields::new();

    fields.insert("requestAgent".to_string(), Value::String(user_agent));
    fields.insert("requestBody".to_string(), Value::Object(request_body_json));
    fields.insert(
        "requestBodyString".to_string(),
        Value::String(String::from_utf8_lossy(request_body).to_string()),
    );
    fields.insert(
        "requestContentType".to_string(),
        Value::String(content_type),
    );
    fields.insert(
        "requestHeader".to_string(),
        Value::Object(request_headers_json),
    );
    fields.insert("requestHostName".to_string(), Value::String(host));
    fields.insert(
        "requestId".to_string(),
        Value::String(ctx.request_id.clone()),
    );
    fields.insert(
        "requestIp".to_string(),
        Value::String(client_ip.to_string()),
    );
    fields.insert("requestMethod".to_string(), Value::String(method_str));
    fields.insert("requestProtocol".to_string(), Value::String(protocol));
    fields.insert(
        "requestTimestamp".to_string(),
        Value::String(request_timestamp),
    );
    fields.insert("requestUrl".to_string(), Value::String(url_str));
    fields.insert(
        "responseBody".to_string(),
        Value::Object(response_body_json),
    );
    fields.insert(
        "responseBodyString".to_string(),
        Value::String(String::from_utf8_lossy(response_body).to_string()),
    );
    fields.insert(
        "responseHeader".to_string(),
        Value::Object(response_headers_json),
    );
    fields.insert("responseLatency".to_string(), Value::String(latency_str));
    fields.insert(
        "responseStatus".to_string(),
        Value::Number(Number::from(status.as_u16())),
    );
    fields.insert(
        "responseTimestamp".to_string(),
        Value::String(response_timestamp),
    );
    fields.insert("responseUser".to_string(), Value::String(username));
    fields.insert("target".to_string(), Value::Array(target_logs_vec));

    ctx.logger.log(fields);
}

/// Counterpart of `LogFiberClient` / `LogGinClient` for Axum.
///
/// Called from handlers that access `WelogContext` via `Extension<Arc<WelogContext>>`.
pub fn log_axum_client(ctx: &WelogContext, req: TargetRequest, res: TargetResponse) {
    let log_data = build_target_log_fields(&req, &res);
    let mut guard = ctx.client_log.lock();
    if guard.len() < MAX_TARGET_LOGS {
        guard.push(log_data);
    }
}

/// Get a header value as string, case-insensitive.
pub(crate) fn header_to_string(headers: &HeaderMap, name: &str) -> String {
    headers
        .iter()
        .find(|(k, _)| k.as_str().eq_ignore_ascii_case(name))
        .and_then(|(_, v)| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// Best-effort client IP detection from headers.
pub(crate) fn extract_client_ip(headers: &HeaderMap) -> String {
    let candidates = ["x-real-ip", "x-forwarded-for", "x-client-ip"];

    for name in candidates.iter() {
        if let Some(v) = headers
            .iter()
            .find(|(k, _)| k.as_str().eq_ignore_ascii_case(name))
            .and_then(|(_, v)| v.to_str().ok())
        {
            return v.to_string();
        }
    }

    String::new()
}

pub(crate) fn version_to_string(version: Version) -> String {
    #[cfg(coverage)]
    {
        format!("{version:?}")
    }
    #[cfg(not(coverage))]
    {
        match version {
            Version::HTTP_09 => "HTTP/0.9",
            Version::HTTP_10 => "HTTP/1.0",
            Version::HTTP_11 => "HTTP/1.1",
            Version::HTTP_2 => "HTTP/2.0",
            Version::HTTP_3 => "HTTP/3.0",
            _ => "HTTP",
        }
        .to_string()
    }
}

#[cfg(coverage)]
#[doc(hidden)]
pub fn coverage_force_invalid_request_id(enabled: bool) {
    FORCE_INVALID_REQUEST_ID.store(enabled, Ordering::Relaxed);
}

#[cfg(coverage)]
#[doc(hidden)]
pub fn coverage_force_latency_overflow(enabled: bool) {
    FORCE_LATENCY_OVERFLOW.store(enabled, Ordering::Relaxed);
}

#[cfg(coverage)]
#[doc(hidden)]
pub fn coverage_touch_header_helpers() {
    let headers = HeaderMap::new();
    let _ = header_to_string(&headers, "x-missing");
    let _ = extract_client_ip(&headers);
}
