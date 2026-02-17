use http::{header, HeaderMap, HeaderValue};
use serdir::{SerdirError, ServedDir, ServedDirBuilder};
use std::collections::HashMap;
use std::io::Read;
use tempfile::TempDir;

struct TestContext {
    tmp: TempDir,
    builder: ServedDirBuilder,
}

impl TestContext {
    fn write_file(&self, name: &str, contents: &str) {
        std::fs::write(self.tmp.path().join(name), contents.as_bytes())
            .expect("failed to write test file")
    }

    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().to_path_buf();
        Self {
            tmp,
            builder: ServedDir::builder(path).expect("failed to create builder"),
        }
    }
}

fn fixed_hash(_: &mut dyn Read) -> Result<Option<u64>, std::io::Error> {
    Ok(Some(0x1234))
}

fn no_hash(_: &mut dyn Read) -> Result<Option<u64>, std::io::Error> {
    Ok(None)
}

fn hash_error(_: &mut dyn Read) -> Result<Option<u64>, std::io::Error> {
    Err(std::io::Error::other("hash calculation failed"))
}

fn default_hasher(file: &mut dyn Read) -> Result<Option<u64>, std::io::Error> {
    Ok(Some(rapidhash::v3::rapidhash_v3_file(file)?))
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_get() {
    let context = TestContext::new();
    context.write_file("one.txt", "one");
    context.write_file("two.json", "more content");
    let served_dir = context.builder.build();
    let hdrs = HeaderMap::new();

    let e1 = served_dir.get("one.txt", &hdrs).await.unwrap();
    assert_eq!(e1.len(), 3);
    assert_eq!(e1.header(&header::CONTENT_TYPE).unwrap(), "text/plain");

    let e2 = served_dir.get("two.json", &hdrs).await.unwrap();
    assert_eq!(e2.len(), 12);
    assert_eq!(
        e2.header(&header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_not_found() {
    let context = TestContext::new();
    let served_dir = context.builder.build();
    let hdrs = HeaderMap::new();

    let err = served_dir.get("non-existent.txt", &hdrs).await.unwrap_err();
    assert!(matches!(err, SerdirError::NotFound(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_forbidden_paths() {
    let context = TestContext::new();
    let served_dir = context.builder.build();
    let hdrs = HeaderMap::new();

    // 1. Contains ".." in middle
    let err = served_dir
        .get("include/../etc/passwd", &hdrs)
        .await
        .unwrap_err();
    assert!(matches!(err, SerdirError::InvalidPath(msg) if msg.contains(".. segment")));

    // 2. Contains ".." at start
    let err = served_dir.get("../etc/passwd", &hdrs).await.unwrap_err();
    assert!(matches!(err, SerdirError::InvalidPath(msg) if msg.contains(".. segment")));

    // 3. Contains ".." at end
    let err = served_dir.get("foo/..", &hdrs).await.unwrap_err();
    assert!(matches!(err, SerdirError::InvalidPath(msg) if msg.contains(".. segment")));

    // 4. Contains null byte
    let err = served_dir.get("test\0file.txt", &hdrs).await.unwrap_err();
    assert!(matches!(err, SerdirError::InvalidPath(msg) if msg.contains("NUL byte")));

    // 5. Absolute path (leading /)
    // Note: ServedDir::get strips one leading slash, so we test with two.
    let err = served_dir.get("//etc/passwd", &hdrs).await.unwrap_err();
    assert!(matches!(err, SerdirError::InvalidPath(msg) if msg.contains("absolute")));

    // 6. Windows-style paths (tested if on Windows)
    if cfg!(windows) {
        // Backslash as separator
        let err = served_dir.get(r"foo\..\bar", &hdrs).await.unwrap_err();
        assert!(matches!(err, SerdirError::InvalidPath(msg) if msg.contains(".. segment")));

        // Drive letter
        let err = served_dir.get(r"C:\etc\passwd", &hdrs).await.unwrap_err();
        assert!(matches!(
            err,
            SerdirError::InvalidPath(msg) if msg.contains("prefix") || msg.contains("absolute")
        ));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_strip_prefix() {
    let context = TestContext::new();
    context.write_file("real.txt", "real content");

    let served_dir = context.builder.strip_prefix("/static/").build();
    let hdrs = HeaderMap::new();

    // Should work with the prefix
    let e = served_dir.get("/static/real.txt", &hdrs).await.unwrap();
    assert_eq!(e.read_bytes().unwrap(), "real content");

    // Should fail without the prefix
    let err = served_dir.get("real.txt", &hdrs).await.unwrap_err();
    assert!(matches!(err, SerdirError::NotFound(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_content_types() {
    let context = TestContext::new();
    context.write_file("index.html", "html");
    context.write_file("style.css", "css");
    context.write_file("script.js", "js");
    context.write_file("image.webp", "webp");
    context.write_file("unknown.foo", "foo");

    let served_dir = context.builder.build();
    let hdrs = HeaderMap::new();

    let e = served_dir.get("index.html", &hdrs).await.unwrap();
    assert_eq!(e.header(&header::CONTENT_TYPE).unwrap(), "text/html");

    let e = served_dir.get("style.css", &hdrs).await.unwrap();
    assert_eq!(e.header(&header::CONTENT_TYPE).unwrap(), "text/css");

    let e = served_dir.get("script.js", &hdrs).await.unwrap();
    assert_eq!(e.header(&header::CONTENT_TYPE).unwrap(), "text/javascript");

    let e = served_dir.get("image.webp", &hdrs).await.unwrap();
    assert_eq!(e.header(&header::CONTENT_TYPE).unwrap(), "image/webp");

    let e = served_dir.get("unknown.foo", &hdrs).await.unwrap();
    assert_eq!(
        e.header(&header::CONTENT_TYPE).unwrap(),
        "application/octet-stream"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_common_headers() {
    let context = TestContext::new();
    context.write_file("one.txt", "one");

    let served_dir = context
        .builder
        .common_header(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=3600"),
        )
        .common_header(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        )
        .build();
    let hdrs = HeaderMap::new();

    let e = served_dir.get("one.txt", &hdrs).await.unwrap();
    assert_eq!(
        e.header(&header::CACHE_CONTROL).unwrap(),
        "public, max-age=3600"
    );
    assert_eq!(e.header(&header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(), "*");
    assert_eq!(e.header(&header::CONTENT_TYPE).unwrap(), "text/plain");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_unexpected_br_path() {
    let context = TestContext::new();
    let path = context.tmp.path();

    // Create the raw file
    context.write_file("test.txt", "raw content");

    // Create a directory where the .br file is expected
    std::fs::create_dir(path.join("test.txt.br")).unwrap();

    let served_dir = context.builder.static_compression(true, true, true).build();
    let mut hdrs = HeaderMap::new();
    hdrs.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));

    // Should ignore the directory and serve the raw file
    let e = served_dir.get("test.txt", &hdrs).await.unwrap();
    assert!(e.header(&header::CONTENT_ENCODING).is_none());
    assert_eq!(e.read_bytes().unwrap(), "raw content");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_unexpected_gz_path() {
    let context = TestContext::new();
    let path = context.tmp.path();

    // Create the raw file
    context.write_file("test.txt", "raw content");

    // Create a directory where the .gz file is expected
    std::fs::create_dir(path.join("test.txt.gz")).unwrap();

    let served_dir = context.builder.static_compression(true, true, true).build();
    let mut hdrs = HeaderMap::new();
    hdrs.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip"));

    // Should ignore the directory and serve the raw file
    let e = served_dir.get("test.txt", &hdrs).await.unwrap();
    assert!(e.header(&header::CONTENT_ENCODING).is_none());
    assert_eq!(e.read_bytes().unwrap(), "raw content");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_get_directory() {
    let context = TestContext::new();
    let path = context.tmp.path();
    std::fs::create_dir(path.join("subdir")).unwrap();

    let served_dir = context.builder.build();
    let hdrs = HeaderMap::new();

    let err = served_dir.get("subdir", &hdrs).await.unwrap_err();
    assert!(matches!(err, SerdirError::IsDirectory(_)));
    if let SerdirError::IsDirectory(err_path) = err {
        assert_eq!(err_path, path.join("subdir"));
    }
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn test_served_dir_symlink() {
    use std::os::unix::fs::symlink;
    let context = TestContext::new();
    let path = context.tmp.path();

    // Create the raw file
    context.write_file("target.txt", "target content");

    // Create a symlink pointing to the target file
    symlink(path.join("target.txt"), path.join("link.txt")).unwrap();

    let served_dir = context.builder.build();
    let hdrs = HeaderMap::new();

    // Should follow the symlink and serve the target file
    let e = served_dir.get("link.txt", &hdrs).await.unwrap();
    assert_eq!(e.read_bytes().unwrap(), "target content");
    assert_eq!(e.header(&header::CONTENT_TYPE).unwrap(), "text/plain");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_append_index_html() {
    let context = TestContext::new();
    let path = context.tmp.path();
    std::fs::create_dir(path.join("subdir")).unwrap();
    std::fs::write(path.join("subdir").join("index.html"), b"index content").unwrap();

    let builder = ServedDir::builder(path.to_path_buf()).unwrap();

    // 1. append_index_html disabled (default)
    let served_dir = builder.build();
    let hdrs = HeaderMap::new();
    let err = served_dir.get("subdir", &hdrs).await.unwrap_err();
    assert!(matches!(err, SerdirError::IsDirectory(_)));

    // 2. append_index_html enabled
    let builder = ServedDir::builder(path.to_path_buf()).unwrap();
    let served_dir = builder.append_index_html(true).build();
    let e = served_dir.get("subdir", &hdrs).await.unwrap();
    assert_eq!(e.read_bytes().unwrap(), "index content");
    assert_eq!(e.header(&header::CONTENT_TYPE).unwrap(), "text/html");

    // 3. append_index_html enabled, but index.html missing
    std::fs::create_dir(path.join("empty_subdir")).unwrap();
    let err = served_dir.get("empty_subdir", &hdrs).await.unwrap_err();
    assert!(matches!(err, SerdirError::NotFound(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_compression_priority() {
    let context = TestContext::new();
    context.write_file("test.txt", "raw content");
    context.write_file("test.txt.gz", "fake gzip content");
    context.write_file("test.txt.br", "fake brotli content");
    context.write_file("test.txt.zstd", "fake zstd content");

    let served_dir = context.builder.static_compression(true, true, true).build();
    let mut hdrs = HeaderMap::new();
    hdrs.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("br, gzip, zstd"),
    );

    // 1. All 3 available -> Brotli used
    let e = served_dir.get("test.txt", &hdrs).await.unwrap();
    assert_eq!(e.header(&header::CONTENT_ENCODING).unwrap(), "br");
    assert_eq!(e.read_bytes().unwrap(), "fake brotli content");

    // 2. Zstandard and Gzip available -> Zstandard used
    std::fs::remove_file(context.tmp.path().join("test.txt.br")).unwrap();
    let e = served_dir.get("test.txt", &hdrs).await.unwrap();
    assert_eq!(e.header(&header::CONTENT_ENCODING).unwrap(), "zstd");
    assert_eq!(e.read_bytes().unwrap(), "fake zstd content");

    // 3. Only Gzip available -> Gzip used
    std::fs::remove_file(context.tmp.path().join("test.txt.zstd")).unwrap();
    let e = served_dir.get("test.txt", &hdrs).await.unwrap();
    assert_eq!(e.header(&header::CONTENT_ENCODING).unwrap(), "gzip");
    assert_eq!(e.read_bytes().unwrap(), "fake gzip content");

    // 4. None available -> Raw used
    std::fs::remove_file(context.tmp.path().join("test.txt.gz")).unwrap();
    let e = served_dir.get("test.txt", &hdrs).await.unwrap();
    assert!(e.header(&header::CONTENT_ENCODING).is_none());
    assert_eq!(e.read_bytes().unwrap(), "raw content");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_not_found_path_config() {
    let context = TestContext::new();
    context.write_file("404.html", "not found");
    std::fs::create_dir(context.tmp.path().join("subdir")).unwrap();

    let path = context.tmp.path().to_path_buf();
    let builder = ServedDir::builder(path).unwrap();

    // 1. Successfully set relative path to file
    let _served_dir = builder
        .not_found_path("404.html")
        .expect("failed to set not_found_path");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_static_compression_config() {
    let context = TestContext::new();
    context.write_file("test.txt", "raw content");
    context.write_file("test.txt.br", "fake brotli content");
    context.write_file("test.txt.gz", "fake gzip content");
    context.write_file("test.txt.zstd", "fake zstd content");

    let mut hdrs = HeaderMap::new();
    hdrs.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("br, gzip, zstd"),
    );

    // 1. Only Brotli enabled
    let served_dir = context
        .builder
        .static_compression(true, false, false)
        .build();
    let e = served_dir.get("test.txt", &hdrs).await.unwrap();
    assert_eq!(e.header(&header::CONTENT_ENCODING).unwrap(), "br");

    // 2. Only Zstandard enabled
    let builder = ServedDir::builder(context.tmp.path().to_path_buf()).unwrap();
    let served_dir = builder.static_compression(false, false, true).build();
    let e = served_dir.get("test.txt", &hdrs).await.unwrap();
    assert_eq!(e.header(&header::CONTENT_ENCODING).unwrap(), "zstd");

    // 3. Only Gzip enabled
    let builder = ServedDir::builder(context.tmp.path().to_path_buf()).unwrap();
    let served_dir = builder.static_compression(false, true, false).build();
    let e = served_dir.get("test.txt", &hdrs).await.unwrap();
    assert_eq!(e.header(&header::CONTENT_ENCODING).unwrap(), "gzip");

    // 4. None enabled (even if files exist and client supports them)
    let builder = ServedDir::builder(context.tmp.path().to_path_buf()).unwrap();
    let served_dir = builder.static_compression(false, false, false).build();
    let e = served_dir.get("test.txt", &hdrs).await.unwrap();
    assert!(e.header(&header::CONTENT_ENCODING).is_none());

    // 5. Both Brotli and Zstandard enabled (Brotli preferred)
    let builder = ServedDir::builder(context.tmp.path().to_path_buf()).unwrap();
    let served_dir = builder.static_compression(true, false, true).build();
    let e = served_dir.get("test.txt", &hdrs).await.unwrap();
    assert_eq!(e.header(&header::CONTENT_ENCODING).unwrap(), "br");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_not_found_path_behavior() {
    let context = TestContext::new();
    context.write_file("404.html", "custom 404 content");
    context.write_file("exists.txt", "found");

    let served_dir = context.builder.not_found_path("404.html").unwrap().build();
    let hdrs = HeaderMap::new();

    // 1. Existing file -> Ok
    let e = served_dir.get("exists.txt", &hdrs).await.unwrap();
    assert_eq!(e.read_bytes().unwrap(), "found");

    // 2. Non-existent file -> NotFound(Some(entity))
    let err = served_dir.get("missing.txt", &hdrs).await.unwrap_err();
    if let SerdirError::NotFound(Some(entity)) = err {
        assert_eq!(entity.read_bytes().unwrap(), "custom 404 content");
        assert_eq!(entity.header(&header::CONTENT_TYPE).unwrap(), "text/html");
    } else {
        panic!("expected NotFound(Some(_)), got {:?}", err);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_default_hash_function_sets_etag() {
    let context = TestContext::new();
    context.write_file("one.txt", "one");
    let served_dir = context.builder.file_hasher(default_hasher).build();
    let hdrs = HeaderMap::new();

    let entity = served_dir.get("one.txt", &hdrs).await.unwrap();
    assert!(entity.etag.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_custom_hash_function_sets_etag() {
    let context = TestContext::new();
    context.write_file("one.txt", "one");
    let served_dir = context.builder.file_hasher(fixed_hash).build();
    let hdrs = HeaderMap::new();

    let entity = served_dir.get("one.txt", &hdrs).await.unwrap();
    let etag = entity.etag.expect("etag should be present");
    let etag_val: HeaderValue = etag.into();
    assert_eq!(etag_val.to_str().unwrap(), r#""0000000000001234""#);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_custom_hash_function_can_disable_etag() {
    let context = TestContext::new();
    context.write_file("one.txt", "one");
    let served_dir = context.builder.file_hasher(no_hash).build();
    let hdrs = HeaderMap::new();

    let entity = served_dir.get("one.txt", &hdrs).await.unwrap();
    assert!(entity.etag.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_hash_function_error_is_propagated() {
    let context = TestContext::new();
    context.write_file("one.txt", "one");
    let served_dir = context.builder.file_hasher(hash_error).build();
    let hdrs = HeaderMap::new();

    let err = served_dir.get("one.txt", &hdrs).await.unwrap_err();
    match err {
        SerdirError::IOError(inner) => {
            assert_eq!(inner.kind(), std::io::ErrorKind::Other);
            assert_eq!(inner.to_string(), "hash calculation failed");
        }
        other => panic!("expected IOError, got {:?}", other),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_not_found_path_uses_custom_hash_function() {
    let context = TestContext::new();
    context.write_file("404.html", "custom 404 content");
    let served_dir = context
        .builder
        .file_hasher(fixed_hash)
        .not_found_path("404.html")
        .unwrap()
        .build();
    let hdrs = HeaderMap::new();

    let err = served_dir.get("missing.txt", &hdrs).await.unwrap_err();
    if let SerdirError::NotFound(Some(entity)) = err {
        let etag = entity.etag.expect("etag should be present");
        let etag_val: HeaderValue = etag.into();
        assert_eq!(etag_val.to_str().unwrap(), r#""0000000000001234""#);
    } else {
        panic!("expected NotFound(Some(_)), got {:?}", err);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_etag_cache_is_used() {
    let context = TestContext::new();
    context.write_file("test.txt", "content");

    static CALL_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn counting_hasher(_: &mut dyn Read) -> Result<Option<u64>, std::io::Error> {
        CALL_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Some(0x1234))
    }

    let served_dir = context.builder.file_hasher(counting_hasher).build();
    let hdrs = HeaderMap::new();

    // First call should trigger hasher
    served_dir.get("test.txt", &hdrs).await.unwrap();
    assert_eq!(CALL_COUNT.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Second call should use cached value
    served_dir.get("test.txt", &hdrs).await.unwrap();
    assert_eq!(CALL_COUNT.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_served_dir_custom_extensions() {
    let context = TestContext::new();
    context.write_file("test.custom", "custom content");
    context.write_file("another.thing", "thing content");
    context.write_file("test.html", "html content");

    let mut custom_extensions = HashMap::new();
    custom_extensions.insert(
        "custom".to_string(),
        HeaderValue::from_static("text/custom"),
    );

    let served_dir = context
        .builder
        .known_extensions(custom_extensions)
        .known_extension("thing", HeaderValue::from_static("application/thing"))
        .build();
    let hdrs = HeaderMap::new();

    // 1. Verify known_extensions works
    let e = served_dir.get("test.custom", &hdrs).await.unwrap();
    assert_eq!(e.header(&header::CONTENT_TYPE).unwrap(), "text/custom");

    // 2. Verify known_extension works
    let e = served_dir.get("another.thing", &hdrs).await.unwrap();
    assert_eq!(
        e.header(&header::CONTENT_TYPE).unwrap(),
        "application/thing"
    );

    // 3. Verify defaults are gone because known_extensions replaced the map
    let e = served_dir.get("test.html", &hdrs).await.unwrap();
    assert_eq!(
        e.header(&header::CONTENT_TYPE).unwrap(),
        "application/octet-stream"
    );
}
