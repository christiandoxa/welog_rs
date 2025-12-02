//! gRPC counterpart of the Go interceptors.
//!
//! In Go there are `NewGRPCUnary`, `NewGRPCStream`, and `LogGRPCClient`.
//! This module provides the same capabilities for Tonic:
//! - `WelogGrpcInterceptor` to inject request ID, logger, and client-log vector
//! - `with_grpc_unary_logging` to wrap unary RPC handlers
//! - `with_grpc_stream_logging` to wrap streaming RPC handlers (logs when the handler returns)
//! - `log_grpc_client` to append outbound target logs

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose};
use chrono::{DateTime, SecondsFormat, Utc};
use http::Extensions;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Map, Value};
use tonic::metadata::{Ascii, KeyAndValueRef, MetadataKey, MetadataMap, MetadataValue};
use tonic::service::Interceptor;
use tonic::transport::server::TcpConnectInfo;
use tonic::{Code, GrpcMethod, Request, Response, Status};
use uuid::Uuid;
use whoami;

use crate::generalkey;
use crate::logger;
use crate::model::{TargetRequest, TargetResponse};
use crate::util::{LogFields, build_target_log_fields};

const REQUEST_ID_METADATA_KEY: &str = "x-request-id";

/// Request-scoped context stored in gRPC request extensions.
#[derive(Clone)]
pub struct GrpcContext {
    pub(crate) request_id: String,
    pub(crate) logger: Arc<logger::Logger>,
    pub(crate) client_log: Arc<Mutex<Vec<LogFields>>>,
}

impl GrpcContext {
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn logger(&self) -> Arc<logger::Logger> {
        self.logger.clone()
    }
}

/// Interceptor that injects `GrpcContext` and ensures an `x-request-id` metadata exists.
pub struct WelogGrpcInterceptor;

impl Interceptor for WelogGrpcInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let existing = fetch_request_id(request.metadata());
        let request_id = if existing.is_empty() {
            Uuid::new_v4().to_string()
        } else {
            existing
        };

        set_request_id(request.metadata_mut(), &request_id);

        let ctx = Arc::new(GrpcContext {
            request_id,
            logger: Arc::new(logger::logger().clone()),
            client_log: Arc::new(Mutex::new(Vec::new())),
        });

        request.extensions_mut().insert(ctx);

        Ok(request)
    }
}

/// Wrap a unary RPC handler to produce structured logs similar to `logGRPCUnary` in Go.
pub async fn with_grpc_unary_logging<Req, Res, F, Fut>(
    mut request: Request<Req>,
    handler: F,
) -> Result<Response<Res>, Status>
where
    Req: Serialize,
    Res: Serialize,
    F: FnOnce(Request<Req>) -> Fut,
    Fut: Future<Output = Result<Response<Res>, Status>>,
{
    let request_time: DateTime<Utc> = Utc::now();
    let start = Instant::now();

    let (request_fields, request_string) = capture_payload(&request);

    let ctx = ensure_context(&mut request);
    let metadata = request.metadata().clone();
    let method = grpc_method(&request);
    let peer = peer_address(request.extensions());

    let response = handler(request).await;
    let latency = start.elapsed();

    match response {
        Ok(mut resp) => {
            set_request_id(resp.metadata_mut(), &ctx.request_id);

            let (response_fields, response_string) = capture_payload(&resp);

            log_grpc_unary(
                &ctx,
                &method,
                &metadata,
                &peer,
                Code::Ok,
                None,
                request_time,
                latency,
                request_fields,
                request_string,
                response_fields,
                response_string,
            );

            Ok(resp)
        }
        Err(status) => {
            log_grpc_unary(
                &ctx,
                &method,
                &metadata,
                &peer,
                status.code(),
                Some(status.message()),
                request_time,
                latency,
                request_fields,
                request_string,
                LogFields::new(),
                String::new(),
            );

            Err(status)
        }
    }
}

/// Wrap a streaming RPC handler to log lifecycle data, mirroring `logGRPCStream` in Go.
///
/// Provide booleans for whether the RPC is client/server streaming.
pub async fn with_grpc_stream_logging<Req, Res, F, Fut>(
    mut request: Request<Req>,
    handler: F,
    is_client_stream: bool,
    is_server_stream: bool,
) -> Result<Response<Res>, Status>
where
    Req: Serialize,
    F: FnOnce(Request<Req>) -> Fut,
    Fut: Future<Output = Result<Response<Res>, Status>>,
{
    let request_time: DateTime<Utc> = Utc::now();
    let start = Instant::now();

    let (request_fields, request_string) = capture_payload(&request);

    let ctx = ensure_context(&mut request);
    let metadata = request.metadata().clone();
    let method = grpc_method(&request);
    let peer = peer_address(request.extensions());

    let response = handler(request).await;
    let latency = start.elapsed();

    match response {
        Ok(mut resp) => {
            set_request_id(resp.metadata_mut(), &ctx.request_id);

            log_grpc_stream(
                &ctx,
                &method,
                &metadata,
                &peer,
                Code::Ok,
                None,
                request_time,
                latency,
                request_fields,
                request_string,
                is_client_stream,
                is_server_stream,
            );

            Ok(resp)
        }
        Err(status) => {
            log_grpc_stream(
                &ctx,
                &method,
                &metadata,
                &peer,
                status.code(),
                Some(status.message()),
                request_time,
                latency,
                request_fields,
                request_string,
                is_client_stream,
                is_server_stream,
            );

            Err(status)
        }
    }
}

