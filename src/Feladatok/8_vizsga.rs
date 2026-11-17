use anyhow::{Context, Result, bail};
use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const SEARCH_NODE_LIMIT: usize = 500_000;
const SEARCH_TIME_LIMIT: Duration = Duration::from_secs(12);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Source {
    #[serde(alias = "instanceId")]
    instance: String,
    enrollments_url: String,
    availability_url: String,
    #[serde(default)]
    goal: Goal,
    timezone: String,
    utc_offset: String,
    days: Vec<String>,
    slots: Vec<Slot>,
    rooms: Vec<Room>,
    exams: Vec<Exam>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Goal {
    #[serde(default)]
    max_score: usize,
}

#[derive(Debug, Deserialize)]
struct Slot {
    slot: usize,
    start: String,
    end: String,
}

#[derive(Debug, Deserialize)]
struct Room {
    id: String,
    #[serde(default)]
    capacity: usize,
    #[serde(default)]
    equipment: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Exam {
    id: String,
    teacher: String,
    duration: usize,
    #[serde(default)]
    equipment: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct Interval {
    start: i64,
    end: i64,
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    day: usize,
    slot: usize,
    room: usize,
    start: i32,
    end: i32,
}

#[derive(Debug, Serialize)]
struct ScheduleEntry<'a> {
    exam: &'a str,
    day: &'a str,
    slot: usize,
    room: &'a str,
}

#[derive(Debug, Serialize)]
struct Solution<'a> {
    instance: &'a str,
    schedule: Vec<ScheduleEntry<'a>>,
}

struct Search<'a> {
    exams: &'a [Exam],
    candidates: &'a [Vec<Candidate>],
    shared_students: &'a [Vec<usize>],
    conflicts: Vec<Vec<bool>>,
    assigned: Vec<Option<Candidate>>,
    best: Option<Vec<Candidate>>,
    best_score: usize,
    max_score: usize,
    nodes: usize,
    started: Instant,
    stopped: bool,
    limit_reached: bool,
}

pub async fn solve(client: &Client) -> Result<()> {
    let api_endpoint = crate::api_url("scheduler")?;

    let source = fetch_source(client, &api_endpoint).await?;
    let enrollments = fetch_text(client, &source.enrollments_url)
        .await
        .context("Could not fetch the enrollment CSV")?;
    let availability = fetch_text(client, &source.availability_url)
        .await
        .context("Could not fetch the teacher availability calendar")?;

    let students = parse_enrollments(&enrollments, &source.exams)?;
    let absences = parse_absences(&availability, &source)?;
    let domains = build_domains(&source, &students, &absences)?;
    let shared_students = student_intersections(&students);
    let conflicts = (0..source.exams.len())
        .map(|left| {
            (0..source.exams.len())
                .map(|right| {
                    left != right
                        && (shared_students[left][right] > 0
                            || source.exams[left]
                                .teacher
                                .eq_ignore_ascii_case(&source.exams[right].teacher))
                })
                .collect()
        })
        .collect();

    let mut search = Search {
        exams: &source.exams,
        candidates: &domains,
        shared_students: &shared_students,
        conflicts,
        assigned: vec![None; source.exams.len()],
        best: None,
        best_score: usize::MAX,
        max_score: source.goal.max_score,
        nodes: 0,
        started: Instant::now(),
        stopped: false,
        limit_reached: false,
    };
    search.visit(0, 0);

    let schedule = match search.best {
        Some(schedule) => schedule,
        None if search.limit_reached => bail!(
            "Exam schedule search reached its limit after {} nodes and {:?} without finding a schedule (maximum score: {})",
            search.nodes,
            search.started.elapsed(),
            search.max_score
        ),
        None => bail!(
            "No valid exam schedule met the maximum score of {} after searching {} nodes",
            search.max_score,
            search.nodes
        ),
    };
    if search.best_score > source.goal.max_score {
        bail!(
            "Best schedule scores {}, above the allowed maximum of {} (searched {} nodes)",
            search.best_score,
            source.goal.max_score,
            search.nodes
        );
    }

    let entries = source
        .exams
        .iter()
        .zip(schedule)
        .map(|(exam, placement)| ScheduleEntry {
            exam: &exam.id,
            day: &source.days[placement.day],
            slot: placement.slot,
            room: &source.rooms[placement.room].id,
        })
        .collect();
    let solution = Solution {
        instance: &source.instance,
        schedule: entries,
    };
    let response = client
        .post(format!("{api_endpoint}/solution"))
        .json(&solution)
        .send()
        .await
        .context("POST /solution request failed")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("Could not read the solution response")?;
    if !status.is_success() {
        bail!("POST /solution returned {status}: {body}");
    }
    let response: Value =
        serde_json::from_str(&body).context("POST /solution returned invalid JSON")?;
    if response.get("status").is_some() && response.get("data").is_none_or(Value::is_null) {
        bail!(
            "POST /solution was rejected (status: {}, message: {})",
            response.get("status").unwrap_or(&Value::Null),
            response.get("message").unwrap_or(&Value::Null)
        );
    }
    println!(
        "Submitted schedule with score {} after searching {} nodes:\n{}",
        search.best_score, search.nodes, body
    );
    Ok(())
}

