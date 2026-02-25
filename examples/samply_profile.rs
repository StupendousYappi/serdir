// Copyright (c) 2025 The serdir developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! An example for profiling with `samply`.
//! Serves a 1MB file from a temporary directory and performs 100,000 requests.

use anyhow::{Context, Result};
use hyper::server::conn;
use hyper_util::rt::TokioIo;
use serdir::{CacheSettings, ServedDir};
use std::fs::File;
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Instant;
use tempfile::TempDir;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    // 1. Create a temp dir containing a 1mb file.
    let temp_dir = TempDir::new().context("failed to create temp dir")?;
    let file_path = temp_dir.path().join("file.bin");
    let mut file = File::create(&file_path).context("failed to create file")?;

    // Write 1MB of data (1024 * 1024 bytes)
    let buffer = vec![0u8; 1024];
    for _ in 0..1024 {
        file.write_all(&buffer).context("failed to write to file")?;
    }
    file.flush().context("failed to flush file")?;

    println!("Created 1MB file at {}", file_path.display());

    // 2. Start up a hyper web server and use a ServedDir to serve the temp dir.
    let served_dir = ServedDir::builder(temp_dir.path().to_str().unwrap())
        .context("failed to create ServedDir builder")?
        .cache_resources(CacheSettings::default())
        .build();

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let listener = TcpListener::bind(addr)
        .await
        .context("failed to bind listener")?;
    let local_addr = listener.local_addr()?;

    println!("Serving on http://{}", local_addr);

    let service = served_dir.into_hyper_service();

    // Spawn server task
    let server_task = tokio::spawn(async move {
        loop {
            let (tcp, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    eprintln!("accept error: {}", e);
                    continue;
                }
            };
            let service = service.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(tcp);
                if let Err(err) = conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await
                {
                    eprintln!("Error serving connection: {:?}", err);
                }
            });
        }
    });

    // 3. Have a `reqwest` HTTP client perform 100000 gets of the file.
    let client = reqwest::Client::new();
    let url = format!("http://{}/file.bin", local_addr);

    let iterations = 100_000;
    println!("Starting {} GET requests...", iterations);

    let start = Instant::now();
    for i in 1..=iterations {
        let resp = client.get(&url).send().await.context("GET failed")?;
        if !resp.status().is_success() {
            anyhow::bail!("GET failed with status: {}", resp.status());
        }
        let _bytes = resp.bytes().await.context("failed to read bytes")?;

        if i % 10_000 == 0 {
            println!("Completed {}/{} requests...", i, iterations);
        }
    }

    let duration = start.elapsed();
    println!("Finished {} requests in {:?}", iterations, duration);
    println!(
        "Average rps: {:.2}",
        iterations as f64 / duration.as_secs_f64()
    );

    // Abort server task before exiting
    server_task.abort();

    Ok(())
}
