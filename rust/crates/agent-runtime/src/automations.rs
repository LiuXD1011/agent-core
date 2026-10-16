//! Scheduled automations: definitions, recurrence rules, run records and the
//! delivery outbox. The scheduling semantics mirror the WorkBuddy desktop
//! scheduler: 30s idle ticks, deterministic dispatch jitter with a wider
//! window in peak hours, a 24h catch-up window for missed runs, expiry at the
//! validity boundary, and a retrying delivery outbox with bounded backoff.
use agent_core::types::timestamp;
use chrono::{Datelike, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;

const SCHEMA: u32 = 1;
const FILE_LIMIT: u64 = 16 * 1024 * 1024;
const DEFINITION_LIMIT: usize = 100;
const RUN_RECORD_LIMIT: usize = 500;
const NOTIFICATION_LIMIT: usize = 200;
/// Occurrence search horizon in days, matching the WorkBuddy scheduler.
const OCCURRENCE_HORIZON_DAYS: u64 = 1830;
/// A missed run inside this window still runs once (catch-up).
pub const MISSED_WINDOW_MS: u64 = 24 * 60 * 60 * 1000;
/// Lateness beyond three ticks counts as missed rather than late.
pub const MISSED_TOLERANCE_MS: u64 = 5 * 60 * 1000;
/// Interrupted runs in a row beyond this break the recovery circuit.
pub const INTERRUPT_CIRCUIT_LIMIT: u32 = 5;
/// Delivery backoff schedule in milliseconds (±20% jitter in the worker).
pub const DELIVERY_BACKOFF_MS: [u64; 4] = [3_000, 30_000, 600_000, 3_600_000];
pub const DELIVERY_MAX_ATTEMPTS: u32 = 5;
pub const DELIVERY_LEASE_MS: u64 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationStatus {
    Active,
    Paused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationRunStatus {
    Running,
    Success,
    Failed,
    Skipped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationRunKind {
    Scheduled,
    Missed,
    Manual,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationSchedule {
    Once { scheduled_at: u64 },
    Recurring { rrule: String },
}

/// One scheduled task definition. Organizing state only: running an
/// automation never mutates the definition besides bookkeeping fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationDefinition {
    pub id: String,
    pub name: String,
    pub prompt: String,
    pub status: AutomationStatus,
    pub schedule: AutomationSchedule,
    /// Workspaces the prompt runs in; one run per workspace per occurrence.
    pub workspaces: Vec<PathBuf>,
    pub model: Option<String>,
    pub allow_changes: bool,
    pub allow_commands: bool,
    pub valid_from: Option<u64>,
    pub valid_until: Option<u64>,
    /// Computed occurrence; persisted so the UI can show the schedule.
    pub next_run_at: Option<u64>,
    pub last_run_at: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
    pub deleted_at: Option<u64>,
}

/// Per-automation runtime bookkeeping kept beside the definitions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationRuntimeState {
    pub last_error: Option<String>,
    pub running: bool,
    pub running_started_at: Option<u64>,
    pub consecutive_interrupt_count: u32,
}

/// One historical run of an automation (the WorkBuddy automation_runs row).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationRunRecord {
    pub id: String,
    pub automation_id: String,
    pub automation_name: String,
    pub session_id: Option<String>,
    pub status: AutomationRunStatus,
    pub kind: AutomationRunKind,
    /// Original schedule slot for a catch-up run.
    pub missed_scheduled_at: Option<u64>,
    pub workspace: PathBuf,
    pub output: String,
    pub artifacts: Vec<String>,
    pub success: Option<bool>,
    pub failure_code: Option<String>,
    pub reason_code: Option<String>,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    pub read: bool,
}

/// In-app message-center entry produced by the delivery worker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationNotification {
    pub id: String,
    pub title: String,
    pub body: String,
    pub created_at: u64,
    pub read: bool,
}

/// Delivery outbox row. The desktop sender writes an in-app notification;
/// retry, lease and retention semantics follow the WorkBuddy outbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationDelivery {
    pub id: String,
    pub dedupe_key: String,
    pub automation_id: String,
    pub automation_name: String,
    pub run_id: String,
    pub success: bool,
    pub summary: String,
    pub status: AutomationDeliveryStatus,
    pub attempt_count: u32,
    pub next_attempt_at: u64,
    pub lease_expire_at: Option<u64>,
    pub last_error: Option<String>,
    pub created_at: u64,
    pub finished_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationDeliveryStatus {
    Pending,
    RetryWaiting,
    Succeeded,
    Failed,
    Skipped,
    Abandoned,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationCatalog {
    pub revision: u64,
    pub definitions: Vec<AutomationDefinition>,
    pub runs: Vec<AutomationRunRecord>,
    pub notifications: Vec<AutomationNotification>,
    pub deliveries: Vec<AutomationDelivery>,
    pub runtime: BTreeMap<String, AutomationRuntimeState>,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    schema: u32,
    revision: u64,
    #[serde(default)]
    definitions: Vec<AutomationDefinition>,
    #[serde(default)]
    runs: Vec<AutomationRunRecord>,
    #[serde(default)]
    notifications: Vec<AutomationNotification>,
    #[serde(default)]
    deliveries: Vec<AutomationDelivery>,
    #[serde(default)]
    runtime: BTreeMap<String, AutomationRuntimeState>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutomationStoreError {
    InvalidDefinition,
    InvalidStore,
    MissingDefinition,
    DuplicateName,
    LimitReached,
    Storage,
    Busy,
    Conflict,
}

impl std::fmt::Display for AutomationStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidDefinition => "定时任务定义无效",
            Self::InvalidStore => "定时任务存储无效，原文件保持不变",
            Self::MissingDefinition => "定时任务不存在",
            Self::DuplicateName => "同名定时任务已存在",
            Self::LimitReached => "定时任务数量已达上限（100）",
            Self::Storage => "定时任务存储失败",
            Self::Busy => "定时任务存储正被占用，请重试",
            Self::Conflict => "定时任务已被其他操作修改，请刷新后重试",
        })
    }
}

