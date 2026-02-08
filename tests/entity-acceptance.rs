// Copyright (c) 2016-2018 The http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![cfg(feature = "hyper")]

use hyper_util::rt::TokioIo;
use once_cell::sync::Lazy;
use serve_files::ServedDir;
use std::fs::File;
use std::io::{Error, ErrorKind};
use std::time::Duration;
use tokio::net::TcpListener;

const WITH_ETAG_BODY: &[u8] = b"01234567890123456789";
const WITHOUT_ETAG_BODY: &[u8] = b"0123456789";
const FIXED_ETAG: &str = r#""0000000000001234""#;
const MIME: &str = "text/plain";
const SOME_DATE_STR: &str = "Sun, 06 Nov 1994 08:49:37 GMT";
const LATER_DATE_STR: &str = "Sun, 06 Nov 1994 09:49:37 GMT";

fn test_file_hasher(file: &File) -> Result<Option<u64>, std::io::Error> {
    match file.metadata()?.len() {
        20 => Ok(Some(0x1234)),
        10 => Ok(None),
        len => Err(Error::new(
            ErrorKind::InvalidData,
            format!("unexpected test file length: {len}"),
        )),
    }
}

fn new_server() -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("with_etag.txt"), WITH_ETAG_BODY).unwrap();
        std::fs::write(tmp.path().join("without_etag.txt"), WITHOUT_ETAG_BODY).unwrap();
        let service = ServedDir::builder(tmp.path().to_path_buf())
            .unwrap()
            .file_hasher(test_file_hasher)
            .build()
            .into_hyper_service();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        rt.block_on(async move {
            let _tmp = tmp;
            let addr = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0));
            let listener = TcpListener::bind(addr).await.unwrap();
            let addr = listener.local_addr().unwrap();
            tx.send(addr).unwrap();
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let io = TokioIo::new(tcp);
                let service = service.clone();
                tokio::task::spawn(async move {
                    hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .await
                        .unwrap();
                });
            }
        });
    });
    let addr = rx.recv().unwrap();
    format!("http://{}:{}", addr.ip(), addr.port())
}

static SERVER: Lazy<String> = Lazy::new(new_server);

