# serdir

[![crates.io](https://img.shields.io/crates/v/serdir.svg)](https://crates.io/crates/serdir)
[![Released API docs](https://docs.rs/serdir/badge.svg)](https://docs.rs/serdir/)
[![CI](https://github.com/StupendousYappi/serdir/workflows/CI/badge.svg)](https://github.com/StupendousYappi/serdir/actions?query=workflow%3ACI)

Rust helpers for serving HTTP GET and HEAD responses with
[hyper](https://crates.io/crates/hyper) 1.x and
[tokio](https://crates.io/crates/tokio).

This crate provides utilities for serving static files in Rust web applications.

# Features

- Range requests
- Large file support via chunked streaming
- Live content changes
- ETag header generation and conditional GET requests
- Serving files that have been pre-compressed using gzip, brotli or zstd
- Cached runtime compression of files using brotli
- Content type detection based on filename extensions
- Serving directory paths using `index.html` pages
- Customizing 404 response content
- Support for the Tower [`Service`](https://docs.rs/tower/latest/tower/trait.Service.html) and [`Layer`](https://docs.rs/tower/latest/tower/trait.Layer.html) APIs

This crate is derived from [http-serve](https://github.com/scottlamb/http-serve/).

# Example

Serve files via Hyper:

```rust
use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use serdir::ServedDir;
use serdir::compression::BrotliLevel;
use std::net::{Ipv4Addr, SocketAddr};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() {
    let service = ServedDir::builder("./static")
        .unwrap()
        .append_index_html(true)
        .cached_compression(BrotliLevel::L5)
        .build()
        .into_hyper_service();

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 1337));
    let listener = TcpListener::bind(addr).await.unwrap();

    loop {
        let (tcp, _) = listener.accept().await.unwrap();
        let service = service.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(tcp);
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service)
                .await
            {
                eprintln!("connection error: {err}");
            }
        });
    }
}
```

Serve files via Tower:

```rust
use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use serdir::ServedDir;
use std::net::{Ipv4Addr, SocketAddr};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() {
    let service = ServedDir::builder("./static")
        .unwrap()
        .append_index_html(true)
        .static_compression(false, true, false) // gzip only
        .strip_prefix("/static")
        .build()
        .into_tower_service();

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 1337));
    let listener = TcpListener::bind(addr).await.unwrap();

    loop {
        let (tcp, _) = listener.accept().await.unwrap();
        let service = service.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(tcp);
            let hyper_service = TowerToHyperService::new(service);
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, hyper_service)
                .await
            {
                eprintln!("connection error: {err}");
            }
        });
    }
}
```

Run the builtin example:

```
$ cargo run --example hyper --features hyper .
```

## Optional features

This project defines 3 optional build features that can be enabled at compile time:

- `runtime-compression` - enables the cached compression strategy, i.e. runtime
compression of served files using Brotli
- `tower` - enables integration with Tower-based web frameworks like Axum and
Poem by providing APIs to convert a `ServedDir` into a `tower::Service` or
`tower::Layer`
- `hyper` - enables direct integration with the `hyper` web server by providing
APIs to convert a `ServedDir` into a `hyper::Service`

## Authors

See the [AUTHORS](AUTHORS) file for details.

## License

Your choice of MIT or Apache; see [LICENSE-MIT.txt](LICENSE-MIT.txt) or
[LICENSE-APACHE](LICENSE-APACHE.txt), respectively.