impl std::error::Error for AutomationStoreError {}

fn io_error(error: std::io::Error) -> AutomationStoreError {
    let _ = error;
    AutomationStoreError::Storage
}

/// Parsed recurrence rule. Frequencies and fields mirror the WorkBuddy
/// parser: INTERVAL (default 1), BYHOUR, BYMINUTE, BYDAY, BYMONTHDAY, BYMONTH,
/// with weekly/monthly/yearly requiring their day selectors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rrule {
    pub freq: Frequency,
    pub interval: u32,
    pub byhour: Vec<u32>,
    pub byminute: Vec<u32>,
    pub byday: Vec<chrono::Weekday>,
    pub bymonthday: Vec<u32>,
    pub bymonth: Vec<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frequency {
    Minutely,
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

fn parse_weekday(raw: &str) -> Option<chrono::Weekday> {
    match raw.to_ascii_uppercase().as_str() {
        "MO" => Some(chrono::Weekday::Mon),
        "TU" => Some(chrono::Weekday::Tue),
        "WE" => Some(chrono::Weekday::Wed),
        "TH" => Some(chrono::Weekday::Thu),
        "FR" => Some(chrono::Weekday::Fri),
        "SA" => Some(chrono::Weekday::Sat),
        "SU" => Some(chrono::Weekday::Sun),
        _ => None,
    }
}

fn parse_number_list(raw: &str, low: u32, high: u32) -> Result<Vec<u32>, AutomationStoreError> {
    let mut values = BTreeSet::new();
    for part in raw.split(',') {
        let value: u32 = part
            .trim()
            .parse()
            .map_err(|_| AutomationStoreError::InvalidDefinition)?;
        if value < low || value > high {
            return Err(AutomationStoreError::InvalidDefinition);
        }
        values.insert(value);
    }
    Ok(values.into_iter().collect())
}

pub fn parse_rrule(raw: &str) -> Result<Rrule, AutomationStoreError> {
    let mut freq = None;
    let mut interval = 1u32;
    let mut byhour = vec![];
    let mut byminute = vec![];
    let mut byday = vec![];
    let mut bymonthday = vec![];
    let mut bymonth = vec![];
    for part in raw.trim().trim_matches(';').split(';') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key.trim().to_ascii_uppercase().as_str() {
            "FREQ" => {
                freq = Some(match value.trim().to_ascii_uppercase().as_str() {
                    "MINUTELY" => Frequency::Minutely,
                    "HOURLY" => Frequency::Hourly,
                    "DAILY" => Frequency::Daily,
                    "WEEKLY" => Frequency::Weekly,
                    "MONTHLY" => Frequency::Monthly,
                    "YEARLY" => Frequency::Yearly,
                    _ => return Err(AutomationStoreError::InvalidDefinition),
                });
            }
            "INTERVAL" => {
                interval = value
                    .trim()
                    .parse()
                    .map_err(|_| AutomationStoreError::InvalidDefinition)?;
                if interval == 0 {
                    return Err(AutomationStoreError::InvalidDefinition);
                }
            }
            "BYHOUR" => byhour = parse_number_list(value, 0, 23)?,
            "BYMINUTE" => byminute = parse_number_list(value, 0, 59)?,
            "BYDAY" => {
                let mut days: Vec<chrono::Weekday> = vec![];
                for part in value.split(',') {
                    let day = parse_weekday(part.trim())
                        .ok_or(AutomationStoreError::InvalidDefinition)?;
                    if !days.contains(&day) {
                        days.push(day);
                    }
                }
                byday = days;
            }
            "BYMONTHDAY" => bymonthday = parse_number_list(value, 1, 31)?,
            "BYMONTH" => bymonth = parse_number_list(value, 1, 12)?,
            _ => {}
        }
    }
    let freq = freq.ok_or(AutomationStoreError::InvalidDefinition)?;
    match freq {
        Frequency::Weekly if byday.is_empty() => Err(AutomationStoreError::InvalidDefinition),
        Frequency::Monthly if bymonthday.is_empty() => Err(AutomationStoreError::InvalidDefinition),
        Frequency::Yearly if bymonth.is_empty() || bymonthday.is_empty() => {
            Err(AutomationStoreError::InvalidDefinition)
        }
        _ => Ok(Rrule {
            freq,
            interval,
            byhour: if byhour.is_empty() { vec![0] } else { byhour },
            byminute: if byminute.is_empty() {
                vec![0]
            } else {
                byminute
            },
            byday,
            bymonthday,
            bymonth,
        }),
    }
}

