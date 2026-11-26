#[cfg(feature = "hip")]
use anyhow::anyhow;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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
        .get(crate::api_url("blathy-mine/source")?)
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
            .post(crate::api_url("blathy-mine/solution")?)
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
    if !has_amd_gpu()? {
        return find_nonce_cpu(challenge, zeros, deadline).await;
    }

    if !rocm_is_installed() {
        bail!("AMD GPU detected, but ROCm is not installed or hipcc is unavailable");
    }

    #[cfg(feature = "hip")]
    {
        return find_nonce_hip(challenge, zeros, deadline).await;
    }

    #[cfg(not(feature = "hip"))]
    {
        println!("HIP feature is not enabled. Switching to CPU mining.");
        find_nonce_cpu(challenge, zeros, deadline).await
    }
}

async fn find_nonce_cpu(challenge: &str, zeros: usize, deadline: Instant) -> Result<String> {
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

fn has_amd_gpu() -> Result<bool> {
    #[cfg(target_os = "linux")]
    {
        let drm_devices = match fs::read_dir("/sys/class/drm") {
            Ok(devices) => devices,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error).context("Could not inspect Linux DRM devices for an AMD GPU");
            }
        };
        for entry in drm_devices {
            let entry = entry.context("Could not read a Linux DRM device entry")?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name
                .strip_prefix("card")
                .is_some_and(|suffix| suffix.parse::<u32>().is_ok())
            {
                continue;
            }

            match fs::read_to_string(entry.path().join("device/vendor")) {
                Ok(vendor) if vendor.trim().eq_ignore_ascii_case("0x1002") => return Ok(true),
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).context("Could not read a DRM device vendor identifier");
                }
            }
        }
        return Ok(false);
    }

    #[cfg(not(target_os = "linux"))]
    Ok(false)
}

fn rocm_is_installed() -> bool {
    let rocm_path = env::var_os("ROCM_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/opt/rocm"));
    let hipcc = rocm_path.join("bin/hipcc");
    if hipcc.is_file() {
        return Command::new(hipcc)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success());
    }

    env::var_os("PATH").is_some_and(|paths| {
        env::split_paths(&paths).any(|path| {
            let candidate = path.join("hipcc");
            candidate.is_file()
                && Command::new(candidate)
                    .arg("--version")
                    .output()
                    .is_ok_and(|output| output.status.success())
        })
    })
}

#[cfg(feature = "hip")]
async fn find_nonce_hip(challenge: &str, zeros: usize, deadline: Instant) -> Result<String> {
    if challenge.len() > 235 {
        return find_nonce_cpu(challenge, zeros, deadline)
            .await
            .context("Challenge is too long for the HIP kernel; CPU search failed");
    }

    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        bail!("Search deadline expired");
    }
    let timeout_ms = u64::try_from(remaining.as_millis())
        .context("Remaining challenge time exceeds the HIP timeout range")?
        .max(1);
    let challenge = challenge.as_bytes().to_vec();
    let result = tokio::task::spawn_blocking(move || {
        s3_hip_find_nonce(&challenge, zeros as u32, timeout_ms)
    })
    .await
    .context("HIP nonce search worker panicked")??;

    match result {
        HipSearchResult::Found(nonce) => Ok(nonce.to_string()),
        HipSearchResult::TimedOut => bail!("Search deadline expired"),
    }
}

#[cfg(feature = "hip")]
enum HipSearchResult {
    Found(u64),
    TimedOut,
}

#[cfg(feature = "hip")]
fn s3_hip_find_nonce(challenge: &[u8], zeros: u32, timeout_ms: u64) -> Result<HipSearchResult> {
    unsafe extern "C" {
        fn s3_hip_find_nonce(
            challenge: *const u8,
            challenge_length: usize,
            zeros: u32,
            timeout_ms: u64,
            result: *mut u64,
        ) -> i32;
    }

    let mut nonce = 0_u64;
    let status = unsafe {
        s3_hip_find_nonce(
            challenge.as_ptr(),
            challenge.len(),
            zeros,
            timeout_ms,
            &mut nonce,
        )
    };
    match status {
        0 => Ok(HipSearchResult::Found(nonce)),
        1 => Ok(HipSearchResult::TimedOut),
        -2 => bail!("HIP nonce search exhausted its supported range"),
        -3 => bail!("ROCm is installed, but HIP reports no available GPU devices"),
        code => Err(anyhow!("HIP nonce search failed with error code {code}")),
    }
}

fn has_leading_zeroes(digest: &[u8], zeros: usize) -> bool {
    let whole_bytes = zeros / 2;
    digest[..whole_bytes].iter().all(|byte| *byte == 0)
        && (zeros % 2 == 0 || digest[whole_bytes] < 0x10)
}
