use anyhow::{Context, Result, bail};
use reqwest::header::{CONTENT_TYPE, HeaderMap};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;

#[derive(Debug, Deserialize)]
struct Source {
    numbers: Vec<i128>,
}

pub async fn solve(client: &reqwest::Client) -> Result<()> {
    let response = client
        .get(crate::api_url("header-hunter/source")?)
        .send()
        .await
        .context("GET /source request failed")?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .bytes()
        .await
        .context("Could not read GET /source response")?;

    if !status.is_success() {
        bail!(
            "GET /source returned {status}: {}",
            String::from_utf8_lossy(&body)
        );
    }

    let operations = required_header(&headers, "X-Operation")?;
    let answer_format = required_header(&headers, "X-Answer-Format")?;
    let request_id = required_header(&headers, "X-Request-Id")?;
    let source: Source =
        serde_json::from_slice(&body).context("GET /source returned invalid JSON")?;
    let result = evaluate(&source.numbers, operations)?;

    let (content_type, answer_body) = match answer_format {
        "json" => (
            "application/json",
            serde_json::to_string(&json!({ "result": result }))
                .context("Could not serialize the JSON answer")?,
        ),
        "text" => ("text/plain", result.to_string()),
        other => bail!("Unsupported X-Answer-Format value: {other}"),
    };

    let response = client
        .post(crate::api_url("header-hunter/solution")?)
        .header("X-Request-Id", request_id)
        .header(CONTENT_TYPE, content_type)
        .body(answer_body)
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

fn required_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str> {
    headers
        .get(name)
        .with_context(|| format!("GET /source response is missing {name}"))?
        .to_str()
        .with_context(|| format!("GET /source header {name} is not valid text"))
}

fn evaluate(numbers: &[i128], operations: &str) -> Result<i128> {
    let mut values = numbers.to_vec();

    for operation in operations.split(',').map(str::trim) {
        let (name, argument) = operation
            .split_once(':')
            .map_or((operation, None), |(name, argument)| (name, Some(argument)));

        match (name, argument) {
            ("reverse-digits", None) => {
                for value in &mut values {
                    *value = reverse_digits(*value)?;
                }
            }
            ("digit-sum", None) => {
                for value in &mut values {
                    *value = digit_sum(*value);
                }
            }
            ("filter-odd", None) => values.retain(|value| value % 2 != 0),
            ("filter-even", None) => values.retain(|value| value % 2 == 0),
            ("double", None) => {
                for value in &mut values {
                    *value = value
                        .checked_mul(2)
                        .with_context(|| format!("Integer overflow in operation {operation}"))?;
                }
            }
            ("unique", None) => {
                let mut seen = HashSet::new();
                values.retain(|value| seen.insert(*value));
            }
            ("take", Some(count)) => values.truncate(parse_argument(count, operation)?),
            ("drop", Some(count)) => {
                let count = parse_argument(count, operation)?.min(values.len());
                values.drain(..count);
            }
            ("reverse", None) => values.reverse(),
            ("sort-asc", None) => values.sort_unstable(),
            ("sort-desc", None) => values.sort_unstable_by(|left, right| right.cmp(left)),
            ("add", Some(argument)) => {
                let argument = parse_integer(argument, operation)?;
                for value in &mut values {
                    *value = value
                        .checked_add(argument)
                        .with_context(|| format!("Integer overflow in operation {operation}"))?;
                }
            }
            ("multiply", Some(argument)) => {
                let argument = parse_integer(argument, operation)?;
                for value in &mut values {
                    *value = value
                        .checked_mul(argument)
                        .with_context(|| format!("Integer overflow in operation {operation}"))?;
                }
            }
            ("sum", None) => {
                values = vec![values.iter().try_fold(0i128, |sum, value| {
                    sum.checked_add(*value)
                        .context("Integer overflow in sum operation")
                })?];
            }
            ("min", None) => {
                values = vec![*values.iter().min().context("min operation has no values")?];
            }
            ("max", None) => {
                values = vec![*values.iter().max().context("max operation has no values")?];
            }
            ("count", None) => values = vec![values.len() as i128],
            ("square", None) => {
                for value in &mut values {
                    *value = value
                        .checked_mul(*value)
                        .with_context(|| format!("Integer overflow in operation {operation}"))?;
                }
            }
            ("mod", Some(argument)) => {
                let argument = parse_integer(argument, operation)?;
                for value in &mut values {
                    *value = value
                        .checked_rem(argument)
                        .with_context(|| format!("Invalid modulo operation {operation}"))?;
                }
            }
            ("product-mod", Some(argument)) => {
                let modulus = parse_integer(argument, operation)?;
                if modulus <= 0 {
                    bail!("Invalid modulo operation {operation}: modulus must be positive");
                }
                let modulus_i128 = modulus;
                let modulus = modulus as u128;
                let product = values
                    .iter()
                    .fold(1_u128 % modulus, |product, value| {
                        multiply_mod(
                            product,
                            value.rem_euclid(modulus_i128) as u128,
                            modulus,
                        )
                    });
                values = vec![i128::try_from(product)
                    .context("Product modulo result is outside the supported integer range")?];
            }
            _ => bail!("Unsupported operation: {operation}"),
        }
    }

    if values.len() != 1 {
        bail!(
            "Operations must produce exactly one integer, but produced {}",
            values.len()
        );
    }
    Ok(values[0])
}

fn multiply_mod(mut left: u128, mut right: u128, modulus: u128) -> u128 {
    let mut product = 0;
    while right > 0 {
        if right & 1 == 1 {
            product = (product + left) % modulus;
        }
        right >>= 1;
        if right > 0 {
            left = (left + left) % modulus;
        }
    }
    product
}

fn parse_argument(value: &str, operation: &str) -> Result<usize> {
    value
        .parse()
        .with_context(|| format!("Invalid numeric argument in operation {operation}"))
}

fn parse_integer(value: &str, operation: &str) -> Result<i128> {
    value
        .parse()
        .with_context(|| format!("Invalid numeric argument in operation {operation}"))
}

fn reverse_digits(value: i128) -> Result<i128> {
    let sign = value.signum();
    let mut digits = value.unsigned_abs();
    let mut reversed = 0u128;
    while digits > 0 {
        reversed = reversed
            .checked_mul(10)
            .and_then(|value| value.checked_add(digits % 10))
            .context("Integer overflow in reverse-digits operation")?;
        digits /= 10;
    }
    i128::try_from(reversed)
        .context("Reversed number is outside the supported integer range")?
        .checked_mul(sign)
        .context("Integer overflow in reverse-digits operation")
}

fn digit_sum(value: i128) -> i128 {
    let mut digits = value.unsigned_abs();
    let mut sum = 0;
    while digits > 0 {
        sum += digits % 10;
        digits /= 10;
    }
    sum as i128
}
