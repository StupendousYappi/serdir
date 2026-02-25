// Copyright (c) 2016-2018 The http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use criterion::{criterion_group, criterion_main, Criterion};
use hyper_util::rt::TokioIo;
use serdir::resource_cache::CacheSettings;
use serdir::ServedDir;
use std::fs::File;
use std::io::Write;
use std::net::SocketAddr;
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

struct BenchServer {
    addr: SocketAddr,
    _temp_dir: TempDir,
    shutdown_tx: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl BenchServer {
    fn new(kib: usize) -> Self {
        let temp_dir = tempfile::tempdir().unwrap();
        let tmppath = temp_dir.path().join("f");
        let mut tmpfile = File::create(tmppath).unwrap();
        for _ in 0..kib {
            tmpfile.write_all(&[0; 1024]).unwrap();
        }
        tmpfile.flush().unwrap();

        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let path = temp_dir.path().to_owned();

        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let _guard = rt.enter();
            let cache_settings = CacheSettings::new()
                .max_total_weight(5_000_000)
                .max_item_weight(2_000_000);
            let service = ServedDir::builder(&path)
                .unwrap()
                .cache_resources(cache_settings)
                .build()
                .into_hyper_service();
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
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, service)
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

    fn url(&self) -> String {
        format!("http://{}:{}/f", self.addr.ip(), self.addr.port())
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

fn serve_full_entity(b: &mut criterion::Bencher, kib: &usize) {
    let server = BenchServer::new(*kib);
    let client = reqwest::Client::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let url = server.url();
    b.to_async(&rt).iter(|| async {
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(reqwest::StatusCode::OK, resp.status());
        let b = resp.bytes().await.unwrap();
        assert_eq!(1024 * *kib, b.len());
    });
}

fn serve_last_byte_1mib(b: &mut criterion::Bencher) {
    let server = BenchServer::new(1024);
    let client = reqwest::Client::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let url = server.url();
    b.to_async(&rt).iter(|| async {
        let resp = client
            .get(&url)
            .header("Range", "bytes=-1")
            .send()
            .await
            .unwrap();
        assert_eq!(reqwest::StatusCode::PARTIAL_CONTENT, resp.status());
        let b = resp.bytes().await.unwrap();
        assert_eq!(1, b.len());
    });
}

fn criterion_benchmark(c: &mut Criterion) {
    let mut g = c.benchmark_group("serve_full_entity");
    g.throughput(criterion::Throughput::Elements(1));
    g.bench_function("1kib-rps", |b| serve_full_entity(b, &1));
    g.bench_function("1mib-rps", |b| serve_full_entity(b, &1024));

    g.throughput(criterion::Throughput::Bytes(1024))
        .bench_function("1kib-bps", |b| serve_full_entity(b, &1));
    g.throughput(criterion::Throughput::Bytes(1024 * 1024))
        .bench_function("1mib-bps", |b| serve_full_entity(b, &1024));
    g.finish();

    let mut g = c.benchmark_group("serve_last_byte_1mib");
    g.throughput(criterion::Throughput::Elements(1));
    g.bench_function("rps", serve_last_byte_1mib);
    g.finish();
}

criterion_group! {
    name = benches;

    // Tweak the config to run more quickly; http-serve has many bench cases.
    config = Criterion::default()
        .sample_size(100)
        .warm_up_time(Duration::from_millis(1000))
        .measurement_time(Duration::from_secs(5));
    targets = criterion_benchmark
}
criterion_main!(benches);
