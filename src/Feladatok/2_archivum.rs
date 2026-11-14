use anyhow::{Context, Result, bail};
use reqwest::{Client, StatusCode, Url, header::RETRY_AFTER};
use serde::Deserialize;
use serde_json::{Number, Value, json};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const MAX_RATE_LIMIT_RETRIES: usize = 20;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Source {
    snapshot_at: String,
    records_at_snapshot: usize,
    query: Query,
    first_page: String,
    limits: Limits,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Limits {
    requests_per_second: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Query {
    filters: Vec<Filter>,
    aggregate: Aggregate,
    result_type: String,
}

#[derive(Debug, Deserialize)]
struct Filter {
    field: String,
    op: String,
    value: Value,
}

#[derive(Debug, Deserialize)]
struct Aggregate {
    op: String,
    field: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    id: i64,
    author: String,
    year: i64,
    category: String,
    pages: i64,
    #[serde(default)]
    tags: Vec<String>,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct Page {
    records: Option<Vec<Record>>,
    items: Option<Vec<Record>>,
    next: Option<String>,
}

struct RateLimiter {
    request_interval: Duration,
    last_request: Option<Instant>,
}

impl RateLimiter {
    fn new(requests_per_second: u64) -> Result<Self> {
        if requests_per_second == 0 {
            bail!("GET /source returned a zero requestsPerSecond limit");
        }
        Ok(Self {
            request_interval: Duration::from_secs_f64(1.0 / requests_per_second as f64),
            last_request: None,
        })
    }

    async fn wait(&mut self) {
        if let Some(last_request) = self.last_request {
            let elapsed = last_request.elapsed();
            if elapsed < self.request_interval {
                tokio::time::sleep(self.request_interval - elapsed).await;
            }
        }
        self.last_request = Some(Instant::now());
    }
}

pub async fn solve(client: &Client) -> Result<()> {
    let source_value = get_json(client, &crate::api_url("archive/source")?, None).await?;
    let source: Source = serde_json::from_value(payload(source_value))
        .context("GET /source returned an invalid archive task")?;
    let snapshot = parse_timestamp(&source.snapshot_at)
        .context("GET /source returned an invalid snapshotAt timestamp")?;
    let mut limiter = RateLimiter::new(source.limits.requests_per_second)?;
    let records = fetch_snapshot(client, &source, snapshot, &mut limiter).await?;

    let mut matching_records = Vec::new();
    for record in records {
        let mut matches = true;
        for filter in &source.query.filters {
            if !matches_filter(&record, filter)? {
                matches = false;
                break;
            }
        }
        if matches {
            matching_records.push(record);
        }
    }
    let result = aggregate(
        &matching_records,
        &source.query.aggregate,
        &source.query.result_type,
    )?;

    let response = client
        .post(crate::api_url("archive/solution")?)
        .json(&json!({ "result": result }))
        .send()
        .await
        .context("POST /solution request failed")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("Could not read POST /solution response")?;
    if !status.is_success() {
        bail!("POST /solution returned {status}: {body}");
    }

    println!("{body}");
    Ok(())
}

pub async fn zero_solve(run_key: &str) -> Result<()> {
    let keyed_client = super::client_with_run_key(run_key)?;
    solve(&keyed_client).await
}

async fn fetch_snapshot(
    client: &Client,
    source: &Source,
    snapshot: i128,
    limiter: &mut RateLimiter,
) -> Result<Vec<Record>> {
    if source.records_at_snapshot == 0 {
        return Ok(Vec::new());
    }

    let base =
        Url::parse(&crate::api_url("archive")?).context("Archive API base URL is invalid")?;
    let records_path = format!("{}/records", base.path());
    let mut next = Some(source.first_page.clone());
    let mut records = HashMap::new();

    while let Some(page_url) = next {
        let url = Url::parse(&page_url).context("Archive page URL is invalid")?;
        if url.origin() != base.origin()
            || url.path() != records_path
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            bail!("Archive pagination returned an unexpected URL: {page_url}");
        }

        let page_value = get_json(client, &page_url, Some(limiter)).await?;
        let page_payload = payload(page_value);
        if page_payload.get("next").is_none() {
            bail!("Archive page response is missing its next field");
        }
        let mut page: Page = serde_json::from_value(page_payload)
            .context("Archive page response has an invalid shape")?;
        let page_records = page
            .records
            .take()
            .or(page.items.take())
            .context("Archive page response has no records array")?;

        for record in page_records {
            if parse_timestamp(&record.created_at).with_context(|| {
                format!("Record {} has an invalid createdAt timestamp", record.id)
            })? <= snapshot
            {
                records.entry(record.id).or_insert(record);
            }
        }

        if records.len() > source.records_at_snapshot {
            bail!(
                "Fetched {} snapshot records, but /source reported {}",
                records.len(),
                source.records_at_snapshot
            );
        }
        if records.len() == source.records_at_snapshot {
            break;
        }
        next = page.next;
    }

    if records.len() != source.records_at_snapshot {
        bail!(
            "Archive pagination ended after {} snapshot records; /source reported {}",
            records.len(),
            source.records_at_snapshot
        );
    }

    Ok(records.into_values().collect())
}

async fn get_json(
    client: &Client,
    url: &str,
    mut limiter: Option<&mut RateLimiter>,
) -> Result<Value> {
    let mut rate_limit_retries = 0;
    loop {
        if let Some(limiter) = limiter.as_deref_mut() {
            limiter.wait().await;
        }

        let response = client
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {url} request failed"))?;

        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            if rate_limit_retries >= MAX_RATE_LIMIT_RETRIES {
                let body = response
                    .text()
                    .await
                    .context("Could not read rate-limit response")?;
                bail!("GET {url} exceeded the rate-limit retry count: {body}");
            }
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .context("429 response is missing the Retry-After header")?
                .to_str()
                .context("429 Retry-After header is not valid text")?
                .parse::<f64>()
                .context("429 Retry-After header is not a number of seconds")?;
            if !retry_after.is_finite() || retry_after < 0.0 {
                bail!("429 Retry-After header contains an invalid delay");
            }
            tokio::time::sleep(Duration::from_secs_f64(retry_after)).await;
            rate_limit_retries += 1;
            continue;
        }

        let status = response.status();
        let body = response
            .bytes()
            .await
            .with_context(|| format!("Could not read GET {url} response"))?;
        if !status.is_success() {
            bail!(
                "GET {url} returned {status}: {}",
                String::from_utf8_lossy(&body)
            );
        }
        return serde_json::from_slice(&body)
            .with_context(|| format!("GET {url} returned invalid JSON"));
    }
}

fn payload(value: Value) -> Value {
    value.get("data").cloned().unwrap_or(value)
}

fn matches_filter(record: &Record, filter: &Filter) -> Result<bool> {
    match (filter.field.as_str(), filter.op.as_str()) {
        ("author", "eq") => Ok(filter
            .value
            .as_str()
            .context("author eq filter value must be a string")?
            == record.author),
        ("category", "eq") => Ok(filter
            .value
            .as_str()
            .context("category eq filter value must be a string")?
            == record.category),
        ("tags", "contains") => {
            let tag = filter
                .value
                .as_str()
                .context("tags contains filter value must be a string")?;
            Ok(record.tags.iter().any(|record_tag| record_tag == tag))
        }
        ("year", "between") => {
            let bounds = filter
                .value
                .as_array()
                .filter(|bounds| bounds.len() == 2)
                .context("year between filter must contain two numeric bounds")?;
            let start = bounds[0]
                .as_i64()
                .context("year between lower bound must be an integer")?;
            let end = bounds[1]
                .as_i64()
                .context("year between upper bound must be an integer")?;
            Ok(start <= record.year && record.year <= end)
        }
        ("pages", "gte") => Ok(record.pages
            >= filter
                .value
                .as_i64()
                .context("pages gte filter value must be an integer")?),
        ("pages", "lte") => Ok(record.pages
            <= filter
                .value
                .as_i64()
                .context("pages lte filter value must be an integer")?),
        _ => bail!("Unsupported archive filter: {} {}", filter.field, filter.op),
    }
}

fn aggregate(records: &[Record], aggregate: &Aggregate, result_type: &str) -> Result<Value> {
    match (aggregate.op.as_str(), result_type) {
        ("count", "integer") => Ok(json!(records.len())),
        ("ids", "id-list") => {
            let mut ids: Vec<_> = records.iter().map(|record| record.id).collect();
            ids.sort_unstable();
            ids.dedup();
            Ok(json!(ids))
        }
        ("sum", "integer") => {
            let sum = records.iter().try_fold(0i128, |sum, record| {
                sum.checked_add(aggregate_field(record, aggregate)?)
                    .context("Archive sum exceeds the supported integer range")
            })?;
            Number::from_i128(sum)
                .map(Value::Number)
                .context("Archive sum cannot be represented as a JSON integer")
        }
        ("min", "integer") | ("max", "integer") => {
            let values = records
                .iter()
                .map(|record| aggregate_field(record, aggregate))
                .collect::<Result<Vec<_>>>()?;
            let value = if aggregate.op == "min" {
                values.iter().min()
            } else {
                values.iter().max()
            }
            .context("Cannot calculate min or max of an empty result set")?;
            Number::from_i128(*value)
                .map(Value::Number)
                .context("Aggregate cannot be represented as a JSON integer")
        }
        ("avg", "decimal") => {
            if records.is_empty() {
                bail!("Cannot calculate the average of an empty result set");
            }
            let sum = records.iter().try_fold(0i128, |sum, record| {
                sum.checked_add(aggregate_field(record, aggregate)?)
                    .context("Archive average sum exceeds the supported integer range")
            })?;
            let average = ((sum as f64 / records.len() as f64) * 100.0).round() / 100.0;
            Number::from_f64(average)
                .map(Value::Number)
                .context("Archive average is not a finite JSON number")
        }
        (operation, result_type) => bail!(
            "Unsupported archive aggregate/result type combination: {operation}/{result_type}"
        ),
    }
}

fn aggregate_field(record: &Record, aggregate: &Aggregate) -> Result<i128> {
    match aggregate.field.as_deref() {
        Some("id") => Ok(i128::from(record.id)),
        Some("year") => Ok(i128::from(record.year)),
        Some("pages") => Ok(i128::from(record.pages)),
        Some(field) => bail!("Unsupported numeric aggregate field: {field}"),
        None => bail!("Aggregate {} requires a field", aggregate.op),
    }
}

fn parse_timestamp(value: &str) -> Result<i128> {
    let (date, time_and_zone) = value
        .split_once('T')
        .context("Timestamp must contain a T separator")?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts
        .next()
        .context("Missing timestamp year")?
        .parse()?;
    let month: i64 = date_parts
        .next()
        .context("Missing timestamp month")?
        .parse()?;
    let day: i64 = date_parts
        .next()
        .context("Missing timestamp day")?
        .parse()?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) {
        bail!("Timestamp has an invalid date");
    }
    let days_in_month = match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1..=days_in_month).contains(&day) {
        bail!("Timestamp has an invalid day of month");
    }

    let (time, offset_seconds) = if let Some(time) = time_and_zone.strip_suffix('Z') {
        (time, 0)
    } else {
        let offset_start = time_and_zone
            .char_indices()
            .skip(8)
            .find_map(|(index, ch)| matches!(ch, '+' | '-').then_some(index))
            .context("Timestamp must end in Z or a numeric UTC offset")?;
        let (time, offset) = time_and_zone.split_at(offset_start);
        let sign = if offset.starts_with('+') { 1 } else { -1 };
        let mut parts = offset[1..].split(':');
        let hours: i64 = parts.next().context("Missing UTC offset hour")?.parse()?;
        let minutes: i64 = parts.next().context("Missing UTC offset minute")?.parse()?;
        if parts.next().is_some() || !(0..=23).contains(&hours) || !(0..=59).contains(&minutes) {
            bail!("Timestamp has an invalid UTC offset");
        }
        (time, sign * (hours * 3600 + minutes * 60))
    };

    let (whole_seconds, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut time_parts = whole_seconds.split(':');
    let hour: i64 = time_parts
        .next()
        .context("Missing timestamp hour")?
        .parse()?;
    let minute: i64 = time_parts
        .next()
        .context("Missing timestamp minute")?
        .parse()?;
    let second: i64 = time_parts
        .next()
        .context("Missing timestamp second")?
        .parse()?;
    if time_parts.next().is_some()
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
    {
        bail!("Timestamp has an invalid time");
    }
    if !fraction.bytes().all(|byte| byte.is_ascii_digit()) || fraction.len() > 9 {
        bail!("Timestamp has an invalid fractional second");
    }
    let nanos = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<i64>()? * 10_i64.pow((9 - fraction.len()) as u32)
    };

    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds =
        i128::from(days) * 86_400 + i128::from(hour * 3600 + minute * 60 + second - offset_seconds);
    Ok(seconds * 1_000_000_000 + i128::from(nanos))
}