#[tokio::test]
async fn serve_without_etag() {
    let client = reqwest::Client::new();
    let url = format!("{}/without_etag.txt", *SERVER);

    // Full body.
    let resp = client.get(&url).send().await.unwrap();
    let if_modified_since = httpdate::fmt_http_date(
        httpdate::parse_http_date(
            resp.headers()
                .get(reqwest::header::LAST_MODIFIED)
                .unwrap()
                .to_str()
                .unwrap(),
        )
        .unwrap()
            + Duration::from_secs(1),
    );
    assert_eq!(reqwest::StatusCode::OK, resp.status());
    assert_eq!(
        resp.headers().get(reqwest::header::CONTENT_TYPE).unwrap(),
        MIME
    );
    assert!(resp.headers().get(reqwest::header::ETAG).is_none());
    assert!(resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .is_some());
    assert_eq!(resp.headers().get(reqwest::header::CONTENT_RANGE), None);
    let buf = resp.bytes().await.unwrap();
    assert_eq!(WITHOUT_ETAG_BODY, &buf[..]);

    // If-Match any should still send the full body.
    let resp = client
        .get(&url)
        .header("If-Match", "*")
        .send()
        .await
        .unwrap();
    assert_eq!(reqwest::StatusCode::OK, resp.status());
    assert_eq!(
        resp.headers().get(reqwest::header::CONTENT_TYPE).unwrap(),
        MIME
    );
    assert!(resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .is_some());
    assert_eq!(resp.headers().get(reqwest::header::CONTENT_RANGE), None);
    let buf = resp.bytes().await.unwrap();
    assert_eq!(WITHOUT_ETAG_BODY, &buf[..]);

    // If-Match by etag doesn't match (as this request has no etag).
    let resp = client
        .get(&url)
        .header("If-Match", "\"foo\"")
        .send()
        .await
        .unwrap();
    assert_eq!(reqwest::StatusCode::PRECONDITION_FAILED, resp.status());

    // If-None-Match any.
    let resp = client
        .get(&url)
        .header("If-None-Match", "*")
        .send()
        .await
        .unwrap();
    assert_eq!(reqwest::StatusCode::NOT_MODIFIED, resp.status());
    assert_eq!(resp.headers().get(reqwest::header::CONTENT_RANGE), None);
    let buf = resp.bytes().await.unwrap();
    assert_eq!(b"", &buf[..]);

    // If-None-Match by etag doesn't match (as this request has no etag).
    let resp = client
        .get(&url)
        .header("If-None-Match", "\"foo\"")
        .send()
        .await
        .unwrap();
    assert_eq!(reqwest::StatusCode::OK, resp.status());
    assert_eq!(
        resp.headers().get(reqwest::header::CONTENT_TYPE).unwrap(),
        MIME
    );
    assert_eq!(resp.headers().get(reqwest::header::CONTENT_RANGE), None);
    let buf = resp.bytes().await.unwrap();
    assert_eq!(WITHOUT_ETAG_BODY, &buf[..]);

    // Unmodified since supplied date.
    let resp = client
        .get(&url)
        .header("If-Modified-Since", if_modified_since)
        .send()
        .await
        .unwrap();
    assert_eq!(reqwest::StatusCode::NOT_MODIFIED, resp.status());
    assert_eq!(resp.headers().get(reqwest::header::CONTENT_RANGE), None);
    let buf = resp.bytes().await.unwrap();
    assert_eq!(b"", &buf[..]);

    // Range serving - basic case.
    let resp = client
        .get(&url)
        .header("Range", "bytes=1-3")
        .send()
        .await
        .unwrap();
    assert_eq!(reqwest::StatusCode::PARTIAL_CONTENT, resp.status());
    assert!(resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .is_some());
    assert_eq!(
        resp.headers().get(reqwest::header::CONTENT_RANGE).unwrap(),
        &format!("bytes 1-3/{}", WITHOUT_ETAG_BODY.len())
    );
    let buf = resp.bytes().await.unwrap();
    assert_eq!(b"123", &buf[..]);

    // Range serving - multiple ranges.
    let resp = client
        .get(&url)
        .header("Range", "bytes=0-1, 3-4")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers().get(reqwest::header::CONTENT_RANGE), None);
    assert_eq!(reqwest::StatusCode::OK, resp.status());
    assert!(resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .is_some());
    assert_eq!(
        resp.headers().get(reqwest::header::CONTENT_TYPE).unwrap(),
        MIME
    );
    let buf = resp.bytes().await.unwrap();
    assert_eq!(WITHOUT_ETAG_BODY, &buf[..]);

    // Range serving - not satisfiable.
    let resp = client
        .get(&url)
        .header("Range", "bytes=500-")
        .send()
        .await
        .unwrap();
    assert_eq!(reqwest::StatusCode::RANGE_NOT_SATISFIABLE, resp.status());
    assert_eq!(
        resp.headers().get(reqwest::header::CONTENT_RANGE).unwrap(),
        &format!("bytes */{}", WITHOUT_ETAG_BODY.len())
    );
    let buf = resp.bytes().await.unwrap();
    assert_eq!(b"", &buf[..]);

    // Range serving - matching If-Range by date doesn't honor the range.
    let resp = client
        .get(&url)
        .header("Range", "bytes=1-3")
        .header("If-Range", SOME_DATE_STR)
        .send()
        .await
        .unwrap();
    assert_eq!(reqwest::StatusCode::OK, resp.status());
    assert_eq!(
        resp.headers().get(reqwest::header::CONTENT_TYPE).unwrap(),
        MIME
    );
    let buf = resp.bytes().await.unwrap();
    assert_eq!(WITHOUT_ETAG_BODY, &buf[..]);

    // Range serving - non-matching If-Range by date ignores the range.
    let resp = client
        .get(&url)
        .header("Range", "bytes=1-3")
        .header("If-Range", LATER_DATE_STR)
        .send()
        .await
        .unwrap();
    assert_eq!(reqwest::StatusCode::OK, resp.status());
    assert_eq!(
        resp.headers().get(reqwest::header::CONTENT_TYPE).unwrap(),
        MIME
    );
    assert_eq!(resp.headers().get(reqwest::header::CONTENT_RANGE), None);
    let buf = resp.bytes().await.unwrap();
    assert_eq!(WITHOUT_ETAG_BODY, &buf[..]);

    // Range serving - this resource has no etag, so any If-Range by etag ignores the range.
    let resp = client
        .get(&url)
        .header("Range", "bytes=1-3")
        .header("If-Range", FIXED_ETAG)
        .send()
        .await
        .unwrap();
    assert_eq!(reqwest::StatusCode::OK, resp.status());
    assert_eq!(
        resp.headers().get(reqwest::header::CONTENT_TYPE).unwrap(),
        MIME
    );
    assert_eq!(resp.headers().get(reqwest::header::CONTENT_RANGE), None);
    let buf = resp.bytes().await.unwrap();
    assert_eq!(WITHOUT_ETAG_BODY, &buf[..]);
}

