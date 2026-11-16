use anyhow::{Context, Result, bail};
use futures::future::try_join_all;
use image::{GrayImage, ImageBuffer, Luma, imageops};
use rqrr::PreparedImage;
use serde::Deserialize;
use serde_json::json;
use std::time::{Duration, Instant};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Source {
    grid: usize,
    tile_count: usize,
    tile_size: u32,
    image_size: u32,
    module_size: u32,
    quiet_zone: u32,
    tiles: Vec<String>,
    expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct SourceResponse {
    data: Source,
}

#[derive(Clone)]
struct OrientedTile {
    image: GrayImage,
    edges: [Vec<bool>; 4],
    modules: Vec<bool>,
}

pub async fn solve(client: &reqwest::Client) -> Result<()> {
    let started = Instant::now();
    let source_url = crate::api_url("broken-qr/source")?;
    let response = client
        .get(&source_url)
        .send()
        .await
        .context("GET /broken-qr/source request failed")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("Could not read GET /broken-qr/source response")?;
    if !status.is_success() {
        bail!(
            "GET /broken-qr/source returned {status}: {}",
            &body
        );
    }

    let response: SourceResponse =
        serde_json::from_str(&body).context("GET /broken-qr/source returned invalid JSON")?;
    let source = response.data;
    validate_source(&source)?;
    let deadline = started
        .checked_add(Duration::from_secs(source.expires_in))
        .context("Challenge expiry is too far in the future")?;

    let downloads = source
        .tiles
        .iter()
        .enumerate()
        .map(|(index, url)| async move {
            let response = client
                .get(url)
                .send()
                .await
                .with_context(|| format!("GET tile {index} request failed"))?;
            let status = response.status();
            let bytes = response
                .bytes()
                .await
                .with_context(|| format!("Could not read tile {index}"))?;
            if !status.is_success() {
                bail!(
                    "GET tile {index} returned {status}: {}",
                    String::from_utf8_lossy(&bytes)
                );
            }
            let image = image::load_from_memory(&bytes)
                .with_context(|| format!("Tile {index} was not a supported image"))?
                .to_luma8();
            Ok::<_, anyhow::Error>(image)
        });
    let photos = try_join_all(downloads).await?;

    let mut tiles = Vec::with_capacity(source.tile_count);
    for (index, photo) in photos.iter().enumerate() {
        if Instant::now() >= deadline {
            bail!("The broken-QR challenge expired while downloading its tiles");
        }
        let rectified = rectify_tile(photo, source.tile_size)
            .with_context(|| format!("Could not straighten tile {index}"))?;
        let threshold = otsu_threshold(&rectified);
        for rotation in 0..4 {
            let oriented = match rotation {
                0 => rectified.clone(),
                1 => imageops::rotate90(&rectified),
                2 => imageops::rotate180(&rectified),
                _ => imageops::rotate270(&rectified),
            };
            tiles.push(OrientedTile::new(oriented, threshold, source.module_size));
        }
    }

    let mut board = vec![None; source.tile_count];
    let mut used = vec![false; source.tile_count];
    let mut attempts = 0usize;
    let answer = search_assembly(
        &tiles,
        &mut board,
        &mut used,
        source.grid,
        source.image_size,
        source.module_size,
        source.quiet_zone,
        deadline,
        &mut attempts,
    )?
    .context("Could not assemble and decode the broken QR code")?;
    println!("Decoded message: {answer}");
    let answer_code = extract_answer_code(&answer)?;

    let response = client
        .post(crate::api_url("broken-qr/solution")?)
        .json(&json!({ "answer": answer_code }))
        .send()
        .await
        .context("POST /broken-qr/solution request failed")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("Could not read POST /broken-qr/solution response")?;
    if !status.is_success() {
        bail!("POST /broken-qr/solution returned {status}: {body}");
    }
    println!("{body}");
    Ok(())
}

fn validate_source(source: &Source) -> Result<()> {
    if !(2..=3).contains(&source.grid) {
        bail!("Unsupported broken-QR grid size: {}", source.grid);
    }
    let expected_tiles = source
        .grid
        .checked_mul(source.grid)
        .context("Tile count overflow")?;
    if source.tile_count != expected_tiles || source.tiles.len() != expected_tiles {
        bail!(
            "Broken-QR source has {} tiles for a {}x{} grid",
            source.tiles.len(),
            source.grid,
            source.grid
        );
    }
    if source.module_size == 0 {
        bail!("Broken-QR source contains an invalid module size");
    }
    let total_modules = source.image_size / source.module_size;
    let minimum_size = source
        .quiet_zone
        .checked_mul(2)
        .and_then(|quiet_zone| quiet_zone.checked_add(21));
    if source.tile_size == 0
        || source.tile_size.checked_mul(source.grid as u32) != Some(source.image_size)
        || source.quiet_zone == 0
        || source.image_size % source.module_size != 0
        || (source.grid == 3 && source.tile_size % source.module_size != 0)
        || minimum_size.is_none_or(|minimum_size| minimum_size > total_modules)
    {
        bail!("Broken-QR source contains invalid image or QR dimensions");
    }
    Ok(())
}

impl OrientedTile {
    fn new(image: GrayImage, threshold: u8, module_size: u32) -> Self {
        let width = image.width() as usize;
        let height = image.height() as usize;
        let top = (0..width)
            .map(|x| image.get_pixel(x as u32, 0).0[0] < threshold)
            .collect();
        let right = (0..height)
            .map(|y| image.get_pixel(image.width() - 1, y as u32).0[0] < threshold)
            .collect();
        let bottom = (0..width)
            .map(|x| image.get_pixel(x as u32, image.height() - 1).0[0] < threshold)
            .collect();
        let left = (0..height)
            .map(|y| image.get_pixel(0, y as u32).0[0] < threshold)
            .collect();
        let modules_per_side = image.width() / module_size;
        let mut modules = Vec::with_capacity((modules_per_side * modules_per_side) as usize);
        for module_y in 0..modules_per_side {
            for module_x in 0..modules_per_side {
                let x = module_x * module_size + module_size / 2;
                let y = module_y * module_size + module_size / 2;
                modules.push(image.get_pixel(x, y).0[0] < threshold);
            }
        }
        Self {
            image,
            edges: [top, right, bottom, left],
            modules,
        }
    }
}

fn search_assembly(
    tiles: &[OrientedTile],
    board: &mut [Option<usize>],
    used: &mut [bool],
    grid: usize,
    image_size: u32,
    module_size: u32,
    quiet_zone: u32,
    deadline: Instant,
    attempts: &mut usize,
) -> Result<Option<String>> {
    if Instant::now() >= deadline {
        bail!("The broken-QR challenge expired before the tiles could be assembled");
    }
    if *attempts >= 500_000 {
        bail!("Reached the assembly search limit without decoding a QR code");
    }
    if board.iter().all(Option::is_some) {
        *attempts += 1;
        let image = assemble_image(tiles, board, grid, image_size)?;
        return Ok(decode_qr(image).filter(|message| extract_answer_code(message).is_ok()));
    }

    let position = next_position(board, grid).context("No unfilled tile position found")?;
    let row = position / grid;
    let column = position % grid;
    let above = (row > 0).then(|| board[position - grid]).flatten();
    let left = (column > 0).then(|| board[position - 1]).flatten();
    let mut candidates = Vec::new();

    for (tile_index, is_used) in used.iter().enumerate() {
        if *is_used {
            continue;
        }
        for rotation in 0..4 {
            let oriented_index = tile_index * 4 + rotation;
            let tile = &tiles[oriented_index];
            let mut score = 0.0;
            if grid == 2 {
                if let Some(above_index) = above {
                    let edge_score = edge_mismatch(&tiles[above_index].edges[2], &tile.edges[0]);
                    if edge_score > 0.34 {
                        continue;
                    }
                    score += edge_score;
                }
                if let Some(left_index) = left {
                    let edge_score = edge_mismatch(&tiles[left_index].edges[1], &tile.edges[3]);
                    if edge_score > 0.34 {
                        continue;
                    }
                    score += edge_score;
                }
            } else if let Some(function_score) =
                function_pattern_mismatch(tile, position, grid, module_size, quiet_zone)
            {
                if function_score > 0.18 {
                    continue;
                }
                score += function_score;
            }
            candidates.push((score, tile_index, oriented_index));
        }
    }
    candidates.sort_by(|a, b| a.0.total_cmp(&b.0));

    for (_, tile_index, oriented_index) in candidates {
        if Instant::now() >= deadline {
            break;
        }
        used[tile_index] = true;
        board[position] = Some(oriented_index);
        match search_assembly(
            tiles,
            board,
            used,
            grid,
            image_size,
            module_size,
            quiet_zone,
            deadline,
            attempts,
        ) {
            Ok(Some(answer)) => return Ok(Some(answer)),
            Ok(None) => {}
            Err(error) => {
                board[position] = None;
                used[tile_index] = false;
                return Err(error);
            }
        }
        board[position] = None;
        used[tile_index] = false;
    }

    Ok(None)
}

fn edge_mismatch(left: &[bool], right: &[bool]) -> f64 {
    if left.len() != right.len() || left.is_empty() {
        return 1.0;
    }
    left.iter().zip(right).filter(|(a, b)| a != b).count() as f64 / left.len() as f64
}

fn next_position(board: &[Option<usize>], grid: usize) -> Option<usize> {
    if grid == 3 {
        [0, 2, 6, 8, 1, 3, 4, 5, 7]
            .into_iter()
            .find(|position| board[*position].is_none())
    } else {
        board.iter().position(Option::is_none)
    }
}

fn function_pattern_mismatch(
    tile: &OrientedTile,
    position: usize,
    grid: usize,
    module_size: u32,
    quiet_zone: u32,
) -> Option<f64> {
    let tile_modules = tile.image.width() / module_size;
    if tile_modules == 0 || tile.modules.len() != (tile_modules * tile_modules) as usize {
        return None;
    }
    let total_modules = tile_modules * grid as u32;
    let symbol_size = total_modules.checked_sub(quiet_zone * 2)?;
    let tile_column = position as u32 % grid as u32;
    let tile_row = position as u32 / grid as u32;
    let mut known = 0usize;
    let mut mismatches = 0usize;

    for local_y in 0..tile_modules {
        for local_x in 0..tile_modules {
            let global_x = tile_column * tile_modules + local_x;
            let global_y = tile_row * tile_modules + local_y;
            let expected = if global_x < quiet_zone
                || global_y < quiet_zone
                || global_x >= total_modules - quiet_zone
                || global_y >= total_modules - quiet_zone
            {
                Some(false)
            } else {
                qr_function_module(global_x - quiet_zone, global_y - quiet_zone, symbol_size)
            };
            if let Some(expected) = expected {
                known += 1;
                let index = (local_y * tile_modules + local_x) as usize;
                if tile.modules[index] != expected {
                    mismatches += 1;
                }
            }
        }
    }

    (known > 0).then_some(mismatches as f64 / known as f64)
}

fn qr_function_module(x: u32, y: u32, size: u32) -> Option<bool> {
    let finder_origins = [(0, 0), (size - 7, 0), (0, size - 7)];
    for (origin_x, origin_y) in finder_origins {
        let dx = x.checked_sub(origin_x);
        let dy = y.checked_sub(origin_y);
        if let (Some(dx), Some(dy)) = (dx, dy)
            && dx < 7
            && dy < 7
        {
            return Some(
                dx == 0
                    || dx == 6
                    || dy == 0
                    || dy == 6
                    || (2..=4).contains(&dx) && (2..=4).contains(&dy),
            );
        }
    }

    if (x == 7 && y <= 7)
        || (y == 7 && x <= 7)
        || (x == size - 8 && y <= 7)
        || (y == 7 && x >= size - 8)
        || (x == 7 && y >= size - 8)
        || (y == size - 8 && x <= 7)
    {
        return Some(false);
    }
    if y == 6 && (8..size - 8).contains(&x) {
        return Some(x % 2 == 0);
    }
    if x == 6 && (8..size - 8).contains(&y) {
        return Some(y % 2 == 0);
    }

    let alignment_center = match size {
        25 => Some(18),
        29 => Some(22),
        33 => Some(26),
        37 => Some(30),
        41 => Some(34),
        45 => Some(38),
        49 => Some(42),
        53 => Some(46),
        _ => None,
    };
    if let Some(center) = alignment_center {
        let dx = x.abs_diff(center);
        let dy = y.abs_diff(center);
        if dx <= 2 && dy <= 2 {
            return Some(dx == 2 || dy == 2 || (dx == 0 && dy == 0));
        }
    }
    None
}

fn assemble_image(
    tiles: &[OrientedTile],
    board: &[Option<usize>],
    grid: usize,
    image_size: u32,
) -> Result<GrayImage> {
    let tile_size = image_size / grid as u32;
    let mut output = GrayImage::new(image_size, image_size);
    for (position, oriented_index) in board.iter().enumerate() {
        let oriented_index = oriented_index.context("Incomplete tile arrangement")?;
        let tile = &tiles[oriented_index].image;
        imageops::replace(
            &mut output,
            tile,
            (position % grid) as i64 * tile_size as i64,
            (position / grid) as i64 * tile_size as i64,
        );
    }
    Ok(output)
}

fn decode_qr(image: GrayImage) -> Option<String> {
    let mut prepared = PreparedImage::prepare(image);
    prepared
        .detect_grids()
        .into_iter()
        .find_map(|grid| grid.decode().ok().map(|(_, text)| text))
}

fn extract_answer_code(message: &str) -> Result<String> {
    let marker = "válaszkód:";
    let lower = message.to_lowercase();
    let start = lower
        .find(marker)
        .context("Decoded QR message did not contain a 'Válaszkód:' field")?
        + marker.len();
    let code = message[start..]
        .trim()
        .split_whitespace()
        .next()
        .context("The QR message contained an empty answer code")?
        .trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '-');
    if code.is_empty()
        || !code.chars().all(|character| {
            character.is_ascii_uppercase() || character.is_ascii_digit() || character == '-'
        })
    {
        bail!("Decoded QR message contained an invalid answer code");
    }
    Ok(code.to_owned())
}

