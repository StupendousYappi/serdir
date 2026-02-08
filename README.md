# serve-files

[![crates.io](https://img.shields.io/crates/v/serve-files.svg)](https://crates.io/crates/serve-files)
[![Released API docs](https://docs.rs/serve-files/badge.svg)](https://docs.rs/serve-files/)
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

*   Serve a directory tree:
    ```
    $ cargo run --example serve_dir .
    ```

## Authors

See the [AUTHORS](AUTHORS) file for details.

## License

Your choice of MIT or Apache; see [LICENSE-MIT.txt](LICENSE-MIT.txt) or
[LICENSE-APACHE](LICENSE-APACHE.txt), respectively.
