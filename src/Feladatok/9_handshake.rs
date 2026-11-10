use aes_gcm::{
    Aes256Gcm, Nonce, Tag,
    aead::{AeadInPlace, KeyInit},
};
use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use hkdf::Hkdf;
use p256::{
    PublicKey, SecretKey,
    ecdh::diffie_hellman,
    ecdsa::{Signature, VerifyingKey, signature::Verifier},
    elliptic_curve::Generate,
    pkcs8::{DecodePublicKey, EncodePublicKey, LineEnding},
};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const API_BASE: &str =
    "https://codeweek.blathy.info/api/task/HkcFuUnk8AEOjH0qSvZdTfhgVZWgHca5/secret-handshake";

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    status: Value,
    message: String,
    data: Option<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub instance_id: String,
    pub server_public_key: String,
    pub signing_public_key: String,
    pub salt: String,
    pub info: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub instance_id: String,
    pub public_key: String,
}

#[derive(Debug, Deserialize)]
pub struct InboxMessage {
    pub id: u64,
    pub nonce: String,
    pub ciphertext: String,
    pub tag: String,
    pub signature: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InboxData {
    instance_id: String,
    messages: Vec<InboxMessage>,
}

#[derive(Debug, Serialize)]
pub struct EncryptedSolution {
    pub nonce: String,
    pub ciphertext: String,
    pub tag: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum PlainMessage {
    Word {
        position: usize,
        word: String,
    },
    Format {
        words: usize,
        separator: String,
        case: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SelftestRequest {
    shared_secret_sha256: String,
    key_sha256: String,
}

pub struct HandshakeKeys {
    pub hello: Hello,
    pub shared_secret_sha256: [u8; 32],
    pub aes_key: [u8; 32],
}

pub fn encrypt_answer(key: &[u8; 32], answer: &str) -> Result<EncryptedSolution> {
    let plaintext = serde_json::to_vec(&serde_json::json!({ "answer": answer }))
        .context("Could not serialize the answer")?;
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce)
        .map_err(|error| anyhow::anyhow!("Could not generate a random AES-GCM nonce: {error}"))?;
    let mut ciphertext = plaintext;
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|_| anyhow::anyhow!("AES-256-GCM key must be exactly 32 bytes"))?;
    let tag = cipher
        .encrypt_in_place_detached(Nonce::from_slice(&nonce), b"", &mut ciphertext)
        .map_err(|_| anyhow::anyhow!("Could not encrypt the solution"))?;

    Ok(EncryptedSolution {
        nonce: STANDARD.encode(nonce),
        ciphertext: STANDARD.encode(ciphertext),
        tag: STANDARD.encode(tag),
    })
}

pub fn decrypt_message(key: &[u8; 32], message: &InboxMessage) -> Result<Vec<u8>> {
    let nonce = STANDARD
        .decode(&message.nonce)
        .context("Message nonce is not valid base64")?;
    let mut ciphertext = STANDARD
        .decode(&message.ciphertext)
        .context("Message ciphertext is not valid base64")?;
    let tag = STANDARD
        .decode(&message.tag)
        .context("Message tag is not valid base64")?;

    let nonce: &[u8; 12] = nonce
        .as_slice()
        .try_into()
        .context("AES-GCM nonce must be exactly 12 bytes")?;
    let tag: &[u8; 16] = tag
        .as_slice()
        .try_into()
        .context("AES-GCM tag must be exactly 16 bytes")?;
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|_| anyhow::anyhow!("AES-256-GCM key must be exactly 32 bytes"))?;

    cipher
        .decrypt_in_place_detached(
            Nonce::from_slice(nonce),
            b"",
            &mut ciphertext,
            Tag::from_slice(tag),
        )
        .map_err(|_| anyhow::anyhow!("Message {} failed AES-GCM authentication", message.id))?;

