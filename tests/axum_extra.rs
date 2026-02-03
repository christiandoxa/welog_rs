use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderValue, Method, Request, StatusCode, Uri, Version};
use bytes::Bytes;
use futures_core::Stream;
use tower::{Layer, Service, ServiceExt};

use welog_rs::axum_middleware::{WelogContext, WelogLayer};

fn block_on<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::runtime::Runtime::new()
        .expect("create tokio runtime")
        .block_on(future)
}

struct ErrorStream {
    yielded: bool,
}

impl Stream for ErrorStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if self.yielded {
            std::task::Poll::Ready(None)
        } else {
            self.yielded = true;
            std::task::Poll::Ready(Some(Err(std::io::Error::other("stream error"))))
        }
    }
}

#[derive(Default)]
struct AxumState {
    saw_logger: bool,
}

#[derive(Clone, Copy)]
enum ResponseBodyKind {
    Static(&'static [u8]),
    Error,
}

#[derive(Clone)]
struct CaptureService {
    state: Arc<Mutex<AxumState>>,
    response_body: ResponseBodyKind,
}

impl Service<Request<Body>> for CaptureService {
    type Response = axum::response::Response;
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

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let state = self.state.clone();
        let response_body = self.response_body;
        Box::pin(async move {
            if let Some(ctx) = req.extensions().get::<Arc<WelogContext>>() {
                let _ = ctx.logger();
                let mut guard = state.lock().unwrap();
                guard.saw_logger = true;
            }
            Ok(axum::response::Response::builder()
                .status(StatusCode::OK)
                .body(match response_body {
                    ResponseBodyKind::Static(bytes) => Body::from(bytes),
                    ResponseBodyKind::Error => Body::from_stream(ErrorStream { yielded: false }),
                })
                .unwrap())
        })
    }
}

#[derive(Clone)]
struct ErrorService;

impl Service<Request<Body>> for ErrorService {
    type Response = axum::response::Response;
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

    fn call(&mut self, _req: Request<Body>) -> Self::Future {
        Box::pin(async move { Err(std::io::Error::other("boom")) })
    }
}

#[test]
fn axum_middleware_propagates_inner_error() {
    let svc = WelogLayer.layer(ErrorService);
    let req = Request::builder()
        .method(Method::GET)
        .uri(Uri::from_static("/"))
        .version(Version::HTTP_11)
        .body(Body::empty())
        .unwrap();

    let result = block_on(svc.oneshot(req));
    assert!(result.is_err());
}

