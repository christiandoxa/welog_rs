#[cfg(coverage)]
mod app {
    use std::process::Command;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use welog_rs::app::{build_app, root_handler};

    pub fn coverage_call_main_binary() {
        let status = Command::new(env!("CARGO_BIN_EXE_welog_rs"))
            .status()
            .expect("run welog_rs binary");
        assert!(status.success());
    }

    pub async fn coverage_call_root_handler() {
        let app = build_app().route("/alt", axum::routing::get(root_handler));
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
    }
}

#[cfg(coverage)]
#[test]
fn main_executes_under_coverage() {
    app::coverage_call_main_binary();
}

#[cfg(coverage)]
#[test]
fn root_handler_executes_under_coverage() {
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(app::coverage_call_root_handler());
}
