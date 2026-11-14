use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use p256::elliptic_curve::Generate;
use reqwest::{Client, StatusCode, Url, header::HeaderMap};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use std::time::{SystemTime, UNIX_EPOCH};

pub async fn solve(_client: &Client) -> Result<()> {
    solve_with_run_key(None).await
}

pub async fn zero_solve(run_key: &str) -> Result<()> {
    solve_with_run_key(Some(run_key)).await
}

async fn solve_with_run_key(run_key: Option<&str>) -> Result<()> {
    let builder = Client::builder().redirect(reqwest::redirect::Policy::custom(|attempt| {
        if attempt.status() == StatusCode::SEE_OTHER {
            attempt.stop()
        } else {
            attempt.follow()
        }
    }));
    let builder = match run_key {
        Some(run_key) => builder.default_headers(super::run_key_headers(run_key)?),
        None => builder,
    };
    let client = builder
        .build()
        .context("A kézi 303-kezelésű HTTP-kliens létrehozása sikertelen")?;

    let response = client
        .get(crate::api_url("broken-login/source")?)
        .send()
        .await
        .context("A /source kérés sikertelen")?;

    let body = response.text().await?;
    let json: serde_json::Value =
        serde_json::from_str(&body).context("A /source válasza nem érvényes JSON")?;

    let data = &json["data"];
    let login = data["login"]
        .as_str()
        .context("Hiányzik a data.login mező")?;
    let username = data["username"]
        .as_str()
        .context("Hiányzik a data.username mező")?;
    let password = data["password"]
        .as_str()
        .context("Hiányzik a data.password mező")?;

    let response = client
        .post(login)
        .json(&serde_json::json!({
            "username": username,
            "password": password,
        }))
        .send()
        .await
        .context("A login kérés sikertelen")?;

    let status = response.status();
    let headers = response.headers().clone();

    if status != StatusCode::GONE {
        return Ok(());
    }

    {
        let link_header = headers
            .get(reqwest::header::LINK)
            .context("Nincs Link fejléc")?
            .to_str()
            .context("A Link fejléc nem érvényes szöveg")?;
        let start = link_header
            .find('<')
            .context("A Link fejlécből hiányzik a <")?
            + 1;
        let end = link_header[start..]
            .find('>')
            .map(|offset| start + offset)
            .context("A Link fejlécből hiányzik a >")?;

        let next_url = reqwest::Url::parse(login)
            .context("A régi belépési cím nem érvényes URL")?
            .join(&link_header[start..end])
            .context("A Link fejléc címe nem oldható fel")?;

        let response = client
            .post(next_url.clone())
            .json(&serde_json::json!({
                "username": username,
                "password": password,
            }))
            .send()
            .await
            .context("A v2 login kérés sikertelen")?;

        let status = response.status();
        let prerequisite_headers = match status {
            StatusCode::PRECONDITION_REQUIRED => response.headers().clone(),
            StatusCode::UNAUTHORIZED => {
                let challenge = response
                    .headers()
                    .get(reqwest::header::WWW_AUTHENTICATE)
                    .context("A kezdeti 401 válaszból hiányzik a WWW-Authenticate fejléc")?
                    .to_str()
                    .context("A WWW-Authenticate fejléc nem érvényes szöveg")?
                    .to_owned();
                let nonce = challenge_parameter(&challenge, "nonce")
                    .context("A kihívásból hiányzik a nonce")?;
                let algorithm = challenge_parameter(&challenge, "algorithm")
                    .context("A kihívásból hiányzik az algorithm")?;
                let proof = hmac_proof(password, &nonce, &algorithm)?;

                let response = client
                    .post(next_url.clone())
                    .json(&serde_json::json!({
                        "username": username,
                        "nonce": nonce,
                        "proof": proof,
                    }))
                    .send()
                    .await
                    .context("A HMAC-válaszos v2 login kérés sikertelen")?;
                let status = response.status();
                let headers = response.headers().clone();
                if status != StatusCode::PRECONDITION_REQUIRED {
                    let body = response
                        .text()
                        .await
                        .context("A HMAC-válaszos v2 login válasza nem olvasható")?;
                    anyhow::bail!(
                        "A HMAC-válaszos v2 login {status} státuszt adott; 428 Precondition Required volt várható. Fejlécnevek: {}. Válasz: {body}",
                        headers
                            .keys()
                            .map(|name| name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                headers
            }
            _ => {
                let headers = response.headers().clone();
                let body = response
                    .text()
                    .await
                    .context("A kezdeti v2 login válasza nem olvasható")?;
                anyhow::bail!(
                    "A kezdeti v2 login {status} státuszt adott; 401 vagy 428 volt várható. Fejlécnevek: {}. Válasz: {body}",
                    headers
                        .keys()
                        .map(|name| name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        };

        let register_url = link_with_rel(&prerequisite_headers, "register", &next_url)?;

        let response = client
            .request(reqwest::Method::OPTIONS, register_url.to_string())
            .send()
            .await
            .context("Az eszköz regisztráció options-se sikertelen.")?;

        let options_status = response.status();
        let options_body = response
            .bytes()
            .await
            .context("Az OPTIONS válasz nem olvasható")?;
        if !options_status.is_success() {
            anyhow::bail!(
                "Az eszközregisztráció OPTIONS kérése {options_status} státuszt adott: {}",
                String::from_utf8_lossy(&options_body)
            );
        }
        let options: serde_json::Value = serde_json::from_slice(&options_body)
            .context("Az eszközregisztráció OPTIONS válasza nem érvényes JSON")?;
        let platforms = options["data"]["methods"]["POST"]["fields"]["platform"]["enum"]
            .as_array()
            .context("Az OPTIONS válaszból hiányzik a platform enum lista")?;
        let platform = platforms
            .first()
            .and_then(serde_json::Value::as_str)
            .context("Az OPTIONS válasz nem adott meg elfogadott platformot")?;

        let response = client
            .post(register_url)
            .json(&serde_json::json!({
                "username": username,
                "deviceName": "codeweek-laptop",
                "platform": platform,
            }))
            .send()
            .await
            .context("Az eszközregisztráció sikertelen")?;

        let registration_status = response.status();
        let registration_headers = response.headers().clone();
        let registration_body = response
            .bytes()
            .await
            .context("A regisztrációs válasz nem olvasható")?;
        if registration_status != StatusCode::CREATED {
            anyhow::bail!(
                "Az eszközregisztráció {registration_status} státuszt adott: {}",
                String::from_utf8_lossy(&registration_body)
            );
        }
        let registration: serde_json::Value = serde_json::from_slice(&registration_body)
            .context("A regisztrációs válasz nem érvényes JSON")?;
        let registration_enrollment = find_otpauth_uri(&registration)
            .or_else(|| find_otpauth_uri_in_headers(&registration_headers));
        let registration_data = &registration["data"];
        let device_id = field_by_names(registration_data, &["deviceId", "device_id", "id"])
            .context("A 201 válaszból nem található a device id mező")?;
        let device_header = field_by_names(
            registration_data,
            &[
                "headerName",
                "header_name",
                "deviceHeader",
                "deviceHeaderName",
                "header",
                "requestHeader",
            ],
        )
        .or_else(|| field_by_names(&registration, &["headerName", "header_name", "header"]))
        .with_context(|| {
            format!(
                "A 201 válaszban nem található a device header neve. JSON data kulcsok: {}; HTTP fejlécnevek: {}",
                json_keys(registration_data),
                registration_headers
                    .keys()
                    .map(|name| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

        let challenge_response = client
            .post(next_url.clone())
            .header(device_header, device_id)
            .json(&serde_json::json!({
                "username": username,
                "password": password,
            }))
            .send()
            .await
            .context("A regisztrált eszköz login challenge kérése sikertelen")?;
        if challenge_response.status() != StatusCode::UNAUTHORIZED {
            let status = challenge_response.status();
            let body = challenge_response
                .text()
                .await
                .context("A login válasz nem olvasható")?;
            println!("Eszközös login státusz: {status}\nVálasz: {body}");
            return Ok(());
        }

        let challenge_headers = challenge_response.headers().clone();
        let challenge = match challenge_headers.get(reqwest::header::WWW_AUTHENTICATE) {
            Some(value) => value
                .to_str()
                .context("A WWW-Authenticate fejléc nem érvényes szöveg")?
                .to_owned(),
            None => {
                let status = challenge_response.status();
                let body = challenge_response
                    .text()
                    .await
                    .context("A challenge válasz törzse nem olvasható")?;
                anyhow::bail!(
                    "A regisztrált eszköz login challenge válasza {status}, de nincs WWW-Authenticate fejléc. Fejlécnevek: {}. Válasz: {body}",
                    challenge_headers
                        .keys()
                        .map(|name| name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        };
        let nonce = challenge_parameter(&challenge, "nonce")
            .context("A friss kihívásból hiányzik a nonce")?;
        let algorithm = challenge_parameter(&challenge, "algorithm")
            .context("A friss kihívásból hiányzik az algorithm")?;
        let proof = hmac_proof(password, &nonce, &algorithm)?;
        let response = client
            .post(next_url.clone())
            .header(device_header, device_id)
            .json(&serde_json::json!({
                "username": username,
                "nonce": nonce,
                "proof": proof,
            }))
            .send()
            .await
            .context("Az eszközazonosítós HMAC login sikertelen")?;
        let status = response.status();
        if status != StatusCode::SEE_OTHER {
            let body = response
                .text()
                .await
                .context("A login válasz nem olvasható")?;
            anyhow::bail!("Az eszközös HMAC login {status} státuszt adott: {body}");
        }

        let headers = response.headers();
        let enrollment_header = headers.values().find_map(|value| {
            value
                .to_str()
                .ok()
                .map(str::trim)
                .filter(|value| value.starts_with("otpauth://totp/"))
                .map(str::to_owned)
        });
        let location = headers
            .get(reqwest::header::LOCATION)
            .context("A 303 válaszból hiányzik a Location fejléc")?
            .to_str()
            .context("A Location fejléc nem érvényes szöveg")?;
        let session_cookie = headers
            .get(reqwest::header::SET_COOKIE)
            .context("A 303 válaszból hiányzik a Set-Cookie fejléc")?
            .to_str()
            .context("A Set-Cookie fejléc nem érvényes szöveg")?
            .split(';')
            .next()
            .context("A Set-Cookie fejléc üres")?
            .to_owned();
        let continuation_url = next_url
            .join(location)
            .context("A Location URL nem oldható fel")?;

        let response = client
            .get(continuation_url.clone())
            .header(reqwest::header::COOKIE, &session_cookie)
            .send()
            .await
            .context("A munkamenet folytatása sikertelen")?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("A munkamenet válasza nem olvasható")?;

        if !status.is_success() {
            anyhow::bail!("A munkamenet folytatása {status} státuszt adott: {body}");
        }
        let session: serde_json::Value =
            serde_json::from_str(&body).context("A munkamenet válasza nem érvényes JSON")?;
        let enrollment = enrollment_header
            .as_deref()
            .or(registration_enrollment.as_deref())
            .or_else(|| session["data"]["otpEnrollment"].as_str())
            .context(
                "A regisztrációs és 303 válaszból, valamint a munkamenet JSON-ból is hiányzik az otpauth://totp URI",
            )?;
        let otp = current_totp(enrollment)?;
        let (code_verifier, code_challenge) = create_pkce_pair()?;

        let response = client
            .post(continuation_url.clone())
            .header(reqwest::header::COOKIE, &session_cookie)
            .json(&serde_json::json!({
                "otp": otp,
                "code_challenge": code_challenge,
                "code_challenge_method": "S256",
            }))
            .send()
            .await
            .context("A TOTP kód beküldése sikertelen")?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .text()
            .await
            .context("A TOTP válasza nem olvasható")?;
        if !status.is_success() {
            anyhow::bail!("A TOTP/PKCE kérés {status} státuszt adott: {body}");
        }

        let authorization: serde_json::Value =
            serde_json::from_str(&body).context("A TOTP válasza nem érvényes JSON")?;
        let code = authorization["data"]["code"]
            .as_str()
            .context("A TOTP válaszból hiányzik az authorization code")?;
        let token_url = link_with_rel(&headers, "token", &continuation_url)?;
        let response = client
            .post(token_url.clone())
            .header(reqwest::header::COOKIE, &session_cookie)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("code_verifier", code_verifier.as_str()),
            ])
            .send()
            .await
            .context("A PKCE token-csere sikertelen")?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .text()
            .await
            .context("A token-válasz nem olvasható")?;

        if !status.is_success() {
            anyhow::bail!("A PKCE token-csere {status} státuszt adott: {body}");
        }
        let token: serde_json::Value =
            serde_json::from_str(&body).context("A token-válasz nem érvényes JSON")?;
        let access_token = token["access_token"]
            .as_str()
            .context("A token-válaszból hiányzik az access_token")?;
        let vault_url = link_with_rel(&headers, "resource", &token_url)?;

        let response = client
            .get(vault_url.clone())
            .bearer_auth(access_token)
            .send()
            .await
            .context("A széfvégpont lekérése sikertelen")?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .text()
            .await
            .context("A széfválasz nem olvasható")?;

        if !status.is_success() {
            anyhow::bail!("A széfvégpont {status} státuszt adott: {body}");
        }
        let vault: serde_json::Value =
            serde_json::from_str(&body).context("A széfválasz nem érvényes JSON")?;
        let secret = find_string_field(&vault, "secret")
            .context("A széfválaszból hiányzik a secret mező")?;
        if !secret.starts_with("BLATHY-") {
            anyhow::bail!("A széfválasz secret mezője nem BLATHY- kezdetű");
        }
        let submit_url = link_with_rel(&headers, "solution", &vault_url)?;

        let response = client
            .post(submit_url)
            .json(&serde_json::json!({ "secret": secret }))
            .send()
            .await
            .context("A titok beküldése sikertelen")?;

        let body = response
            .text()
            .await
            .context("A titokbeküldési válasz nem olvasható")?;

        println!("{body}");
    }

    Ok(())
}

fn field_by_names<'a>(value: &'a serde_json::Value, names: &[&str]) -> Option<&'a str> {
    let object = value.as_object()?;
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(serde_json::Value::as_str))
}

fn json_keys(value: &serde_json::Value) -> String {
    value
        .as_object()
        .map(|object| {
            object
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|| "(nem objektum)".to_owned())
}

fn find_otpauth_uri(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(value) if value.starts_with("otpauth://totp/") => {
            Some(value.clone())
        }
        serde_json::Value::Array(values) => values.iter().find_map(find_otpauth_uri),
        serde_json::Value::Object(values) => values.values().find_map(find_otpauth_uri),
        _ => None,
    }
}

fn find_string_field<'a>(value: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    match value {
        serde_json::Value::Object(values) => values
            .get(field)
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                values
                    .values()
                    .find_map(|value| find_string_field(value, field))
            }),
        serde_json::Value::Array(values) => values
            .iter()
            .find_map(|value| find_string_field(value, field)),
        _ => None,
    }
}

fn find_otpauth_uri_in_headers(headers: &HeaderMap) -> Option<String> {
    headers.values().find_map(|value| {
        value
            .to_str()
            .ok()
            .map(str::trim)
            .filter(|value| value.starts_with("otpauth://totp/"))
            .map(str::to_owned)
    })
}

fn link_with_rel(headers: &HeaderMap, relation: &str, base_url: &Url) -> Result<Url> {
    let mut available_links = Vec::new();
    for header_value in headers.get_all(reqwest::header::LINK) {
        let header_value = header_value
            .to_str()
            .context("A Link fejléc nem érvényes szöveg")?;
        for value in split_link_values(header_value) {
            available_links.push(value.to_owned());
            let Some(open) = value.find('<') else {
                continue;
            };
            let Some(close_offset) = value[open + 1..].find('>') else {
                continue;
            };
            let close = open + 1 + close_offset;
            let parameters = &value[close + 1..];
            let has_relation = parameters.split(';').any(|parameter| {
                parameter
                    .trim()
                    .split_once('=')
                    .is_some_and(|(name, value)| {
                        name.trim() == "rel"
                            && value
                                .trim()
                                .trim_matches('"')
                                .split_ascii_whitespace()
                                .any(|value| value == relation)
                    })
            });

            if has_relation {
                return base_url
                    .join(&value[open + 1..close])
                    .context("A regisztrációs Link URL nem oldható fel");
            }
        }
    }

    anyhow::bail!(
        "Nem található rel=\"{relation}\" kapcsolat a Link fejlécben. Link értékek: {}",
        if available_links.is_empty() {
            "(nincs)".to_owned()
        } else {
            available_links.join(" | ")
        }
    )
}

fn split_link_values(header: &str) -> Vec<&str> {
    let mut values = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut in_uri = false;
    for (index, character) in header.char_indices() {
        match character {
            '"' => in_quotes = !in_quotes,
            '<' if !in_quotes => in_uri = true,
            '>' if !in_quotes => in_uri = false,
            ',' if !in_quotes && !in_uri => {
                let value = header[start..index].trim();
                if !value.is_empty() {
                    values.push(value);
                }
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    let value = header[start..].trim();
    if !value.is_empty() {
        values.push(value);
    }
    values
}

fn challenge_parameter(challenge: &str, name: &str) -> Option<String> {
    challenge.split(',').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        (key == name).then(|| value.trim().trim_matches('"').to_owned())
    })
}

fn hmac_proof(password: &str, nonce: &str, algorithm: &str) -> Result<String> {
    let bytes = match algorithm {
        "HMAC-SHA256" => {
            let mut mac = Hmac::<Sha256>::new_from_slice(password.as_bytes())
                .context("Érvénytelen HMAC-SHA256 kulcs")?;
            mac.update(nonce.as_bytes());
            mac.finalize().into_bytes().to_vec()
        }
        "HMAC-SHA512" => {
            let mut mac = Hmac::<Sha512>::new_from_slice(password.as_bytes())
                .context("Érvénytelen HMAC-SHA512 kulcs")?;
            mac.update(nonce.as_bytes());
            mac.finalize().into_bytes().to_vec()
        }
        other => anyhow::bail!("Nem támogatott HMAC algoritmus: {other}"),
    };

    Ok(hex::encode(bytes))
}

fn current_totp(enrollment: &str) -> Result<String> {
    let enrollment = Url::parse(enrollment).context("Az OTP enrollment URI érvénytelen")?;
    if enrollment.scheme() != "otpauth" || enrollment.host_str() != Some("totp") {
        anyhow::bail!("Az OTP enrollment URI nem otpauth://totp típusú");
    }

    let mut secret = None;
    let mut algorithm = "SHA1".to_owned();
    let mut digits = 6_u32;
    let mut period = 30_u64;
    for (name, value) in enrollment.query_pairs() {
        match name.as_ref() {
            "secret" => secret = Some(value.into_owned()),
            "algorithm" => {
                if !value.is_empty() {
                    algorithm = value.into_owned();
                }
            }
            "digits" => {
                digits = value
                    .parse()
                    .context("Az OTP URI digits paramétere nem érvényes egész szám")?
            }
            "period" => {
                period = value
                    .parse()
                    .context("Az OTP URI period paramétere nem érvényes egész szám")?
            }
            _ => {}
        }
    }
    let secret = secret.context("Az OTP URI-ból hiányzik a secret paraméter")?;
    let digits = u32::try_from(digits).context("Az OTP digits értéke túl nagy")?;
    if !(1..=9).contains(&digits) || period == 0 {
        anyhow::bail!("Az OTP URI digits vagy period értéke érvénytelen");
    }
    if algorithm != "SHA1" {
        anyhow::bail!("Nem támogatott TOTP algoritmus: {algorithm}");
    }
    let secret = decode_base32(&secret)?;
    let unix_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("A rendszeridő 1970 előtti")?
        .as_secs();

    totp_at(&secret, unix_time, period, digits)
}

fn decode_base32(encoded: &str) -> Result<Vec<u8>> {
    let mut decoded = Vec::new();
    let mut buffer = 0_u32;
    let mut bits = 0_u8;

    for byte in encoded.bytes().filter(|byte| *byte != b'=') {
        let value = match byte.to_ascii_uppercase() {
            b'A'..=b'Z' => byte.to_ascii_uppercase() - b'A',
            b'2'..=b'7' => byte - b'2' + 26,
            _ => anyhow::bail!("Az OTP secret nem érvényes Base32"),
        };
        buffer = (buffer << 5) | u32::from(value);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            decoded.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }

    if decoded.is_empty() {
        anyhow::bail!("Az OTP Base32 secret üres");
    }
    Ok(decoded)
}

fn totp_at(secret: &[u8], unix_time: u64, period: u64, digits: u32) -> Result<String> {
    let counter = (unix_time / period).to_be_bytes();
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).context("Érvénytelen TOTP kulcs")?;
    mac.update(&counter);
    let hash = mac.finalize().into_bytes();
    let offset = usize::from(hash[hash.len() - 1] & 0x0f);
    let binary = (u32::from(hash[offset] & 0x7f) << 24)
        | (u32::from(hash[offset + 1]) << 16)
        | (u32::from(hash[offset + 2]) << 8)
        | u32::from(hash[offset + 3]);
    let modulus = 10_u32.pow(digits);
    Ok(format!(
        "{:0digits$}",
        binary % modulus,
        digits = digits as usize
    ))
}

fn create_pkce_pair() -> Result<(String, String)> {
    let entropy = <[u8; 32]>::try_generate().map_err(|error| {
        anyhow::anyhow!("A PKCE verifier biztonságos véletlen generálása sikertelen: {error}")
    })?;
    let verifier = URL_SAFE_NO_PAD.encode(entropy);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Ok((verifier, challenge))
}
