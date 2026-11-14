use anyhow::{Context, Result, bail};
use futures::{FutureExt, future::BoxFuture};
use reqwest::Client;
use serde::Deserialize;
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

#[tokio::main(flavor = "multi_thread")]
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
    //? 10 - Make sure server is running! Start it with cargo run --bin parking_server
    // feladatok::parkolo::solve().await?;

    //* Mellék küldetések:
    //? S1
    // kuldetesek::s1::solve(&CLIENT).await?;
    //? S2
    // kuldetesek::s2::solve(&CLIENT).await?;
    //? S3
    //kuldetesek::s3::solve(&CLIENT).await?;

    //* Aréna:
    //? A1
    // arena::a1::solve(&CLIENT).await?;
    //? A2
    // arena::a2::solve(&CLIENT).await?;

    //* Zero to Hero:
    zero_to_hero::<serde_json::Value>(&CLIENT).await?;

    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct RunResponse<T> {
    pub status: u16,
    pub message: String,
    pub data: RunData<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunData<T> {
    pub run_key: String,
    pub header: String,
    pub deadline: String,
    pub seconds: u64,
    pub start_window: u64,
    pub start_window_ends: String,
    pub tasks: Vec<T>,
}

async fn zero_to_hero<T: for<'de> Deserialize<'de>>(client: &Client) -> Result<()> {
    let api_endpoint = api_url("zero-to-hero")?;
    let resp = client
        .post(format!("{api_endpoint}/start"))
        .send()
        .await
        .context("POST /start request failed")?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .context("Could not read POST /start response")?;
    if !status.is_success() {
        bail!("POST /start returned {status}: {}", &body);
    }

    let run_resp: RunResponse<T> = serde_json::from_str(&body).context("Failed to parse body")?;
    let run_key = run_resp.data.run_key;

    let jobs: Vec<BoxFuture<'static, Result<()>>> = vec![
        {
            let key = run_key.clone();
            async move { feladatok::fejlec::zero_solve(&key).await }.boxed()
        },
        {
            let key = run_key.clone();
            async move { feladatok::archivum::zero_solve(&key).await }.boxed()
        },
        {
            let key = run_key.clone();
            async move { feladatok::belepes::zero_solve(&key).await }.boxed()
        },
        {
            let key = run_key.clone();
            async move { feladatok::labirintus::zero_solve(&key).await }.boxed()
        },
        {
            let key = run_key.clone();
            async move { feladatok::cimke::zero_solve(&key).await }.boxed()
        },
        {
            let key = run_key.clone();
            async move { feladatok::expedicio::zero_solve(&key).await }.boxed()
        },
        {
            let key = run_key.clone();
            async move { feladatok::ballisztika::zero_solve(&key).await }.boxed()
        },
        {
            let key = run_key.clone();
            async move { feladatok::vizsga::zero_solve(&key).await }.boxed()
        },
        {
            let key = run_key.clone();
            async move { feladatok::handshake::zero_solve(&key).await }.boxed()
        },
        { async move { feladatok::parkolo::solve().await }.boxed() }, // Doesn't need the key
    ];

    let mut tasks = tokio::task::JoinSet::new();

    for job in jobs {
        tasks.spawn(job);
    }

    while let Some(result) = tasks.join_next().await {
        result??; // propagate both the Tokio JoinError and the anyhow::Error
    }

    println!("Zero to hero solved!");
    Ok(())
}