/// Monday-based start of the local week containing `date`, midnight.
fn start_of_week(date: chrono::NaiveDate) -> chrono::NaiveDate {
    date - chrono::Duration::days(date.weekday().num_days_from_monday().into())
}

fn end_of_day_ms(valid_until: u64) -> u64 {
    // A validity end at midnight means "through the end of that day".
    let secs = (valid_until / 1000) as i64;
    let millis = (valid_until % 1000) as u32;
    let date = match chrono::DateTime::from_timestamp_millis(secs * 1000 + millis as i64) {
        Some(moment) => moment.with_timezone(&chrono::Local).date_naive(),
        None => return valid_until,
    };
    match chrono::Local
        .from_local_datetime(&date.and_hms_opt(23, 59, 59).unwrap_or_default())
        .single()
    {
        Some(end) => end.timestamp_millis().max(0) as u64 + 999,
        None => valid_until,
    }
}

fn local_ymd_ms(date: chrono::NaiveDate, hour: u32, minute: u32) -> Option<u64> {
    use chrono::TimeZone;
    let moment = chrono::Local
        .from_local_datetime(&date.and_hms_opt(hour, minute, 0)?)
        .single()?;
    let millis = moment.timestamp_millis();
    (millis >= 0).then_some(millis as u64)
}

fn date_of(ms: u64) -> Option<chrono::NaiveDate> {
    let secs = (ms / 1000) as i64;
    Some(
        chrono::DateTime::from_timestamp_millis(secs * 1000)?
            .with_timezone(&chrono::Local)
            .date_naive(),
    )
}

/// Next occurrence for interval frequencies (minutely/hourly), anchored on
/// the previous run like the WorkBuddy code. The result may stay in the
/// past when the anchor is old; the scheduler then classifies it as missed.
fn interval_next_run(
    rrule: &Rrule,
    anchor_ms: u64,
    valid_from: Option<u64>,
    valid_until: Option<u64>,
) -> Option<u64> {
    let step = match rrule.freq {
        Frequency::Minutely => (rrule.interval as u64) * 60_000,
        _ => (rrule.interval as u64) * 3_600_000,
    };
    if step == 0 {
        return None;
    }
    let mut next = anchor_ms + step;
    if let Some(valid_from) = valid_from {
        next = next.max(valid_from);
    }
    // Shift onto an allowed weekday when BYDAY is present.
    if !rrule.byday.is_empty() {
        for _ in 0..7 {
            let date = date_of(next)?;
            if rrule.byday.contains(&date.weekday()) {
                break;
            }
            let next_date = date.succ_opt()?;
            let day_start = local_ymd_ms(date, 0, 0)?;
            let time_of_day = next.saturating_sub(day_start).min(86_399_000);
            next = local_ymd_ms(next_date, 0, 0)? + time_of_day;
        }
    }
    match valid_until {
        Some(valid_until) if next > end_of_day_ms(valid_until) => None,
        _ => Some(next),
    }
}

/// Calendar frequencies: walk days forward, emitting local BYHOUR:BYMINUTE
/// times on days whose day/week/month index matches the interval anchor.
fn calendar_next_run(
    rrule: &Rrule,
    anchor_date: chrono::NaiveDate,
    after_ms: u64,
    valid_from: Option<u64>,
    valid_until: Option<u64>,
) -> Option<u64> {
    let until = valid_until.map(end_of_day_ms).unwrap_or(u64::MAX);
    let mut date = date_of(after_ms)?;
    let horizon = date + chrono::Duration::days(OCCURRENCE_HORIZON_DAYS as i64);
    while date <= horizon {
        let matches = match rrule.freq {
            Frequency::Daily => {
                ((date - anchor_date).num_days() as u64).is_multiple_of(rrule.interval as u64)
            }
            Frequency::Weekly => {
                rrule.byday.contains(&date.weekday())
                    && ((start_of_week(date) - start_of_week(anchor_date)).num_days() as u64 / 7)
                        .is_multiple_of(rrule.interval as u64)
            }
            Frequency::Monthly => {
                let index =
                    |date: chrono::NaiveDate| date.year() as i64 * 12 + date.month() as i64 - 1;
                rrule.bymonthday.contains(&date.day())
                    && (index(date) - index(anchor_date)) % rrule.interval as i64 == 0
            }
            Frequency::Yearly => {
                rrule.bymonth.contains(&date.month())
                    && rrule.bymonthday.contains(&date.day())
                    && (date.year() - anchor_date.year()) % rrule.interval as i32 == 0
            }
            _ => false,
        };
        if matches {
            let mut times: BTreeSet<u64> = BTreeSet::new();
            for hour in &rrule.byhour {
                for minute in &rrule.byminute {
                    if let Some(ms) = local_ymd_ms(date, *hour, *minute) {
                        times.insert(ms);
                    }
                }
            }
            for ms in times {
                if ms >= after_ms && ms >= valid_from.unwrap_or(0) && ms <= until {
                    return Some(ms);
                }
            }
        }
        date = date.succ_opt()?;
    }
    None
}