pub async fn zero_solve(run_key: &str) -> Result<()> {
    let keyed_client = super::client_with_run_key(run_key)?;
    solve(&keyed_client).await
}

impl Search<'_> {
    fn visit(&mut self, assigned_count: usize, score: usize) {
        if self.stopped {
            return;
        }
        self.nodes += 1;
        if self.nodes >= SEARCH_NODE_LIMIT || self.started.elapsed() >= SEARCH_TIME_LIMIT {
            self.stopped = true;
            self.limit_reached = true;
            return;
        }
        if score > self.max_score || score >= self.best_score {
            return;
        }
        if assigned_count == self.exams.len() {
            self.best_score = score;
            self.best = Some(
                self.assigned
                    .iter()
                    .map(|candidate| candidate.expect("all exams are assigned"))
                    .collect(),
            );
            if score == 0 {
                self.stopped = true;
            }
            return;
        }

        let mut selected = None;
        let mut selected_domain = Vec::new();
        let mut selected_degree = 0;
        for exam_index in 0..self.exams.len() {
            if self.assigned[exam_index].is_some() {
                continue;
            }
            let mut domain = Vec::new();
            for candidate in &self.candidates[exam_index] {
                if self.fits_assigned(exam_index, candidate) {
                    let added_score = self.incremental_score(exam_index, candidate.day);
                    if score + added_score <= self.max_score
                        && score + added_score < self.best_score
                    {
                        domain.push((*candidate, added_score));
                    }
                }
            }
            if domain.is_empty() {
                return;
            }
            let degree = (0..self.exams.len())
                .filter(|&other| {
                    self.assigned[other].is_none() && self.conflicts[exam_index][other]
                })
                .count();
            if selected.is_none()
                || domain.len() < selected_domain.len()
                || (domain.len() == selected_domain.len() && degree > selected_degree)
            {
                selected = Some(exam_index);
                selected_degree = degree;
                selected_domain = domain;
                if selected_domain.len() == 1 {
                    break;
                }
            }
        }

        selected_domain.sort_by_key(|(candidate, added_score)| {
            (*added_score, candidate.day, candidate.start, candidate.room)
        });
        let exam_index = selected.expect("an unassigned exam exists");
        for (candidate, added_score) in selected_domain {
            self.assigned[exam_index] = Some(candidate);
            self.visit(assigned_count + 1, score + added_score);
            self.assigned[exam_index] = None;
            if self.stopped {
                return;
            }
        }
    }

    fn fits_assigned(&self, exam_index: usize, candidate: &Candidate) -> bool {
        (0..self.exams.len()).all(|other| {
            let Some(other_candidate) = self.assigned[other] else {
                return true;
            };
            if candidate.day != other_candidate.day
                || candidate.start >= other_candidate.end
                || other_candidate.start >= candidate.end
            {
                return true;
            }
            candidate.room != other_candidate.room && !self.conflicts[exam_index][other]
        })
    }

    fn incremental_score(&self, exam_index: usize, day: usize) -> usize {
        self.assigned
            .iter()
            .enumerate()
            .filter_map(|(other, candidate)| {
                candidate
                    .filter(|candidate| candidate.day == day)
                    .map(|_| self.shared_students[exam_index][other])
            })
            .sum()
    }
}

