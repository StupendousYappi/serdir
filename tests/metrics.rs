#![cfg(feature = "metrics")]

use http::Request;
use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshot};
use serdir::ServedDir;

fn get_counter(snapshot: Snapshot, name: &str, labels: &[(&str, &str)]) -> u64 {
    for (key, _unit, _desc, value) in snapshot.into_vec() {
        if key.key().name() == name {
            let mut matches = true;
            for (k, v) in labels {
                if !key.key().labels().any(|l| l.key() == *k && l.value() == *v) {
                    matches = false;
                    break;
                }
            }
            if matches {
                if let DebugValue::Counter(c) = value {
                    return c;
                }
            }
        }
    }
    0
}

#[tokio::test(flavor = "multi_thread")]
async fn test_metrics_recorded() {
    // Install the recorder
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    recorder.install().unwrap();

    let td = tempfile::tempdir().unwrap();
    let p = td.path().join("index.html");
    std::fs::write(&p, "test file").unwrap();

    let served_dir = ServedDir::builder(td.path()).unwrap().build();

    let req = Request::get("/index.html")
        .body(serdir::Body::empty())
        .unwrap();
    let res = served_dir.get_response(&req).await.unwrap();
    assert_eq!(res.status(), 200);

    let count = get_counter(
        snapshotter.snapshot(),
        "serdir.request_count",
        &[
            ("status", "200"),
            ("content_type", "text/html"),
            ("encoding", "identity"),
        ],
    );

    let mut dbg_out = String::new();
    for (key, _, _, value) in snapshotter.snapshot().into_vec() {
        use std::fmt::Write;
        writeln!(&mut dbg_out, "{key:?} = {value:?}").unwrap();
    }

    assert_eq!(
        count, 1,
        "Expected 1 request count. Got metrics dump:\n{}",
        dbg_out
    );

    // Test Brotli compression metrics if feature enabled
    #[cfg(feature = "runtime-compression")]
    {
        use serdir::compression::CompressionStrategy;

        let served_dir = ServedDir::builder(td.path())
            .unwrap()
            .compression(CompressionStrategy::cached_compression())
            .build();

        let mut req = Request::get("/index.html")
            .body(serdir::Body::empty())
            .unwrap();
        req.headers_mut().insert(
            http::header::ACCEPT_ENCODING,
            http::HeaderValue::from_static("br"),
        );

        let res = served_dir.get_response(&req).await.unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(
            res.headers().get(http::header::CONTENT_ENCODING).unwrap(),
            "br"
        );

        let snapshot = snapshotter.snapshot();
        let mut found_before = false;
        let mut found_after = false;

        for (key, _, _, value) in snapshot.into_vec() {
            if key.key().name() == "serdir.brotli_size_before_bytes" {
                if let DebugValue::Counter(c) = value {
                    if c > 0 {
                        found_before = true;
                    }
                }
            }
            if key.key().name() == "serdir.brotli_size_after_bytes" {
                if let DebugValue::Counter(c) = value {
                    if c > 0 {
                        found_after = true;
                    }
                }
            }
        }
        assert!(found_before, "Brotli size before metric not found or zero");
        assert!(found_after, "Brotli size after metric not found or zero");
    }
}