/// Compute the next run for a definition: the WorkBuddy computeNextRunAt.
pub fn compute_next_run_at(
    definition: &AutomationDefinition,
    now: u64,
    after: u64,
    last_run_at: Option<u64>,
) -> Option<u64> {
    match &definition.schedule {
        AutomationSchedule::Once { scheduled_at } => {
            let at = *scheduled_at;
            if at < after || at < definition.valid_from.unwrap_or(0) {
                None
            } else {
                match definition.valid_until {
                    Some(valid_until) if at > end_of_day_ms(valid_until) => None,
                    _ => Some(at),
                }
            }
        }
        AutomationSchedule::Recurring { rrule } => {
            let rrule = parse_rrule(rrule).ok()?;
            match rrule.freq {
                Frequency::Minutely | Frequency::Hourly => {
                    let anchor = last_run_at.unwrap_or(now);
                    interval_next_run(
                        &rrule,
                        anchor,
                        definition.valid_from,
                        definition.valid_until,
                    )
                }
                _ => {
                    let anchor_date = date_of(
                        definition
                            .created_at
                            .max(last_run_at.unwrap_or(definition.created_at)),
                    )?;
                    calendar_next_run(
                        &rrule,
                        anchor_date,
                        after,
                        definition.valid_from,
                        definition.valid_until,
                    )
                }
            }
        }
    }
}

/// FNV-1a 64-bit, the WorkBuddy stableFractionFromString hash.
fn fnv1a64(input: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Deterministic dispatch delay: a wide 10-minute window in the 9-10 peak
/// hours, otherwise 30 seconds, keyed by automation and occurrence.
pub fn dispatch_delay_ms(automation_id: &str, next_run_at: u64, local_hour: u32) -> u64 {
    let fraction = fnv1a64(&format!("{automation_id}:{next_run_at}")) as f64 / u64::MAX as f64;
    let window: u64 = if local_hour == 9 || local_hour == 10 {
        10 * 60 * 1000
    } else {
        30 * 1000
    };
    (fraction * window as f64) as u64
}

/// What the scheduler should do with an automation on this tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DueDecision {
    Skip,
    /// Validity ended; the definition stops without running.
    Expire {
        reason_code: &'static str,
    },
    /// The occurrence was skipped; recurring schedules recompute their slot.
    SkipMissed {
        reason_code: &'static str,
    },
    Run {
        kind: AutomationRunKind,
        missed_scheduled_at: Option<u64>,
    },
}

pub fn decide_due_automation(
    definition: &AutomationDefinition,
    runtime: &AutomationRuntimeState,
    now: u64,
    local_hour: u32,
    active_runs: usize,
    concurrency: usize,
) -> DueDecision {
    if definition.status != AutomationStatus::Active {
        return DueDecision::Skip;
    }
    let Some(next_run_at) = definition.next_run_at else {
        return DueDecision::Skip;
    };
    if now < next_run_at {
        return DueDecision::Skip;
    }
    if runtime.running {
        return DueDecision::Skip;
    }
    if definition
        .last_run_at
        .is_some_and(|last| last >= next_run_at)
    {
        return DueDecision::Skip;
    }
    if let Some(valid_until) = definition.valid_until {
        if now > end_of_day_ms(valid_until) {
            return DueDecision::Expire {
                reason_code: "valid_until_passed",
            };
        }
    }
    let due_at = next_run_at + dispatch_delay_ms(&definition.id, next_run_at, local_hour);
    if now < due_at {
        return DueDecision::Skip;
    }
    let missed = now - due_at > MISSED_TOLERANCE_MS;
    if missed && now - next_run_at > MISSED_WINDOW_MS {
        return match definition.schedule {
            AutomationSchedule::Once { .. } => DueDecision::Expire {
                reason_code: "once_window_expired",
            },
            AutomationSchedule::Recurring { .. } => DueDecision::SkipMissed {
                reason_code: "missed_window_expired",
            },
        };
    }
    if runtime.consecutive_interrupt_count >= INTERRUPT_CIRCUIT_LIMIT {
        return DueDecision::SkipMissed {
            reason_code: "recovery_circuit_broken",
        };
    }
    if active_runs >= concurrency {
        return DueDecision::Skip;
    }
    DueDecision::Run {
        kind: if missed {
            AutomationRunKind::Missed
        } else {
            AutomationRunKind::Scheduled
        },
        missed_scheduled_at: missed.then_some(next_run_at),
    }
}

/// Pure decision for one outbox row: send now, wait for the lease or backoff,
/// give up, or drop the row because a newer delivery superseded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliveryDecision {
    Send,
    Wait,
    Retry { next_attempt_at: u64 },
    Fail,
    SkipSuperseded,
    Abandon,
}

