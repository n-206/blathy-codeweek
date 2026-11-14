use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Debug, serde::Deserialize)]
struct ApiResponse<T> {
    status: Value,
    message: String,
    data: Option<T>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GameState {
    uuid: String,
    scenario: String,
    status: String,
    tick: u32,
    building: Building,
    elevators: Vec<Elevator>,
    hall_calls: Vec<HallCall>,
    #[serde(default)]
    stats: Value,
    #[serde(default)]
    result: Option<GameResult>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Building {
    top_floor: u32,
    ticks: u32,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GameResult {
    score: f64,
    avg_wait: f64,
    avg_travel: f64,
    relative_score: f64,
}

#[derive(Debug, serde::Deserialize)]
struct Elevator {
    id: usize,
    floor: u32,
    doors: String,
    status: String,
    load: usize,
    capacity: usize,
    destinations: Vec<u32>,
}

#[derive(Debug, serde::Deserialize)]
struct HallCall {
    floor: u32,
    direction: String,
    since: u32,
}

#[derive(Debug, Serialize)]
struct TickRequest {
    tick: u32,
    commands: Vec<Command>,
}

#[derive(Debug, Serialize)]
struct Command {
    elevator: usize,
    action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    direction: Option<&'static str>,
}

pub async fn solve(client: &Client) -> Result<()> {
    play_scenario(client, "morning-rush", "practice").await
}

async fn play_scenario(client: &Client, scenario: &str, mode: &str) -> Result<()> {
    if ![
        "morning-rush",
        "lunch-break",
        "parents-evening",
        "exam-day",
        "breakdown",
    ]
    .contains(&scenario)
    {
        bail!("Unknown lift-controller scenario: {scenario}");
    }
    if !["practice", "ranked", "final"].contains(&mode) {
        bail!("Unknown lift-controller mode: {mode}");
    }

    let game = post_data::<GameState, _>(
        client,
        &crate::api_url("lift-controller/games")?,
        &serde_json::json!({
            "mode": mode,
            "scenario": scenario
        }),
    )
    .await
    .with_context(|| format!("Could not start {mode} lift-controller game for {scenario}"))?;

    play_game(client, game).await
}

async fn play_game(client: &Client, mut game: GameState) -> Result<()> {
    let mut last_directions = vec![None; game.elevators.len()];

    while game.status == "running" && game.tick < game.building.ticks {
        let commands = choose_commands(&game, &mut last_directions);
        game = post_data(
            client,
            &crate::api_url(&format!("lift-controller/games/{}/tick", game.uuid))?,
            &TickRequest {
                tick: game.tick,
                commands,
            },
        )
        .await
        .with_context(|| format!("Could not submit tick for game {}", game.uuid))?;
    }

    if let Some(result) = game.result {
        println!(
            "Lift game {} ({}) finished at tick {}: score={:.2}, relative={:.4}, wait={:.2}, travel={:.2}, stats={}",
            game.uuid,
            game.scenario,
            game.tick,
            result.score,
            result.relative_score,
            result.avg_wait,
            result.avg_travel,
            game.stats
        );
    } else {
        println!(
            "Lift game {} ({}) ended with status={} at tick {} and no result: stats={}",
            game.uuid, game.scenario, game.status, game.tick, game.stats
        );
    }
    Ok(())
}

fn choose_commands(game: &GameState, last_directions: &mut [Option<i8>]) -> Vec<Command> {
    let assignments = assign_calls(game, last_directions);
    let mut commands = Vec::new();

    for elevator in &game.elevators {
        if elevator.status != "ok" {
            continue;
        }

        let Some(direction_memory) = last_directions.get_mut(elevator.id) else {
            continue;
        };
        let call_here = best_call_at(&game.hall_calls, elevator.floor);

        let morning_rush_call = game.scenario == "morning-rush"
            && elevator.load < elevator.capacity
            && elevator.floor == 0
            && game
                .hall_calls
                .iter()
                .any(|call| call.floor == 0 && call.direction == "up");

        let command = if morning_rush_call {
            *direction_memory = Some(1);
            Command {
                elevator: elevator.id,
                action: "open",
                direction: Some("up"),
            }
        } else if elevator.doors == "open" {
            let route_direction = if elevator.load > 0 {
                direction_to_destination(elevator, *direction_memory)
            } else {
                None
            };
            let boarding_call = call_here.filter(|call| {
                elevator.load < elevator.capacity
                    && route_direction
                        .is_none_or(|direction| direction == direction_value(&call.direction))
            });

            if let Some(call) = boarding_call {
                *direction_memory = Some(direction_value(&call.direction));
                Command {
                    elevator: elevator.id,
                    action: "open",
                    direction: Some(call_direction(&call.direction)),
                }
            } else {
                Command {
                    elevator: elevator.id,
                    action: "idle",
                    direction: None,
                }
            }
        } else if elevator.destinations.contains(&elevator.floor) {
            let direction = direction_to_destination(elevator, *direction_memory);
            direction.map_or(
                Command {
                    elevator: elevator.id,
                    action: "open",
                    direction: None,
                },
                |direction| {
                    *direction_memory = Some(direction);
                    Command {
                        elevator: elevator.id,
                        action: "open",
                        direction: Some(direction_name(direction)),
                    }
                },
            )
        } else if elevator.load > 0 {
            if let Some(direction) = direction_to_destination(elevator, *direction_memory) {
                if elevator.load < elevator.capacity
                    && call_here.is_some_and(|call| direction_value(&call.direction) == direction)
                {
                    Command {
                        elevator: elevator.id,
                        action: "open",
                        direction: Some(direction_name(direction)),
                    }
                } else {
                    *direction_memory = Some(direction);
                    Command {
                        elevator: elevator.id,
                        action: direction_name(direction),
                        direction: None,
                    }
                }
            } else {
                Command {
                    elevator: elevator.id,
                    action: "idle",
                    direction: None,
                }
            }
        } else {
            let target_floor = assignments
                .get(&elevator.id)
                .map(|&call_index| game.hall_calls[call_index].floor)
                .unwrap_or_else(|| standby_floor(game, elevator.id));
            if let Some(call) = call_here {
                let direction = direction_value(&call.direction);
                *direction_memory = Some(direction);
                Command {
                    elevator: elevator.id,
                    action: "open",
                    direction: Some(call_direction(&call.direction)),
                }
            } else {
                let direction = match target_floor.cmp(&elevator.floor) {
                    std::cmp::Ordering::Greater => Some(1),
                    std::cmp::Ordering::Less => Some(-1),
                    std::cmp::Ordering::Equal => None,
                };
                if let Some(direction) = direction {
                    *direction_memory = Some(direction);
                    Command {
                        elevator: elevator.id,
                        action: direction_name(direction),
                        direction: None,
                    }
                } else {
                    Command {
                        elevator: elevator.id,
                        action: "idle",
                        direction: None,
                    }
                }
            }
        };
        commands.push(command);
    }

    commands
}

fn standby_floor(game: &GameState, elevator_id: usize) -> u32 {
    if game.scenario == "morning-rush" || game.elevators.len() <= 1 {
        return 0;
    }

    game.building.top_floor * elevator_id as u32 / (game.elevators.len() - 1) as u32
}

fn assign_calls(game: &GameState, last_directions: &[Option<i8>]) -> HashMap<usize, usize> {
    let mut pairs = Vec::new();
    for elevator in &game.elevators {
        if elevator.status != "ok" || elevator.doors == "open" || elevator.load > 0 {
            continue;
        }
        for (call_index, call) in game.hall_calls.iter().enumerate() {
            let distance = elevator.floor.abs_diff(call.floor) as i64;
            let age = game.tick.saturating_sub(call.since).min(20) as i64;
            let direction = direction_value(&call.direction);
            let reversal_penalty = last_directions
                .get(elevator.id)
                .copied()
                .flatten()
                .filter(|&last| last != direction)
                .map_or(0, |_| 15);
            let cost = distance * 100 - age * 4 + reversal_penalty;
            pairs.push((cost, elevator.id, call_index));
        }
    }
    pairs.sort_unstable();

    let mut assigned_elevators = HashSet::new();
    let mut assigned_calls = HashSet::new();
    let mut assignments = HashMap::new();
    for (_, elevator, call) in pairs {
        if !assigned_elevators.contains(&elevator) && !assigned_calls.contains(&call) {
            assigned_elevators.insert(elevator);
            assigned_calls.insert(call);
            assignments.insert(elevator, call);
        }
    }
    assignments
}

fn best_call_at(calls: &[HallCall], floor: u32) -> Option<&HallCall> {
    calls
        .iter()
        .filter(|call| call.floor == floor)
        .max_by_key(|call| call.since)
}

fn direction_to_destination(elevator: &Elevator, last_direction: Option<i8>) -> Option<i8> {
    let has_above = elevator
        .destinations
        .iter()
        .any(|&destination| destination > elevator.floor);
    let has_below = elevator
        .destinations
        .iter()
        .any(|&destination| destination < elevator.floor);

    match (has_above, has_below, last_direction) {
        (false, false, _) => None,
        (true, false, _) => Some(1),
        (false, true, _) => Some(-1),
        (true, true, Some(direction @ (1 | -1))) => Some(direction),
        (true, true, _) => {
            let nearest = elevator
                .destinations
                .iter()
                .filter(|&&destination| destination != elevator.floor)
                .min_by_key(|&&destination| destination.abs_diff(elevator.floor))?;
            Some(if *nearest > elevator.floor { 1 } else { -1 })
        }
    }
}

fn direction_value(direction: &str) -> i8 {
    if direction == "down" { -1 } else { 1 }
}

fn direction_name(direction: i8) -> &'static str {
    if direction < 0 { "down" } else { "up" }
}

fn call_direction(direction: &str) -> &'static str {
    if direction == "down" { "down" } else { "up" }
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
    let response_body = response
        .text()
        .await
        .with_context(|| format!("Could not read POST {endpoint} response"))?;
    if !http_status.is_success() {
        bail!("POST {endpoint} returned {http_status}: {response_body}");
    }

    let api_response: ApiResponse<T> = serde_json::from_str(&response_body)
        .with_context(|| format!("POST {endpoint} returned invalid JSON"))?;
    api_response.data.with_context(|| {
        format!(
            "POST {endpoint} returned no data (status: {}, message: {})",
            api_response.status, api_response.message
        )
    })
}
