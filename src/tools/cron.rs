use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;

use crate::tool::Tool;
use crate::cron::{CronService, CronJob, CronSchedule, CronPayload};

pub struct CronTool {
    cron_service: Arc<CronService>,
    default_channel: Arc<std::sync::Mutex<String>>,
    default_chat_id: Arc<std::sync::Mutex<String>>,
    in_cron_context: AtomicBool,
}

impl CronTool {
    pub fn new(cron_service: Arc<CronService>) -> Self {
        Self {
            cron_service,
            default_channel: Arc::new(std::sync::Mutex::new(String::new())),
            default_chat_id: Arc::new(std::sync::Mutex::new(String::new())),
            in_cron_context: AtomicBool::new(false),
        }
    }

    pub fn set_context(&self, channel: &str, chat_id: &str) {
        *self.default_channel.lock().unwrap() = channel.to_string();
        *self.default_chat_id.lock().unwrap() = chat_id.to_string();
    }

    pub fn set_cron_context(&self, active: bool) {
        self.in_cron_context.store(active, Ordering::Relaxed);
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    fn add_job(&self, args: &serde_json::Value) -> Result<String> {
        if self.in_cron_context.load(Ordering::Relaxed) {
            return Ok("Error: cannot schedule new jobs from within a cron job execution".to_string());
        }

        let message = args.get("message").and_then(|v| v.as_str());
        let message = match message {
            Some(m) if !m.trim().is_empty() => m.to_string(),
            _ => return Ok("Error: action='add' requires a non-empty 'message' parameter".to_string()),
        };

        let channel = self.default_channel.lock().unwrap().clone();
        let chat_id = self.default_chat_id.lock().unwrap().clone();
        if channel.is_empty() || chat_id.is_empty() {
            return Ok("Error: no session context (channel/chat_id)".to_string());
        }

        let name = args.get("name").and_then(|v| v.as_str()).unwrap_or_else(|| &message[..message.len().min(30)]).to_string();
        let deliver = args.get("deliver").and_then(|v| v.as_bool()).unwrap_or(true);

        let schedule = if let Some(every) = args.get("every_seconds").and_then(|v| v.as_i64()) {
            CronSchedule::every(every * 1000)
        } else if let Some(expr) = args.get("cron_expr").and_then(|v| v.as_str()) {
            let tz = args.get("tz").and_then(|v| v.as_str()).map(|s| s.to_string());
            CronSchedule::cron(expr.to_string(), tz)
        } else if let Some(at_str) = args.get("at").and_then(|v| v.as_str()) {
            match chrono::DateTime::parse_from_rfc3339(at_str) {
                Ok(dt) => CronSchedule::at(dt.timestamp_millis()),
                Err(_) => return Ok("Error: invalid ISO datetime format for 'at'".to_string()),
            }
        } else {
            return Ok("Error: either every_seconds, cron_expr, or at is required".to_string());
        };

        let delete_after_run = matches!(&schedule, CronSchedule::At { .. });

        let job = CronJob {
            id: uuid::Uuid::new_v4().to_string()[..8].to_string(),
            name,
            enabled: true,
            schedule,
            payload: CronPayload {
                kind: "agent_turn".to_string(),
                message,
                deliver,
                channel: Some(channel),
                to: Some(chat_id),
            },
            created_at_ms: Self::now_ms(),
            updated_at_ms: Self::now_ms(),
            delete_after_run,
            ..Default::default()
        };

        self.cron_service.add_job(job.clone());
        Ok(format!("Created job '{}' (id: {})", job.name, job.id))
    }

    fn list_jobs(&self) -> Result<String> {
        let jobs = self.cron_service.list_jobs();
        if jobs.is_empty() {
            return Ok("No scheduled jobs.".to_string());
        }

        let mut lines = Vec::new();
        for j in &jobs {
            if !j.enabled { continue; }
            let timing = format_schedule(&j.schedule);
            let mut parts = format!("- {} (id: {}, {})", j.name, j.id, timing);
            if let Some(next) = j.state.next_run_at_ms {
                parts.push_str(&format!(", next: {}ms", next));
            }
            lines.push(parts);
        }

        if lines.is_empty() {
            Ok("No active scheduled jobs.".to_string())
        } else {
            Ok(format!("Scheduled jobs:\n{}", lines.join("\n")))
        }
    }

    fn remove_job(&self, args: &serde_json::Value) -> Result<String> {
        let job_id = args.get("job_id").and_then(|v| v.as_str());
        let job_id = match job_id {
            Some(id) if !id.is_empty() => id,
            _ => return Ok("Error: job_id is required for remove".to_string()),
        };

        if self.cron_service.remove_job(job_id) {
            Ok(format!("Removed job {}", job_id))
        } else {
            Ok(format!("Job {} not found", job_id))
        }
    }
}

#[async_trait]
impl Tool for CronTool {
    fn name(&self) -> &str {
        "cron"
    }

