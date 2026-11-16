#[path = "../Feladatok/10_parkolo.rs"]
#[allow(dead_code)]
mod parking;

use std::{env, net::SocketAddr};

use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Source {
    secret: String,
    garage: Value,
}

fn parse_source(body: Value) -> anyhow::Result<Source> {
    let data = body.get("data").cloned().unwrap_or(body);
    serde_json::from_value(data).context("Garage source response is missing secret or garage")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::path::Path::new(".env").exists() {
        dotenvy::from_path(".env").context("Failed to load .env")?;
    }

    let endpoint_url =
        env::var("PARKING_URL").context("PARKING_URL must be set in .env or the environment")?;
    let api_base =
        env::var("API_BASE").context("API_BASE must be set in .env or the environment")?;
    if api_base.trim().is_empty() {
        bail!("API_BASE must not be empty");
    }
    let api_base = format!("{}/parking-garage", api_base.trim_end_matches('/'));
    let parsed_url = reqwest::Url::parse(&endpoint_url).context("PARKING_URL is invalid")?;
    if parsed_url.scheme() != "https" {
        bail!("PARKING_URL must use https");
    }
    let address: SocketAddr = format!(
        "0.0.0.0:{}",
        env::var("PORT").unwrap_or_else(|_| "3000".to_owned())
    )
    .parse()?;

    let client = reqwest::Client::new();
    let source_response = client
        .get(format!("{api_base}/source"))
        .send()
        .await
        .context("Failed to request garage source")?
        .error_for_status()
        .context("Garage source returned an error")?;
    let source_body: Value = source_response
        .json()
        .await
        .context("Garage source returned invalid JSON")?;
    let source = parse_source(source_body)?;

    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("Failed to bind to {address}"))?;
    let app = parking::router_with_garage(source.secret, source.garage)?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    let registration = client
        .put(format!("{api_base}/endpoint"))
        .json(&serde_json::json!({ "url": endpoint_url }))
        .send()
        .await
        .context("Failed to register PARKING_URL")?
        .error_for_status()
        .context("Endpoint registration returned an error")?;
    println!("Parking endpoint registered: {}", registration.status());
    println!("Health check at /health");

    server.await.context("Parking server task failed")??;
    Ok(())
}
