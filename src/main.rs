use anyhow::{Context, Result, bail};
use dialoguer::Select;
use futures::{FutureExt, future::BoxFuture};
use reqwest::Client;
use serde::Deserialize;
use std::env;
use std::io::{self, Write};
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
    let options = [
        ("Zero to Hero", MenuOption::ZeroToHero),
        ("Task 1 - Fejlec", MenuOption::Fejlec),
        ("Task 2 - Archivum", MenuOption::Archivum),
        ("Task 3 - Belepes", MenuOption::Belepes),
        ("Task 4 - Labirintus", MenuOption::Labirintus),
        ("Task 5 - Cimke", MenuOption::Cimke),
        ("Task 6 - Expedicio", MenuOption::Expedicio),
        ("Task 7 - Ballisztika", MenuOption::Ballisztika),
        ("Task 8 - Vizsga", MenuOption::Vizsga),
        ("Task 9 - Handshake", MenuOption::Handshake),
        ("Task 10 - Parkolo", MenuOption::Parkolo),
        ("Side mission S1", MenuOption::SideMissionS1),
        ("Side mission S2", MenuOption::SideMissionS2),
        ("Side mission S3", MenuOption::SideMissionS3),
        ("Arena A1", MenuOption::ArenaA1),
        ("Arena A2", MenuOption::ArenaA2),
        ("Exit", MenuOption::Exit),
    ];
    let labels: Vec<&str> = options.iter().map(|(label, _)| *label).collect();
    print!("\x1B[2J\x1B[H"); // Clearing terminal
    io::stdout()
        .flush()
        .context("Failed to clear the terminal before showing the menu")?;
    let Some(selection) = Select::new()
        .with_prompt("Choose a challenge")
        .items(&labels)
        .default(0)
        .interact_opt()?
    else {
        return Ok(());
    };
    let selection = options[selection].1;

    if selection == MenuOption::Exit {
        return Ok(());
    }

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

    let client = Client::new();

    match selection {
        MenuOption::ZeroToHero => zero_to_hero::<serde_json::Value>(&client).await?,
        MenuOption::Fejlec => feladatok::fejlec::solve(&client).await?,
        MenuOption::Archivum => feladatok::archivum::solve(&client).await?,
        MenuOption::Belepes => feladatok::belepes::solve(&client).await?,
        MenuOption::Labirintus => feladatok::labirintus::solve(&client).await?,
        MenuOption::Cimke => feladatok::cimke::solve(&client).await?,
        MenuOption::Expedicio => feladatok::expedicio::solve(&client).await?,
        MenuOption::Ballisztika => feladatok::ballisztika::solve(&client).await?,
        MenuOption::Vizsga => feladatok::vizsga::solve(&client).await?,
        MenuOption::Handshake => feladatok::handshake::solve(&client).await?,
        MenuOption::Parkolo => feladatok::parkolo::solve().await?,
        MenuOption::SideMissionS1 => kuldetesek::s1::solve(&client).await?,
        MenuOption::SideMissionS2 => kuldetesek::s2::solve(&client).await?,
        MenuOption::SideMissionS3 => kuldetesek::s3::solve(&client).await?,
        MenuOption::ArenaA1 => arena::a1::solve(&client).await?,
        MenuOption::ArenaA2 => arena::a2::solve(&client).await?,
        MenuOption::Exit => {}
    }

    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuOption {
    ZeroToHero,
    Fejlec,
    Archivum,
    Belepes,
    Labirintus,
    Cimke,
    Expedicio,
    Ballisztika,
    Vizsga,
    Handshake,
    Parkolo,
    SideMissionS1,
    SideMissionS2,
    SideMissionS3,
    ArenaA1,
    ArenaA2,
    Exit,
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
