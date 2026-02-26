// Copyright (c) 2016-2018 The http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use criterion::{criterion_group, criterion_main, Criterion};
use http::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use serdir::{Body, ServedDir};
use std::convert::Infallible;
use std::fs::File;
use std::io::Write;
use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tower::{Layer, Service};

#[derive(Clone)]
struct TimeService;

impl Service<Request<hyper::body::Incoming>> for TimeService {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: Request<hyper::body::Incoming>) -> Self::Future {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let body = Body::from(now.to_string());
        let resp = Response::new(body);
        std::future::ready(Ok(resp))
    }
}

struct BenchServer {
    addr: SocketAddr,
    _temp_dir: TempDir,
    shutdown_tx: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl BenchServer {
    fn new() -> Self {
        let temp_dir = tempfile::tempdir().unwrap();
        let tmppath = temp_dir.path().join("index.html");
        let mut tmpfile = File::create(tmppath).unwrap();
        tmpfile
            .write_all(b"<html><body>Hello content</body></html>")
            .unwrap();
        tmpfile.flush().unwrap();

        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let path = temp_dir.path().to_owned();

        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let _guard = rt.enter();

            let layer = ServedDir::builder(&path)
                .unwrap()
                .build()
                .into_tower_layer();

            let service = layer.layer(TimeService);

            rt.block_on(async {
                let addr = SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0));
                let listener = TcpListener::bind(addr).await.unwrap();
                addr_tx.send(listener.local_addr().unwrap()).unwrap();

                loop {
                    tokio::select! {
                        conn = listener.accept() => {
                            let (tcp, _) = conn.unwrap();
                            let io = TokioIo::new(tcp);
                            let service = service.clone();
                            tokio::task::spawn(async move {
                                let hyper_service = TowerToHyperService::new(service);
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, hyper_service)
                                    .await;
                            });
                        }
                        _ = &mut shutdown_rx => break,
                    }
                }
            });
        });

        let addr = addr_rx.recv().unwrap();
        Self {
            addr,
            _temp_dir: temp_dir,
            shutdown_tx: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}:{}{}", self.addr.ip(), self.addr.port(), path)
    }
}

impl Drop for BenchServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn bench_middleware(b: &mut criterion::Bencher, path: &'static str) {
    let server = BenchServer::new();
    let client = reqwest::Client::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let url = server.url(path);
    b.to_async(&rt).iter(|| async {
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(StatusCode::OK, resp.status());
        let _ = resp.bytes().await.unwrap();
    });
}

fn criterion_benchmark(c: &mut Criterion) {
    let mut g = c.benchmark_group("middleware");
    g.throughput(criterion::Throughput::Elements(1));
    g.bench_function("hit", |b| bench_middleware(b, "/index.html"));
    g.bench_function("miss", |b| bench_middleware(b, "/non-existent"));
    g.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(100)
        .warm_up_time(Duration::from_millis(1000))
        .measurement_time(Duration::from_secs(5));
    targets = criterion_benchmark
}
criterion_main!(benches);
