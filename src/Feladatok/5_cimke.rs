use anyhow::{Context, Result, bail};
use reqwest::{StatusCode, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fmt;

const MAX_JOB_POLLS: usize = 240;
const MAX_BUSY_POLLS: usize = 1000;

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    status: Option<Value>,
    message: Option<String>,
    data: Option<T>,
}

impl<T> Envelope<T> {
    fn into_data(self, endpoint: &str) -> Result<T> {
        self.data.with_context(|| {
            format!(
                "{endpoint} returned no data (status: {}, message: {})",
                self.status.unwrap_or(Value::Null),
                self.message.unwrap_or_default()
            )
        })
    }
}

#[derive(Debug, Deserialize)]
struct Source {
    labels: Vec<Label>,
    font: Font,
}

#[derive(Debug, Deserialize)]
struct Label {
    index: u32,
    text: String,
    width: usize,
    height: usize,
    scale: usize,
    margin: Margin,
    border: usize,
    density: u8,
}

#[derive(Debug, Deserialize)]
struct Margin {
    top: usize,
    left: usize,
}

#[derive(Debug, Deserialize)]
struct Font {
    width: usize,
    height: usize,
    #[serde(rename = "bitOrder")]
    bit_order: String,
    glyphs: std::collections::HashMap<char, Vec<u8>>,
}

#[derive(Debug)]
struct SerialResult {
    location: Option<Url>,
    responses: Vec<Vec<u8>>,
    dropped: usize,
}

#[derive(Debug)]
struct SerialApiError {
    status: StatusCode,
    body: String,
    problem: Option<String>,
    printer_state: Option<String>,
}

impl SerialApiError {
    fn is_page_printing_order_error(&self) -> bool {
        self.status == StatusCode::UNPROCESSABLE_ENTITY
            && self.problem.as_deref() == Some("order")
            && self.printer_state.as_deref() == Some("page-printing")
    }
}

impl fmt::Display for SerialApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "POST /serial returned {}: {}",
            self.status, self.body
        )
    }
}

impl std::error::Error for SerialApiError {}

pub async fn solve(client: &reqwest::Client) -> Result<()> {
    let source_response = client
        .get(crate::api_url("label-printer/source")?)
        .send()
        .await
        .context("GET /source request failed")?;
    let source_status = source_response.status();
    let source_body = source_response
        .bytes()
        .await
        .context("Could not read GET /source response")?;
    if !source_status.is_success() {
        bail!(
            "GET /source returned {source_status}: {}",
            String::from_utf8_lossy(&source_body)
        );
    }

    let envelope: Envelope<Source> =
        serde_json::from_slice(&source_body).context("GET /source returned invalid JSON")?;
    let source = envelope.into_data("GET /source")?;
    validate_font(&source.font)?;

    let mut labels = source.labels;
    labels.sort_by_key(|label| label.index);
    if labels.len() != 3 || labels.iter().map(|label| label.index).ne(1..=3) {
        bail!("GET /source must return labels with indexes 1, 2, and 3");
    }

    for label in &labels {
        let packets = label_packets(label, &source.font)?;
        let location = send_with_busy_retry(client, &packets)
            .await?
            .context("Serial response did not include a print-job Location header")?;

        wait_until_ready(client).await?;
        send_packets(client, &[packet(0xf3, &[0x01])?]).await?;

        let result = wait_for_job(client, &location).await?;
        if result.status == "done" && result.result.as_deref() != Some("correct") {
            bail!(
                "Label {} finished with result {:?}",
                label.index,
                result.result
            );
        }
        if label.index == 3 {
            println!("{}", result.body);
        }
    }

    Ok(())
}

pub async fn zero_solve(run_key: &str) -> Result<()> {
    let keyed_client = super::client_with_run_key(run_key)?;
    solve(&keyed_client).await
}

fn validate_font(font: &Font) -> Result<()> {
    if font.width != 8 || font.height != 8 || font.bit_order != "msb-first" {
        bail!(
            "Unsupported font: expected an 8x8 msb-first font, got {}x{} {}",
            font.width,
            font.height,
            font.bit_order
        );
    }
    Ok(())
}