#[tokio::test]
async fn serve_with_strong_etag() {
    let client = reqwest::Client::new();
    let url = format!("{}/with_etag.txt", *SERVER);

    // If-Match any should still send the full body.
    let resp = client
        .get(&url)
        .header("If-Match", "*")
        .send()
        .await
        .unwrap();
    assert_eq!(reqwest::StatusCode::OK, resp.status());
    assert_eq!(
        resp.headers().get(reqwest::header::CONTENT_TYPE).unwrap(),
        MIME
    );
    assert_eq!(
        resp.headers().get(reqwest::header::ETAG).unwrap(),
        FIXED_ETAG
    );
    assert_eq!(resp.headers().get(reqwest::header::CONTENT_RANGE), None);
    assert_eq!(WITH_ETAG_BODY, &resp.bytes().await.unwrap()[..]);

    // If-Match by matching etag should send the full body.
    let resp = client
        .get(&url)
        .header("If-Match", FIXED_ETAG)
        .send()
        .await
        .unwrap();
    assert_eq!(reqwest::StatusCode::OK, resp.status());
    assert_eq!(
        resp.headers().get(reqwest::header::CONTENT_TYPE).unwrap(),
        MIME
    );
    assert_eq!(resp.headers().get(reqwest::header::CONTENT_RANGE), None);
    assert_eq!(WITH_ETAG_BODY, &resp.bytes().await.unwrap()[..]);

    // If-Match by etag which doesn't match.
    let resp = client
        .get(&url)
        .header("If-Match", r#""bar""#)
        .send()
        .await
        .unwrap();
    assert_eq!(reqwest::StatusCode::PRECONDITION_FAILED, resp.status());

    // If-None-Match by etag which matches.
    let resp = client
        .get(&url)
        .header("If-None-Match", FIXED_ETAG)
        .send()
        .await
        .unwrap();
    assert_eq!(reqwest::StatusCode::NOT_MODIFIED, resp.status());
    assert_eq!(resp.headers().get(reqwest::header::CONTENT_RANGE), None);
    assert_eq!(b"", &resp.bytes().await.unwrap()[..]);

    // If-None-Match by etag which doesn't match.
    let resp = client
        .get(&url)
        .header("If-None-Match", r#""bar""#)
        .send()
        .await
        .unwrap();
    assert_eq!(reqwest::StatusCode::OK, resp.status());
    assert_eq!(resp.headers().get(reqwest::header::CONTENT_RANGE), None);
    assert_eq!(WITH_ETAG_BODY, &resp.bytes().await.unwrap()[..]);

    // If-None-Match by etag which doesn't match, If-Modified-Since which does.
    let resp = client
        .get(&url)
        .header("If-None-Match", r#""bar""#)
        .header("If-Modified-Since", LATER_DATE_STR)
        .send()
        .await
        .unwrap();
    assert_eq!(reqwest::StatusCode::OK, resp.status());
    assert_eq!(resp.headers().get(reqwest::header::CONTENT_RANGE), None);
    assert_eq!(WITH_ETAG_BODY, &resp.bytes().await.unwrap()[..]);

    // Range serving - If-Range matching by etag.
    let resp = client
        .get(&url)
        .header("Range", "bytes=1-3")
        .header("If-Range", FIXED_ETAG)
        .send()
        .await
        .unwrap();
    assert_eq!(reqwest::StatusCode::PARTIAL_CONTENT, resp.status());
    assert_eq!(None, resp.headers().get(reqwest::header::CONTENT_TYPE));
    assert_eq!(
        resp.headers().get(reqwest::header::CONTENT_RANGE).unwrap(),
        &format!("bytes 1-3/{}", WITH_ETAG_BODY.len())
    );
    assert_eq!(b"123", &resp.bytes().await.unwrap()[..]);

    // Range serving - If-Range not matching by etag.
    let resp = client
        .get(&url)
        .header("Range", "bytes=1-3")
        .header("If-Range", r#""bar""#)
        .send()
        .await
        .unwrap();
    assert_eq!(reqwest::StatusCode::OK, resp.status());
    assert_eq!(
        resp.headers().get(reqwest::header::CONTENT_TYPE).unwrap(),
        MIME
    );
    assert_eq!(resp.headers().get(reqwest::header::CONTENT_RANGE), None);
    assert_eq!(WITH_ETAG_BODY, &resp.bytes().await.unwrap()[..]);
}

#[tokio::test]
async fn serve_with_strong_etag_multiple_ranges() {
    let client = reqwest::Client::new();
    let url = format!("{}/with_etag.txt", *SERVER);

    let resp = client
        .get(&url)
        .header("Range", "bytes=0-5, 10-15")
        .send()
        .await
        .unwrap();
    // For this small file, multipart overhead exceeds full body length, so full body is sent.
    assert_eq!(reqwest::StatusCode::OK, resp.status());
    assert_eq!(resp.headers().get(reqwest::header::CONTENT_RANGE), None);
    assert!(resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .is_some());
    assert_eq!(
        resp.headers().get(reqwest::header::CONTENT_TYPE).unwrap(),
        MIME
    );
    let buf = resp.bytes().await.unwrap();
    assert_eq!(WITH_ETAG_BODY, &buf[..]);
}

// TODO: stream that returns too much data.
// TODO: stream that returns too little data.
