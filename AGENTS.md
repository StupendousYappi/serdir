# serdir

This is a Rust crate that helps users serve static files over HTTP. The central
API it offers is `ServedDir` which represents a directory of static files that
can be served along with various configuration options, such as compression
settings and file not found behavior. A `ServedDir` can be created using a
`ServedDirBuilder`, and can either be used directly to handle HTTP requests or
can be converted into a `tower::Service`, a `tower::Layer` (i.e. middleware that
delegates requests that produce a not found response to an inner handler) or a
`hyper::Service`.

## Key types

- `ServedDir` - A directory of static files that can be served along with various configuration options
- `ServedDirBuilder` - A builder that provides a fluent API for configuring a `ServedDir`, including
  compression settings, file not found behavior and common response headers
- `FileEntity` - Returned by `ServedDir` when it matches a path to a file, it contains an open file handle
  and file metadata, such as its size, last modified time and ETag hash value
- `SerdirError` - An error type returned by various crate APIs, such as `ServedDirBuilder`, if
  the user provides invalid configuration settings, or by `ServedDir::get` if a file is not found
- `ETag` - A 64 bit hash code of a file's contents, implemented as newtype wrapper around `u64`
- `Body` a custom HTTP response body type that contains a stream of chunks of bytes (each up to 64kb), allowing
  efficient serving of large files
- `FileHasher` - a type alias for a function that takes a file path and returns a hash code of the file's contents,
  used to allow the user to provide a custom hasher for ETag generation
- `CompressionStrategy` - an enum representing the various strategies that can be used to obtain a compressed
  version of a file. Passed to `ServedDirBuilder::compression_strategy`.
- `StaticCompression` - compression settings for static (i.e. ahead of time) compression of served files using GZip,
  Brotli or Zstd.
- `CachedCompression` - compression settings for cached runtime compression of served files using Brotli

## Project structure

The project is organized as a library crate with the following modules:

- `body.rs` - Custom HTTP response body type, allowing static files to be served efficiently as a
stream of chunks
- `brotli_cache.rs` - code for performing Brotli compression at runtime and caching the compressed
  output
- `compression.rs` - utilities for locating the compressed version of a file most appropriate for
- `etag.rs` - ETag parsing and comparison
- `file.rs` - implementation of `FileEntity`, the return type of `ServedDir`
- `integration.rs` - adapter code allowing `ServedDir` to be called via `tower` and `hyper` APIs
- `lib.rs` - crate-level documentation and re-exports
- `platform.rs` - platform-specific code handling differences between Unix and Windows environments
- `range.rs` - range header parsing and validation
- `served_dir.rs` - The `ServedDir` type, including code for matching a path against a file,
  determining what compression strategy to use for the response and choosing a `Content-Type` header
  result
- `serving.rs` - wrapper code for converting a `http::Request` into `http::Response` by serving a
static file

## Cargo aliases

This project defines Cargo aliases in `.cargo/config.toml` for several common
operations:

- `cargo check-all` (runs `cargo check` with all features enabled)
- `cargo test-all` (runs all tests, including those gated by features)
- `cargo run-tower-service` (runs the `tower_service`)
- `cargo run-tower-middleware` (runs the `tower_middleware` example)
- `cargo build-docs` (rebuilds the docs with all features enabled)

## Optional features

This project defines 3 optional Cargo features that can be enabled at compile time:

- `runtime-compression` - enables the cached compression strategy, i.e. runtime
compression of served files using Brotli
- `tower` - enables integration with Tower-based web frameworks like Axum and
Poem by providing APIs to convert a `ServedDir` into a `tower::Service` or
`tower::Layer`
- `hyper` - enables direct integration with the `hyper` web server by providing
APIs to convert a `ServedDir` into a `hyper::Service`

Many tests related to these features are feature-gated. To run the full test
suite, run `cargo test-all`.

## Platforms

This project supports both Unix and Windows platforms, and contains a small
amount of platform-specific code for reading file contents in the `platform.rs`
module.

If you have the `x86_64-pc-windows-msvc` target for rustc installed, you can perform
syntax checking for the Windows build on Linux by running 

```
cargo check --all-features --target x86_64-pc-windows-msvc
```

The cargo `check-windows` alias provides a shorthand for the above command.

## Testing

Run the full test suite via `cargo test-all`.

Perform syntax checking of all code via `cargo check-all`.

## Documentation

When making changes to the code, don't change the crate-level documentation in `src/lib.rs`. I'll update it myself
once all the code changes are complete.
