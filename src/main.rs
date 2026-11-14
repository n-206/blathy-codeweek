use anyhow::{Context, Result};
use reqwest::Client;
use std::env;
use tokio::sync::OnceCell;

#[path = "Arena/mod.rs"]
pub mod arena;
#[path = "Feladatok/mod.rs"]
pub mod feladatok;
#[path = "Kuldetesek/mod.rs"]
pub mod kuldetesek;

static API_ROOT: OnceCell<String> = OnceCell::const_new();

pub fn api_url(path: &str) -> Result<String> {
    let api_root = API_ROOT
        .get()
        .context("API_BASE has not been initialized")?;
    Ok(join_api_url(api_root, path))
}

fn join_api_url(api_root: &str, path: &str) -> String {
    format!(
        "{}/{}",
        api_root.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::path::Path::new(".env").exists() {
        dotenvy::from_path(".env").context("Failed to load .env")?;
    }

    let api_base =
        env::var("API_BASE").context("API_BASE must be set in .env or the environment")?;
    if api_base.trim().is_empty() {
        anyhow::bail!("API_BASE must not be empty");
    }
    API_ROOT
        .set(api_base)
        .map_err(|_| anyhow::anyhow!("API_BASE has already been initialized"))?;

    let CLIENT = Client::new();

    println!("Client has been set up");

    //? 1
    // feladatok::fejlec::solve(&CLIENT).await?;
    //? 2
    // feladatok::archivum::solve(&CLIENT).await?;
    //? 3
    // feladatok::belepes::solve(&CLIENT).await?;
    //? 4
    // feladatok::labirintus::solve(&CLIENT).await?;
    //? 5
    // feladatok::cimke::solve(&CLIENT).await?;
    //? 6
    // feladatok::expedicio::solve(&CLIENT).await?;
    //? 7
    // feladatok::ballisztika::solve(&CLIENT).await?;
    //? 8
    // feladatok::vizsga::solve(&CLIENT).await?;
    // ? 9
    // feladatok::handshake::solve(&CLIENT).await?;

    //* Mellék küldetések:
    //? S1
    kuldetesek::s1::solve(&CLIENT).await?;
    //? S2
    // kuldetesek::s2::solve(&CLIENT).await?;
    //? S3
    //kuldetesek::s3::solve(&CLIENT).await?;

    //* Aréna:
    //? A1
    // arena::a1::solve(&CLIENT).await?;
    //? A2
    // arena::a2::solve(&CLIENT).await?;

    Ok(())
}
