# serve-files

[![CI](https://github.com/StupendousYappi/serve-files/workflows/CI/badge.svg)](https://github.com/StupendousYappi/serve-files/actions?query=workflow%3ACI)

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
- Support for the Tower [Service](https://docs.rs/tower/latest/tower/trait.Service.html) and [Layer](https://docs.rs/tower/latest/tower/trait.Layer.html) APIs

This crate is derived from [http-serve](https://github.com/scottlamb/http-serve/).

Examples:

*   Serve a directory tree using hyper:
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