async fn fetch_source(client: &Client, api_endpoint: &str) -> Result<Source> {
    let value = fetch_json(client, &format!("{api_endpoint}/source")).await?;
    let payload = value.get("data").cloned().unwrap_or(value);
    serde_json::from_value(payload).context("The scheduler source response has an invalid shape")
}

async fn fetch_text(client: &Client, url: &str) -> Result<String> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url} request failed"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("Could not read GET {url} response"))?;
    if !status.is_success() {
        bail!("GET {url} returned {status}: {body}");
    }
    Ok(body)
}

async fn fetch_json(client: &Client, url: &str) -> Result<Value> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url} request failed"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("Could not read GET {url} response"))?;
    if !status.is_success() {
        bail!("GET {url} returned {status}: {body}");
    }
    serde_json::from_str(&body).with_context(|| format!("GET {url} returned invalid JSON"))
}

fn parse_enrollments(csv: &str, exams: &[Exam]) -> Result<Vec<HashSet<String>>> {
    let delimiter = csv
        .lines()
        .next()
        .map(|header| {
            if header.matches(';').count() > header.matches(',').count() {
                b';'
            } else {
                b','
            }
        })
        .unwrap_or(b',');
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .from_reader(csv.as_bytes());
    let headers = reader
        .headers()
        .context("Enrollment CSV has no header")?
        .clone();
    let normalized: Vec<String> = headers.iter().map(normalize_header).collect();
    let student_index = find_header(
        &normalized,
        &["studentid", "diakid", "tanuloid", "student", "diak"],
    )
    .unwrap_or(0);
    let exam_index = find_header(&normalized, &["examid", "exam", "vizsgaid", "vizsga"])
        .unwrap_or_else(|| headers.len().saturating_sub(1));
    if student_index == exam_index || headers.is_empty() {
        bail!("Enrollment CSV must contain student and exam columns");
    }

    let exam_indices: HashMap<&str, usize> = exams
        .iter()
        .enumerate()
        .map(|(index, exam)| (exam.id.as_str(), index))
        .collect();
    let mut enrollments = vec![HashSet::new(); exams.len()];
    for (row_index, row) in reader.records().enumerate() {
        let row = row.with_context(|| format!("Invalid enrollment CSV row {}", row_index + 2))?;
        let student = row
            .get(student_index)
            .context("Enrollment row has no student identifier")?
            .trim();
        let exam_id = row
            .get(exam_index)
            .context("Enrollment row has no exam identifier")?
            .trim();
        if student.is_empty() || exam_id.is_empty() {
            bail!(
                "Enrollment row {} contains an empty identifier",
                row_index + 2
            );
        }
        let exam_index = *exam_indices
            .get(exam_id)
            .with_context(|| format!("Enrollment references unknown exam {exam_id}"))?;
        enrollments[exam_index].insert(student.to_owned());
    }
    Ok(enrollments)
}

