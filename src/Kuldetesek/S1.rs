use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, NaiveDateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct Source {
    case: String,
    logs: Vec<LogSource>,
}

#[derive(Debug, Deserialize)]
struct LogSource {
    name: String,
    url: String,
}

#[derive(Debug)]
struct AppRecord {
    timestamp: i64,
    event: Option<String>,
    session: Option<String>,
    file: Option<String>,
    role_change_to: Option<String>,
    role_change_via: Option<String>,
    raw: String,
}

#[derive(Debug)]
struct NginxRecord {
    timestamp: i64,
    ip: String,
    method: String,
    path: String,
    status: u16,
    user_agent: String,
}

#[derive(Debug)]
struct AuthRecord {
    timestamp: i64,
    user: Option<String>,
    raw: String,
}

pub async fn solve(client: &reqwest::Client) -> Result<()> {
    let source_url = crate::api_url("log-detective/source")?;
    let source_text = get_text(client, &source_url, "log-detective source").await?;
    let source: Source =
        serde_json::from_str(&source_text).context("Log-detective source was invalid JSON")?;

    let nginx_url = log_url(&source, "nginx.log")?;
    let app_url = log_url(&source, "app.jsonl")?;
    let auth_url = log_url(&source, "auth.log")?;
    let (nginx_text, app_text, auth_text) = tokio::try_join!(
        get_text(client, nginx_url, "nginx.log"),
        get_text(client, app_url, "app.jsonl"),
        get_text(client, auth_url, "auth.log"),
    )?;

    let app = parse_app_log(&app_text)?;
    let nginx = parse_nginx_log(&nginx_text)?;
    let auth = parse_auth_log(&auth_text, &app)?;

    let export = select_attack_export(&app, &nginx)?;
    let session = export
        .session
        .as_deref()
        .context("Export event did not include a session")?;
    let file = export
        .file
        .as_deref()
        .context("Export event did not include a file path")?;
    let login = app
        .iter()
        .find(|record| {
            record.event.as_deref() == Some("login.ok")
                && record.session.as_deref() == Some(session)
        })
        .context("Could not find the login associated with the export session")?;

    let clock_offset = infer_auth_clock_offset(&app, &auth)?;
    let account = auth
        .iter()
        .find(|record| record.user.is_some() && record.timestamp - clock_offset == login.timestamp)
        .and_then(|record| record.user.as_deref())
        .context("Could not match the export session to an auth.log account")?;

    let attacker_ip = nginx
        .iter()
        .find(|record| {
            record.timestamp == export.timestamp
                && record.method == "POST"
                && record.path.split('?').next() == Some("/api/admin/export")
        })
        .map(|record| record.ip.as_str())
        .context("Could not match the export event to an nginx client IP")?;

    let first_seen = first_seen(attacker_ip, &nginx, &app, &auth, clock_offset)
        .context("The attacker IP did not appear in any parsed log record")?;
    let first_seen = DateTime::<Utc>::from_timestamp(first_seen, 0)
        .context("First-seen timestamp was outside the supported range")?
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    println!(
        "Case {} candidate: ip={attacker_ip}, firstSeen={first_seen}, account={account}, file={file}",
        source.case
    );

    let solution_url = crate::api_url("log-detective/solution")?;
    let response = client
        .post(&solution_url)
        .json(&serde_json::json!({
            "ip": attacker_ip,
            "firstSeen": first_seen,
            "account": account,
            "file": file,
        }))
        .send()
        .await
        .context("POST log-detective solution request failed")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("Could not read log-detective solution response")?;
    if !status.is_success() {
        bail!("POST log-detective solution returned {status}: {body}");
    }

    println!("{body}");
    Ok(())
}

async fn get_text(client: &reqwest::Client, url: &str, label: &str) -> Result<String> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {label} request failed"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("Could not read GET {label} response"))?;
    if !status.is_success() {
        bail!("GET {label} returned {status}: {body}");
    }
    Ok(body)
}

fn log_url<'a>(source: &'a Source, name: &str) -> Result<&'a str> {
    source
        .logs
        .iter()
        .find(|log| log.name == name)
        .map(|log| log.url.as_str())
        .with_context(|| format!("Log-detective source did not provide {name}"))
}

