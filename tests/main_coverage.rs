#[cfg(coverage)]
mod app {
    #![allow(dead_code)]
    include!("../src/main.rs");

    pub fn coverage_call_main() {
        main();
    }

    pub async fn coverage_call_root_handler() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let app = Router::new()
            .route("/", get(root_handler))
            .layer(WelogLayer);

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
    app::coverage_call_main();
}

#[cfg(coverage)]
#[test]
fn root_handler_executes_under_coverage() {
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(app::coverage_call_root_handler());
}
