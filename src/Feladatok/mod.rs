use anyhow::{Context, Result};
use reqwest::{
    Client,
    header::{HeaderMap, HeaderName, HeaderValue},
};

pub(crate) fn run_key_headers(run_key: &str) -> Result<HeaderMap> {
    let value =
        HeaderValue::from_str(run_key).context("Run key is not a valid HTTP header value")?;
    let mut headers = HeaderMap::new();
    headers.insert(HeaderName::from_static("x-run-key"), value);
    Ok(headers)
}

pub(crate) fn client_with_run_key(run_key: &str) -> Result<Client> {
    Client::builder()
        .default_headers(run_key_headers(run_key)?)
        .build()
        .context("Could not build HTTP client with X-Run-Key")
}

#[path = "2_archivum.rs"]
pub mod archivum;
#[path = "7_ballisztika.rs"]
pub mod ballisztika;
#[path = "3_belepes.rs"]
pub mod belepes;
#[path = "5_cimke.rs"]
pub mod cimke;
#[path = "6_expedicio.rs"]
pub mod expedicio;
#[path = "1_fejlec.rs"]
pub mod fejlec;
#[path = "9_handshake.rs"]
pub mod handshake;
#[path = "4_labirintus.rs"]
pub mod labirintus;
#[path = "10_parkolo.rs"]
pub mod parkolo;
#[path = "8_vizsga.rs"]
pub mod vizsga;