fn normalize_header(header: &str) -> String {
    header
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn find_header(headers: &[String], aliases: &[&str]) -> Option<usize> {
    headers
        .iter()
        .position(|header| aliases.iter().any(|alias| header == alias))
}

fn student_intersections(students: &[HashSet<String>]) -> Vec<Vec<usize>> {
    let mut intersections = vec![vec![0; students.len()]; students.len()];
    for left in 0..students.len() {
        for right in left + 1..students.len() {
            let count = if students[left].len() < students[right].len() {
                students[left]
                    .iter()
                    .filter(|student| students[right].contains(*student))
                    .count()
            } else {
                students[right]
                    .iter()
                    .filter(|student| students[left].contains(*student))
                    .count()
            };
            intersections[left][right] = count;
            intersections[right][left] = count;
        }
    }
    intersections
}

fn build_domains(
    source: &Source,
    students: &[HashSet<String>],
    absences: &HashMap<String, Vec<Interval>>,
) -> Result<Vec<Vec<Candidate>>> {
    if source.days.is_empty() || source.slots.is_empty() {
        bail!("The scheduler source must contain at least one day and one slot");
    }
    let dates: Vec<NaiveDate> = source
        .days
        .iter()
        .map(|day| {
            NaiveDate::parse_from_str(day, "%Y-%m-%d")
                .with_context(|| format!("Invalid scheduler day {day}"))
        })
        .collect::<Result<_>>()?;
    let mut slots: Vec<&Slot> = source.slots.iter().collect();
    slots.sort_by_key(|slot| slot.slot);
    let slot_times = slots
        .iter()
        .map(|slot| {
            Ok((
                parse_clock(&slot.start)
                    .with_context(|| format!("Invalid start time for slot {}", slot.slot))?,
                parse_clock(&slot.end)
                    .with_context(|| format!("Invalid end time for slot {}", slot.slot))?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let room_equipment: Vec<HashSet<String>> = source
        .rooms
        .iter()
        .map(|room| {
            room.equipment
                .iter()
                .map(|equipment| equipment.to_lowercase())
                .collect()
        })
        .collect();
    let mut domains = Vec::with_capacity(source.exams.len());
    for (exam_index, exam) in source.exams.iter().enumerate() {
        if exam.duration == 0 {
            bail!("Exam {} has zero duration", exam.id);
        }
        let required: HashSet<String> = exam
            .equipment
            .iter()
            .map(|equipment| equipment.to_lowercase())
            .collect();
        let teacher = exam.teacher.to_lowercase();
        let mut domain = Vec::new();
        for (day_index, date) in dates.iter().enumerate() {
            for slot_index in 0..slots.len() {
                let Some(last_slot_index) = slot_index.checked_add(exam.duration - 1) else {
                    continue;
                };
                if last_slot_index >= slots.len() {
                    continue;
                }
                let first_slot = slots[slot_index];
                if slots[slot_index..=last_slot_index]
                    .windows(2)
                    .any(|pair| pair[1].slot != pair[0].slot + 1)
                {
                    continue;
                }
                let start = slot_times[slot_index].0;
                let end = slot_times[last_slot_index].1;
                if end <= start {
                    continue;
                }
                let absolute = Interval {
                    start: i64::from(date.num_days_from_ce()) * 1440 + i64::from(start),
                    end: i64::from(date.num_days_from_ce()) * 1440 + i64::from(end),
                };
                if absences.get(&teacher).is_some_and(|intervals| {
                    intervals.iter().any(|absence| overlaps(*absence, absolute))
                }) {
                    continue;
                }
                for (room_index, room) in source.rooms.iter().enumerate() {
                    if students[exam_index].len() > room.capacity
                        || !required.is_subset(&room_equipment[room_index])
                    {
                        continue;
                    }
                    domain.push(Candidate {
                        day: day_index,
                        slot: first_slot.slot,
                        room: room_index,
                        start,
                        end,
                    });
                }
            }
        }
        if domain.is_empty() {
            bail!("Exam {} has no possible day, time, and room", exam.id);
        }
        domains.push(domain);
    }
    Ok(domains)
}

fn parse_clock(value: &str) -> Result<i32> {
    let time = NaiveTime::parse_from_str(value, "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(value, "%H:%M:%S"))
        .with_context(|| format!("Invalid time {value}"))?;
    Ok(time.num_seconds_from_midnight() as i32 / 60)
}

fn overlaps(left: Interval, right: Interval) -> bool {
    left.start < right.end && right.start < left.end
}

fn parse_absences(ics: &str, source: &Source) -> Result<HashMap<String, Vec<Interval>>> {
    let unfolded = unfold_ical(ics);
    let mut absences: HashMap<String, Vec<Interval>> = HashMap::new();
    let mut event = Vec::new();
    let mut inside_event = false;
    for line in unfolded {
        if line.eq_ignore_ascii_case("BEGIN:VEVENT") {
            inside_event = true;
            event.clear();
        } else if line.eq_ignore_ascii_case("END:VEVENT") && inside_event {
            add_event_absence(&event, source, &mut absences)?;
            inside_event = false;
        } else if inside_event {
            event.push(line);
        }
    }
    if inside_event {
        bail!("Availability calendar contains an unterminated VEVENT");
    }
    Ok(absences)
}

fn unfold_ical(ics: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw_line in ics.lines() {
        let line = raw_line.trim_end_matches('\r');
        if (line.starts_with(' ') || line.starts_with('\t'))
            && let Some(previous) = lines.last_mut()
        {
            previous.push_str(&line[1..]);
        } else if !line.is_empty() {
            lines.push(line.to_owned());
        }
    }
    lines
}

fn add_event_absence(
    lines: &[String],
    source: &Source,
    absences: &mut HashMap<String, Vec<Interval>>,
) -> Result<()> {
    let mut status_cancelled = false;
    let mut attendees = Vec::new();
    let mut start = None;
    let mut end = None;
    for line in lines {
        let Some((property, value)) = line.split_once(':') else {
            continue;
        };
        let mut parts = property.split(';');
        let name = parts.next().unwrap_or_default().to_ascii_uppercase();
        let parameters: HashMap<String, String> = parts
            .filter_map(|parameter| parameter.split_once('='))
            .map(|(key, value)| (key.to_ascii_uppercase(), value.trim_matches('"').to_owned()))
            .collect();
        match name.as_str() {
            "STATUS" if value.eq_ignore_ascii_case("CANCELLED") => status_cancelled = true,
            "ATTENDEE" => {
                let attendee = value.trim();
                if attendee
                    .get(..7)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("mailto:"))
                {
                    attendees.push(attendee[7..].to_lowercase());
                }
            }
            "DTSTART" => start = Some((value.to_owned(), parameters)),
            "DTEND" => end = Some((value.to_owned(), parameters)),
            _ => {}
        }
    }
    if status_cancelled || attendees.is_empty() {
        return Ok(());
    }
    let (Some((start_value, start_params)), Some((end_value, end_params))) = (start, end) else {
        bail!("Non-cancelled availability event is missing DTSTART or DTEND");
    };
    let start = parse_ical_datetime(&start_value, &start_params, source)
        .context("Invalid DTSTART in availability calendar")?;
    let end = parse_ical_datetime(&end_value, &end_params, source)
        .context("Invalid DTEND in availability calendar")?;
    if end <= start {
        bail!("Availability event DTEND must be later than DTSTART");
    }
    for attendee in attendees {
        absences
            .entry(attendee)
            .or_default()
            .push(Interval { start, end });
    }
    Ok(())
}

fn parse_ical_datetime(
    value: &str,
    parameters: &HashMap<String, String>,
    source: &Source,
) -> Result<i64> {
    let utc_offset = parse_utc_offset(&source.utc_offset)?;
    let timezone = parameters.get("TZID").map(String::as_str);
    let (datetime, explicit_utc) = if let Some(value) = value.strip_suffix('Z') {
        (value, true)
    } else {
        (value, false)
    };
    let parsed = if datetime.len() == 8 {
        let date = NaiveDate::parse_from_str(datetime, "%Y%m%d")?;
        date.and_hms_opt(0, 0, 0)
            .context("Invalid all-day iCalendar date")?
    } else {
        NaiveDateTime::parse_from_str(datetime, "%Y%m%dT%H%M%S")
            .or_else(|_| NaiveDateTime::parse_from_str(datetime, "%Y%m%dT%H%M"))
            .context("Invalid iCalendar date-time")?
    };
    let is_utc = explicit_utc || matches!(timezone, Some("UTC" | "Etc/UTC"));
    if let Some(tzid) = timezone
        && tzid != source.timezone
        && tzid != "UTC"
        && tzid != "Etc/UTC"
    {
        bail!("Unsupported iCalendar timezone {tzid}");
    }
    let minutes = i64::from(parsed.date().num_days_from_ce()) * 1440
        + i64::from(parsed.time().num_seconds_from_midnight() / 60);
    Ok(minutes + if is_utc { i64::from(utc_offset) } else { 0 })
}

fn parse_utc_offset(value: &str) -> Result<i32> {
    let sign = match value.as_bytes().first() {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => bail!("Invalid UTC offset {value}"),
    };
    let (hours, minutes) = value[1..]
        .split_once(':')
        .context("UTC offset must have the form +HH:MM")?;
    let hours: i32 = hours.parse().context("Invalid UTC offset hour")?;
    let minutes: i32 = minutes.parse().context("Invalid UTC offset minute")?;
    if hours > 23 || minutes > 59 {
        bail!("Invalid UTC offset {value}");
    }
    Ok(sign * (hours * 60 + minutes))
}
