use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    status: Value,
    message: String,
    data: Option<T>,
}

#[derive(Debug, Deserialize)]
struct Source {
    entrance: String,
}

#[derive(Debug, Deserialize)]
pub struct Room {
    pub id: String,
    pub name: String,
    pub exit: bool,
    #[serde(default)]
    pub exits: Vec<Passage>,
    #[serde(default)]
    pub items: Vec<Item>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Passage {
    pub door: String,
    pub to: Option<String>,
    pub cost: u32,
    pub one_way: bool,
    pub lock: Option<String>,
    pub locked: bool,
}

#[derive(Debug, Deserialize)]
pub struct Item {
    pub id: String,
    #[serde(rename = "type")]
    pub item_type: String,
    pub name: Option<String>,
    pub color: Option<String>,
    pub text: Option<String>,
    pub picked_up: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct Solution {
    pub path: Vec<String>,
}

async fn get_data<T: for<'de> Deserialize<'de>>(client: &Client, endpoint: &str) -> Result<T> {
    let response = client
        .get(crate::api_url(&format!("labyrinth{endpoint}"))?)
        .send()
        .await
        .with_context(|| format!("GET {endpoint} request failed"))?;

    let http_status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("Could not read GET {endpoint} response"))?;

    if !http_status.is_success() {
        bail!("GET {endpoint} returned {http_status}: {body}");
    }

    let api_response: ApiResponse<T> = serde_json::from_str(&body)
        .with_context(|| format!("GET {endpoint} returned invalid JSON: {body}"))?;

    api_response.data.with_context(|| {
        format!(
            "GET {endpoint} returned no data (status: {}, message: {})",
            api_response.status, api_response.message
        )
    })
}

async fn send_action(request: reqwest::RequestBuilder, description: &str) -> Result<()> {
    let response = request
        .send()
        .await
        .with_context(|| format!("{description} request failed"))?;
    let http_status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("Could not read {description} response"))?;

    if !http_status.is_success() {
        bail!("{description} returned {http_status}: {body}");
    }

    let api_response: ApiResponse<Value> = serde_json::from_str(&body)
        .with_context(|| format!("{description} returned invalid JSON: {body}"))?;
    if api_response.data.is_none() {
        bail!(
            "{description} returned no data (status: {}, message: {})",
            api_response.status,
            api_response.message
        );
    }
    Ok(())
}

pub async fn solve(client: &Client) -> Result<()> {
    let source: Source = get_data(client, "/source").await?;
    let rooms = explore(client, &source.entrance).await?;
    let path = shortest_path(&rooms, &source.entrance)?;
    let solution = Solution { path };
    submit_solution(client, &solution).await
}

pub async fn zero_solve(run_key: &str) -> Result<()> {
    let keyed_client = super::client_with_run_key(run_key)?;
    solve(&keyed_client).await
}

