use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
    path::Path,
};

use anyhow::{Result, Context};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::any,
};
use chrono::{DateTime, Duration, Timelike, Utc, naive::NaiveDate};
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;

const BODY_LIMIT: usize = 1024 * 1024;
type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
struct AppState {
    secret: Arc<str>,
    garage: Arc<Mutex<GarageState>>,
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

pub async fn solve() -> Result<()> {
    if Path::new(".env").exists() {
        dotenvy::from_path(".env").context("Failed to load .env")?;
    }

    let api_base =
        std::env::var("API_BASE").context("API_BASE must be set in .env or the environment")?;
    if api_base.trim().is_empty() {
        anyhow::bail!("API_BASE must not be empty");
    }
    let api_base = format!("{}/parking-garage", api_base.trim_end_matches('/'));
    let response = reqwest::Client::new()
        .post(format!("{api_base}/runs"))
        .send()
        .await
        .context("Failed to start parking garage run")?;
    let body = response
        .text()
        .await
        .context("Failed to read run response")?;

    if !body.trim().is_empty() {
        println!("{body}");
    }

    Ok(())
}

pub fn router_with_garage(secret: impl Into<String>, garage: Value) -> Result<Router> {
    let garage: Garage = serde_json::from_value(garage)?;
    if !valid_garage(&garage) {
        anyhow::bail!("source returned invalid garage data");
    }

    let state = AppState {
        secret: Arc::from(secret.into()),
        garage: Arc::new(Mutex::new(garage_state(garage))),
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
    if method == Method::POST && path.ends_with("/reset") {
        return reset(&state, &body);
    }
    if method == Method::POST && path.ends_with("/cars") {
        return arrive(&state, &headers, &body, sim_time);
    }
    if method == Method::DELETE {
        if let Some(plate) = deleted_plate(path) {
            return depart(&state, plate, sim_time);
        }
    }
    if method == Method::GET && path.ends_with("/spots") {
        return spots(&state, uri.query());
    }
    if method == Method::GET && path.ends_with("/stats") {
        return stats(&state);
    }

    error(StatusCode::NOT_FOUND, "not_found")
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