fn label_packets(label: &Label, font: &Font) -> Result<Vec<Vec<u8>>> {
    if label.width == 0 || label.height == 0 || label.scale == 0 {
        bail!("Label {} has a zero-sized dimension or scale", label.index);
    }
    let bytes_per_row = label.width.checked_add(7).context("Label width overflow")? / 8;
    let mut rows = vec![vec![0u8; bytes_per_row]; label.height];

    if label.border > 0 {
        for y in 0..label.height {
            for x in 0..label.width {
                if x < label.border
                    || x >= label.width.saturating_sub(label.border)
                    || y < label.border
                    || y >= label.height.saturating_sub(label.border)
                {
                    set_pixel(&mut rows, label.width, x, y);
                }
            }
        }
    }

    for (index, character) in label.text.chars().enumerate() {
        let glyph = font
            .glyphs
            .get(&character)
            .with_context(|| format!("Font has no glyph for {character:?}"))?;
        if glyph.len() != font.height {
            bail!("Glyph for {character:?} does not have {} rows", font.height);
        }
        let x0 = index
            .checked_mul(font.width)
            .and_then(|x| x.checked_mul(label.scale))
            .and_then(|x| x.checked_add(label.margin.left))
            .context("Text position overflow")?;
        for (glyph_y, bits) in glyph.iter().enumerate() {
            for glyph_x in 0..font.width {
                if bits & (0x80 >> glyph_x) == 0 {
                    continue;
                }
                let glyph_x = glyph_x
                    .checked_mul(label.scale)
                    .context("Text position overflow")?;
                let glyph_y = glyph_y
                    .checked_mul(label.scale)
                    .context("Text position overflow")?;
                let x = x0.checked_add(glyph_x).context("Text position overflow")?;
                let y = label
                    .margin
                    .top
                    .checked_add(glyph_y)
                    .context("Text position overflow")?;
                for dy in 0..label.scale {
                    for dx in 0..label.scale {
                        let px = x.checked_add(dx).context("Pixel position overflow")?;
                        let py = y.checked_add(dy).context("Pixel position overflow")?;
                        set_pixel(&mut rows, label.width, px, py);
                    }
                }
            }
        }
    }

    let mut packets = vec![
        packet(0x21, &[label.density])?,
        packet(0x23, &[0x01])?,
        packet(0x01, &[0x01])?,
        packet(0x03, &[0x01])?,
    ];

    let page_size = [
        u16::try_from(label.height)
            .context("Label height does not fit the printer protocol")?
            .to_be_bytes()
            .as_slice(),
        u16::try_from(
            bytes_per_row
                .checked_mul(8)
                .context("Label width overflow")?,
        )
        .context("Padded label width does not fit the printer protocol")?
        .to_be_bytes()
        .as_slice(),
    ]
    .concat();
    packets.push(packet(0x13, &page_size)?);

    for (row_number, row) in rows.iter().enumerate() {
        let black_pixels: usize = row.iter().map(|byte| byte.count_ones() as usize).sum();
        let black_pixels = u16::try_from(black_pixels).context("Row has too many black pixels")?;
        let mut data = Vec::with_capacity(6 + row.len());
        data.extend_from_slice(
            &u16::try_from(row_number)
                .context("Row number does not fit the printer protocol")?
                .to_be_bytes(),
        );
        data.push(0x00);
        data.push(black_pixels as u8);
        data.push((black_pixels >> 8) as u8);
        data.push(0x01);
        data.extend_from_slice(row);
        packets.push(packet(0x85, &data)?);
    }
    packets.push(packet(0xe3, &[0x01])?);
    Ok(packets)
}

fn set_pixel(rows: &mut [Vec<u8>], width: usize, x: usize, y: usize) {
    if x < width && y < rows.len() {
        rows[y][x / 8] |= 0x80 >> (x % 8);
    }
}

fn packet(command: u8, data: &[u8]) -> Result<Vec<u8>> {
    let length = u8::try_from(data.len()).context("NIIMBOT packet data exceeds 255 bytes")?;
    let checksum = data
        .iter()
        .fold(command ^ length, |checksum, byte| checksum ^ byte);
    let mut bytes = Vec::with_capacity(data.len() + 7);
    bytes.extend_from_slice(&[0x55, 0x55, command, length]);
    bytes.extend_from_slice(data);
    bytes.extend_from_slice(&[checksum, 0xaa, 0xaa]);
    Ok(bytes)
}

async fn send_with_busy_retry(
    client: &reqwest::Client,
    packets: &[Vec<u8>],
) -> Result<Option<Url>> {
    let mut pending = packets.to_vec();
    let mut location = None;
    let mut has_sent_packets = false;
    let mut recovered_page = false;
    while !pending.is_empty() {
        let result = match send_packets(client, &pending).await {
            Ok(result) => {
                has_sent_packets = true;
                result
            }
            Err(error)
                if !has_sent_packets
                    && !recovered_page
                    && error
                        .downcast_ref::<SerialApiError>()
                        .is_some_and(SerialApiError::is_page_printing_order_error) =>
            {
                recovered_page = true;
                wait_until_ready(client).await?;
                send_packets(client, &[packet(0xf3, &[0x01])?]).await?;
                continue;
            }
            Err(error) => return Err(error),
        };
        if result.location.is_some() {
            location = result.location;
        }
        if !result
            .responses
            .iter()
            .any(|response| is_busy_response(response))
        {
            if result.dropped != 0 {
                bail!(
                    "Serial endpoint reported {} dropped packets without a busy response",
                    result.dropped
                );
            }
            break;
        }
        if result.dropped == 0 || result.dropped > pending.len() {
            bail!(
                "Serial endpoint reported an invalid dropped packet count: {}",
                result.dropped
            );
        }
        pending = pending.split_off(pending.len() - result.dropped);
        wait_until_ready(client).await?;
    }
    Ok(location)
}

