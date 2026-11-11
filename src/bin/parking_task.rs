use std::path::Path;

use anyhow::Context;

const API_BASE: &str =
    "https://codeweek.blathy.info/api/task/HkcFuUnk8AEOjH0qSvZdTfhgVZWgHca5/parking-garage";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if Path::new(".env").exists() {
        dotenvy::from_path(".env").context("Failed to load .env")?;
    }

    let response = reqwest::Client::new()
        .post(format!("{API_BASE}/runs"))
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
