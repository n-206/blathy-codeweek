use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::any,
};
use chrono::{DateTime, Duration, Timelike, Utc, naive::NaiveDate};
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    path::Path,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

const BODY_LIMIT: usize = 1024 * 1024;
type HmacSha256 = Hmac<Sha256>;
static PARKING_SERVER: tokio::sync::OnceCell<ServerHandle> = tokio::sync::OnceCell::const_new();

struct ServerHandle {
    run_key_echo: Arc<tokio::sync::RwLock<Option<HeaderValue>>>,
    managed: bool,
}

#[derive(Clone)]
struct AppState {
    secret: Arc<str>,
    garage: Arc<Mutex<GarageState>>,
    run_key_echo: Arc<tokio::sync::RwLock<Option<HeaderValue>>>,
}

#[derive(Default)]
struct GarageState {
    spots: Vec<OccupiedSpot>,
    tariff: Option<Tariff>,
    revenue: i64,
    idempotency: HashMap<String, Value>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Spot {
    id: i64,
    size: String,
    charger: bool,
    distance: i64,
}

struct OccupiedSpot {
    spot: Spot,
    car: Option<ParkedCar>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Tariff {
    free_minutes: i64,
    hourly_rate: i64,
    night_rate: i64,
    night_start: u32,
    night_end: u32,
    daily_cap: i64,
}

#[derive(Deserialize)]
struct Garage {
    spots: Vec<Spot>,
    tariff: Tariff,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ResetRequest {
    Wrapped { garage: Garage },
    Direct(Garage),
}

#[derive(Deserialize)]
struct CarRequest {
    plate: String,
    size: String,
}

#[derive(Clone)]
struct ParkedCar {
    plate: String,
    entry_time: DateTime<Utc>,
}

pub async fn zero_solve(client: &reqwest::Client, run_key: &str) -> Result<()> {
    solve_with_client(client, Some(run_key)).await
}

pub async fn start_server_if_needed(client: &reqwest::Client) -> Result<()> {
    start_server_for_run(client, None).await
}

async fn start_server_for_run(client: &reqwest::Client, run_key: Option<&str>) -> Result<()> {
    let server = PARKING_SERVER.get_or_try_init(|| async {
            configured_endpoint_url()?;
            let api_base =
                std::env::var("API_BASE").context("API_BASE must be set in .env or the environment")?;
            if api_base.trim().is_empty() {
                bail!("API_BASE must not be empty");
            }
            let api_base = format!("{}/parking-garage", api_base.trim_end_matches('/'));
            let port: u16 = std::env::var("PORT")
                .unwrap_or_else(|_| "3000".to_owned())
                .parse()
                .context("PORT must be a valid port number")?;
            if port == 0 {
                bail!("PORT must not be zero");
            }
            let address = SocketAddr::from(([0, 0, 0, 0], port));

            let source_response = client
                .get(format!("{api_base}/source"))
                .send()
                .await
                .context("Failed to fetch parking garage source for server startup")?;
            let source_status = source_response.status();
            let source_body = source_response
                .text()
                .await
                .context("Failed to read parking garage source for server startup")?;
            if !source_status.is_success() {
                bail!(
                    "GET /parking-garage/source returned {source_status}: {source_body}"
                );
            }
            let source: Value = serde_json::from_str(&source_body)
                .context("Parking garage source returned invalid JSON")?;
            let source = source.get("data").cloned().unwrap_or(source);
            let source: ServerSource = serde_json::from_value(source)
                .context("Parking garage source is missing secret or garage")?;

            let listener = match tokio::net::TcpListener::bind(address).await {
                Ok(listener) => listener,
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                    if run_key.is_some() {
                        bail!(
                            "PORT {port} is already in use by a server that cannot be configured to echo this Zero to Hero run key"
                        );
                    }
                    let health_url = format!("http://127.0.0.1:{port}/health");
                    let health = reqwest::Client::new()
                        .get(&health_url)
                        .send()
                        .await
                        .with_context(|| {
                            format!(
                                "PORT {port} is already in use and parking server health check failed"
                            )
                        })?;
                    if health.status().is_success() {
                        return Ok(ServerHandle {
                            run_key_echo: Arc::new(tokio::sync::RwLock::new(None)),
                            managed: false,
                        });
                    }
                    bail!(
                        "PORT {port} is already in use, but {health_url} returned {}",
                        health.status()
                    );
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("Failed to bind to {address}"));
                }
            };
            let run_key_echo = Arc::new(tokio::sync::RwLock::new(
                run_key.map(HeaderValue::from_str).transpose()
                    .context("Run key is not a valid HTTP header value")?,
            ));
            let app = router_with_state(
                source.secret,
                source.garage,
                run_key_echo.clone(),
            )?;
            tokio::spawn(async move {
                if let Err(error) = axum::serve(listener, app).await {
                    eprintln!("Parking-garage server stopped unexpectedly: {error}");
                }
            });
            println!("Parking-garage server started on {address}");
            Ok(ServerHandle {
                run_key_echo,
                managed: true,
            })
        })
        .await?;
    if run_key.is_some() && !server.managed {
        bail!("The running parking server cannot be configured to echo the Zero to Hero run key");
    }
    if let Some(run_key) = run_key {
        let run_key =
            HeaderValue::from_str(run_key).context("Run key is not a valid HTTP header value")?;
        *server.run_key_echo.write().await = Some(run_key);
    } else {
        *server.run_key_echo.write().await = None;
    }
    Ok(())
}

pub async fn register_run_endpoint(client: &reqwest::Client, api_base: &str) -> Result<()> {
    let endpoint_url = configured_endpoint_url()?;

    let response = client
        .put(format!("{api_base}/endpoint"))
        .json(&serde_json::json!({ "url": endpoint_url }))
        .send()
        .await
        .context("Failed to register PARKING_URL for this run")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("Failed to read parking endpoint registration response")?;
    if !status.is_success() {
        bail!("Parking endpoint registration returned {status}: {body}");
    }
    Ok(())
}

pub async fn solve(client: &reqwest::Client) -> Result<()> {
    solve_with_client(client, None).await
}

async fn solve_with_client(client: &reqwest::Client, run_key: Option<&str>) -> Result<()> {
    if Path::new(".env").exists() {
        dotenvy::from_path(".env").context("Failed to load .env")?;
    }

    if let Some(run_key) = run_key {
        start_server_for_run(client, Some(run_key)).await?;
    } else {
        start_server_if_needed(client).await?;
    }
    let api_base =
        std::env::var("API_BASE").context("API_BASE must be set in .env or the environment")?;
    if api_base.trim().is_empty() {
        bail!("API_BASE must not be empty");
    }
    let api_base = format!("{}/parking-garage", api_base.trim_end_matches('/'));
    register_run_endpoint(&client, &api_base).await?;

    let source_response = client
        .get(format!("{api_base}/source"))
        .send()
        .await
        .context("Failed to fetch parking garage source")?;
    let source_status = source_response.status();
    let source_body = source_response
        .text()
        .await
        .context("Failed to read parking garage source")?;
    if !source_status.is_success() {
        bail!("GET /parking-garage/source returned {source_status}: {source_body}");
    }
    let _source: Value = serde_json::from_str(&source_body)
        .context("Parking garage source returned invalid JSON")?;

    let response = client
        .post(format!("{api_base}/runs"))
        .send()
        .await
        .context("Failed to start parking garage run")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("Failed to read run response")?;

    if !status.is_success() {
        bail!("POST /parking-garage/runs returned {status}: {body}");
    }

    if !body.trim().is_empty() {
        println!("{body}");
    }

    Ok(())
}

fn configured_endpoint_url() -> Result<String> {
    let endpoint_url =
        std::env::var("PARKING_URL").context("PARKING_URL must be set for the parking task")?;
    let parsed_url = reqwest::Url::parse(&endpoint_url).context("PARKING_URL is invalid")?;
    if parsed_url.scheme() != "https" {
        bail!("PARKING_URL must use https");
    }
    Ok(endpoint_url)
}

#[derive(Deserialize)]
struct ServerSource {
    secret: String,
    garage: Value,
}

pub fn router_with_garage(secret: impl Into<String>, garage: Value) -> Result<Router> {
    router_with_state(secret, garage, Arc::new(tokio::sync::RwLock::new(None)))
}

fn router_with_state(
    secret: impl Into<String>,
    garage: Value,
    run_key_echo: Arc<tokio::sync::RwLock<Option<HeaderValue>>>,
) -> Result<Router> {
    let garage: Garage = serde_json::from_value(garage)?;
    if !valid_garage(&garage) {
        bail!("source returned invalid garage data");
    }

    let state = AppState {
        secret: Arc::from(secret.into()),
        garage: Arc::new(Mutex::new(garage_state(garage))),
        run_key_echo,
    };
    Ok(app_router(state))
}

fn app_router(state: AppState) -> Router {
    Router::new().fallback(any(dispatch)).with_state(state)
}

fn garage_state(garage: Garage) -> GarageState {
    GarageState {
        spots: garage
            .spots
            .into_iter()
            .map(|spot| OccupiedSpot { spot, car: None })
            .collect(),
        tariff: Some(garage.tariff),
        ..GarageState::default()
    }
}

async fn dispatch(State(state): State<AppState>, request: Request<Body>) -> Response {
    let response = dispatch_request(state.clone(), request).await;
    echo_run_key(response, &state.run_key_echo).await
}

async fn dispatch_request(state: AppState, request: Request<Body>) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    if method == Method::GET && matches!(uri.path().rsplit('/').next(), Some("health")) {
        return (
            StatusCode::OK,
            Json(json!({ "status": "ok", "message": "hello" })),
        )
            .into_response();
    }

