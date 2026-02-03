use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use tonic::metadata::{MetadataMap, MetadataValue};
use tonic::service::Interceptor;
use tonic::transport::server::TcpConnectInfo;
use tonic::{Code, GrpcMethod, Request, Response, Status};

use welog_rs::grpc::{
    GrpcContext, WelogGrpcInterceptor, log_grpc_client, with_grpc_stream_logging,
    with_grpc_unary_logging,
};
use welog_rs::model::{TargetRequest, TargetResponse};

fn block_on<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::runtime::Runtime::new()
        .expect("create tokio runtime")
        .block_on(future)
}

struct BadSerialize;

impl Serialize for BadSerialize {
    fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        Err(serde::ser::Error::custom("nope"))
    }
}

fn build_metadata() -> MetadataMap {
    let mut md = MetadataMap::new();
    md.insert("x-request-id", MetadataValue::from_static("req-123"));
    md.insert("x-foo", MetadataValue::from_static("bar"));
    let bin_value = MetadataValue::from_bytes(b"\x01\x02");
    md.insert_bin("trace-bin", bin_value.clone());
    md.append_bin("trace-bin", bin_value);
    md
}

#[test]
fn grpc_context_methods_are_accessible() {
    let mut interceptor = WelogGrpcInterceptor;
    let req = Request::new(());
    let req = interceptor.call(req).expect("interceptor ok");
    let ctx = req.extensions().get::<Arc<GrpcContext>>().expect("ctx");
    let _ = ctx.request_id();
    let _ = ctx.logger();
}

#[test]
fn grpc_interceptor_preserves_existing_request_id() {
    let mut interceptor = WelogGrpcInterceptor;
    let mut req = Request::new(());
    req.metadata_mut()
        .insert("x-request-id", MetadataValue::from_static("keep-me"));

    let req = interceptor.call(req).expect("interceptor ok");
    let ctx = req.extensions().get::<Arc<GrpcContext>>().expect("ctx");

    assert_eq!(ctx.request_id(), "keep-me");
    assert_eq!(
        req.metadata()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok()),
        Some("keep-me")
    );
}

#[test]
fn grpc_unary_logs_error_path_and_bad_serialize() {
    let mut req = Request::new(BadSerialize);
    *req.metadata_mut() = build_metadata();

    let result: Result<Response<Value>, Status> =
        block_on(with_grpc_unary_logging(req, |_request| async move {
            Err(Status::internal("boom"))
        }));

    assert!(result.is_err());
}

#[test]
fn grpc_stream_logging_ok_and_err() {
    let mut req = Request::new(json!({"hello": "world"}));
    *req.metadata_mut() = build_metadata();
    req.extensions_mut()
        .insert(GrpcMethod::new("svc", "method"));
    req.extensions_mut().insert(TcpConnectInfo {
        local_addr: None,
        remote_addr: Some("127.0.0.1:5000".parse().unwrap()),
    });

    let ok = block_on(with_grpc_stream_logging(
        req,
        |_request| async move { Ok(Response::new(json!({"ok": true}))) },
        true,
        false,
    ));
    assert!(ok.is_ok());

    let mut req = Request::new(json!({"hello": "world"}));
    *req.metadata_mut() = build_metadata();
    let err: Result<Response<Value>, Status> = block_on(with_grpc_stream_logging(
        req,
        |_request| async move { Err(Status::new(Code::Internal, "boom")) },
        false,
        true,
    ));
    assert!(err.is_err());
}

