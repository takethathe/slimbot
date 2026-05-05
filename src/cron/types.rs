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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cron_schedule_serde_at() {
        let schedule = CronSchedule::at(12345);
        let json = serde_json::to_string(&schedule).unwrap();
        assert!(json.contains("at"));

        let parsed: CronSchedule = serde_json::from_str(&json).unwrap();
        match parsed {
            CronSchedule::At { at_ms } => assert_eq!(at_ms, 12345),
            _ => panic!("expected At variant"),
        }
    }

    #[test]
    fn test_cron_schedule_serde_every() {
        let schedule = CronSchedule::every(60_000);
        let json = serde_json::to_string(&schedule).unwrap();
        assert!(json.contains("every"));

        let parsed: CronSchedule = serde_json::from_str(&json).unwrap();
        match parsed {
            CronSchedule::Every { every_ms } => assert_eq!(every_ms, 60_000),
            _ => panic!("expected Every variant"),
        }
    }

    #[test]
    fn test_cron_schedule_serde_cron() {
        let schedule = CronSchedule::cron("0 */5 * * * *".to_string(), Some("UTC".to_string()));
        let json = serde_json::to_string(&schedule).unwrap();
        assert!(json.contains("cron"));
        assert!(json.contains("UTC"));

        let parsed: CronSchedule = serde_json::from_str(&json).unwrap();
        match parsed {
            CronSchedule::Cron { expr, tz } => {
                assert_eq!(expr, "0 */5 * * * *");
                assert_eq!(tz, Some("UTC".to_string()));
            }
            _ => panic!("expected Cron variant"),
        }
    }

    #[test]
    fn test_cron_schedule_serde_cron_no_timezone() {
        let schedule = CronSchedule::cron("0 0 * * * *".to_string(), None);
        let json = serde_json::to_string(&schedule).unwrap();
        let parsed: CronSchedule = serde_json::from_str(&json).unwrap();
        match parsed {
            CronSchedule::Cron { expr, tz } => {
                assert_eq!(expr, "0 0 * * * *");
                assert!(tz.is_none());
            }
            _ => panic!("expected Cron variant"),
        }
    }

    #[test]
    fn test_cron_job_default() {
        let job = CronJob::default();
        assert!(job.enabled);
        assert!(!job.delete_after_run);
        assert_eq!(job.id, "");
        assert_eq!(job.name, "");
    }

    #[test]
    fn test_cron_job_enabled_defaults_true_when_omitted() {
        let json = r#"{
            "id": "j1",
            "name": "test",
            "schedule": {"kind": "every", "every_ms": 1000}
        }"#;
        let job: CronJob = serde_json::from_str(json).unwrap();
        assert!(job.enabled);
    }

    #[test]
    fn test_cron_job_explicit_disabled() {
        let json = r#"{
            "id": "j2",
            "name": "test",
            "enabled": false,
            "schedule": {"kind": "every", "every_ms": 1000}
        }"#;
        let job: CronJob = serde_json::from_str(json).unwrap();
        assert!(!job.enabled);
    }

    #[test]
    fn test_cron_payload_default() {
        let payload = CronPayload::default();
        assert_eq!(payload.kind, "");
        assert!(!payload.deliver);
        assert!(payload.channel.is_none());
    }

    #[test]
    fn test_cron_job_state_default() {
        let state = CronJobState::default();
        assert!(state.next_run_at_ms.is_none());
        assert!(state.last_run_at_ms.is_none());
        assert!(state.last_status.is_none());
        assert!(state.last_error.is_none());
        assert!(state.run_history.is_empty());
    }

    #[test]
    fn test_cron_run_record_serde() {
        let record = CronRunRecord {
            run_at_ms: 1000,
            status: "ok".to_string(),
            duration_ms: 500,
            error: None,
        };
        let json = serde_json::to_string(&record).unwrap();
        let parsed: CronRunRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.run_at_ms, 1000);
        assert_eq!(parsed.duration_ms, 500);
        assert!(parsed.error.is_none());
    }

    #[test]
    fn test_cron_run_record_error() {
        let record = CronRunRecord {
            run_at_ms: 1000,
            status: "error".to_string(),
            duration_ms: 0,
            error: Some("timeout".to_string()),
        };
        let json = serde_json::to_string(&record).unwrap();
        let parsed: CronRunRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.error, Some("timeout".to_string()));
    }

    #[test]
    fn test_cron_store_default() {
        let store = CronStore::default();
        assert_eq!(store.version, 0);
        assert!(store.jobs.is_empty());
    }

    #[test]
    fn test_cron_store_serde_roundtrip() {
        let store = CronStore {
            version: 1,
            jobs: vec![
                CronJob {
                    id: "j1".to_string(),
                    name: "job one".to_string(),
                    enabled: true,
                    schedule: CronSchedule::every(60_000),
                    payload: CronPayload::default(),
                    state: CronJobState::default(),
                    created_at_ms: 0,
                    updated_at_ms: 0,
                    delete_after_run: false,
                },
            ],
        };

        let json = serde_json::to_string(&store).unwrap();
        let parsed: CronStore = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.jobs.len(), 1);
        assert_eq!(parsed.jobs[0].id, "j1");
    }

    #[test]
    fn test_now_ms_is_monotonic() {
        let a = now_ms();
        let b = now_ms();
        assert!(b >= a);
    }

    #[test]
    fn test_cron_job_delete_after_run_default_false() {
        let json = r#"{
            "id": "j3",
            "name": "test",
            "schedule": {"kind": "every", "every_ms": 1000}
        }"#;
        let job: CronJob = serde_json::from_str(json).unwrap();
        assert!(!job.delete_after_run);
    }
}