    let headers = request.headers().clone();
    let body = match to_bytes(request.into_body(), BODY_LIMIT).await {
        Ok(body) => body,
        Err(_) => return error(StatusCode::BAD_REQUEST, "invalid_request"),
    };

    if !authorized(&state.secret, &method, &uri, &headers, &body) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let Some(sim_time) = simulated_time(&headers) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };

    let path = uri.path();
    let response = if method == Method::POST && path.ends_with("/reset") {
        reset(&state, &body)
    } else if method == Method::POST && path.ends_with("/cars") {
        arrive(&state, &headers, &body, sim_time)
    } else if method == Method::DELETE {
        match deleted_plate(path) {
            Some(plate) => depart(&state, plate, sim_time),
            None => error(StatusCode::NOT_FOUND, "not_found"),
        }
    } else if method == Method::GET && path.ends_with("/spots") {
        spots(&state, uri.query())
    } else if method == Method::GET && path.ends_with("/stats") {
        stats(&state)
    } else {
        error(StatusCode::NOT_FOUND, "not_found")
    };
    response
}

async fn echo_run_key(
    mut response: Response,
    run_key: &tokio::sync::RwLock<Option<HeaderValue>>,
) -> Response {
    if let Some(run_key) = run_key.read().await.as_ref() {
        response.headers_mut().insert("x-run-key", run_key.clone());
    }
    response
}

