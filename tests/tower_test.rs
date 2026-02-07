#![cfg(feature = "tower")]

use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serve_files::Body;
use serve_files::ServedDir;
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use tower::{Layer, Service};

#[derive(Clone)]
struct FallbackService {
    status: StatusCode,
    body: &'static str,
    hits: Arc<AtomicUsize>,
}

impl Service<Request<()>> for FallbackService {
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future =
        futures_core::future::BoxFuture<'static, Result<http::Response<Body>, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: Request<()>) -> Self::Future {
        let hits = self.hits.clone();
        let status = self.status;
        let body = self.body;
        Box::pin(async move {
            hits.fetch_add(1, Ordering::SeqCst);
            Ok(http::Response::builder()
                .status(status)
                .body(Body::from(body))
                .unwrap())
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_service() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path();
    std::fs::write(path.join("index.html"), "Hello, world!").unwrap();

    let mut service = ServedDir::builder(path.to_path_buf())
        .unwrap()
        .build()
        .into_tower_service();

    // Successful request
    let req = Request::builder().uri("/index.html").body(()).unwrap();

    let response = service.call(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body();
    let body_bytes = body.collect().await.unwrap().to_bytes();
    assert_eq!(body_bytes, "Hello, world!");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path();

    let mut service = ServedDir::builder(path.to_path_buf())
        .unwrap()
        .build()
        .into_tower_service();

    // Not found request
    let req = Request::builder().uri("/missing.txt").body(()).unwrap();

    let response = service.call(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_directory_handling() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path();
    std::fs::create_dir(path.join("subdir")).unwrap();

    let mut service = ServedDir::builder(path.to_path_buf())
        .unwrap()
        .build()
        .into_tower_service();

    // Directory request -> Should result in 404 (ServedDir::get returns IsDirectory, mapped to 404 in tower.rs)
    // unless index.html is present and append_index_html is true, but here it is not.
    let req = Request::builder().uri("/subdir").body(()).unwrap();

    let response = service.call(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_layer_serves_file_without_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path();
    std::fs::write(path.join("index.html"), "Hello, layer!").unwrap();

    let layer = ServedDir::builder(path.to_path_buf())
        .unwrap()
        .build()
        .into_tower_layer();

    let hits = Arc::new(AtomicUsize::new(0));
    let fallback = FallbackService {
        status: StatusCode::IM_A_TEAPOT,
        body: "fallback",
        hits: hits.clone(),
    };
    let mut service = layer.layer(fallback);

    let req = Request::builder().uri("/index.html").body(()).unwrap();
    let response = service.call(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(hits.load(Ordering::SeqCst), 0);

    let body = response.into_body();
    let body_bytes = body.collect().await.unwrap().to_bytes();
    assert_eq!(body_bytes, "Hello, layer!");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_layer_falls_through_on_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path();

    let layer = ServedDir::builder(path.to_path_buf())
        .unwrap()
        .build()
        .into_tower_layer();

    let hits = Arc::new(AtomicUsize::new(0));
    let fallback = FallbackService {
        status: StatusCode::IM_A_TEAPOT,
        body: "fallback",
        hits: hits.clone(),
    };
    let mut service = layer.layer(fallback);

    let req = Request::builder().uri("/missing.txt").body(()).unwrap();
    let response = service.call(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_layer_falls_through_on_invalid_path() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path();

    let layer = ServedDir::builder(path.to_path_buf())
        .unwrap()
        .build()
        .into_tower_layer();

    let hits = Arc::new(AtomicUsize::new(0));
    let fallback = FallbackService {
        status: StatusCode::IM_A_TEAPOT,
        body: "invalid path fallback",
        hits: hits.clone(),
    };
    let mut service = layer.layer(fallback);

    let req = Request::builder().uri("/../secret.txt").body(()).unwrap();
    let response = service.call(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_layer_uses_not_found_path_instead_of_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path();
    std::fs::write(path.join("404.html"), "custom 404").unwrap();

    let layer = ServedDir::builder(path.to_path_buf())
        .unwrap()
        .not_found_path("404.html")
        .unwrap()
        .build()
        .into_tower_layer();

    let hits = Arc::new(AtomicUsize::new(0));
    let fallback = FallbackService {
        status: StatusCode::IM_A_TEAPOT,
        body: "fallback",
        hits: hits.clone(),
    };
    let mut service = layer.layer(fallback);

    let req = Request::builder().uri("/missing.txt").body(()).unwrap();
    let response = service.call(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(hits.load(Ordering::SeqCst), 0);

    let body = response.into_body();
    let body_bytes = body.collect().await.unwrap().to_bytes();
    assert_eq!(body_bytes, "custom 404");
}