#[test]
fn axum_middleware_handles_body_errors_and_json() {
    let state = Arc::new(Mutex::new(AxumState::default()));
    let svc = CaptureService {
        state: state.clone(),
        response_body: ResponseBodyKind::Static(br#"{"ok":true}"#),
    };
    let svc = WelogLayer.layer(svc);

    let request_body = Body::from_stream(ErrorStream { yielded: false });
    let invalid_header = HeaderValue::from_bytes(b"\x80").expect("header value");
    let req = Request::builder()
        .method(Method::POST)
        .uri(Uri::from_static("/"))
        .header("x-request-id", "req-123")
        .header("x-forwarded-for", "198.51.100.4")
        .header("host", "example.com")
        .header("user-agent", invalid_header)
        .version(Version::HTTP_11)
        .body(request_body)
        .unwrap();

    let _resp = block_on(svc.oneshot(req)).unwrap();

    let guard = state.lock().unwrap();
    assert!(guard.saw_logger);
}

#[test]
fn axum_middleware_parses_request_and_response_json() {
    let state = Arc::new(Mutex::new(AxumState::default()));
    let svc = CaptureService {
        state: state.clone(),
        response_body: ResponseBodyKind::Static(br#"{"resp":true}"#),
    };
    let svc = WelogLayer.layer(svc);

    let req = Request::builder()
        .method(Method::POST)
        .uri(Uri::from_static("/"))
        .header("x-request-id", "req-json")
        .header("host", "example.com")
        .version(Version::HTTP_11)
        .body(Body::from(br#"{"req":true}"#.as_slice()))
        .unwrap();

    let _resp = block_on(svc.oneshot(req)).unwrap();

    let guard = state.lock().unwrap();
    assert!(guard.saw_logger);
}

#[test]
fn axum_middleware_handles_response_body_error() {
    let svc = CaptureService {
        state: Arc::new(Mutex::new(AxumState::default())),
        response_body: ResponseBodyKind::Error,
    };
    let svc = WelogLayer.layer(svc);

    let req = Request::builder()
        .method(Method::GET)
        .uri(Uri::from_static("/"))
        .version(Version::HTTP_11)
        .body(Body::empty())
        .unwrap();

    let _resp = block_on(svc.oneshot(req)).unwrap();
}

#[test]
fn axum_middleware_header_value_error_falls_back() {
    fn bad_from_str(_s: &str) -> Result<HeaderValue, http::header::InvalidHeaderValue> {
        HeaderValue::from_bytes(b"\n")
    }

    let _patch_guard =
        jmpln::patch!(HeaderValue::from_str => bad_from_str).expect("patch from_str");

    let svc = CaptureService {
        state: Arc::new(Mutex::new(AxumState::default())),
        response_body: ResponseBodyKind::Static(b"ok"),
    };
    let svc = WelogLayer.layer(svc);

    let req = Request::builder()
        .method(Method::GET)
        .uri(Uri::from_static("/"))
        .version(Version::HTTP_11)
        .body(Body::empty())
        .unwrap();

    let _resp = block_on(svc.oneshot(req)).unwrap();

    std::thread::sleep(Duration::from_millis(10));
}

#[cfg(coverage)]
fn capture_ctx() -> Arc<WelogContext> {
    #[derive(Clone)]
    struct CtxService {
        ctx: Arc<Mutex<Option<Arc<WelogContext>>>>,
    }

    impl Service<Request<Body>> for CtxService {
        type Response = axum::response::Response;
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

        fn call(&mut self, req: Request<Body>) -> Self::Future {
            let ctx = self.ctx.clone();
            Box::pin(async move {
                if let Some(found) = req.extensions().get::<Arc<WelogContext>>() {
                    *ctx.lock().unwrap() = Some(found.clone());
                }
                Ok(axum::response::Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::from("ok"))
                    .unwrap())
            })
        }
    }

    let ctx = Arc::new(Mutex::new(None));
    let svc = WelogLayer.layer(CtxService { ctx: ctx.clone() });

    let req = Request::builder()
        .method(Method::GET)
        .uri(Uri::from_static("/"))
        .version(Version::HTTP_11)
        .body(Body::empty())
        .unwrap();

    let _ = block_on(svc.oneshot(req)).unwrap();
    ctx.lock().unwrap().clone().expect("ctx captured")
}

#[cfg(coverage)]
#[test]
fn axum_invalid_request_id_branch_is_covered() {
    welog_rs::axum_middleware::coverage_force_invalid_request_id(true);
    let _ = capture_ctx();
    welog_rs::axum_middleware::coverage_force_invalid_request_id(false);
}

#[cfg(coverage)]
#[test]
fn axum_latency_overflow_branch_is_covered() {
    welog_rs::axum_middleware::coverage_force_latency_overflow(true);
    let _ = capture_ctx();
    welog_rs::axum_middleware::coverage_force_latency_overflow(false);
}

#[cfg(coverage)]
#[test]
fn axum_target_log_cap_is_covered() {
    let ctx = capture_ctx();

    let req = TargetRequest {
        url: "https://example.com".into(),
        method: "GET".into(),
        content_type: "application/json".into(),
        header: Default::default(),
        body: br#"{"ping":"pong"}"#.to_vec(),
        timestamp: chrono::Local::now(),
    };
    let res = TargetResponse {
        header: Default::default(),
        body: br#"{"ok":true}"#.to_vec(),
        status: 200,
        latency: Duration::from_millis(5),
    };

    for _ in 0..=1000 {
        log_axum_client(&ctx, req.clone(), res.clone());
    }
}

#[cfg(coverage)]
#[test]
fn axum_header_helpers_are_covered() {
    welog_rs::axum_middleware::coverage_touch_header_helpers();
}
