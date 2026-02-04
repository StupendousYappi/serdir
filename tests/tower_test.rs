#![cfg(feature = "tower")]

use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serve_files::served_dir::ServedDir;
use tower::Service;

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_service() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path();
    std::fs::write(path.join("index.html"), "Hello, world!").unwrap();

    let mut service = ServedDir::builder(path.to_path_buf())
        .unwrap()
        .build_tower_service();

    // Successful request
    let req = Request::builder()
        .uri("/index.html")
        .body(())
        .unwrap();

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
        .build_tower_service();

    // Not found request
    let req = Request::builder()
        .uri("/missing.txt")
        .body(())
        .unwrap();

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
        .build_tower_service();

    // Directory request -> Should result in 404 (ServedDir::get returns IsDirectory, mapped to 404 in tower.rs)
    // unless index.html is present and append_index_html is true, but here it is not.
    let req = Request::builder()
        .uri("/subdir")
        .body(())
        .unwrap();

    let response = service.call(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
