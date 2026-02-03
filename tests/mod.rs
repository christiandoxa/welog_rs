use std::future::Future;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Method, StatusCode, Uri, Version};
use chrono::Local;
use serde_json::{Value, json};
use tonic::service::Interceptor;
use tonic::{Request, Response};
use tower::{Layer, Service, ServiceExt};
use whoami;

use welog_rs::axum_middleware::{WelogContext, WelogLayer, log_axum_client};
use welog_rs::envkey;
use welog_rs::grpc::{GrpcContext, WelogGrpcInterceptor, with_grpc_unary_logging};
use welog_rs::model::{TargetRequest, TargetResponse};
use welog_rs::util::{build_target_log_fields, header_to_map};

static LOG_FILE_LOCK: StdMutex<()> = StdMutex::new(());

fn block_on<F, T>(future: F) -> T
where
    F: Future<Output = T>,
{
    tokio::runtime::Runtime::new()
        .expect("create tokio runtime")
        .block_on(future)
}

fn clear_elastic_env() {
    unsafe {
        std::env::remove_var(envkey::ELASTIC_URL);
        std::env::remove_var(envkey::ELASTIC_INDEX);
        std::env::remove_var(envkey::ELASTIC_USERNAME);
        std::env::remove_var(envkey::ELASTIC_PASSWORD);
    }
}

#[derive(Clone, Copy)]
enum AxumBodyKind {
    Static(&'static [u8]),
}

#[derive(Clone, Copy)]
enum AxumServiceMode {
    Ok {
        status: StatusCode,
        body: AxumBodyKind,
    },
}

#[derive(Default)]
struct AxumTestState {
    request_id: Option<String>,
    has_context: bool,
    context_request_id: Option<String>,
}

#[derive(Clone)]
struct AxumTestService {
    mode: AxumServiceMode,
    log_target: bool,
    state: Arc<StdMutex<AxumTestState>>,
}

impl Service<http::Request<Body>> for AxumTestService {
    type Response = http::Response<Body>;
    type Error = std::io::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let mode = self.mode;
        let log_target = self.log_target;
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

            if log_target {
                if let Some(ctx) = parts.extensions.get::<Arc<WelogContext>>() {
                    log_axum_client(
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

            let mut guard = state.lock().unwrap();
            guard.request_id = request_id;
            guard.has_context = has_context;
            guard.context_request_id = context_request_id;
            drop(guard);

            match mode {
                AxumServiceMode::Ok { status, body } => {
                    let body = match body {
                        AxumBodyKind::Static(bytes) => Body::from(bytes),
                    };
                    Ok(http::Response::builder().status(status).body(body).unwrap())
                }
            }
        })
    }
}

fn axum_test_service(
    mode: AxumServiceMode,
    log_target: bool,
) -> (AxumTestService, Arc<StdMutex<AxumTestState>>) {
    let state = Arc::new(StdMutex::new(AxumTestState::default()));
    (
        AxumTestService {
            mode,
            log_target,
            state: state.clone(),
        },
        state,
    )
}

#[test]
fn header_to_map_converts_values_to_strings() {
    let mut headers = HeaderMap::new();
    headers.insert("x-test", "value".parse().unwrap());
    let map = header_to_map(&headers);
    assert_eq!(map.get("x-test"), Some(&Value::String("value".into())));
}

#[test]
fn build_target_log_fields_maps_request_and_response() {
    let req = TargetRequest {
        url: "https://example.com".into(),
        method: "POST".into(),
        content_type: "application/json".into(),
        header: Default::default(),
        body: br#"{"ping":"pong"}"#.to_vec(),
        timestamp: Local::now(),
    };
    let res = TargetResponse {
        header: Default::default(),
        body: br#"{"ok":true}"#.to_vec(),
        status: 200,
        latency: Duration::from_millis(12),
    };

    let fields = build_target_log_fields(&req, &res);
    assert_eq!(
        fields.get("targetRequestMethod"),
        Some(&Value::String("POST".into()))
    );
    assert_eq!(
        fields.get("targetResponseStatus"),
        Some(&Value::Number(200.into()))
    );
}

#[test]
fn axum_layer_sets_request_id_and_context() {
    let _guard = LOG_FILE_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    clear_elastic_env();

    let (inner, state) = axum_test_service(
        AxumServiceMode::Ok {
            status: StatusCode::OK,
            body: AxumBodyKind::Static(b"ok"),
        },
        false,
    );

    let svc = WelogLayer.layer(inner);

    let req = http::Request::builder()
        .method(Method::GET)
        .uri(Uri::from_static("/"))
        .version(Version::HTTP_11)
        .body(Body::empty())
        .unwrap();

    let _resp = block_on(svc.oneshot(req)).unwrap();

    let guard = state.lock().unwrap();
    assert!(guard.request_id.is_some());
    assert!(guard.has_context);
    assert_eq!(guard.request_id, guard.context_request_id);
}

#[test]
fn axum_layer_logs_with_patched_username_and_target() {
    let _guard = LOG_FILE_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    clear_elastic_env();

    fn patched_username() -> Result<String, whoami::Error> {
        Ok("patched-user".to_string())
    }

    let _patch_guard =
        jmpln::patch!(whoami::username => patched_username).expect("patch whoami::username");

    let (inner, _state) = axum_test_service(
        AxumServiceMode::Ok {
            status: StatusCode::OK,
            body: AxumBodyKind::Static(b"ok"),
        },
        true,
    );

    let svc = WelogLayer.layer(inner);

    let req = http::Request::builder()
        .method(Method::GET)
        .uri(Uri::from_static("/"))
        .version(Version::HTTP_11)
        .body(Body::empty())
        .unwrap();

    let _resp = block_on(svc.oneshot(req)).unwrap();
}

#[test]
fn grpc_interceptor_injects_request_id_and_context() {
    let mut interceptor = WelogGrpcInterceptor;
    let req = Request::new(());
    let req = interceptor.call(req).expect("interceptor ok");

    let request_id = req
        .metadata()
        .get("x-request-id")
        .and_then(|val| val.to_str().ok())
        .map(str::to_string);

    assert!(request_id.is_some());
    assert!(req.extensions().get::<Arc<GrpcContext>>().is_some());
}

#[test]
fn grpc_unary_logging_sets_response_request_id() {
    let _guard = LOG_FILE_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    clear_elastic_env();

    let req = Request::new(json!({"message": "hello"}));

    let response = block_on(with_grpc_unary_logging(
        req,
        |request: Request<Value>| async move {
            let msg = request
                .get_ref()
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("");
            Ok(Response::new(json!({"message": format!("hello {msg}")})))
        },
    ))
    .expect("grpc unary ok");

    let request_id = response
        .metadata()
        .get("x-request-id")
        .and_then(|val| val.to_str().ok())
        .map(str::to_string);
    assert!(request_id.is_some());
}