    fn description(&self) -> &str {
        "Schedule reminders and recurring tasks. Actions: add, list, remove."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "list", "remove"],
                    "description": "Action to perform"
                },
                "name": { "type": "string", "description": "Optional: human-readable label for the job" },
                "message": { "type": "string", "description": "REQUIRED when action=add. Instruction for the agent" },
                "every_seconds": { "type": "integer", "description": "Interval in seconds (recurring)" },
                "cron_expr": { "type": "string", "description": "Cron expression like '0 9 * * *'" },
                "at": { "type": "string", "description": "ISO datetime for one-time execution (e.g. '2026-05-05T10:30:00')" },
                "deliver": { "type": "boolean", "description": "Whether to deliver result to user channel (default true)" },
                "job_id": { "type": "string", "description": "REQUIRED when action=remove" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");

        match action {
            "add" => self.add_job(&args),
            "list" => self.list_jobs(),
            "remove" => self.remove_job(&args),
            _ => Ok(format!("Unknown action: {}", action)),
        }
    }
}

fn format_schedule(schedule: &CronSchedule) -> String {
    match schedule {
        CronSchedule::Every { every_ms } => {
            let ms = *every_ms;
            if ms % 3_600_000 == 0 { format!("every {}h", ms / 3_600_000) }
            else if ms % 60_000 == 0 { format!("every {}m", ms / 60_000) }
            else if ms % 1000 == 0 { format!("every {}s", ms / 1000) }
            else { format!("every {}ms", ms) }
        }
        CronSchedule::Cron { expr, tz } => {
            let tz_str = tz.as_ref().map(|t| format!(" ({})", t)).unwrap_or_default();
            format!("cron: {}{}", expr, tz_str)
        }
        CronSchedule::At { at_ms } => format!("at {}ms", at_ms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_cron_tool() -> CronTool {
        let tmp = tempfile::tempdir().unwrap();
        let cron_svc = Arc::new(CronService::new(tmp.path()));
        CronTool::new(cron_svc)
    }

    #[tokio::test]
    async fn test_cron_tool_add_and_list() {
        let tool = make_cron_tool();
        tool.set_context("webui", "chat-1");

        let result = tool.execute(serde_json::json!({
            "action": "add",
            "name": "test job",
            "message": "do something",
            "every_seconds": 300
        })).await.unwrap();
        assert!(result.contains("Created job"));

        let result = tool.execute(serde_json::json!({
            "action": "list"
        })).await.unwrap();
        assert!(result.contains("test job"));
    }

    #[tokio::test]
    async fn test_cron_tool_add_and_remove() {
        let tool = make_cron_tool();
        tool.set_context("webui", "chat-1");

        let add_result = tool.execute(serde_json::json!({
            "action": "add",
            "name": "remove me",
            "message": "will be removed",
            "every_seconds": 60
        })).await.unwrap();
        let job_id = add_result.split("id: ").last().unwrap().trim_end_matches(')');

        let result = tool.execute(serde_json::json!({
            "action": "remove",
            "job_id": job_id
        })).await.unwrap();
        assert!(result.contains("Removed job"));
    }

    #[tokio::test]
    async fn test_cron_tool_add_missing_message() {
        let tool = make_cron_tool();
        tool.set_context("webui", "chat-1");

        let result = tool.execute(serde_json::json!({
            "action": "add",
            "name": "no message"
        })).await.unwrap();
        assert!(result.contains("requires a non-empty"));
    }

    #[tokio::test]
    async fn test_cron_tool_remove_missing_job() {
        let tool = make_cron_tool();

        let result = tool.execute(serde_json::json!({
            "action": "remove",
            "job_id": "nonexistent"
        })).await.unwrap();
        assert!(result.contains("not found"));
    }
}
