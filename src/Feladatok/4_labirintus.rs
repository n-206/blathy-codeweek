use anyhow::{Context, Result, bail};
use futures::{StreamExt, TryStreamExt, stream};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::time::Duration;

const REQUEST_CONCURRENCY: usize = 8;
const ROOM_API_REQUEST_INTERVAL: Duration = Duration::from_millis(60);

struct RequestRateLimiter {
    next_request: tokio::sync::Mutex<tokio::time::Instant>,
}

impl RequestRateLimiter {
    fn new() -> Self {
        Self {
            next_request: tokio::sync::Mutex::new(tokio::time::Instant::now()),
        }
    }

    async fn wait(&self) {
        let mut next_request = self.next_request.lock().await;
        let now = tokio::time::Instant::now();
        if *next_request > now {
            tokio::time::sleep_until(*next_request).await;
        }
        *next_request = tokio::time::Instant::now() + ROOM_API_REQUEST_INTERVAL;
    }
}

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

async fn get_data<T: for<'de> Deserialize<'de>>(
    client: &Client,
    endpoint: &str,
    rate_limiter: Option<&RequestRateLimiter>,
) -> Result<T> {
    if let Some(rate_limiter) = rate_limiter {
        rate_limiter.wait().await;
    }
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

async fn send_action(
    request: reqwest::RequestBuilder,
    description: &str,
    rate_limiter: &RequestRateLimiter,
) -> Result<()> {
    rate_limiter.wait().await;
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
    let source: Source = get_data(client, "/source", None).await?;
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
    let rate_limiter = RequestRateLimiter::new();
    let mut rooms = HashMap::new();
    let mut pending = VecDeque::from([entrance.to_owned()]);
    let mut discovered = HashSet::from([entrance.to_owned()]);
    let mut keys = HashSet::new();

    loop {
        while !pending.is_empty() {
            let batch: Vec<_> = (0..REQUEST_CONCURRENCY)
                .filter_map(|_| pending.pop_front())
                .collect();
            let limiter = &rate_limiter;
            let discovered_rooms = stream::iter(batch.into_iter().map(|room_id| async move {
                let room: Room =
                    get_data(client, &format!("/room/{room_id}"), Some(limiter)).await?;
                for item in &room.items {
                    if item.item_type == "key" {
                        item.color.as_deref().with_context(|| {
                            format!("Key {} in room {room_id} has no color", item.id)
                        })?;
                    }
                }
                for item in &room.items {
                    if item.item_type == "key" && item.picked_up != Some(true) {
                        send_action(
                            client
                                .post(crate::api_url(&format!("labyrinth/room/{room_id}/pickup"))?)
                                .json(&json!({ "item": item.id })),
                            &format!("POST /room/{room_id}/pickup"),
                            limiter,
                        )
                        .await?;
                    }
                }
                Ok::<_, anyhow::Error>((room_id, room))
            }))
            .buffer_unordered(REQUEST_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await?;

            for (room_id, room) in discovered_rooms {
                for item in &room.items {
                    if item.item_type == "key" {
                        let color = item.color.as_deref().with_context(|| {
                            format!("Key {} in room {room_id} has no color", item.id)
                        })?;
                        keys.insert(color.to_owned());
                    }
                }
                for passage in &room.exits {
                    if let Some(destination) = &passage.to
                        && discovered.insert(destination.clone())
                    {
                        pending.push_back(destination.clone());
                    }
                }
                rooms.insert(room_id, room);
            }
        }

        let locked_doors: Vec<_> = rooms
            .iter()
            .flat_map(|(room_id, room)| {
                room.exits
                    .iter()
                    .filter(|passage| {
                        passage.locked
                            && passage
                                .lock
                                .as_ref()
                                .is_some_and(|color| keys.contains(color))
                    })
                    .filter_map(|passage| {
                        passage
                            .lock
                            .as_ref()
                            .map(|color| (room_id.clone(), passage.door.clone(), color.clone()))
                    })
            })
            .collect();

        if locked_doors.is_empty() {
            break;
        }

        let limiter = &rate_limiter;
        stream::iter(
            locked_doors
                .iter()
                .cloned()
                .map(|(room_id, door_id, color)| async move {
                    send_action(
                        client
                            .patch(crate::api_url(&format!("labyrinth/door/{door_id}"))?)
                            .json(&json!({ "key": color })),
                        &format!("PATCH /door/{door_id}"),
                        limiter,
                    )
                    .await
                    .with_context(|| format!("Could not unlock door {door_id} from room {room_id}"))
                }),
        )
        .buffer_unordered(REQUEST_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;

        let rooms_to_refresh: HashSet<_> = locked_doors
            .iter()
            .map(|(room_id, _, _)| room_id.clone())
            .collect();
        let limiter = &rate_limiter;
        let refreshed_rooms =
            stream::iter(rooms_to_refresh.into_iter().map(|room_id| async move {
                let room: Room =
                    get_data(client, &format!("/room/{room_id}"), Some(limiter)).await?;
                Ok::<_, anyhow::Error>((room_id, room))
            }))
            .buffer_unordered(REQUEST_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await?;
        let refreshed_rooms: HashMap<_, _> = refreshed_rooms.into_iter().collect();

        for (room_id, door_id, _) in &locked_doors {
            let passage = refreshed_rooms
                .get(room_id)
                .and_then(|room| room.exits.iter().find(|passage| passage.door == *door_id))
                .with_context(|| format!("Room {room_id} no longer lists opened door {door_id}"))?;
            if passage.locked || passage.to.is_none() {
                bail!("Door {door_id} did not become traversable after unlocking");
            }
        }

        for (room_id, room) in refreshed_rooms {
            for passage in &room.exits {
                if let Some(destination) = &passage.to
                    && discovered.insert(destination.clone())
                {
                    pending.push_back(destination.clone());
                }
            }
            rooms.insert(room_id, room);
        }
    }

    Ok(rooms)
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SearchState {
    room: usize,
    keys: Vec<u64>,
}

fn shortest_path(rooms: &HashMap<String, Room>, entrance: &str) -> Result<Vec<String>> {
    let room_ids: Vec<&str> = rooms.keys().map(String::as_str).collect();
    let room_indexes: HashMap<&str, usize> = room_ids
        .iter()
        .enumerate()
        .map(|(index, room_id)| (*room_id, index))
        .collect();
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

    let mut room_keys = vec![Vec::new(); room_ids.len()];
    let mut room_exits = vec![false; room_ids.len()];
    let mut passages = vec![Vec::new(); room_ids.len()];
    for (room_index, room_id) in room_ids.iter().enumerate() {
        let room = rooms
            .get(*room_id)
            .with_context(|| format!("Room {room_id} is missing from the explored map"))?;
        room_exits[room_index] = room.exit;
        room_keys[room_index] = room
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
        for passage in &room.exits {
            let Some(destination) = passage.to.as_deref() else {
                continue;
            };
            let Some(next_room_index) = room_indexes.get(destination).copied() else {
                bail!(
                    "Passage {} leads to unexplored room {destination}",
                    passage.door
                );
            };
            let required_key = match passage.lock.as_ref() {
                Some(lock) => {
                    let Some(&key_index) = key_indexes.get(lock) else {
                        continue;
                    };
                    Some(key_index)
                }
                None => None,
            };
            passages[room_index].push((next_room_index, passage.cost, required_key));
        }
    }

    let entrance_index = *room_indexes
        .get(entrance)
        .with_context(|| format!("Entrance room {entrance} was not explored"))?;
    let initial_state = SearchState {
        room: entrance_index,
        keys: keys_for_room(&[], &room_keys[entrance_index]),
    };

    let mut best_cost = HashMap::from([(initial_state.clone(), 0_u64)]);
    let mut states = vec![initial_state];
    let mut previous = vec![None];
    let mut queue = BinaryHeap::from([Reverse((0_u64, 0_usize))]);
    let mut destination = None;

    while let Some(Reverse((cost, state_index))) = queue.pop() {
        if best_cost.get(&states[state_index]) != Some(&cost) {
            continue;
        }

        let room_index = states[state_index].room;
        if room_exits[room_index] {
            destination = Some(state_index);
            break;
        }

        let successors = {
            let state = &states[state_index];
            let mut successors = Vec::new();
            for &(next_room_index, passage_cost, required_key) in &passages[room_index] {
                if required_key.is_some_and(|key_index| !has_key(&state.keys, key_index)) {
                    continue;
                }
                let next_state = SearchState {
                    room: next_room_index,
                    keys: keys_for_room(&state.keys, &room_keys[next_room_index]),
                };
                let next_cost = cost
                    .checked_add(u64::from(passage_cost))
                    .context("Route cost overflowed")?;
                if best_cost
                    .get(&next_state)
                    .is_some_and(|known_cost| *known_cost <= next_cost)
                {
                    continue;
                }
                successors.push((next_cost, next_state));
            }
            successors
        };

        for (next_cost, next_state) in successors {
            best_cost.insert(next_state.clone(), next_cost);
            previous.push(Some(state_index));
            states.push(next_state);
            queue.push(Reverse((next_cost, states.len() - 1)));
        }
    }

    let mut state_index = destination.context("No route from the entrance to an exit was found")?;
    let mut path = Vec::new();
    loop {
        path.push(room_ids[states[state_index].room].to_owned());
        let Some(predecessor) = previous[state_index] else {
            break;
        };
        state_index = predecessor;
    }
    path.reverse();
    Ok(path)
}

fn keys_for_room(existing: &[u64], room_keys: &[usize]) -> Vec<u64> {
    let mut keys = existing.to_vec();
    for &key_index in room_keys {
        let word = key_index / u64::BITS as usize;
        if keys.len() <= word {
            keys.resize(word + 1, 0);
        }
        keys[word] |= 1_u64 << (key_index % u64::BITS as usize);
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