pub fn decide_delivery(
    delivery: &AutomationDelivery,
    has_newer_for_automation: bool,
    now: u64,
) -> DeliveryDecision {
    if has_newer_for_automation {
        return DeliveryDecision::SkipSuperseded;
    }
    if !matches!(
        delivery.status,
        AutomationDeliveryStatus::Pending | AutomationDeliveryStatus::RetryWaiting
    ) {
        return DeliveryDecision::Wait;
    }
    if delivery.created_at + MISSED_WINDOW_MS <= now {
        return DeliveryDecision::Abandon;
    }
    let leased = delivery.lease_expire_at.is_some_and(|lease| lease > now);
    if delivery.next_attempt_at > now || leased {
        return DeliveryDecision::Wait;
    }
    if delivery.attempt_count >= DELIVERY_MAX_ATTEMPTS {
        return DeliveryDecision::Fail;
    }
    DeliveryDecision::Send
}

/// Backoff for a failed attempt (1-based), ±20% deterministic jitter.
pub fn delivery_backoff_ms(attempt: u32, jitter_fraction: f64) -> u64 {
    let index = (attempt as usize).saturating_sub(1);
    let base = DELIVERY_BACKOFF_MS[index.min(DELIVERY_BACKOFF_MS.len() - 1)];
    let spread = base as f64 * 0.4;
    let offset = (jitter_fraction - 0.5) * spread;
    (base as f64 + offset).max(1000.0) as u64
}

/// JSON file store guarded by a sidecar lock file, following the queue store.
pub struct AutomationStore {
    directory: PathBuf,
    _lock: File,
}

impl AutomationStore {
    pub fn open(directory: impl Into<PathBuf>) -> Result<Self, AutomationStoreError> {
        let directory = directory.into();
        fs::create_dir_all(&directory).map_err(io_error)?;
        let path = directory.join("automations.json");
        let lock_path = directory.join("automations.lock");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        let lock = options.open(&lock_path).map_err(io_error)?;
        lock.try_lock().map_err(|_| AutomationStoreError::Busy)?;
        let size = path.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        if size > FILE_LIMIT {
            return Err(AutomationStoreError::InvalidStore);
        }
        Ok(Self {
            directory,
            _lock: lock,
        })
    }

    fn path(&self) -> PathBuf {
        self.directory.join("automations.json")
    }