/// Append outbound target log entries, equivalent to `LogGRPCClient`.
pub fn log_grpc_client(ctx: &GrpcContext, req: TargetRequest, res: TargetResponse) {
    let log_data = build_target_log_fields(&req, &res);
    let mut guard = ctx.client_log.lock();
    guard.push(log_data);
}

fn ensure_context<T>(request: &mut Request<T>) -> Arc<GrpcContext> {
    if let Some(ctx) = request.extensions().get::<Arc<GrpcContext>>() {
        return ctx.clone();
    }

    let request_id = fetch_request_id(request.metadata());
    let ctx = Arc::new(GrpcContext {
        request_id,
        logger: Arc::new(logger::logger().clone()),
        client_log: Arc::new(Mutex::new(Vec::new())),
    });
    request.extensions_mut().insert(ctx.clone());
    ctx
}

fn capture_payload<T, E>(envelope: &E) -> (LogFields, String)
where
    T: Serialize,
    E: GrpcEnvelope<T>,
{
    serialize_message(envelope.get_ref())
}

fn serialize_message<T: Serialize>(msg: &T) -> (LogFields, String) {
    let mut fields = LogFields::new();
    let string_repr;

    match serde_json::to_value(msg) {
        Ok(Value::Object(obj)) => {
            fields = obj;
            string_repr = serde_json::to_string(msg).unwrap_or_default();
        }
        Ok(other) => {
            string_repr = other.to_string();
        }
        Err(err) => {
            string_repr = format!("{:?}", err);
        }
    }

    (fields, string_repr)
}

#[allow(clippy::too_many_arguments)]
fn log_grpc_unary(
    ctx: &GrpcContext,
    method: &str,
    metadata: &MetadataMap,
    peer: &str,
    code: Code,
    error: Option<&str>,
    request_time: DateTime<Utc>,
    latency: Duration,
    request_body: LogFields,
    request_body_string: String,
    response_body: LogFields,
    response_body_string: String,
) {
    let response_time = request_time
        + chrono::Duration::from_std(latency).unwrap_or_else(|_| chrono::Duration::zero());

    let mut fields = LogFields::new();

    fields.insert("grpcMethod".into(), Value::String(method.to_string()));
    fields.insert("grpcRequest".into(), Value::Object(request_body));
    fields.insert(
        "grpcRequestString".into(),
        Value::String(request_body_string),
    );
    fields.insert(
        "grpcRequestMeta".into(),
        Value::Object(metadata_to_map(metadata)),
    );
    fields.insert("grpcPeer".into(), Value::String(peer.to_string()));
    fields.insert(
        "grpcStatusCode".into(),
        Value::String(format!("{:?}", code).to_uppercase()),
    );
    fields.insert(
        "grpcError".into(),
        Value::String(error.unwrap_or_default().to_string()),
    );
    fields.insert("grpcResponse".into(), Value::Object(response_body));
    fields.insert(
        "grpcResponseString".into(),
        Value::String(response_body_string),
    );
    fields.insert("requestId".into(), Value::String(ctx.request_id.clone()));
    fields.insert(
        "requestTimestamp".into(),
        Value::String(request_time.to_rfc3339_opts(SecondsFormat::Nanos, true)),
    );
    fields.insert(
        "responseTimestamp".into(),
        Value::String(response_time.to_rfc3339_opts(SecondsFormat::Nanos, true)),
    );
    fields.insert(
        "responseLatency".into(),
        Value::String(format!("{:?}", latency)),
    );
    fields.insert("responseUser".into(), Value::String(whoami::username()));
    fields.insert(
        "target".into(),
        Value::Array(read_client_log(&ctx.client_log)),
    );

    ctx.logger.log(fields);
}

