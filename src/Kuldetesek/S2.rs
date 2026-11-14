use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::read::GzDecoder;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;
use std::io::Read;

#[derive(Debug, Deserialize)]
struct Source {
    payload: String,
    layers: usize,
}

pub async fn solve(client: &reqwest::Client) -> Result<()> {
    let response = client
        .get(crate::api_url("onion/source")?)
        .send()
        .await
        .context("GET /onion/source request failed")?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .context("Could not read GET /onion/source response")?;

    if !status.is_success() {
        bail!(
            "GET /onion/source returned {status}: {}",
            String::from_utf8_lossy(&body)
        );
    }

    let source: Source =
        serde_json::from_slice(&body).context("GET /onion/source returned invalid JSON")?;
    if !(1..=16).contains(&source.layers) {
        bail!("Unsupported onion layer count: {}", source.layers);
    }

    let mut visited = HashSet::new();
    let answer = peel(
        source.payload.into_bytes(),
        source.layers,
        false,
        &mut visited,
    )?
    .context("Could not find a valid Hungarian sentence in the onion")?;

    let response = client
        .post(crate::api_url("onion/solution")?)
        .json(&json!({ "answer": answer }))
        .send()
        .await
        .context("POST /onion/solution request failed")?;
    let status = response.status();
    let response_body = response
        .text()
        .await
        .context("Could not read POST /onion/solution response")?;

    if !status.is_success() {
        bail!("POST /onion/solution returned {status}: {response_body}");
    }

    println!("{response_body}");
    Ok(())
}

fn peel(
    bytes: Vec<u8>,
    layers_left: usize,
    xor_allowed: bool,
    visited: &mut HashSet<(usize, bool, Vec<u8>)>,
) -> Result<Option<String>> {
    if !visited.insert((layers_left, xor_allowed, bytes.clone())) {
        return Ok(None);
    }
    if layers_left == 0 {
        return Ok(find_answer(&bytes));
    }

    if let Ok(text) = std::str::from_utf8(&bytes) {
        if let Ok(decoded) = STANDARD.decode(text) {
            if decoded != bytes
                && let Some(answer) = peel(decoded, layers_left - 1, true, visited)?
            {
                return Ok(Some(answer));
            }
        }

        if let Some(decoded) = decode_hex(text)
            && decoded != bytes
            && let Some(answer) = peel(decoded, layers_left - 1, true, visited)?
        {
            return Ok(Some(answer));
        }
    }

    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut decoder = GzDecoder::new(bytes.as_slice());
        let mut decoded = Vec::new();
        if decoder.read_to_end(&mut decoded).is_ok()
            && decoded != bytes
            && let Some(answer) = peel(decoded, layers_left - 1, false, visited)?
        {
            return Ok(Some(answer));
        }
    }

    let mut rotated = bytes.clone();
    rot13(&mut rotated);
    if rotated != bytes
        && let Some(answer) = peel(rotated, layers_left - 1, xor_allowed, visited)?
    {
        return Ok(Some(answer));
    }

    let mut reversed = bytes.clone();
    reversed.reverse();
    if reversed != bytes
        && let Some(answer) = peel(reversed, layers_left - 1, xor_allowed, visited)?
    {
        return Ok(Some(answer));
    }

    if xor_allowed {
        for key in 1..=u8::MAX {
            let decoded = bytes.iter().map(|byte| byte ^ key).collect();
            if let Some(answer) = peel(decoded, layers_left - 1, false, visited)? {
                return Ok(Some(answer));
            }
        }
    }

    Ok(None)
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_digit(pair[0])?;
            let low = hex_digit(pair[1])?;
            Some(high * 16 + low)
        })
        .collect()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn rot13(bytes: &mut [u8]) {
    for byte in bytes {
        *byte = match *byte {
            b'a'..=b'm' | b'A'..=b'M' => *byte + 13,
            b'n'..=b'z' | b'N'..=b'Z' => *byte - 13,
            _ => *byte,
        };
    }
}

fn find_answer(bytes: &[u8]) -> Option<String> {
    let sentence = std::str::from_utf8(bytes).ok()?;
    if !sentence.chars().all(|character| {
        character.is_alphabetic() || character.is_whitespace() || ".,!?;:-'\"()".contains(character)
    }) {
        return None;
    }

    let mut uppercase_words = Vec::new();
    let mut word_count = 0;
    let mut current = String::new();

    let finish_word = |word: &mut String, word_count: &mut usize, answers: &mut Vec<String>| {
        if word.is_empty() {
            return;
        }
        *word_count += 1;
        if word.chars().count() >= 4 && word.chars().all(char::is_uppercase) {
            answers.push(std::mem::take(word));
        } else {
            word.clear();
        }
    };

    for character in sentence.chars() {
        if character.is_alphabetic() {
            current.push(character);
        } else {
            finish_word(&mut current, &mut word_count, &mut uppercase_words);
        }
    }
    finish_word(&mut current, &mut word_count, &mut uppercase_words);

    if word_count >= 4 && uppercase_words.len() == 1 {
        uppercase_words.pop()
    } else {
        None
    }
}