fn parse_app_log(text: &str) -> Result<Vec<AppRecord>> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            let value: Value = serde_json::from_str(line)
                .with_context(|| format!("Invalid JSON on app.jsonl line {}", index + 1))?;
            let timestamp = value
                .get("ts")
                .and_then(Value::as_str)
                .with_context(|| format!("Missing timestamp on app.jsonl line {}", index + 1))?;
            let timestamp = DateTime::parse_from_rfc3339(timestamp)
                .with_context(|| format!("Invalid timestamp on app.jsonl line {}", index + 1))?
                .with_timezone(&Utc)
                .timestamp();

            Ok(AppRecord {
                timestamp,
                event: value
                    .get("event")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                session: value
                    .get("session")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                file: value.get("file").and_then(Value::as_str).map(str::to_owned),
                role_change_to: value.get("to").and_then(Value::as_str).map(str::to_owned),
                role_change_via: value.get("via").and_then(Value::as_str).map(str::to_owned),
                raw: line.to_owned(),
            })
        })
        .collect()
}

fn parse_nginx_log(text: &str) -> Result<Vec<NginxRecord>> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 7 {
                bail!("Malformed nginx.log line {}", index + 1);
            }
            let timestamp = NaiveDateTime::parse_from_str(
                fields[3].trim_start_matches('['),
                "%d/%b/%Y:%H:%M:%S",
            )
            .with_context(|| format!("Invalid timestamp on nginx.log line {}", index + 1))?
            .and_utc()
            .timestamp();

            Ok(NginxRecord {
                timestamp,
                ip: fields[0].to_owned(),
                method: fields[5].trim_start_matches('"').to_owned(),
                path: fields[6].to_owned(),
                status: fields[8]
                    .parse()
                    .with_context(|| format!("Invalid status on nginx.log line {}", index + 1))?,
                user_agent: line
                    .split('"')
                    .nth(5)
                    .context(format!(
                        "Missing user agent on nginx.log line {}",
                        index + 1
                    ))?
                    .to_owned(),
            })
        })
        .collect()
}

fn parse_auth_log(text: &str, app: &[AppRecord]) -> Result<Vec<AuthRecord>> {
    let app_times: Vec<_> = app.iter().map(|record| record.timestamp).collect();
    let first_app_time = *app_times
        .iter()
        .min()
        .context("app.jsonl did not contain any events")?;
    let last_app_time = *app_times
        .iter()
        .max()
        .context("app.jsonl did not contain any events")?;
    let midpoint = first_app_time + (last_app_time - first_app_time) / 2;
    let base_year = DateTime::<Utc>::from_timestamp(midpoint, 0)
        .context("app.jsonl timestamps were outside the supported range")?
        .year();

    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 3 {
                bail!("Malformed auth.log line {}", index + 1);
            }

            let mut candidates = Vec::new();
            for year in (base_year - 1)..=(base_year + 1) {
                let timestamp = NaiveDateTime::parse_from_str(
                    &format!("{year} {} {} {}", fields[0], fields[1], fields[2]),
                    "%Y %b %e %H:%M:%S",
                )
                .with_context(|| format!("Invalid timestamp on auth.log line {}", index + 1))?
                .and_utc()
                .timestamp();
                candidates.push((timestamp, timestamp.abs_diff(midpoint)));
            }
            let timestamp = candidates
                .into_iter()
                .min_by_key(|(_, distance)| *distance)
                .map(|(timestamp, _)| timestamp)
                .context("Could not determine auth.log year")?;

            let user = line
                .split_once("web login OK user=")
                .map(|(_, rest)| {
                    rest.split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .to_owned()
                })
                .filter(|user| !user.is_empty());

            Ok(AuthRecord {
                timestamp,
                user,
                raw: line.to_owned(),
            })
        })
        .collect()
}

