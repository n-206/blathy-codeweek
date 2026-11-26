use anyhow::{Context, Result, bail};
use num_bigint::BigUint;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::net::Ipv6Addr;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceResponse {
    data: Source,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Source {
    address: String,
    prefix_length: u8,
    count: usize,
    _expires_in: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ResultData {
    network: String,
    first: String,
    last: String,
    expanded: String,
    address_count: String,
    subnets64: String,
    subnets: Vec<String>,
}

pub async fn solve(client: &Client) -> Result<()> {
    let source_url = crate::api_url("ipv6-subnet/source")?;
    let response = client
        .get(&source_url)
        .send()
        .await
        .context("GET /broken-qr/source request failed")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("Could not read GET /broken-qr/source response")?;
    if !status.is_success() {
        bail!("GET /broken-qr/source returned {status}: {}", &body);
    }

    let response: SourceResponse = serde_json::from_str(&body)?;
    let source = response.data;

    let result = calculate(source)?;

    let response_url = crate::api_url("ipv6-subnet/solution")?;
    let response = client.post(response_url).json(&result).send().await?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("Could not read POST /broken-qr/solution response")?;
    if !status.is_success() {
        bail!("POST /broken-qr/solution returned {status}: {body}");
    }

    println!("{body}");
    Ok(())
}

fn canonical(value: u128) -> String {
    Ipv6Addr::from(value).to_string()
}

fn calculate(data: Source) -> Result<ResultData> {
    let prefix = data.prefix_length;
    let count = data.count;
    let address = data.address;

    if prefix > 128 {
        bail!("Prefix cannot be longer than 128");
    }
    if count == 0 {
        bail!("Count has to be at least 1")
    }

    let addr: Ipv6Addr = address
        .parse()
        .map_err(|e| format!("Faulty ipv6 address: {e}"))
        .unwrap();
    let value = u128::from(addr);
    let mask = if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    };

    let network_value = value & mask;
    let host_bits = 128 - prefix;
    let last_value = network_value
        | if host_bits == 128 {
            u128::MAX
        } else if host_bits == 0 {
            0
        } else {
            (1u128 << host_bits) - 1
        };

    let borrowed_bits = if count <= 1 {
        0
    } else {
        usize::BITS as u8 - (count - 1).leading_zeros() as u8
    };

    let subnet_prefix = prefix
        .checked_add(borrowed_bits)
        .filter(|&p| p <= 128)
        .ok_or("There is not enough address for the asked number of subnets")
        .unwrap();

    let available = BigUint::from(1u8) << borrowed_bits as usize;
    if available < BigUint::from(count) {
        bail!("There is not enough network for the asked subnets");
    }

    let subnet_size = 1u128 << (128 - subnet_prefix);
    let subnets: Vec<String> = (0..count)
        .map(|i| {
            let subnet_address = network_value + (i as u128) * subnet_size;
            format!("{}/{}", canonical(subnet_address), subnet_prefix)
        })
        .collect();

    let address_count = (BigUint::from(1u8) << host_bits as usize).to_string();

    let subnets64 = if prefix <= 64 {
        (BigUint::from(1u8) << (64 - prefix) as usize).to_string()
    } else {
        "0".to_string()
    };

    let expanded = addr
        .segments()
        .iter()
        .map(|part| format!("{part:04x}"))
        .collect::<Vec<_>>()
        .join(":");

    Ok(ResultData {
        network: format!("{}/{}", canonical(network_value), prefix),
        first: canonical(network_value),
        last: canonical(last_value),
        expanded,
        address_count,
        subnets64,
        subnets,
    })
}
