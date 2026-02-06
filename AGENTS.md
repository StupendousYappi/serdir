This is a Rust crate that helps users serve static files over HTTP. The two central APIs it offers
are `ServedDir` and `FileEntity`. The `ServedDir` type accepts path strings and HTTP request
headers, looks for a file in a directory matching those criteria, returning a `FileEntity` that can
be turned into the body of an HTTP response. It offers APIs to expose a `ServedDir` via both
[`Tower`](https://docs.rs/tower/latest/tower/) and [`Hyper`](https://docs.rs/hyper/latest/hyper/)
APIs, allowing compatibility with much of the Rust web app ecosystem.

This crate is derived from the [`http-serve`](https://github.com/scottlamb/http-serve ) crate, but 
modified to focus purely on serving static files.

## The `serve` function

The `serve` function serves HTTP `GET` and `HEAD` requests for a given byte-ranged `Entity`. It handles conditional GETs
and subrange requests. Its function signature is:

```rust
pub fn serve<Ent: Entity, B: http_body::Body + From<Box<dyn Stream<Item = Result<Ent::Data, Ent::Error>> + Send>>, BI>(
    entity: Ent,
    req: &http:Request<BI>,
) -> http:Response<B>
```

The definition of the `Entity` trait is:

```rust
pub trait Entity:
    'static
    + Send
    + Sync {
    type Error: 'static + Send + Sync;
    type Data: 'static + Send + Sync + Buf + From<Vec<u8>> + From<&'static [u8]>;

    // Required methods
    fn len(&self) -> u64;
    fn get_range(
        &self,
        range: Range<u64>,
    ) -> Box<dyn Stream<Item = Result<Self::Data, Self::Error>> + Send + Sync>;
    fn add_headers(&self, _: &mut HeaderMap);
    fn etag(&self) -> Option<HeaderValue>;
    fn last_modified(&self) -> Option<SystemTime>;

    // Provided method
    fn is_empty(&self) -> bool { ... }
}
```

Responses returned by `serve` will add an `ETAG` header to the response if the input `Entity` provides one.

## Project structure

The project is organized as a library crate with the following modules:

- `lib.rs` - crate-level documentation and re-exports
- `compression.rs` - utilities for locating the compressed version of the file most appropriate for
  the request
- `etag.rs` - ETag parsing and comparison
- `served_dir.rs` - The `ServedDir` type, including code for matching a path against a file, and
  determining what compression strategy to use for the response.
- `brotli_cache.rs` - code for performing Brotli compression at runtime and caching the compressed
  result
- `range.rs` - range header parsing and validation
- `platform.rs` - platform-specific code handling differences between Unix and Windows environments
- `body.rs` - HTTP response body implementation
- `file.rs` - file entity implementation
- `tower.rs` - adapter code allowing crate functionality to be exposed using `tower` APIs

## Testing

The project comes with a basic suite of unit tests that can be run with `cargo test`.

You can check for correct syntax using `cargo check`.

## Documentation

When making changes to the code, don't change the crate-level documentation in `src/lib.rs`. I'll update it myself
once all the code changes are complete.