    fn load_stored(&self) -> Result<Stored, AutomationStoreError> {
        let mut bytes = Vec::new();
        match File::open(self.path()) {
            Ok(mut file) => {
                file.read_to_end(&mut bytes).map_err(io_error)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Stored {
                    schema: SCHEMA,
                    revision: 0,
                    definitions: vec![],
                    runs: vec![],
                    notifications: vec![],
                    deliveries: vec![],
                    runtime: BTreeMap::new(),
                    extra: Map::new(),
                })
            }
            Err(error) => return Err(io_error(error)),
        }
        if bytes.is_empty() {
            return Ok(Stored {
                schema: SCHEMA,
                revision: 0,
                definitions: vec![],
                runs: vec![],
                notifications: vec![],
                deliveries: vec![],
                runtime: BTreeMap::new(),
                extra: Map::new(),
            });
        }
        if bytes.len() as u64 > FILE_LIMIT {
            return Err(AutomationStoreError::InvalidStore);
        }
        let stored: Stored =
            serde_json::from_slice(&bytes).map_err(|_| AutomationStoreError::InvalidStore)?;
        if stored.schema != SCHEMA {
            return Err(AutomationStoreError::InvalidStore);
        }
        Ok(stored)
    }

    fn save_stored(&self, stored: &Stored) -> Result<(), AutomationStoreError> {
        let bytes = serde_json::to_vec_pretty(stored).map_err(|_| AutomationStoreError::Storage)?;
        let path = self.path();
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).map_err(|_| AutomationStoreError::Storage)?;
        let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let temporary = path.with_extension(format!("json.{nonce}.tmp"));
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(io_error)?;
            file.write_all(&bytes).map_err(io_error)?;
            file.sync_all().map_err(io_error)?;
        }
        fs::rename(&temporary, &path).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            io_error(error)
        })?;
        Ok(())
    }

    pub fn catalog(&self) -> Result<AutomationCatalog, AutomationStoreError> {
        let stored = self.load_stored()?;
        Ok(AutomationCatalog {
            revision: stored.revision,
            definitions: stored.definitions,
            runs: stored.runs,
            notifications: stored.notifications,
            deliveries: stored.deliveries,
            runtime: stored.runtime,
        })
    }

    /// Mutate the store under one revision; the closure may reject with an
    /// error that aborts the write, or return a value read from the catalog.
    pub fn update<T>(
        &self,
        expected_revision: Option<u64>,
        mutate: impl FnOnce(&mut AutomationCatalog) -> Result<T, AutomationStoreError>,
    ) -> Result<T, AutomationStoreError> {
        let mut stored = self.load_stored()?;
        if let Some(expected) = expected_revision {
            if stored.revision != expected {
                return Err(AutomationStoreError::Conflict);
            }
        }
        let mut catalog = AutomationCatalog {
            revision: stored.revision,
            definitions: stored.definitions,
            runs: stored.runs,
            notifications: stored.notifications,
            deliveries: stored.deliveries,
            runtime: stored.runtime,
        };
        let result = mutate(&mut catalog)?;
        catalog.runs.truncate(RUN_RECORD_LIMIT);
        catalog.notifications.truncate(NOTIFICATION_LIMIT);
        stored.revision += 1;
        stored.definitions = catalog.definitions;
        stored.runs = catalog.runs;
        stored.notifications = catalog.notifications;
        stored.deliveries = catalog.deliveries;
        stored.runtime = catalog.runtime;
        self.save_stored(&stored)?;
        Ok(result)
    }

    fn validate_definition(definition: &AutomationDefinition) -> Result<(), AutomationStoreError> {
        if definition.id.is_empty()
            || !definition
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(AutomationStoreError::InvalidDefinition);
        }
        if definition.name.trim().is_empty() || definition.prompt.trim().is_empty() {
            return Err(AutomationStoreError::InvalidDefinition);
        }
        if definition.workspaces.is_empty() {
            return Err(AutomationStoreError::InvalidDefinition);
        }
        if let AutomationSchedule::Once { scheduled_at } = definition.schedule {
            if scheduled_at == 0 {
                return Err(AutomationStoreError::InvalidDefinition);
            }
        }
        if let AutomationSchedule::Recurring { rrule } = &definition.schedule {
            parse_rrule(rrule)?;
        }
        Ok(())
    }

    pub fn create(
        &self,
        mut definition: AutomationDefinition,
    ) -> Result<AutomationDefinition, AutomationStoreError> {
        let now = timestamp();
        definition.created_at = now;
        definition.updated_at = now;
        definition.deleted_at = None;
        Self::validate_definition(&definition)?;
        self.update(None, |catalog| {
            if catalog.definitions.len() >= DEFINITION_LIMIT {
                return Err(AutomationStoreError::LimitReached);
            }
            if catalog
                .definitions
                .iter()
                .filter(|other| other.deleted_at.is_none())
                .any(|other| other.name == definition.name)
            {
                return Err(AutomationStoreError::DuplicateName);
            }
            definition.next_run_at = compute_next_run_at(&definition, now, now, None);
            catalog.definitions.push(definition.clone());
            Ok(definition)
        })
    }

    #[allow(clippy::type_complexity)]
    pub fn update_definition(
        &self,
        id: &str,
        expected_revision: u64,
        mutate: impl FnOnce(&mut AutomationDefinition) -> Result<(), AutomationStoreError>,
    ) -> Result<AutomationDefinition, AutomationStoreError> {
        let id = id.to_owned();
        self.update(Some(expected_revision), |catalog| {
            let now = timestamp();
            let definition = catalog
                .definitions
                .iter_mut()
                .find(|other| other.id == id && other.deleted_at.is_none())
                .ok_or(AutomationStoreError::MissingDefinition)?;
            mutate(definition)?;
            Self::validate_definition(definition)?;
            definition.updated_at = now;
            definition.next_run_at =
                compute_next_run_at(definition, now, now, definition.last_run_at);
            Ok(definition.clone())
        })
    }

    pub fn delete(&self, id: &str, expected_revision: u64) -> Result<(), AutomationStoreError> {
        let id = id.to_owned();
        self.update(Some(expected_revision), |catalog| {
            let now = timestamp();
            let definition = catalog
                .definitions
                .iter_mut()
                .find(|other| other.id == id && other.deleted_at.is_none())
                .ok_or(AutomationStoreError::MissingDefinition)?;
            definition.deleted_at = Some(now);
            definition.updated_at = now;
            Ok(())
        })
    }

    /// Record a finished run, advance bookkeeping and enqueue the delivery.
    #[allow(clippy::too_many_arguments)]
    pub fn record_run(
        &self,
        record: AutomationRunRecord,
        delivery: Option<AutomationDelivery>,
    ) -> Result<(), AutomationStoreError> {
        let automation_id = record.automation_id.clone();
        let started = matches!(record.status, AutomationRunStatus::Running);
        let interrupted = matches!(record.status, AutomationRunStatus::Skipped);
        let failed = matches!(record.status, AutomationRunStatus::Failed);
        let finished_at = record.finished_at;
        self.update(None, |catalog| {
            catalog.runs.insert(0, record);
            if let Some(delivery) = delivery {
                catalog.deliveries.insert(0, delivery);
            }
            if let Some(definition) = catalog
                .definitions
                .iter_mut()
                .find(|other| other.id == automation_id)
            {
                definition.last_run_at = finished_at;
                let runtime = catalog.runtime.entry(automation_id.clone()).or_default();
                runtime.running = started;
                runtime.running_started_at = started.then_some(timestamp());
                if interrupted {
                    runtime.consecutive_interrupt_count += 1;
                } else if !started {
                    runtime.consecutive_interrupt_count = 0;
                }
                if failed {
                    runtime.last_error = Some("run_failed".to_owned());
                } else if !started {
                    runtime.last_error = None;
                }
                definition.next_run_at = if started {
                    // The slot is consumed while the run holds the definition.
                    None
                } else {
                    compute_next_run_at(
                        definition,
                        timestamp(),
                        timestamp(),
                        definition.last_run_at,
                    )
                };
            } else {
                catalog.runtime.remove(&automation_id);
            }
            Ok(())
        })
    }

    pub fn mark_running(&self, automation_id: &str) -> Result<(), AutomationStoreError> {
        let automation_id = automation_id.to_owned();
        self.update(None, |catalog| {
            if let Some(definition) = catalog
                .definitions
                .iter_mut()
                .find(|other| other.id == automation_id)
            {
                definition.next_run_at = None;
            }
            let runtime = catalog.runtime.entry(automation_id).or_default();
            runtime.running = true;
            runtime.running_started_at = Some(timestamp());
            Ok(())
        })
    }

    /// Recompute the slot after a skipped occurrence (catch-up expiry etc.).
    pub fn recompute_after_skip(
        &self,
        automation_id: &str,
        reason_code: &str,
    ) -> Result<(), AutomationStoreError> {
        let automation_id = automation_id.to_owned();
        let reason = reason_code.to_owned();
        self.update(None, |catalog| {
            let now = timestamp();
            let Some(definition) = catalog
                .definitions
                .iter_mut()
                .find(|other| other.id == automation_id)
            else {
                return Err(AutomationStoreError::MissingDefinition);
            };
            definition.last_run_at = Some(now);
            definition.next_run_at =
                compute_next_run_at(definition, now, now, definition.last_run_at);
            let runtime = catalog.runtime.entry(automation_id).or_default();
            runtime.last_error = Some(reason);
            Ok(())
        })
    }

    pub fn expire(
        &self,
        automation_id: &str,
        reason_code: &str,
    ) -> Result<(), AutomationStoreError> {
        let automation_id = automation_id.to_owned();
        let reason = reason_code.to_owned();
        self.update(None, |catalog| {
            let now = timestamp();
            let definition = catalog
                .definitions
                .iter_mut()
                .find(|other| other.id == automation_id)
                .ok_or(AutomationStoreError::MissingDefinition)?;
            definition.status = AutomationStatus::Paused;
            definition.next_run_at = None;
            definition.updated_at = now;
            let runtime = catalog.runtime.entry(automation_id).or_default();
            runtime.last_error = Some(reason);
            Ok(())
        })
    }

    /// One delivery-worker pass at the given instant with deterministic
    /// jitter (0.0-1.0). The sender returns Ok on delivery; failures schedule
    /// the bounded backoff. Returns the delivered rows: callers that persist
    /// delivery side effects (for example in-app notifications) must do so
    /// after this pass returns — writes inside the sender closure are
    /// overwritten by this pass's own save.
    pub fn process_deliveries_with(
        &self,
        now: u64,
        jitter_fraction: f64,
        deliver: impl Fn(&AutomationDelivery) -> Result<(), String>,
    ) -> Result<Vec<AutomationDelivery>, AutomationStoreError> {
        let mut processed = Vec::new();
        self.update(None, |catalog| {
            let mut seen_automations = BTreeSet::new();
            // Newest first: only the latest row per automation stays live.
            for index in 0..catalog.deliveries.len() {
                let delivery = catalog.deliveries[index].clone();
                let superseded = !seen_automations.insert(delivery.automation_id.clone());
                let decision = decide_delivery(&delivery, superseded, now);
                match decision {
                    DeliveryDecision::SkipSuperseded => {
                        catalog.deliveries[index].status = AutomationDeliveryStatus::Skipped;
                        catalog.deliveries[index].finished_at = Some(now);
                    }
                    DeliveryDecision::Abandon => {
                        catalog.deliveries[index].status = AutomationDeliveryStatus::Abandoned;
                        catalog.deliveries[index].finished_at = Some(now);
                    }
                    DeliveryDecision::Fail => {
                        catalog.deliveries[index].status = AutomationDeliveryStatus::Failed;
                        catalog.deliveries[index].finished_at = Some(now);
                    }
                    DeliveryDecision::Send => {
                        let attempt = delivery.attempt_count + 1;
                        catalog.deliveries[index].attempt_count = attempt;
                        catalog.deliveries[index].lease_expire_at = Some(now + DELIVERY_LEASE_MS);
                        let outcome =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                deliver(&delivery)
                            }))
                            .unwrap_or_else(|_| Err("delivery sender panicked".into()));
                        match outcome {
                            Ok(()) => {
                                catalog.deliveries[index].status =
                                    AutomationDeliveryStatus::Succeeded;
                                catalog.deliveries[index].finished_at = Some(now);
                                catalog.deliveries[index].last_error = None;
                                processed.push(delivery.clone());
                            }
                            Err(error) => {
                                if attempt >= DELIVERY_MAX_ATTEMPTS {
                                    catalog.deliveries[index].status =
                                        AutomationDeliveryStatus::Failed;
                                    catalog.deliveries[index].finished_at = Some(now);
                                } else {
                                    catalog.deliveries[index].status =
                                        AutomationDeliveryStatus::RetryWaiting;
                                    catalog.deliveries[index].next_attempt_at =
                                        now + delivery_backoff_ms(attempt, jitter_fraction);
                                }
                                catalog.deliveries[index].last_error = Some(error);
                                catalog.deliveries[index].lease_expire_at = None;
                            }
                        }
                    }
                    DeliveryDecision::Wait | DeliveryDecision::Retry { .. } => {}
                }
            }
            // Retention: succeeded rows older than 24h, failed rows older than
            // 30 days, keeping the 200 most recent failures.
            let succeeded_cutoff = now - MISSED_WINDOW_MS;
            let failed_cutoff = now - 30 * MISSED_WINDOW_MS;
            let mut failed_kept = 0usize;
            catalog.deliveries.retain(|delivery| match delivery.status {
                AutomationDeliveryStatus::Succeeded => delivery
                    .finished_at
                    .is_none_or(|finished| finished > succeeded_cutoff),
                AutomationDeliveryStatus::Failed => {
                    if delivery
                        .finished_at
                        .is_some_and(|finished| finished <= failed_cutoff)
                    {
                        return false;
                    }
                    failed_kept += 1;
                    failed_kept <= NOTIFICATION_LIMIT
                }
                _ => true,
            });
            Ok(())
        })?;
        Ok(processed)
    }

    /// Delivery pass at the current instant with default jitter.
    pub fn process_deliveries(
        &self,
        deliver: impl Fn(&AutomationDelivery) -> Result<(), String>,
    ) -> Result<Vec<AutomationDelivery>, AutomationStoreError> {
        self.process_deliveries_with(timestamp(), 0.5, deliver)
    }

    /// Insert the in-app notification row produced by a delivery sender.
    pub fn push_notification(
        &self,
        notification: AutomationNotification,
    ) -> Result<(), AutomationStoreError> {
        self.update(None, |catalog| {
            catalog.notifications.insert(0, notification);
            catalog.notifications.truncate(NOTIFICATION_LIMIT);
            Ok(())
        })
    }

    pub fn mark_notifications_read(&self) -> Result<(), AutomationStoreError> {
        self.update(None, |catalog| {
            for notification in &mut catalog.notifications {
                notification.read = true;
            }
            for run in &mut catalog.runs {
                run.read = true;
            }
            Ok(())
        })
    }
}

