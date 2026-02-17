// Copyright (c) 2020 The http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Serves a directory on `http://127.0.0.1:1337/` using `ServedDir::into_tower_service`
//! with a custom directory handler for listing directory contents.

mod common;

use anyhow::{Context, Result};
use bytes::Bytes;
use common::Config;
use http::HeaderValue;
use hyper::server::conn;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use serdir::{SerdirError, ServedDirBuilder};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use tokio::net::TcpListener;

fn custom_directory_listing(path: &Path) -> Result<(Bytes, HeaderValue), SerdirError> {
    let mut listing = String::new();
    listing.push_str("<!DOCTYPE html>\n<title>directory listing</title>\n<ul>\n");

    let mut entries = std::fs::read_dir(path)
        .and_then(|iter| iter.collect::<Result<Vec<_>, _>>())
        .map_err(SerdirError::IOError)?;
    entries.sort_unstable_by_key(|entry| entry.file_name());

    for entry in entries {
        let file_name = entry.file_name();
        let name = match file_name.to_str() {
            None => continue,
            Some(".") => continue,
            Some(name) => name,
        };

        listing.push_str("<li><a href=\"");
        listing.push_str(&htmlescape::encode_minimal(name));
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            listing.push('/');
        }
        listing.push_str("\">");
        listing.push_str(&htmlescape::encode_minimal(name));
        if is_dir {
            listing.push('/');
        }
        listing.push_str("</a></li>\n");
    }

    listing.push_str("</ul>\n");
    Ok((Bytes::from(listing), HeaderValue::from_static("text/html")))
}

#[tokio::main]
async fn main() -> Result<()> {
    run().await
}

async fn run() -> Result<()> {
    let config = Config::from_env();
    let mut builder = ServedDirBuilder::new(config.directory.as_str())
        .context("failed to create ServedDir builder")?
        .append_index_html(true)
        .compression(config.compression_strategy())
        .strip_prefix(config.strip_prefix.unwrap_or_default())
        .directory_handler(custom_directory_listing);
    if let Some(path) = config.not_found_path {
        builder = builder
            .not_found_path(path)
            .context("failed to set --not-found-path")?;
    }
    let served_dir = builder.build();
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 1337));
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;

    println!(
        "Serving {} on http://{}",
        served_dir.dir().display(),
        listener
            .local_addr()
            .context("failed to get listener address")?
    );
    let service = served_dir.into_tower_service();

    loop {
        let (tcp, _) = listener.accept().await.context("accept failed")?;
        let service = service.clone();
        tokio::spawn(async move {
            tcp.set_nodelay(true).context("failed to set TCP_NODELAY")?;
            let io = TokioIo::new(tcp);
            let hyper_service = TowerToHyperService::new(service);
            conn::http1::Builder::new()
                .serve_connection(io, hyper_service)
                .await
                .context("connection error")?;
            Ok::<(), anyhow::Error>(())
        });
    }
}