fn authorized(secret: &str, method: &Method, uri: &Uri, headers: &HeaderMap, body: &[u8]) -> bool {
    let Some(timestamp_text) = headers
        .get("x-timestamp")
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Ok(timestamp) = timestamp_text.parse::<i64>() else {
        return false;
    };
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return false;
    };
    if (i128::from(now.as_secs()) - i128::from(timestamp)).abs() > 30 {
        return false;
    }

    let Some(signature) = headers
        .get("x-signature")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("sha256="))
        .and_then(|value| hex::decode(value).ok())
    else {
        return false;
    };

    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(mac) => mac,
        Err(_) => return false,
    };
    let mut message =
        format!("{}.{}.{}.", timestamp_text, method.as_str(), uri.path(),).into_bytes();
    message.extend_from_slice(body);
    mac.update(&message);
    mac.verify_slice(&signature).is_ok()
}

fn simulated_time(headers: &HeaderMap) -> Option<DateTime<Utc>> {
    let value = headers.get("x-sim-time")?.to_str().ok()?;
    let parsed = DateTime::parse_from_rfc3339(value).ok()?;
    (parsed.offset().local_minus_utc() == 0).then(|| parsed.with_timezone(&Utc))
}

fn reset(state: &AppState, body: &[u8]) -> Response {
    let request = match serde_json::from_slice::<ResetRequest>(body) {
        Ok(request) => request,
        Err(_) => return error(StatusCode::BAD_REQUEST, "invalid_request"),
    };
    let garage = match request {
        ResetRequest::Wrapped { garage } | ResetRequest::Direct(garage) => garage,
    };
    if !valid_garage(&garage) {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    }

    let mut state = match state.garage.lock() {
        Ok(state) => state,
        Err(_) => return internal_error(),
    };
    *state = garage_state(garage);
    StatusCode::NO_CONTENT.into_response()
}