async fn wait_until_ready(client: &reqwest::Client) -> Result<()> {
    for _ in 0..MAX_BUSY_POLLS {
        let result = send_packets(client, &[packet(0xa3, &[0x01])?]).await?;
        if !result
            .responses
            .iter()
            .any(|response| is_busy_response(response))
        {
            return Ok(());
        }
    }
    bail!("Printer remained busy after {MAX_BUSY_POLLS} PrintStatus queries")
}

fn is_busy_response(response: &[u8]) -> bool {
    response == [0x55, 0x55, 0xdb, 0x01, 0x09, 0xd3, 0xaa, 0xaa]
}

async fn send_packets(client: &reqwest::Client, packets: &[Vec<u8>]) -> Result<SerialResult> {
    let hex = packets
        .iter()
        .flatten()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    let response = client
        .post(crate::api_url("label-printer/serial")?)
        .json(&json!({ "hex": hex }))
        .send()
        .await
        .context("POST /serial request failed")?;
    let status = response.status();
    let response_url = response.url().clone();
    let location_header = response
        .headers()
        .get(reqwest::header::LOCATION)
        .map(|value| {
            value
                .to_str()
                .context("Serial Location header is not valid text")
        })
        .transpose()?;
    let location = location_header
        .map(|value| response_url.join(value))
        .transpose()
        .context("Serial Location header is not a valid URL")?;
    let body = response
        .bytes()
        .await
        .context("Could not read POST /serial response")?;
    if !status.is_success() && status != StatusCode::ACCEPTED {
        let body = String::from_utf8_lossy(&body).into_owned();
        let error = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
        return Err(SerialApiError {
            status,
            body,
            problem: error
                .pointer("/details/problem")
                .and_then(Value::as_str)
                .map(str::to_owned),
            printer_state: error
                .pointer("/details/printerState")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }
        .into());
    }

    let envelope: Envelope<Value> =
        serde_json::from_slice(&body).context("POST /serial returned invalid JSON")?;
    let data = envelope.into_data("POST /serial")?;
    let responses = data
        .get("responses")
        .and_then(Value::as_array)
        .context("POST /serial data has no responses array")?
        .iter()
        .map(parse_response_packet)
        .collect::<Result<Vec<_>>>()?;
    let dropped = data
        .get("dropped")
        .map(|value| {
            usize::try_from(
                value
                    .as_u64()
                    .context("POST /serial dropped count is not an unsigned integer")?,
            )
            .context("POST /serial dropped count is too large")
        })
        .transpose()?
        .unwrap_or(0);

    Ok(SerialResult {
        location,
        responses,
        dropped,
    })
}

fn parse_response_packet(value: &Value) -> Result<Vec<u8>> {
    if let Some(hex) = value.as_str() {
        let chunks: Vec<&str> = hex.split_whitespace().collect();
        if chunks.len() == 1 && chunks[0].trim_start_matches("0x").len() > 2 {
            let compact = chunks[0].trim_start_matches("0x");
            if compact.len() % 2 != 0 {
                bail!("Response packet has an odd number of hex digits");
            }
            return (0..compact.len())
                .step_by(2)
                .map(|index| {
                    u8::from_str_radix(&compact[index..index + 2], 16)
                        .with_context(|| format!("Invalid response byte in {hex:?}"))
                })
                .collect();
        }
        return chunks
            .iter()
            .map(|byte| {
                u8::from_str_radix(byte.trim_start_matches("0x"), 16)
                    .with_context(|| format!("Invalid response byte {byte:?}"))
            })
            .collect();
    }
    if let Some(bytes) = value.as_array() {
        return bytes
            .iter()
            .map(|byte| {
                byte.as_u64()
                    .and_then(|byte| u8::try_from(byte).ok())
                    .context("Response packet contains a non-byte value")
            })
            .collect();
    }
    if let Some(hex) = value.get("hex").and_then(Value::as_str) {
        return parse_response_packet(&Value::String(hex.to_owned()));
    }
    bail!("Unsupported serial response packet format: {value}")
}

#[derive(Debug)]
struct JobResult {
    status: String,
    result: Option<String>,
    body: String,
}

async fn wait_for_job(client: &reqwest::Client, location: &Url) -> Result<JobResult> {
    for _ in 0..MAX_JOB_POLLS {
        let response = client
            .get(location.clone())
            .send()
            .await
            .context("GET print job request failed")?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("Could not read print job response")?;
        if !status.is_success() {
            bail!("GET print job returned {status}: {body}");
        }

        let envelope: Envelope<Value> =
            serde_json::from_str(&body).context("GET print job returned invalid JSON")?;
        let data = envelope.into_data("GET print job")?;
        let job = data
            .get("job")
            .context("GET print job response has no data.job object")?;
        let job_status = job
            .get("status")
            .and_then(Value::as_str)
            .context("Print job has no string status")?;
        let result = job.get("result").and_then(Value::as_str).map(str::to_owned);
        if job_status == "done" {
            return Ok(JobResult {
                status: job_status.to_owned(),
                result,
                body,
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    bail!("Print job did not finish after {MAX_JOB_POLLS} polls")
}