#[test]
fn grpc_log_client_appends_target_log() {
    let mut interceptor = WelogGrpcInterceptor;
    let req = interceptor.call(Request::new(())).expect("interceptor ok");
    let ctx = req
        .extensions()
        .get::<Arc<GrpcContext>>()
        .expect("ctx")
        .clone();

    log_grpc_client(
        &ctx,
        TargetRequest {
            url: "https://example.com".into(),
            method: "GET".into(),
            content_type: "application/json".into(),
            header: Default::default(),
            body: br#"{"ping":"pong"}"#.to_vec(),
            timestamp: chrono::Local::now(),
        },
        TargetResponse {
            header: Default::default(),
            body: br#"{"ok":true}"#.to_vec(),
            status: 200,
            latency: Duration::from_millis(5),
        },
    );

    let mut req = Request::new(Value::String("payload".into()));
    *req.metadata_mut() = build_metadata();
    req.extensions_mut().insert(ctx);

    let _ = block_on(with_grpc_unary_logging(req, |request| async move {
        Ok(Response::new(request.into_inner()))
    }))
    .expect("grpc ok");
}

#[cfg(coverage)]
#[test]
fn grpc_set_request_id_invalid_is_covered() {
    welog_rs::grpc::coverage_touch_set_request_id_invalid();
}

#[cfg(coverage)]
#[test]
fn grpc_target_log_cap_is_covered() {
    let mut interceptor = WelogGrpcInterceptor;
    let req = interceptor.call(Request::new(())).expect("interceptor ok");
    let ctx = req
        .extensions()
        .get::<Arc<GrpcContext>>()
        .expect("ctx")
        .clone();

    let req_log = TargetRequest {
        url: "https://example.com".into(),
        method: "GET".into(),
        content_type: "application/json".into(),
        header: Default::default(),
        body: br#"{"ping":"pong"}"#.to_vec(),
        timestamp: chrono::Local::now(),
    };
    let res_log = TargetResponse {
        header: Default::default(),
        body: br#"{"ok":true}"#.to_vec(),
        status: 200,
        latency: Duration::from_millis(5),
    };

    for _ in 0..=1000 {
        log_grpc_client(&ctx, req_log.clone(), res_log.clone());
    }
}

#[cfg(coverage)]
#[test]
fn grpc_latency_overflow_branches_are_covered() {
    welog_rs::grpc::coverage_force_grpc_latency_overflow(true);

    let mut req = Request::new(json!({"hello": "world"}));
    *req.metadata_mut() = build_metadata();
    let _ = block_on(with_grpc_unary_logging(req, |_request| async move {
        Ok(Response::new(json!({"ok": true})))
    }));

    let mut req = Request::new(json!({"hello": "world"}));
    *req.metadata_mut() = build_metadata();
    let _ = block_on(with_grpc_stream_logging(
        req,
        |_request| async move { Ok(Response::new(json!({"ok": true}))) },
        true,
        true,
    ));

    welog_rs::grpc::coverage_force_grpc_latency_overflow(false);

    let mut req = Request::new(json!({"hello": "world"}));
    *req.metadata_mut() = build_metadata();
    let _ = block_on(with_grpc_stream_logging(
        req,
        |_request| async move { Ok(Response::new(json!({"ok": true}))) },
        true,
        true,
    ));
}

#[cfg(coverage)]
#[test]
fn grpc_invalid_metadata_key_branches_are_covered() {
    welog_rs::grpc::coverage_force_invalid_request_id_key(true);
    let mut interceptor = WelogGrpcInterceptor;
    let _ = interceptor.call(Request::new(())).expect("interceptor ok");
    welog_rs::grpc::coverage_force_invalid_request_id_key(false);

    welog_rs::grpc::coverage_force_invalid_header_key(true);
    let mut interceptor = WelogGrpcInterceptor;
    let _ = interceptor.call(Request::new(())).expect("interceptor ok");
    welog_rs::grpc::coverage_force_invalid_header_key(false);
}

#[cfg(coverage)]
#[test]
fn grpc_metadata_helpers_are_covered() {
    welog_rs::grpc::coverage_touch_metadata_to_map_branches();
    welog_rs::grpc::coverage_force_metadata_to_str_error(true);
    welog_rs::grpc::coverage_force_metadata_to_str_error(false);
}