/// Outcome of a scheduler tick for one automation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchedulerOutcome {
    pub definition: AutomationDefinition,
    pub decision: DueDecision,
}

/// One scheduler pass: classify every active definition against a catalog
/// snapshot and apply the store-side bookkeeping (running flags, slot
/// recomputation, expiry) in one revision.
pub fn tick_scheduler(
    store: &AutomationStore,
    now: u64,
    local_hour: u32,
    active_runs: usize,
    concurrency: usize,
) -> Result<Vec<SchedulerOutcome>, AutomationStoreError> {
    let snapshot = store.catalog()?;
    let mut decisions = Vec::new();
    for definition in &snapshot.definitions {
        if definition.deleted_at.is_some() || definition.status != AutomationStatus::Active {
            continue;
        }
        let runtime = snapshot
            .runtime
            .get(&definition.id)
            .cloned()
            .unwrap_or_default();
        let decision = decide_due_automation(
            definition,
            &runtime,
            now,
            local_hour,
            active_runs,
            concurrency,
        );
        if !matches!(decision, DueDecision::Skip) {
            decisions.push((definition.clone(), decision));
        }
    }
    if decisions.is_empty() {
        return Ok(vec![]);
    }
    store.update(None, |catalog| {
        for (snapshot, decision) in &decisions {
            let Some(definition) = catalog
                .definitions
                .iter_mut()
                .find(|other| other.id == snapshot.id)
            else {
                continue;
            };
            if definition.status != AutomationStatus::Active {
                continue;
            }
            match decision {
                DueDecision::Run { .. } => {
                    definition.next_run_at = None;
                    let runtime = catalog.runtime.entry(snapshot.id.clone()).or_default();
                    runtime.running = true;
                    runtime.running_started_at = Some(now);
                }
                DueDecision::SkipMissed { reason_code } => {
                    definition.last_run_at = Some(now);
                    definition.next_run_at =
                        compute_next_run_at(definition, now, now, definition.last_run_at);
                    let runtime = catalog.runtime.entry(snapshot.id.clone()).or_default();
                    runtime.last_error = Some((*reason_code).to_owned());
                }
                DueDecision::Expire { reason_code } => {
                    definition.status = AutomationStatus::Paused;
                    definition.next_run_at = None;
                    definition.updated_at = now;
                    let runtime = catalog.runtime.entry(snapshot.id.clone()).or_default();
                    runtime.last_error = Some((*reason_code).to_owned());
                }
                DueDecision::Skip => {}
            }
        }
        Ok(())
    })?;
    Ok(decisions
        .into_iter()
        .map(|(mut definition, decision)| {
            if matches!(decision, DueDecision::Run { .. }) {
                definition.next_run_at = None;
            }
            SchedulerOutcome {
                definition,
                decision,
            }
        })
        .collect())
}

#[path = "automations_tests.rs"]
#[cfg(test)]
mod automations_tests;
