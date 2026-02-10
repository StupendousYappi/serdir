#![cfg(feature = "hyper")]

use http::{Request, StatusCode};
use http_body_util::BodyExt;
use hyper::service::Service;
use serdir::ServedDir;

#[tokio::test(flavor = "multi_thread")]
async fn test_hyper_service_serves_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path();
    std::fs::write(path.join("index.html"), "Hello, hyper!").unwrap();

    let service = ServedDir::builder(path.to_path_buf())
        .unwrap()
        .build()
        .into_hyper_service();

    let req = Request::builder().uri("/index.html").body(()).unwrap();
    let response = service.call(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body();
    let body_bytes = body.collect().await.unwrap().to_bytes();
    assert_eq!(body_bytes, "Hello, hyper!");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_hyper_service_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path();

    let service = ServedDir::builder(path.to_path_buf())
        .unwrap()
        .build()
        .into_hyper_service();

    let req = Request::builder().uri("/missing.txt").body(()).unwrap();
    let response = service.call(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_hyper_service_directory_handling() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path();
    std::fs::create_dir(path.join("subdir")).unwrap();

    let service = ServedDir::builder(path.to_path_buf())
        .unwrap()
        .build()
        .into_hyper_service();

    let req = Request::builder().uri("/subdir").body(()).unwrap();
    let response = service.call(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
