// Copyright (c) 2016-2018 The http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Benchmarks of serving data built in to the binary via `include_bytes!`, using the
//! `serve` function on an `Entity`.

use bytes::{Bytes, BytesMut};
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use futures_core::Stream;
use futures_util::{future, stream};
use http::header::HeaderValue;
use http::{Request, Response};
use hyper_util::rt::TokioIo;
use once_cell::sync::Lazy;
use serve_files::Body;
use std::convert::TryInto;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::ops::Range;
use std::pin::Pin;
use std::time::{Duration, SystemTime};
use tokio::net::TcpListener;

static WONDERLAND: &[u8] = include_bytes!("wonderland.txt");

struct BytesEntity(Bytes);

impl serve_files::Entity for BytesEntity {
    type Data = Bytes;
    type Error = std::io::Error;

    fn len(&self) -> u64 {
        self.0.len() as u64
    }
    fn get_range(
        &self,
        range: Range<u64>,
    ) -> Pin<Box<dyn Stream<Item = Result<Self::Data, Self::Error>> + Send + Sync>> {
        Box::pin(stream::once(future::ok(
            self.0
                .slice(range.start as usize..range.end as usize)
                .into(),
        )))
    }
    fn add_headers(&self, headers: &mut http::header::HeaderMap) {
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain"),
        );
    }
    fn etag(&self) -> Option<HeaderValue> {
        None
    }
    fn last_modified(&self) -> Option<SystemTime> {
        None
    }
}

// type Body = serve_files::Body;

async fn serve(req: Request<hyper::body::Incoming>) -> Result<Response<Body>, std::io::Error> {
    let path = req.uri().path();
    let resp = match path.as_bytes()[1] {
        b's' => {
            // static entity
            serve_files::serve(BytesEntity(Bytes::from_static(WONDERLAND)), &req)
        }
        b'c' => {
            // copied entity
            let mut b = BytesMut::with_capacity(WONDERLAND.len());
            b.extend_from_slice(WONDERLAND);
            serve_files::serve(BytesEntity(b.freeze()), &req)
        }
        _ => unreachable!(),
    };
    Ok(resp)
}

/// Returns the hostport of a newly-created, never-destructed server.
fn new_server() -> SocketAddr {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        rt.block_on(async {
            let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
            let listener = TcpListener::bind(addr).await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    tcp.set_nodelay(true).unwrap();
                    let io = TokioIo::new(tcp);
                    hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, hyper::service::service_fn(serve))
                        .await
                        .unwrap();
                });
            }
        });
    });
    rx.recv().unwrap()
}

static SERVER: Lazy<SocketAddr> = Lazy::new(new_server);

/// Benchmarks a `GET` request for the given path using raw sockets.
///
/// This avoids using `reqwest` to keep the measurement as focused on the server-side performance
/// as possible. It uses keepalives, not only to focus the measurement on the request but also
/// to avoid errors due to ephemeral port exhaustion. This requires some parsing with `httparse`
/// to read the correct amount of data.
fn get(b: &mut criterion::Bencher, path: &str) {
    let mut v = Vec::new();
    v.extend(b"GET /");
    v.extend(path.as_bytes());
    v.extend(&b" HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: gzip\r\n\r\n"[..]);

    // Add enough buffer space for the uncompressed representation and some headers.
    let mut buf = vec![0u8; WONDERLAND.len() + 8192];

    use socket2::{Domain, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    s.set_reuse_address(true).unwrap();
    s.set_nodelay(true).unwrap();
    s.connect(&(*SERVER).into()).unwrap();
    let mut s: std::net::TcpStream = s.into();
    b.iter(move || {
        s.write_all(&v[..]).unwrap();

        let mut hdrs_buf = [httparse::EMPTY_HEADER; 16];
        let mut resp = httparse::Response::new(&mut hdrs_buf);
        let mut end = s.read(&mut buf[..]).unwrap();
        let mut pos = resp.parse(&buf[..end]).unwrap().unwrap();
        assert_eq!(resp.code, Some(200));
        let mut hdr_len: Option<usize> = None;
        let mut chunked = false;
        for h in resp.headers {
            if h.name.eq_ignore_ascii_case("Content-Length") {
                assert!(!chunked);
                assert!(hdr_len.is_none());
                hdr_len = Some(std::str::from_utf8(h.value).unwrap().parse().unwrap());
            } else if h.name.eq_ignore_ascii_case("Transfer-Encoding") {
                assert!(!chunked);
                assert!(hdr_len.is_none());
                assert!(h.value == b"chunked");
                chunked = true;
            }
        }
        if let Some(l) = hdr_len {
            assert!(end <= pos + l, "end={} pos={} l={}", end, pos, l);
            if end < pos + l {
                s.read_exact(&mut buf[end..pos + l]).unwrap();
            }
            return;
        } else if !chunked {
            panic!("not chunked, no length");
        }
        loop {
            let r = match httparse::parse_chunk_size(&buf[pos..end]) {
                Err(e) => panic!(
                    "error={} pos={} end={} buf[pos..end]={:?}",
                    e,
                    pos,
                    end,
                    String::from_utf8_lossy(&buf[pos..end])
                ),
                Ok(r) => r,
            };
            match r {
                httparse::Status::Partial => {
                    end += s.read(&mut buf[end..]).unwrap();
                    continue;
                }
                httparse::Status::Complete((p, l)) => {
                    let l: usize = l.try_into().unwrap();
                    pos += p;
                    while end < pos + l + 2 {
                        end += s.read(&mut buf[end..]).unwrap();
                    }
                    assert_eq!(buf[pos + l..pos + l + 2], b"\r\n"[..]);
                    if l == 0 {
                        assert!(end == pos + l + 2);
                        return;
                    }
                    pos += l + 2;
                }
            }
        }
    });
}

fn criterion_benchmark(c: &mut Criterion) {
    let mut g = c.benchmark_group("serve");
    g.throughput(Throughput::Bytes(WONDERLAND.len() as u64));
    g.bench_function("static", |b| get(b, "s"));
    g.bench_function("copied", |b| get(b, "c"));
    g.finish();
}

criterion_group! {
    name = benches;

    // Tweak the config to run more quickly; http-serve has many bench cases.
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_secs(1));
    targets = criterion_benchmark
}
criterion_main!(benches);
