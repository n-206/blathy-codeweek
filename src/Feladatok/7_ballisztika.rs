use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

const SCAN_STEP: f64 = 0.001;

type Vec3 = [f64; 3];

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    status: Value,
    message: String,
    data: Option<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Source {
    scene: Scene,
    shots: Vec<Shot>,
    aim: AimTask,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Scene {
    gravity: Vec3,
    bounds: Bounds,
    max_time: f64,
    platforms: Vec<Platform>,
    obstacles: Vec<Obstacle>,
}

#[derive(Debug, Deserialize)]
struct Bounds {
    min: Vec3,
    max: Vec3,
}

#[derive(Debug, Deserialize)]
struct Platform {
    center: Vec3,
    rotation: Rotation,
    targets: Vec<LocalTarget>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Rotation {
    axis: Vec3,
    angle_deg: f64,
}

#[derive(Debug, Deserialize)]
struct LocalTarget {
    id: String,
    local: Vec3,
    radius: f64,
}

#[derive(Debug, Clone)]
struct Target {
    id: String,
    center: Vec3,
    radius: f64,
}

#[derive(Debug, Deserialize)]
struct Obstacle {
    min: Vec3,
    max: Vec3,
}

#[derive(Debug, Deserialize)]
struct Shot {
    id: String,
    origin: Vec3,
    direction: Vec3,
    speed: f64,
}

#[derive(Debug, Deserialize)]
struct AimTask {
    target: String,
    origin: Vec3,
    speed: f64,
}

#[derive(Debug, Serialize)]
struct Solution {
    hits: BTreeMap<String, String>,
    aim: AimSolution,
}

#[derive(Debug, Serialize)]
struct AimSolution {
    direction: Vec3,
}

pub async fn solve(client: &reqwest::Client) -> Result<()> {
    let source: Source = get_data(client, "/source").await?;
    let targets = world_targets(&source.scene)?;

    let mut hits = BTreeMap::new();
    for shot in &source.shots {
        let hit = trace_shot(
            &source.scene,
            &targets,
            shot.origin,
            scale(normalize(shot.direction)?, shot.speed),
        );
        hits.insert(shot.id.clone(), hit);
    }

    let aim = find_aim(&source.scene, &targets, &source.aim)?;
    let solution = Solution {
        hits,
        aim: AimSolution { direction: aim },
    };
    let result = send_solution(client, &solution).await?;
    println!("{result}");
    Ok(())
}

fn world_targets(scene: &Scene) -> Result<Vec<Target>> {
    let mut targets = Vec::new();
    for platform in &scene.platforms {
        let axis =
            normalize(platform.rotation.axis).context("Platform rotation axis must not be zero")?;
        let angle = platform.rotation.angle_deg.to_radians();
        for target in &platform.targets {
            targets.push(Target {
                id: target.id.clone(),
                center: add(
                    platform.center,
                    rotate(target.local, axis, angle.cos(), angle.sin()),
                ),
                radius: target.radius,
            });
        }
    }
    Ok(targets)
}

fn rotate(vector: Vec3, axis: Vec3, cos_angle: f64, sin_angle: f64) -> Vec3 {
    add(
        add(
            scale(vector, cos_angle),
            scale(cross(axis, vector), sin_angle),
        ),
        scale(axis, dot(axis, vector) * (1.0 - cos_angle)),
    )
}

fn trace_shot(scene: &Scene, targets: &[Target], origin: Vec3, velocity: Vec3) -> String {
    let miss_time = first_exit_time(scene, origin, velocity);
    let mut earliest = (miss_time, "miss".to_owned());

    for obstacle in &scene.obstacles {
        if let Some(time) = first_entry_time(miss_time, |t| {
            let point = position(origin, velocity, scene.gravity, t);
            in_box(point, obstacle.min, obstacle.max)
        }) && time < earliest.0
        {
            earliest = (time, "obstacle".to_owned());
        }
    }

    for target in targets {
        if let Some(time) = first_entry_time(miss_time, |t| {
            let delta = sub(position(origin, velocity, scene.gravity, t), target.center);
            dot(delta, delta) <= target.radius * target.radius
        }) && time < earliest.0
        {
            earliest = (time, target.id.clone());
        }
    }

    earliest.1
}

fn first_exit_time(scene: &Scene, origin: Vec3, velocity: Vec3) -> f64 {
    first_entry_time(scene.max_time, |t| {
        let point = position(origin, velocity, scene.gravity, t);
        point[1] < -1e-9
            || (0..3).any(|axis| {
                point[axis] < scene.bounds.min[axis] - 1e-9
                    || point[axis] > scene.bounds.max[axis] + 1e-9
            })
    })
    .unwrap_or(scene.max_time)
}

fn find_aim(scene: &Scene, targets: &[Target], task: &AimTask) -> Result<Vec3> {
    let target = targets
        .iter()
        .find(|target| target.id == task.target)
        .with_context(|| format!("Aim target {} does not exist", task.target))?;

    let mut widest_trajectory: Option<(f64, Vec3)> = None;
    let mut time = SCAN_STEP;
    while time <= scene.max_time {
        let corrected_target = sub(
            sub(target.center, task.origin),
            scale(scene.gravity, 0.5 * time * time),
        );
        let distance = length(corrected_target);
        let radial_error = distance - task.speed * time;
        if distance > 1e-12 && radial_error.abs() <= target.radius {
            let direction = scale(corrected_target, 1.0 / distance);
            let travel = task.speed * time;
            let cosine = ((distance * distance + travel * travel - target.radius * target.radius)
                / (2.0 * distance * travel))
                .clamp(-1.0, 1.0);
            let half_angle = cosine.acos();
            if widest_trajectory
                .as_ref()
                .is_none_or(|(best_angle, _)| half_angle > *best_angle)
            {
                widest_trajectory = Some((half_angle, direction));
            }
            if trace_shot(scene, targets, task.origin, scale(direction, task.speed)) == task.target
            {
                return Ok(direction);
            }
        }
        time += SCAN_STEP;
    }

    if let Some((half_angle, center_direction)) = widest_trajectory {
        let reference = if center_direction[0].abs() < 0.8 {
            [1.0, 0.0, 0.0]
        } else {
            [0.0, 1.0, 0.0]
        };
        let tangent = normalize(cross(center_direction, reference))?;
        let bitangent = cross(center_direction, tangent);
        for ring in 1..=8 {
            let angle = half_angle * ring as f64 / 9.0;
            for segment in 0..96 {
                let azimuth = std::f64::consts::TAU * segment as f64 / 96.0;
                let direction = add(
                    scale(center_direction, angle.cos()),
                    scale(
                        add(
                            scale(tangent, azimuth.cos()),
                            scale(bitangent, azimuth.sin()),
                        ),
                        angle.sin(),
                    ),
                );
                if trace_shot(scene, targets, task.origin, scale(direction, task.speed))
                    == task.target
                {
                    return Ok(direction);
                }
            }
        }
    }

    bail!(
        "Could not find an unobstructed trajectory that hits target {} first",
        task.target
    )
}

fn first_entry_time<F>(max_time: f64, mut inside: F) -> Option<f64>
where
    F: FnMut(f64) -> bool,
{
    if max_time < 0.0 {
        return None;
    }
    if inside(0.0) {
        return Some(0.0);
    }

    let steps = (max_time / SCAN_STEP).ceil().max(1.0) as usize;
    let step = max_time / steps as f64;
    let mut previous = 0.0;
    for index in 1..=steps {
        let current = if index == steps {
            max_time
        } else {
            index as f64 * step
        };
        if inside(current) {
            let (mut low, mut high) = (previous, current);
            for _ in 0..40 {
                let middle = (low + high) * 0.5;
                if inside(middle) {
                    high = middle;
                } else {
                    low = middle;
                }
            }
            return Some(high);
        }
        previous = current;
    }
    None
}

fn position(origin: Vec3, velocity: Vec3, gravity: Vec3, time: f64) -> Vec3 {
    add(
        add(origin, scale(velocity, time)),
        scale(gravity, 0.5 * time * time),
    )
}

fn in_box(point: Vec3, min: Vec3, max: Vec3) -> bool {
    (0..3).all(|axis| point[axis] >= min[axis] && point[axis] <= max[axis])
}

fn normalize(vector: Vec3) -> Result<Vec3> {
    let magnitude = length(vector);
    if magnitude <= 1e-12 {
        bail!("Cannot normalize a zero-length vector");
    }
    Ok(scale(vector, 1.0 / magnitude))
}

fn length(vector: Vec3) -> f64 {
    dot(vector, vector).sqrt()
}

fn dot(left: Vec3, right: Vec3) -> f64 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

fn cross(left: Vec3, right: Vec3) -> Vec3 {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}

fn add(left: Vec3, right: Vec3) -> Vec3 {
    [left[0] + right[0], left[1] + right[1], left[2] + right[2]]
}

fn sub(left: Vec3, right: Vec3) -> Vec3 {
    [left[0] - right[0], left[1] - right[1], left[2] - right[2]]
}

fn scale(vector: Vec3, scalar: f64) -> Vec3 {
    [vector[0] * scalar, vector[1] * scalar, vector[2] * scalar]
}

async fn get_data<T: for<'de> Deserialize<'de>>(
    client: &reqwest::Client,
    endpoint: &str,
) -> Result<T> {
    let response = client
        .get(crate::api_url(&format!("ballistics{endpoint}"))?)
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

async fn send_solution(client: &reqwest::Client, solution: &Solution) -> Result<Value> {
    let response = client
        .post(crate::api_url("ballistics/solution")?)
        .json(solution)
        .send()
        .await
        .context("Could not send the ballistics solution")?;

    let http_status = response.status();
    let body = response
        .text()
        .await
        .context("Could not read the ballistics solution response")?;

    if !http_status.is_success() {
        bail!("Ballistics solution returned {http_status}: {body}");
    }

    serde_json::from_str(&body).context("Ballistics solution returned invalid JSON")
}