fn valid_garage(garage: &Garage) -> bool {
    let mut ids = std::collections::HashSet::new();
    garage.spots.iter().all(|spot| {
        ids.insert(spot.id)
            && matches!(spot.size.as_str(), "small" | "large" | "medium")
            && spot.distance >= 0
    }) && garage.tariff.free_minutes >= 0
        && garage.tariff.hourly_rate >= 0
        && garage.tariff.night_rate >= 0
        && garage.tariff.daily_cap >= 0
        && garage.tariff.night_start < 24
        && garage.tariff.night_end < 24
}

fn arrive(
    state: &AppState,
    headers: &HeaderMap,
    body: &[u8],
    entry_time: DateTime<Utc>,
) -> Response {
    let request = match serde_json::from_slice::<CarRequest>(body) {
        Ok(request) => request,
        Err(_) => return error(StatusCode::BAD_REQUEST, "invalid_request"),
    };
    if !valid_plate(&request.plate)
        || !matches!(request.size.as_str(), "small" | "large" | "electric")
    {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    }

    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut state = match state.garage.lock() {
        Ok(state) => state,
        Err(_) => return internal_error(),
    };
    if let Some(response) = idempotency_key
        .as_ref()
        .and_then(|key| state.idempotency.get(key))
    {
        return (StatusCode::CREATED, Json(response.clone())).into_response();
    }
    if state.spots.iter().any(|spot| {
        spot.car
            .as_ref()
            .is_some_and(|car| car.plate == request.plate)
    }) {
        return error(StatusCode::CONFLICT, "already_parked");
    }

    let Some(index) = choose_spot(&state.spots, &request.size) else {
        return error(StatusCode::CONFLICT, "garage_full");
    };
    let response = json!({
        "plate": request.plate,
        "size": request.size,
        "spot": state.spots[index].spot.id,
        "entryTime": entry_time.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
    });
    state.spots[index].car = Some(ParkedCar {
        plate: request.plate,
        entry_time,
    });
    if let Some(key) = idempotency_key {
        state.idempotency.insert(key, response.clone());
    }
    (StatusCode::CREATED, Json(response)).into_response()
}

fn choose_spot(spots: &[OccupiedSpot], size: &str) -> Option<usize> {
    let nearest = |charger: Option<bool>| {
        spots
            .iter()
            .enumerate()
            .filter(|(_, spot)| {
                spot.car.is_none()
                    && charger.is_none_or(|required| spot.spot.charger == required)
                    && (size == "electric"
                        || (!spot.spot.charger
                            && (size == "small" || (size == "large" && spot.spot.size == "large"))))
            })
            .min_by_key(|(_, spot)| (spot.spot.distance, spot.spot.id))
            .map(|(index, _)| index)
    };

    if size == "electric" {
        nearest(Some(true)).or_else(|| nearest(Some(false)))
    } else {
        nearest(Some(false))
    }
}

