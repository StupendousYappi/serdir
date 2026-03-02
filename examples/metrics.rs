use http::Request;
use metrics_exporter_prometheus::PrometheusBuilder;
use serdir::ServedDir;
use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Setup a prometheus exporter to listen on port 9000
    let builder = PrometheusBuilder::new();
    builder
        .with_http_listener(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 9000))
        .install()?;

    println!("Prometheus metrics are available at http://localhost:9000/metrics");

    // Serve a temporary directory
    let temp_dir = tempfile::tempdir()?;
    let path = temp_dir.path().join("index.html");
    std::fs::write(&path, "<h1>Hello with Metrics!</h1>")?;

    let served_dir = ServedDir::builder(temp_dir.path())?.build();

    println!("Simulating some requests...");
    for _ in 0..5 {
        let req = Request::get("/index.html").body(())?;
        let _resp = served_dir.get_response(&req).await?;
    }

    // Simulate a 404
    for _ in 0..2 {
        let req = Request::get("/missing.html").body(())?;
        let _resp = served_dir.get_response(&req).await?;
    }

    println!("Requests complete. Check metrics at http://localhost:9000/metrics");
    println!("Press Ctrl+C to exit.");

    // In a real app we would start a server here.
    // We just wait indefinitely so the user can query metrics.
    futures_util::future::pending::<Infallible>().await;

    Ok(())
}