fn otsu_threshold(image: &GrayImage) -> u8 {
    let mut histogram = [0u64; 256];
    for pixel in image.pixels() {
        histogram[pixel.0[0] as usize] += 1;
    }
    let total = (image.width() as u64) * (image.height() as u64);
    let weighted_sum = histogram
        .iter()
        .enumerate()
        .map(|(intensity, count)| intensity as u64 * count)
        .sum::<u64>();
    let mut background_weight = 0u64;
    let mut background_sum = 0u64;
    let mut best_variance = 0.0;
    let mut threshold = 128u8;
    for (intensity, count) in histogram.into_iter().enumerate() {
        background_weight += count;
        if background_weight == 0 || background_weight == total {
            continue;
        }
        background_sum += intensity as u64 * count;
        let foreground_weight = total - background_weight;
        let background_mean = background_sum as f64 / background_weight as f64;
        let foreground_mean = (weighted_sum - background_sum) as f64 / foreground_weight as f64;
        let variance = background_weight as f64
            * foreground_weight as f64
            * (background_mean - foreground_mean).powi(2);
        if variance > best_variance {
            best_variance = variance;
            threshold = intensity as u8;
        }
    }
    threshold
}

fn rectify_tile(photo: &GrayImage, tile_size: u32) -> Result<GrayImage> {
    let (width, height) = photo.dimensions();
    let mut paper_points = Vec::new();
    for y in 0..height {
        for x in 0..width {
            let value = photo.get_pixel(x, y).0[0];
            if value >= 180 {
                paper_points.push((x as f64, y as f64));
            }
        }
    }
    if paper_points.len() < (width as usize * height as usize) / 10 {
        bail!("Could not distinguish the paper tile from its background");
    }

    let mut best_angle = 0.0;
    let mut best_area = f64::INFINITY;
    for step in 0..180 {
        let angle = step as f64 * std::f64::consts::PI / 360.0;
        let area = projected_area(&paper_points, angle);
        if area < best_area {
            best_area = area;
            best_angle = angle;
        }
    }
    let refine_from = (best_angle - 0.01).max(0.0);
    let refine_to = (best_angle + 0.01).min(std::f64::consts::FRAC_PI_2);
    let mut angle = refine_from;
    while angle <= refine_to {
        let area = projected_area(&paper_points, angle);
        if area < best_area {
            best_area = area;
            best_angle = angle;
        }
        angle += 0.001;
    }

    let (min_u, max_u, min_v, max_v) = projected_bounds(&paper_points, best_angle);
    let cos = best_angle.cos();
    let sin = best_angle.sin();
    let mut output = ImageBuffer::from_pixel(tile_size, tile_size, Luma([255]));
    for y in 0..tile_size {
        for x in 0..tile_size {
            let u = min_u + (x as f64 + 0.5) * (max_u - min_u) / tile_size as f64;
            let v = min_v + (y as f64 + 0.5) * (max_v - min_v) / tile_size as f64;
            let source_x = (cos * u - sin * v).round() as i32;
            let source_y = (sin * u + cos * v).round() as i32;
            if source_x >= 0 && source_y >= 0 && source_x < width as i32 && source_y < height as i32
            {
                output.put_pixel(x, y, *photo.get_pixel(source_x as u32, source_y as u32));
            }
        }
    }
    Ok(output)
}

fn projected_area(points: &[(f64, f64)], angle: f64) -> f64 {
    let (min_u, max_u, min_v, max_v) = projected_bounds(points, angle);
    (max_u - min_u) * (max_v - min_v)
}

fn projected_bounds(points: &[(f64, f64)], angle: f64) -> (f64, f64, f64, f64) {
    let cos = angle.cos();
    let sin = angle.sin();
    let mut min_u = f64::INFINITY;
    let mut max_u = f64::NEG_INFINITY;
    let mut min_v = f64::INFINITY;
    let mut max_v = f64::NEG_INFINITY;
    for &(x, y) in points {
        let u = cos * x + sin * y;
        let v = -sin * x + cos * y;
        min_u = min_u.min(u);
        max_u = max_u.max(u);
        min_v = min_v.min(v);
        max_v = max_v.max(v);
    }
    (min_u, max_u, min_v, max_v)
}