fn depart(state: &AppState, plate: &str, departure_time: DateTime<Utc>) -> Response {
    if !valid_plate(plate) {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let mut state = match state.garage.lock() {
        Ok(state) => state,
        Err(_) => return internal_error(),
    };
    let Some(index) = state
        .spots
        .iter()
        .position(|spot| spot.car.as_ref().is_some_and(|car| car.plate == plate))
    else {
        return error(StatusCode::NOT_FOUND, "not_parked");
    };
    let Some(car) = state.spots[index].car.as_ref() else {
        return internal_error();
    };
    let Some(minutes) = departure_time
        .signed_duration_since(car.entry_time)
        .num_seconds()
        .checked_div(60)
    else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    if minutes < 0 {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let Some(tariff) = state.tariff.as_ref() else {
        return internal_error();
    };
    let Some(fee) = calculate_fee(car.entry_time, minutes, tariff) else {
        return internal_error();
    };
    let Some(revenue) = state.revenue.checked_add(fee) else {
        return internal_error();
    };

    let spot_id = state.spots[index].spot.id;
    state.spots[index].car = None;
    state.revenue = revenue;
    (
        StatusCode::OK,
        Json(json!({
            "plate": plate,
            "spot": spot_id,
            "minutes": minutes,
            "fee": fee,
        })),
    )
        .into_response()
}

fn calculate_fee(entry_time: DateTime<Utc>, minutes: i64, tariff: &Tariff) -> Option<i64> {
    if minutes <= tariff.free_minutes {
        return Some(0);
    }
    let paid_minutes = minutes.checked_sub(tariff.free_minutes)?;
    let paid_hours = paid_minutes.checked_add(59)?.checked_div(60)?;
    let mut charges: BTreeMap<NaiveDate, i64> = BTreeMap::new();

    for hour in 0..paid_hours {
        let offset = tariff.free_minutes.checked_add(hour.checked_mul(60)?)?;
        let start = entry_time.checked_add_signed(Duration::minutes(offset))?;
        let current_hour = start.hour();
        let is_night = if tariff.night_start <= tariff.night_end {
            current_hour >= tariff.night_start && current_hour < tariff.night_end
        } else {
            current_hour >= tariff.night_start || current_hour < tariff.night_end
        };
        let rate = if is_night {
            tariff.night_rate
        } else {
            tariff.hourly_rate
        };
        let total = charges.entry(start.date_naive()).or_default();
        *total = total.checked_add(rate)?;
    }

    charges.values().try_fold(0_i64, |total, charge| {
        total.checked_add((*charge).min(tariff.daily_cap))
    })
}

fn spots(state: &AppState, query: Option<&str>) -> Response {
    let free_filter = match query.map(parse_free_filter) {
        Some(Ok(value)) => value,
        Some(Err(())) => return error(StatusCode::BAD_REQUEST, "invalid_request"),
        None => None,
    };
    let state = match state.garage.lock() {
        Ok(state) => state,
        Err(_) => return internal_error(),
    };
    let mut list: Vec<_> = state
        .spots
        .iter()
        .filter_map(|spot| {
            let free = spot.car.is_none();
            if free_filter.is_some_and(|filter| filter != free) {
                return None;
            }
            Some(json!({
                "id": spot.spot.id,
                "size": spot.spot.size,
                "charger": spot.spot.charger,
                "distance": spot.spot.distance,
                "free": free,
                "plate": spot.car.as_ref().map(|car| car.plate.as_str()),
            }))
        })
        .collect();
    list.sort_by_key(|spot| spot["id"].as_i64().unwrap_or_default());
    (StatusCode::OK, Json(json!({ "spots": list }))).into_response()
}

fn parse_free_filter(query: &str) -> Result<Option<bool>, ()> {
    let mut result = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "free" {
            if result.is_some() {
                return Err(());
            }
            result = Some(match value {
                "true" => true,
                "false" => false,
                _ => return Err(()),
            });
        }
    }
    Ok(result)
}

fn stats(state: &AppState) -> Response {
    let state = match state.garage.lock() {
        Ok(state) => state,
        Err(_) => return internal_error(),
    };
    let total = state.spots.len();
    let occupied = state.spots.iter().filter(|spot| spot.car.is_some()).count();
    let free_chargers = state
        .spots
        .iter()
        .filter(|spot| spot.spot.charger && spot.car.is_none())
        .count();
    (
        StatusCode::OK,
        Json(json!({
            "totalSpots": total,
            "occupiedSpots": occupied,
            "freeSpots": total - occupied,
            "freeChargers": free_chargers,
            "revenue": state.revenue,
        })),
    )
        .into_response()
}

fn deleted_plate(path: &str) -> Option<&str> {
    let mut parts = path.rsplit('/');
    let plate = parts.next()?;
    let cars = parts.next()?;
    (!plate.is_empty() && cars == "cars").then_some(plate)
}

fn valid_plate(plate: &str) -> bool {
    (2..=12).contains(&plate.len())
        && plate
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'-')
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn internal_error() -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
}