async fn explore(client: &Client, entrance: &str) -> Result<HashMap<String, Room>> {
    let mut rooms = HashMap::new();
    let mut pending = VecDeque::from([entrance.to_owned()]);
    let mut keys = HashSet::new();

    loop {
        while let Some(room_id) = pending.pop_front() {
            if rooms.contains_key(&room_id) {
                continue;
            }

            let room: Room = get_data(client, &format!("/room/{room_id}")).await?;
            for item in &room.items {
                if item.item_type != "key" {
                    continue;
                }

                let color = item
                    .color
                    .as_deref()
                    .with_context(|| format!("Key {} in room {room_id} has no color", item.id))?;
                keys.insert(color.to_owned());

                if item.picked_up != Some(true) {
                    send_action(
                        client
                            .post(crate::api_url(&format!("labyrinth/room/{room_id}/pickup"))?)
                            .json(&json!({ "item": item.id })),
                        &format!("POST /room/{room_id}/pickup"),
                    )
                    .await?;
                }
            }

            for passage in &room.exits {
                if let Some(destination) = &passage.to {
                    pending.push_back(destination.clone());
                }
            }
            rooms.insert(room_id, room);
        }

        let locked_door = rooms.iter().find_map(|(room_id, room)| {
            room.exits.iter().find_map(|passage| {
                (passage.locked
                    && passage
                        .lock
                        .as_ref()
                        .is_some_and(|color| keys.contains(color)))
                .then(|| {
                    (
                        room_id.clone(),
                        passage.door.clone(),
                        passage.lock.clone().expect("lock checked above"),
                    )
                })
            })
        });

        let Some((room_id, door_id, color)) = locked_door else {
            break;
        };

        send_action(
            client
                .patch(crate::api_url(&format!("labyrinth/door/{door_id}"))?)
                .json(&json!({ "key": color })),
            &format!("PATCH /door/{door_id}"),
        )
        .await?;

        let room: Room = get_data(client, &format!("/room/{room_id}")).await?;
        let opened_door = room.exits.iter().find(|passage| passage.door == door_id);
        let opened_door = opened_door
            .with_context(|| format!("Room {room_id} no longer lists opened door {door_id}"))?;
        if opened_door.locked || opened_door.to.is_none() {
            bail!("Door {door_id} did not become traversable after unlocking");
        }
        pending.push_back(opened_door.to.clone().expect("checked above"));
        rooms.insert(room_id, room);
    }

    Ok(rooms)
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SearchState {
    room: String,
    keys: Vec<u64>,
}

fn shortest_path(rooms: &HashMap<String, Room>, entrance: &str) -> Result<Vec<String>> {
    let mut key_indexes = HashMap::new();
    for room in rooms.values() {
        for item in &room.items {
            if item.item_type == "key" {
                let color = item
                    .color
                    .as_deref()
                    .with_context(|| format!("Key {} in room {} has no color", item.id, room.id))?;
                let next_index = key_indexes.len();
                key_indexes.entry(color.to_owned()).or_insert(next_index);
            }
        }
    }

    let mut room_keys: HashMap<&str, Vec<usize>> = HashMap::new();
    for room in rooms.values() {
        let indexes = room
            .items
            .iter()
            .filter(|item| item.item_type == "key")
            .filter_map(|item| {
                item.color
                    .as_deref()
                    .and_then(|color| key_indexes.get(color))
            })
            .copied()
            .collect();
        room_keys.insert(&room.id, indexes);
    }

    if !rooms.contains_key(entrance) {
        bail!("Entrance room {entrance} was not explored");
    }
    let initial_state = SearchState {
        room: entrance.to_owned(),
        keys: keys_for_room(&[], room_keys.get(entrance).map(Vec::as_slice)),
    };

    let mut best_cost = HashMap::from([(initial_state.clone(), 0_u64)]);
    let mut previous: HashMap<SearchState, SearchState> = HashMap::new();
    let mut states = vec![initial_state.clone()];
    let mut queue = BinaryHeap::from([Reverse((0_u64, 0_usize))]);
    let mut destination = None;

    while let Some(Reverse((cost, state_index))) = queue.pop() {
        let state = states[state_index].clone();
        if best_cost.get(&state) != Some(&cost) {
            continue;
        }

        let room = rooms
            .get(&state.room)
            .with_context(|| format!("Room {} is missing from the explored map", state.room))?;
        if room.exit {
            destination = Some(state);
            break;
        }

        for passage in &room.exits {
            let Some(next_room_id) = passage.to.as_deref() else {
                continue;
            };
            if let Some(lock) = &passage.lock {
                let Some(&key_index) = key_indexes.get(lock) else {
                    continue;
                };
                if !has_key(&state.keys, key_index) {
                    continue;
                }
            }

            if !rooms.contains_key(next_room_id) {
                bail!(
                    "Passage {} leads to unexplored room {next_room_id}",
                    passage.door
                );
            }
            let next_state = SearchState {
                room: next_room_id.to_owned(),
                keys: keys_for_room(&state.keys, room_keys.get(next_room_id).map(Vec::as_slice)),
            };
            let next_cost = cost
                .checked_add(u64::from(passage.cost))
                .context("Route cost overflowed")?;
            if best_cost
                .get(&next_state)
                .is_some_and(|known_cost| *known_cost <= next_cost)
            {
                continue;
            }

            best_cost.insert(next_state.clone(), next_cost);
            previous.insert(next_state.clone(), state.clone());
            states.push(next_state);
            queue.push(Reverse((next_cost, states.len() - 1)));
        }
    }

    let mut state = destination.context("No route from the entrance to an exit was found")?;
    let mut path = vec![state.room.clone()];
    while let Some(predecessor) = previous.remove(&state) {
        state = predecessor;
        path.push(state.room.clone());
    }
    path.reverse();
    Ok(path)
}

fn keys_for_room(existing: &[u64], room_keys: Option<&[usize]>) -> Vec<u64> {
    let mut keys = existing.to_vec();
    if let Some(room_keys) = room_keys {
        for &key_index in room_keys {
            let word = key_index / u64::BITS as usize;
            if keys.len() <= word {
                keys.resize(word + 1, 0);
            }
            keys[word] |= 1_u64 << (key_index % u64::BITS as usize);
        }
    }
    keys
}

fn has_key(keys: &[u64], key_index: usize) -> bool {
    let word = key_index / u64::BITS as usize;
    keys.get(word)
        .is_some_and(|bits| bits & (1_u64 << (key_index % u64::BITS as usize)) != 0)
}

async fn submit_solution(client: &Client, solution: &Solution) -> Result<()> {
    let response = client
        .post(crate::api_url("labyrinth/solution")?)
        .json(solution)
        .send()
        .await
        .context("POST /solution request failed")?;
    let http_status = response.status();
    let body = response
        .text()
        .await
        .context("Could not read POST /solution response")?;

    if !http_status.is_success() {
        bail!("POST /solution returned {http_status}: {body}");
    }

    let api_response: ApiResponse<Value> =
        serde_json::from_str(&body).context("POST /solution returned invalid JSON")?;
    if api_response.data.is_none() {
        bail!(
            "POST /solution returned no data (status: {}, message: {})",
            api_response.status,
            api_response.message
        );
    }
    println!("{body}");
    Ok(())
}