    Ok(ciphertext)
}

fn derive_keys(source: &Source) -> Result<HandshakeKeys> {
    let server_public_key = PublicKey::from_public_key_pem(&source.server_public_key)
        .context("Invalid server P-256 public key PEM")?;
    let salt = STANDARD
        .decode(&source.salt)
        .context("Handshake salt is not valid base64")?;
    if salt.len() != 16 {
        bail!("Handshake salt must decode to exactly 16 bytes");
    }

    let private_key = SecretKey::generate();
    let public_key = private_key
        .public_key()
        .to_public_key_pem(LineEnding::LF)
        .context("Could not encode the client public key as SPKI PEM")?;

    let shared_secret = diffie_hellman(
        private_key.to_nonzero_scalar(),
        server_public_key.as_affine(),
    );
    let shared_secret_sha256: [u8; 32] = Sha256::digest(shared_secret.raw_secret_bytes()).into();
    let hkdf = Hkdf::<Sha256>::new(Some(&salt), &shared_secret_sha256);
    let mut aes_key = [0u8; 32];
    hkdf.expand(b"bot", &mut aes_key)
        .context("Could not derive the AES key with HKDF-SHA256")?;

    Ok(HandshakeKeys {
        hello: Hello {
            instance_id: source.instance_id.clone(),
            public_key,
        },
        shared_secret_sha256,
        aes_key,
    })
}

fn derive_password(source: &Source, key: &[u8; 32], messages: &[InboxMessage]) -> Result<String> {
    let verifying_key = VerifyingKey::from_public_key_pem(&source.signing_public_key)
        .context("Invalid signing P-256 public key PEM")?;
    let mut words = std::collections::BTreeMap::new();
    let mut format = None;

    for message in messages {
        let plaintext = decrypt_message(key, message)
            .with_context(|| format!("Could not decrypt message {}", message.id))?;
        let signature_bytes = STANDARD
            .decode(&message.signature)
            .with_context(|| format!("Message {} signature is not valid base64", message.id))?;
        let signature = match Signature::from_der(&signature_bytes) {
            Ok(signature) => signature,
            Err(_) => continue,
        };
        if verifying_key.verify(&plaintext, &signature).is_err() {
            continue;
        }

        let content: PlainMessage = serde_json::from_slice(&plaintext)
            .with_context(|| format!("Authenticated message {} is invalid JSON", message.id))?;
        match content {
            PlainMessage::Word { position, word } => {
                if words.insert(position, word).is_some() {
                    bail!("Duplicate authenticated word position {position}");
                }
            }
            PlainMessage::Format {
                words: expected_words,
                separator,
                case,
            } => {
                if format.replace((expected_words, separator, case)).is_some() {
                    bail!("Inbox contains multiple authenticated format messages");
                }
            }
        }
    }

    let (expected_words, separator, case) =
        format.context("No authenticated format message was found")?;
    if words.len() != expected_words {
        bail!(
            "Expected {expected_words} authenticated words, found {}",
            words.len()
        );
    }

    let ordered_words: Vec<String> = words.into_values().collect();
    let transformed: Vec<String> = ordered_words
        .iter()
        .map(|word| match case.as_str() {
            "lower" => Ok(word.to_lowercase()),
            "upper" => Ok(word.to_uppercase()),
            other => bail!("Unknown password case format: {other}"),
        })
        .collect::<Result<_>>()?;

    Ok(transformed.join(&separator))
}

pub async fn solve(client: &reqwest::Client) -> Result<()> {
    let source = fetch_source(client).await?;
    let keys = derive_keys(&source)?;
    send_hello(client, &keys.hello).await?;
    let inbox = fetch_inbox(client).await?;
    if inbox.instance_id != source.instance_id {
        bail!("Inbox belongs to a different handshake instance");
    }
    if inbox.messages.is_empty() {
        bail!("Handshake inbox contains no messages");
    }

    let answer = derive_password(&source, &keys.aes_key, &inbox.messages)?;
    let selftest = SelftestRequest {
        shared_secret_sha256: hex(&keys.shared_secret_sha256),
        key_sha256: hex(&Sha256::digest(keys.aes_key)),
    };
    send_selftest(client, &selftest).await?;

    let encrypted_solution = encrypt_answer(&keys.aes_key, &answer)?;
    let result = send_solution(client, &encrypted_solution).await?;
    println!("{result}");
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

async fn fetch_source(client: &reqwest::Client) -> Result<Source> {
    let response = client
        .get(format!("{API_BASE}/source"))
        .send()
        .await
        .context("Could not request the handshake source")?;

    let http_status = response.status();

    let body = response
        .text()
        .await
        .context("Could not read the handshake source response")?;

    if http_status != StatusCode::OK {
        bail!("Handshake source returned {http_status}: {body}");
    }

    let api_response: ApiResponse<Source> =
        serde_json::from_str(&body).context("Handshake source returned invalid JSON")?;
    api_response.data.with_context(|| {
        format!(
            "Handshake source returned no data (status: {}, message: {})",
            api_response.status, api_response.message
        )
    })
}

async fn fetch_inbox(client: &reqwest::Client) -> Result<InboxData> {
    let response = client
        .get(format!("{API_BASE}/inbox"))
        .send()
        .await
        .context("Could not request the handshake inbox")?;

    let http_status = response.status();
    let body = response
        .text()
        .await
        .context("Could not read the handshake inbox response")?;

    if !http_status.is_success() {
        bail!("Handshake inbox returned {http_status}: {body}");
    }

    let api_response: ApiResponse<InboxData> =
        serde_json::from_str(&body).context("Handshake inbox returned invalid JSON")?;

    api_response.data.with_context(|| {
        format!(
            "Handshake inbox returned no data (status: {}, message: {})",
            api_response.status, api_response.message
        )
    })
}

async fn send_hello(client: &reqwest::Client, hello: &Hello) -> Result<Value> {
    let response = client
        .post(format!("{API_BASE}/hello"))
        .json(hello)
        .send()
        .await
        .context("Could not send the handshake hello request")?;

    let http_status = response.status();
    let body = response
        .text()
        .await
        .context("Could not read the handshake hello response")?;

    if !http_status.is_success() {
        bail!("Handshake hello returned {http_status}: {body}");
    }

    let api_response: Value =
        serde_json::from_str(&body).context("Handshake hello returned invalid JSON")?;
    Ok(api_response)
}

pub async fn send_solution(
    client: &reqwest::Client,
    solution: &EncryptedSolution,
) -> Result<Value> {
    let response = client
        .post(format!("{API_BASE}/solution"))
        .json(solution)
        .send()
        .await
        .context("Could not send the encrypted solution")?;

    let http_status = response.status();
    let body = response
        .text()
        .await
        .context("Could not read the solution response")?;

    if !http_status.is_success() {
        bail!("Handshake solution returned {http_status}: {body}");
    }

    serde_json::from_str(&body).context("Handshake solution returned invalid JSON")
}

async fn send_selftest(client: &reqwest::Client, selftest: &SelftestRequest) -> Result<Value> {
    let response = client
        .post(format!("{API_BASE}/selftest"))
        .json(selftest)
        .send()
        .await
        .context("Could not send the handshake selftest request")?;

    let http_status = response.status();
    let body = response
        .text()
        .await
        .context("Could not read the selftest response")?;

    if !http_status.is_success() {
        bail!("Handshake selftest failed with {http_status}: {body}");
    }

    serde_json::from_str(&body).context("Handshake selftest returned invalid JSON")
}
