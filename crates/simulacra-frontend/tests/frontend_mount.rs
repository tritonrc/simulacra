use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

#[tokio::test]
async fn get_root_serves_index_html_with_spa_mount_point() {
    let router = simulacra_frontend::frontend_router();

    let response = router
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .expect("root response should include a valid content-type header");
    assert!(
        content_type.starts_with("text/html"),
        "expected text/html content-type, got {content_type}"
    );

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body = std::str::from_utf8(&body).unwrap();
    assert!(
        body.contains("<div id=\"app\""),
        "expected index.html to contain the SPA mount point"
    );
}

#[tokio::test]
async fn get_unknown_js_returns_404_without_spa_fallback() {
    let router = simulacra_frontend::frontend_router();

    let response = router
        .oneshot(Request::get("/unknown.js").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_index_html_directly_returns_200() {
    let router = simulacra_frontend::frontend_router();

    let response = router
        .oneshot(Request::get("/index.html").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}
