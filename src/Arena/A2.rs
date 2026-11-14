use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

const RANKED_GAMES: usize = 20;
const MAX_GUESSES: usize = 10;

type Code = [u8; 4];

#[derive(Debug, serde::Deserialize)]
struct ApiResponse<T> {
    status: Value,
    message: String,
    data: Option<T>,
}

#[derive(Debug, serde::Deserialize)]
struct NewGame {
    uuid: String,
}

#[derive(Debug, Serialize)]
struct GuessRequest {
    guess: Code,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GuessResult {
    black: u8,
    white: u8,
    solved: Option<bool>,
    finished: Option<bool>,
    score: Option<usize>,
}

pub async fn solve(client: &Client) -> Result<()> {
    let mut total_score = 0usize;

    for game_number in 1..=RANKED_GAMES {
        let score = play_ranked_game(client).await?;
        total_score += score;
        println!("Ranked game {game_number}/{RANKED_GAMES}: {score} guesses");
    }

    println!(
        "Average score across {RANKED_GAMES} ranked games: {:.2}",
        total_score as f64 / RANKED_GAMES as f64
    );
    Ok(())
}

async fn play_ranked_game(client: &Client) -> Result<usize> {
    let game: NewGame = post_data(
        client,
        &crate::api_url("code-breaker/games")?,
        &serde_json::json!({
            "mode": "ranked"
        }),
    )
    .await
    .context("Could not start a ranked code-breaker game")?;

    let mut possible = all_codes();
    for attempt in 1..=MAX_GUESSES {
        let guess = choose_guess(&possible);
        let result: GuessResult = post_data(
            client,
            &crate::api_url(&format!("code-breaker/games/{}/guess", game.uuid))?,
            &GuessRequest { guess },
        )
        .await
        .with_context(|| format!("Could not submit guess {attempt} for game {}", game.uuid))?;

        if result.black > 4 || result.white > 4 || result.black + result.white > 4 {
            bail!(
                "Game {} returned invalid feedback: black={}, white={}",
                game.uuid,
                result.black,
                result.white
            );
        }

        println!(
            "  Guess {attempt}: {:?} -> {} black, {} white",
            guess, result.black, result.white
        );

        if result.solved.unwrap_or(result.black == 4) {
            return Ok(result.score.unwrap_or(attempt));
        }
        if result.finished.unwrap_or(attempt == MAX_GUESSES) {
            break;
        }

        possible.retain(|candidate| feedback(candidate, &guess) == (result.black, result.white));
        if possible.is_empty() {
            bail!(
                "Game {} returned feedback inconsistent with all possible codes",
                game.uuid
            );
        }
    }

    if let Some(secret) = get_finished_secret(client, &game.uuid).await? {
        println!(
            "  Game {} was not solved; secret was {:?}",
            game.uuid, secret
        );
    }
    Ok(11)
}

async fn get_finished_secret(client: &Client, uuid: &str) -> Result<Option<Code>> {
    #[derive(serde::Deserialize)]
    struct GameStatus {
        secret: Option<Code>,
    }

    let response = client
        .get(crate::api_url(&format!("code-breaker/games/{uuid}"))?)
        .send()
        .await
        .context("Could not fetch finished game status")?;
    let http_status = response.status();
    let body = response
        .text()
        .await
        .context("Could not read finished game status")?;
    if !http_status.is_success() {
        bail!("GET /games/{uuid} returned {http_status}: {body}");
    }

    let api_response: ApiResponse<GameStatus> =
        serde_json::from_str(&body).context("GET finished game status returned invalid JSON")?;
    api_response.data.map(|data| data.secret).with_context(|| {
        format!(
            "GET /games/{uuid} returned no data (status: {}, message: {})",
            api_response.status, api_response.message
        )
    })
}

async fn post_data<T: DeserializeOwned, B: Serialize>(
    client: &Client,
    endpoint: &str,
    body: &B,
) -> Result<T> {
    let response = client
        .post(endpoint)
        .json(body)
        .send()
        .await
        .with_context(|| format!("POST {endpoint} request failed"))?;
    let http_status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("Could not read POST {endpoint} response"))?;
    if !http_status.is_success() {
        bail!("POST {endpoint} returned {http_status}: {body}");
    }

    let api_response: ApiResponse<T> = serde_json::from_str(&body)
        .with_context(|| format!("POST {endpoint} returned invalid JSON"))?;
    api_response.data.with_context(|| {
        format!(
            "POST {endpoint} returned no data (status: {}, message: {})",
            api_response.status, api_response.message
        )
    })
}

fn all_codes() -> Vec<Code> {
    let mut codes = Vec::with_capacity(6usize.pow(4));
    for a in 1..=6 {
        for b in 1..=6 {
            for c in 1..=6 {
                for d in 1..=6 {
                    codes.push([a, b, c, d]);
                }
            }
        }
    }
    codes
}

fn feedback(secret: &Code, guess: &Code) -> (u8, u8) {
    let black = secret
        .iter()
        .zip(guess)
        .filter(|(secret_digit, guess_digit)| secret_digit == guess_digit)
        .count() as u8;
    let mut secret_counts = [0u8; 7];
    let mut guess_counts = [0u8; 7];
    for (&secret_digit, &guess_digit) in secret.iter().zip(guess) {
        secret_counts[secret_digit as usize] += 1;
        guess_counts[guess_digit as usize] += 1;
    }
    let matching_digits: u8 = (1..=6)
        .map(|digit| secret_counts[digit].min(guess_counts[digit]))
        .sum();
    (black, matching_digits - black)
}

fn choose_guess(possible: &[Code]) -> Code {
    let all = all_codes();
    let mut best_guess = all[0];
    let mut best_worst_bucket = usize::MAX;
    let mut best_squared_buckets = u64::MAX;
    let mut best_is_possible = false;

    for guess in all {
        let is_possible = possible.contains(&guess);
        let mut buckets = [0usize; 25];
        let mut worst_bucket = 0usize;

        for candidate in possible {
            let (black, white) = feedback(candidate, &guess);
            let bucket = &mut buckets[black as usize * 5 + white as usize];
            *bucket += 1;
            worst_bucket = worst_bucket.max(*bucket);
            if worst_bucket > best_worst_bucket {
                break;
            }
        }

        if worst_bucket > best_worst_bucket {
            continue;
        }
        let squared_buckets: u64 = buckets.iter().map(|count| (*count as u64).pow(2)).sum();
        if (worst_bucket, squared_buckets, !is_possible)
            < (best_worst_bucket, best_squared_buckets, !best_is_possible)
        {
            best_guess = guess;
            best_worst_bucket = worst_bucket;
            best_squared_buckets = squared_buckets;
            best_is_possible = is_possible;
        }
    }

    best_guess
}
