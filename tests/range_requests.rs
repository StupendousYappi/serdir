use bytes::Bytes;
use http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serdir::FileEntity;
use std::io::Write;
use tempfile::NamedTempFile;

async fn collect_body(body: serdir::Body) -> Bytes {
    body.collect().await.unwrap().to_bytes()
}

#[tokio::test(flavor = "multi_thread")]
async fn test_range_single() {
    let mut tmp = NamedTempFile::new().unwrap();
    tmp.write_all(b"0123456789abcdef").unwrap();

    let entity = FileEntity::new(tmp.path(), header::HeaderMap::new()).unwrap();
    let req = Request::get("/")
        .header(header::RANGE, "bytes=2-5")
        .body(())
        .unwrap();

    let res: http::Response<serdir::Body> = entity.serve_request(&req, StatusCode::OK);
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(res.headers().get(header::CONTENT_RANGE).unwrap(), "bytes 2-5/16");
    assert_eq!(res.headers().get(header::CONTENT_LENGTH).unwrap(), "4");

    let body = collect_body(res.into_body()).await;
    assert_eq!(body, &b"2345"[..]);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_range_not_satisfiable() {
    let mut tmp = NamedTempFile::new().unwrap();
    tmp.write_all(b"0123456789abcdef").unwrap();

    let entity = FileEntity::new(tmp.path(), header::HeaderMap::new()).unwrap();
    let req = Request::get("/")
        .header(header::RANGE, "bytes=20-")
        .body(())
        .unwrap();

    let res: http::Response<serdir::Body> = entity.serve_request(&req, StatusCode::OK);
    assert_eq!(res.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(res.headers().get(header::CONTENT_RANGE).unwrap(), "bytes */16");

    let body = collect_body(res.into_body()).await;
    assert!(body.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_range_multipart() {
    // To trigger multipart, we need a large enough file so that the estimated multipart
    // overhead (80 bytes per range) doesn't exceed the file size.
    // Let's use a 1000 byte file.
    let mut tmp = NamedTempFile::new().unwrap();
    let content = vec![0u8; 1000];
    tmp.write_all(&content).unwrap();

    let entity = FileEntity::new(tmp.path(), header::HeaderMap::new()).unwrap();
    let req = Request::get("/")
        .header(header::RANGE, "bytes=0-9, 20-29")
        .body(())
        .unwrap();

    let res: http::Response<serdir::Body> = entity.serve_request(&req, StatusCode::OK);
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    let content_type = res.headers().get(header::CONTENT_TYPE).unwrap().to_str().unwrap();
    assert!(content_type.starts_with("multipart/byteranges; boundary="));

    let body = collect_body(res.into_body()).await;
    // We don't want to parse the multipart body exactly here,
    // but we can check it contains the expected parts and the boundary.
    let boundary = content_type.strip_prefix("multipart/byteranges; boundary=").unwrap();
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains(&format!("--{}", boundary)));
    assert!(body_str.contains("Content-Range: bytes 0-9/1000"));
    assert!(body_str.contains("Content-Range: bytes 20-29/1000"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_if_range_strong_etag_match() {
    let mut tmp = NamedTempFile::new().unwrap();
    tmp.write_all(b"0123456789abcdef").unwrap();

    let entity = FileEntity::new(tmp.path(), header::HeaderMap::new()).unwrap();
    let etag = entity.etag().unwrap();

    let req = Request::get("/")
        .header(header::RANGE, "bytes=0-4")
        .header(header::IF_RANGE, etag.clone())
        .body(())
        .unwrap();

    let res: http::Response<serdir::Body> = entity.serve_request(&req, StatusCode::OK);
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(collect_body(res.into_body()).await, &b"01234"[..]);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_if_range_strong_etag_mismatch() {
    let mut tmp = NamedTempFile::new().unwrap();
    tmp.write_all(b"0123456789abcdef").unwrap();

    let entity = FileEntity::new(tmp.path(), header::HeaderMap::new()).unwrap();

    let req = Request::get("/")
        .header(header::RANGE, "bytes=0-4")
        .header(header::IF_RANGE, "\"mismatch\"")
        .body(())
        .unwrap();

    let res: http::Response<serdir::Body> = entity.serve_request(&req, StatusCode::OK);
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(collect_body(res.into_body()).await, &b"0123456789abcdef"[..]);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_if_range_weak_etag() {
    let mut tmp = NamedTempFile::new().unwrap();
    tmp.write_all(b"0123456789abcdef").unwrap();

    let entity = FileEntity::new(tmp.path(), header::HeaderMap::new()).unwrap();
    let etag = entity.etag().unwrap();
    let mut weak_etag = b"W/".to_vec();
    weak_etag.extend_from_slice(etag.as_bytes());

    let req = Request::get("/")
        .header(header::RANGE, "bytes=0-4")
        .header(header::IF_RANGE, weak_etag)
        .body(())
        .unwrap();

    let res: http::Response<serdir::Body> = entity.serve_request(&req, StatusCode::OK);
    // RFC 7233 Section 3.1: "A client MUST NOT generate an If-Range header field
    // containing a weak entity-tag"
    // Section 3.1: "A server MUST ignore an If-Range header field if the
    // entity-tag is weak" (implied by "MUST NOT generate" and strong validation rules)
    // In our implementation, we check if it starts with "W/\" or "\"".
    // If it is weak, it won't match.
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(collect_body(res.into_body()).await, &b"0123456789abcdef"[..]);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_if_range_date() {
    let mut tmp = NamedTempFile::new().unwrap();
    tmp.write_all(b"0123456789abcdef").unwrap();

    let entity = FileEntity::new(tmp.path(), header::HeaderMap::new()).unwrap();

    let req = Request::get("/")
        .header(header::RANGE, "bytes=0-4")
        .header(header::IF_RANGE, "Fri, 31 Dec 1999 23:59:59 GMT")
        .body(())
        .unwrap();

    let res: http::Response<serdir::Body> = entity.serve_request(&req, StatusCode::OK);
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_head_with_range() {
    let mut tmp = NamedTempFile::new().unwrap();
    tmp.write_all(b"0123456789abcdef").unwrap();

    let entity = FileEntity::new(tmp.path(), header::HeaderMap::new()).unwrap();
    let req = Request::head("/")
        .header(header::RANGE, "bytes=0-4")
        .body(())
        .unwrap();

    let res: http::Response<serdir::Body> = entity.serve_request(&req, StatusCode::OK);
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(res.headers().get(header::CONTENT_LENGTH).unwrap(), "5");
    assert!(collect_body(res.into_body()).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_range_too_small_for_multipart() {
    // If file is small, multipart overhead makes it better to serve the whole file.
    let mut tmp = NamedTempFile::new().unwrap();
    tmp.write_all(b"0123456789").unwrap(); // 10 bytes

    let entity = FileEntity::new(tmp.path(), header::HeaderMap::new()).unwrap();
    let req = Request::get("/")
        .header(header::RANGE, "bytes=0-1, 3-4")
        .body(())
        .unwrap();

    let res: http::Response<serdir::Body> = entity.serve_request(&req, StatusCode::OK);
    // Should be 200 OK because multipart would be > 10 bytes.
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(collect_body(res.into_body()).await, &b"0123456789"[..]);
}
