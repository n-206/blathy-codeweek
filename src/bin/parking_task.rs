use std::path::Path;

use anyhow::Context;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if Path::new(".env").exists() {
        dotenvy::from_path(".env").context("Failed to load .env")?;
    }

    let api_base =
        std::env::var("API_BASE").context("API_BASE must be set in .env or the environment")?;
    if api_base.trim().is_empty() {
        anyhow::bail!("API_BASE must not be empty");
    }
    let api_base = format!("{}/parking-garage", api_base.trim_end_matches('/'));
    let response = reqwest::Client::new()
        .post(format!("{api_base}/runs"))
        .send()
        .await
        .context("Failed to start parking garage run")?
        .error_for_status()
        .context("Run request returned an error")?;

    println!("Run started ({}).", response.status());
    if let Some(location) = response.headers().get(reqwest::header::LOCATION) {
        println!(
            "Run status URL: {}",
            location
                .to_str()
                .context("Run Location header is not valid text")?
        );
    }

    let body = response
        .text()
        .await
        .context("Failed to read run response")?;
    if !body.trim().is_empty() {
        println!("{body}");
    }

    Ok(())
}