#[allow(clippy::too_many_arguments)]
fn log_grpc_stream(
    ctx: &GrpcContext,
    method: &str,
    metadata: &MetadataMap,
    peer: &str,
    code: Code,
    error: Option<&str>,
    request_time: DateTime<Utc>,
    latency: Duration,
    request_body: LogFields,
    request_body_string: String,
    is_client_stream: bool,
    is_server_stream: bool,
) {
    let response_time = request_time
        + chrono::Duration::from_std(latency).unwrap_or_else(|_| chrono::Duration::zero());

    let mut fields = LogFields::new();

    fields.insert("grpcMethod".into(), Value::String(method.to_string()));
    fields.insert(
        "grpcRequestMeta".into(),
        Value::Object(metadata_to_map(metadata)),
    );
    fields.insert("grpcPeer".into(), Value::String(peer.to_string()));
    fields.insert(
        "grpcStatusCode".into(),
        Value::String(format!("{:?}", code).to_uppercase()),
    );
    fields.insert(
        "grpcError".into(),
        Value::String(error.unwrap_or_default().to_string()),
    );
    fields.insert("grpcIsClientStream".into(), Value::Bool(is_client_stream));
    fields.insert("grpcIsServerStream".into(), Value::Bool(is_server_stream));
    fields.insert("requestId".into(), Value::String(ctx.request_id.clone()));
    fields.insert(
        "requestTimestamp".into(),
        Value::String(request_time.to_rfc3339_opts(SecondsFormat::Nanos, true)),
    );
    fields.insert(
        "responseTimestamp".into(),
        Value::String(response_time.to_rfc3339_opts(SecondsFormat::Nanos, true)),
    );
    fields.insert(
        "responseLatency".into(),
        Value::String(format!("{:?}", latency)),
    );
    fields.insert("responseUser".into(), Value::String(whoami::username()));
    fields.insert("grpcRequest".into(), Value::Object(request_body));
    fields.insert(
        "grpcRequestString".into(),
        Value::String(request_body_string),
    );
    fields.insert(
        "target".into(),
        Value::Array(read_client_log(&ctx.client_log)),
    );

    ctx.logger.log(fields);
}

fn metadata_to_map(md: &MetadataMap) -> Map<String, Value> {
    use std::collections::HashMap;

    let mut acc: HashMap<String, Vec<String>> = HashMap::new();

    for kv in md.iter() {
        match kv {
            KeyAndValueRef::Ascii(key, value) => {
                let key_str = key.to_string();
                let val_str = value.to_str().map(|s| s.to_string()).unwrap_or_default();
                acc.entry(key_str).or_default().push(val_str);
            }
            KeyAndValueRef::Binary(key, value) => {
                let key_str = key.to_string();
                let encoded = general_purpose::STANDARD.encode(value.as_encoded_bytes());
                acc.entry(key_str).or_default().push(encoded);
            }
        }
    }

    let mut map = Map::new();
    for (k, vals) in acc.into_iter() {
        if vals.len() == 1 {
            map.insert(k, Value::String(vals[0].clone()));
        } else {
            map.insert(
                k,
                Value::Array(vals.into_iter().map(Value::String).collect()),
            );
        }
    }

    map
}

fn read_client_log(client_log: &Arc<Mutex<Vec<LogFields>>>) -> Vec<Value> {
    let guard = client_log.lock();
    guard
        .iter()
        .cloned()
        .map(Value::Object)
        .collect::<Vec<Value>>()
}

fn grpc_method<T>(request: &Request<T>) -> String {
    if let Some(method) = request.extensions().get::<GrpcMethod>() {
        return format!("/{}/{}", method.service(), method.method());
    }

    "unknown".to_string()
}

fn peer_address(extensions: &Extensions) -> String {
    if let Some(info) = extensions.get::<TcpConnectInfo>()
        && let Some(addr) = info.remote_addr()
    {
        return addr.to_string();
    }

    String::new()
}

fn fetch_request_id(md: &MetadataMap) -> String {
    let request_id_key = request_id_metadata_key();
    if let Some(val) = md.get(&request_id_key).and_then(|v| v.to_str().ok())
        && !val.is_empty()
    {
        return val.to_string();
    }

    if let Ok(alt_key) = MetadataKey::<Ascii>::from_bytes(
        generalkey::REQUEST_ID_HEADER
            .to_ascii_lowercase()
            .as_bytes(),
    ) && let Some(val) = md.get(&alt_key).and_then(|v| v.to_str().ok())
        && !val.is_empty()
    {
        return val.to_string();
    }

    String::new()
}

fn request_id_metadata_key() -> MetadataKey<Ascii> {
    MetadataKey::from_bytes(REQUEST_ID_METADATA_KEY.as_bytes())
        .unwrap_or_else(|_| MetadataKey::from_static(REQUEST_ID_METADATA_KEY))
}

fn set_request_id(md: &mut MetadataMap, request_id: &str) {
    let key = request_id_metadata_key();
    if let Ok(value) = MetadataValue::try_from(request_id) {
        md.insert(key, value.clone());
        if let Ok(header_key) = MetadataKey::<Ascii>::from_bytes(
            generalkey::REQUEST_ID_HEADER
                .to_ascii_lowercase()
                .as_bytes(),
        ) {
            md.insert(header_key, value);
        }
    }
}

// Helper type so we can use the same capture code for Request and Response.
trait GrpcEnvelope<T> {
    fn get_ref(&self) -> &T;
}

impl<T> GrpcEnvelope<T> for Request<T> {
    fn get_ref(&self) -> &T {
        Request::get_ref(self)
    }
}

impl<T> GrpcEnvelope<T> for Response<T> {
    fn get_ref(&self) -> &T {
        Response::get_ref(self)
    }
}
