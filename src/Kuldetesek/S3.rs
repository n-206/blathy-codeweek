use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const API_BASE: &str =
    "https://codeweek.blathy.info/api/task/HkcFuUnk8AEOjH0qSvZdTfhgVZWgHca5/blathy-mine";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Challenge {
    level: usize,
    zeros: usize,
    challenge: String,
    expires_in: u64,
    #[serde(default)]
    solve_level: Option<usize>,
    max_level: usize,
}

#[derive(Debug, Deserialize)]
struct SolutionResponse {
    data: Challenge,
}

pub async fn solve(client: &reqwest::Client) -> Result<()> {
    let response = client
        .get(format!("{API_BASE}/source"))
        .send()
        .await
        .context("GET /blathy-mine/source request failed")?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .context("Could not read GET /blathy-mine/source response")?;

    if !status.is_success() {
        bail!(
            "GET /blathy-mine/source returned {status}: {}",
            String::from_utf8_lossy(&body)
        );
    }

    let mut challenge: Challenge =
        serde_json::from_slice(&body).context("GET /blathy-mine/source returned invalid JSON")?;

    loop {
        validate_challenge(&challenge)?;
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(challenge.expires_in))
            .context("Challenge expiry is too far in the future")?;
        println!(
            "Mining level {} ({} leading zeroes)...",
            challenge.level, challenge.zeros
        );
        let nonce = find_nonce(&challenge.challenge, challenge.zeros, deadline)
            .await
            .with_context(|| {
                format!(
                    "Could not solve level {} before it expired",
                    challenge.level
                )
            })?;

        println!("Level {} nonce: {nonce}", challenge.level);
        let response = client
            .post(format!("{API_BASE}/solution"))
            .json(&json!({ "nonce": nonce }))
            .send()
            .await
            .context("POST /blathy-mine/solution request failed")?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("Could not read POST /blathy-mine/solution response")?;

        if !status.is_success() {
            bail!("POST /blathy-mine/solution returned {status}: {body}");
        }
        println!("{body}");

        if challenge.level >= challenge.max_level {
            break;
        }

        let next: SolutionResponse = serde_json::from_str(&body)
            .context("Solution response did not contain the next challenge")?;
        let next = next.data;
        if next.level != challenge.level + 1 {
            bail!(
                "Unexpected next level: expected {}, received {}",
                challenge.level + 1,
                next.level
            );
        }
        if next.max_level != challenge.max_level {
            bail!("Challenge limits changed unexpectedly between levels");
        }
        challenge = next;
    }

    Ok(())
}

fn validate_challenge(challenge: &Challenge) -> Result<()> {
    if challenge.level == 0 || challenge.level > challenge.max_level {
        bail!(
            "Invalid challenge level {} (maximum {})",
            challenge.level,
            challenge.max_level
        );
    }
    if let Some(solve_level) = challenge.solve_level
        && solve_level > challenge.max_level
    {
        bail!(
            "Invalid solve level {solve_level} (maximum {})",
            challenge.max_level
        );
    }
    if challenge.zeros > 64 {
        bail!("Unsupported number of leading zeroes: {}", challenge.zeros);
    }
    if challenge.challenge.is_empty() {
        bail!("Challenge string must not be empty");
    }
    Ok(())
}

async fn find_nonce(challenge: &str, zeros: usize, deadline: Instant) -> Result<String> {
    let worker_count = std::thread::available_parallelism()
        .context("Could not determine available CPU parallelism")?
        .get();
    let worker_count_u64 =
        u64::try_from(worker_count).context("CPU worker count exceeds supported range")?;
    let found = Arc::new(AtomicBool::new(false));
    let prefix = {
        let mut hasher = Sha256::new();
        hasher.update(challenge.as_bytes());
        hasher
    };

    let mut workers = Vec::with_capacity(worker_count);
    for worker_id in 0..worker_count {
        let found = Arc::clone(&found);
        let prefix = prefix.clone();
        workers.push(tokio::task::spawn_blocking(move || {
            let mut candidate = u64::try_from(worker_id).ok()?;
            while !found.load(Ordering::Relaxed) && Instant::now() < deadline {
                let nonce = candidate.to_string();
                let mut hasher = prefix.clone();
                hasher.update(nonce.as_bytes());
                let digest = hasher.finalize();

                if has_leading_zeroes(&digest, zeros)
                    && found
                        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                {
                    return Some(nonce);
                }

                candidate = candidate.checked_add(worker_count_u64)?;
            }
            None
        }));
    }

    for worker in workers {
        if let Some(nonce) = worker.await.context("Nonce search worker panicked")? {
            return Ok(nonce);
        }
    }

    bail!("Search deadline expired")
}

fn has_leading_zeroes(digest: &[u8], zeros: usize) -> bool {
    let whole_bytes = zeros / 2;
    digest[..whole_bytes].iter().all(|byte| *byte == 0)
        && (zeros % 2 == 0 || digest[whole_bytes] < 0x10)
}