fn infer_auth_clock_offset(app: &[AppRecord], auth: &[AuthRecord]) -> Result<i64> {
    let mut app_logins: Vec<_> = app
        .iter()
        .filter(|record| record.event.as_deref() == Some("login.ok"))
        .map(|record| record.timestamp)
        .collect();
    let mut auth_logins: Vec<_> = auth
        .iter()
        .filter(|record| record.user.is_some())
        .map(|record| record.timestamp)
        .collect();
    if app_logins.is_empty() || auth_logins.is_empty() {
        bail!("Could not infer auth.log clock offset without successful logins");
    }

    app_logins.sort_unstable();
    auth_logins.sort_unstable();
    if app_logins.len() != auth_logins.len() {
        bail!(
            "Successful login counts differ between app.jsonl ({}) and auth.log ({})",
            app_logins.len(),
            auth_logins.len()
        );
    }
    let offsets: Vec<_> = auth_logins
        .iter()
        .zip(&app_logins)
        .map(|(auth_time, app_time)| auth_time - app_time)
        .collect();
    let offset = *offsets
        .first()
        .context("Could not infer auth.log clock offset")?;
    if offsets.iter().any(|candidate| *candidate != offset) {
        bail!("auth.log login events do not share one constant clock offset");
    }
    Ok(offset)
}

fn select_attack_export<'a>(app: &'a [AppRecord], nginx: &[NginxRecord]) -> Result<&'a AppRecord> {
    let mut scored = Vec::new();
    for export in app
        .iter()
        .filter(|record| record.event.as_deref() == Some("export.download"))
    {
        let session = export
            .session
            .as_deref()
            .context("Export event did not include a session")?;
        let timestamp = export.timestamp;
        let client_ip = nginx
            .iter()
            .find(|record| {
                record.timestamp == timestamp
                    && record.method == "POST"
                    && record.path.split('?').next() == Some("/api/admin/export")
            })
            .map(|record| record.ip.as_str())
            .with_context(|| format!("No matching nginx export request for session {session}"))?;

        let mut score = 0;
        for event in app
            .iter()
            .filter(|record| record.session.as_deref() == Some(session))
        {
            if event.event.as_deref() == Some("role.change") {
                if event.role_change_to.as_deref() == Some("admin") {
                    score += 40;
                }
                if event.role_change_via.as_deref() == Some("profile.update") {
                    score += 100;
                }
            }
        }
        if export
            .file
            .as_deref()
            .is_some_and(|file| file.contains("/backup/"))
        {
            score += 25;
        }
        let client_records = nginx.iter().filter(|record| record.ip == client_ip);
        let mut has_scanner_ua = false;
        let mut has_failed_login = false;
        for record in client_records {
            let agent = record.user_agent.to_ascii_lowercase();
            if agent.contains("nuclei") || agent.contains("hydra") || agent.contains("zgrab") {
                has_scanner_ua = true;
            }
            if record.path.split('?').next() == Some("/api/login")
                && record.method == "POST"
                && record.status == 401
            {
                has_failed_login = true;
            }
        }
        score += i32::from(has_scanner_ua) * 60;
        score += i32::from(has_failed_login) * 10;
        scored.push((score, export));
    }

    let max_score = scored
        .iter()
        .map(|(score, _)| *score)
        .max()
        .context("No export.download event was found in app.jsonl")?;
    if max_score == 0 {
        bail!("No export has any attacker indicators in the available logs");
    }
    let mut best = scored
        .into_iter()
        .filter(|(score, _)| *score == max_score)
        .map(|(_, export)| export);
    let export = best.next().context("Could not select an export event")?;
    if best.next().is_some() {
        bail!("Multiple exports have equally strong attacker indicators");
    }
    Ok(export)
}

fn first_seen(
    attacker_ip: &str,
    nginx: &[NginxRecord],
    app: &[AppRecord],
    auth: &[AuthRecord],
    auth_clock_offset: i64,
) -> Option<i64> {
    nginx
        .iter()
        .filter(|record| record.ip == attacker_ip)
        .map(|record| record.timestamp)
        .chain(
            app.iter()
                .filter(|record| record.raw.contains(attacker_ip))
                .map(|record| record.timestamp),
        )
        .chain(
            auth.iter()
                .filter(|record| record.raw.contains(attacker_ip))
                .map(|record| record.timestamp - auth_clock_offset),
        )
        .min()
}
