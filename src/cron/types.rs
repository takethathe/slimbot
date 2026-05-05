use std::collections::HashMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum CronSchedule {
    At { at_ms: i64 },
    Every { every_ms: i64 },
    Cron { expr: String, #[serde(default)] tz: Option<String> },
}

impl CronSchedule {
    pub fn at(at_ms: i64) -> Self { Self::At { at_ms } }
    pub fn every(every_ms: i64) -> Self { Self::Every { every_ms } }
    pub fn cron(expr: String, tz: Option<String>) -> Self { Self::Cron { expr, tz } }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CronPayload {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub deliver: bool,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CronRunRecord {
    pub run_at_ms: i64,
    pub status: String,
    #[serde(default)]
    pub duration_ms: i64,
    #[serde(default)]
    pub error: Option<String>,
}

pub const MAX_RUN_HISTORY: usize = 20;

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CronJobState {
    #[serde(default)]
    pub next_run_at_ms: Option<i64>,
    #[serde(default)]
    pub last_run_at_ms: Option<i64>,
    #[serde(default)]
    pub last_status: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub run_history: Vec<CronRunRecord>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CronJob {
    pub id: String,
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub schedule: CronSchedule,
    #[serde(default)]
    pub payload: CronPayload,
    #[serde(default)]
    pub state: CronJobState,
    #[serde(default)]
    pub created_at_ms: i64,
    #[serde(default)]
    pub updated_at_ms: i64,
    #[serde(default)]
    pub delete_after_run: bool,
}

impl Default for CronJob {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            schedule: CronSchedule::every(0),
            payload: CronPayload::default(),
            state: CronJobState::default(),
            created_at_ms: 0,
            updated_at_ms: 0,
            delete_after_run: false,
        }
    }
}

fn default_true() -> bool { true }

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CronStore {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub jobs: Vec<CronJob>,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
