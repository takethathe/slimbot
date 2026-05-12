use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;

use crate::tool::{Tool, ToolContext};
use crate::cron::{CronService, CronJob, CronSchedule, CronPayload};

pub struct CronTool {
    cron_service: Arc<CronService>,
    default_channel: Arc<std::sync::Mutex<String>>,
    default_chat_id: Arc<std::sync::Mutex<String>>,
    in_cron_context: AtomicBool,
}

/// Parse an `at` datetime string into a UTC epoch millisecond timestamp.
/// Returns `None` if the format is not recognized.
fn parse_at_datetime(s: &str) -> Option<i64> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }
    for fmt in &["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            let offset = *chrono::Local::now().offset();
            return Some(
                chrono::DateTime::<chrono::Utc>::from(ndt.and_local_timezone(offset).unwrap())
                    .timestamp_millis(),
            );
        }
    }
    None
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
            let ts = match parse_at_datetime(at_str) {
                Some(ts) => ts,
                None => return Ok("Error: invalid datetime format for 'at'. Use '2026-05-05T10:30:00' (local time)".to_string()),
            };
            CronSchedule::at(ts)
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

    fn set_context(&self, ctx: &ToolContext) {
        *self.default_channel.lock().unwrap() = ctx.channel.clone();
        *self.default_chat_id.lock().unwrap() = ctx.chat_id.clone();
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
        tool.set_context(&ToolContext { channel: "webui".into(), chat_id: "chat-1".into() });

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
        tool.set_context(&ToolContext { channel: "webui".into(), chat_id: "chat-1".into() });

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
        tool.set_context(&ToolContext { channel: "webui".into(), chat_id: "chat-1".into() });

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

    #[tokio::test]
    async fn test_cron_tool_add_without_context() {
        let tool = make_cron_tool();
        // No set_context called — channel/chat_id will be empty

        let result = tool.execute(serde_json::json!({
            "action": "add",
            "message": "test",
            "every_seconds": 60
        })).await.unwrap();
        assert!(result.contains("no session context"));
    }

    #[tokio::test]
    async fn test_cron_tool_unknown_action() {
        let tool = make_cron_tool();

        let result = tool.execute(serde_json::json!({
            "action": "invalid"
        })).await.unwrap();
        assert!(result.contains("Unknown action"));
    }

    #[tokio::test]
    async fn test_cron_tool_add_cron_expression() {
        let tool = make_cron_tool();
        tool.set_context(&ToolContext { channel: "webui".into(), chat_id: "chat-1".into() });

        let result = tool.execute(serde_json::json!({
            "action": "add",
            "name": "cron job",
            "message": "run at 9am",
            "cron_expr": "0 0 9 * * *"
        })).await.unwrap();
        assert!(result.contains("Created job"));

        let list = tool.execute(serde_json::json!({
            "action": "list"
        })).await.unwrap();
        assert!(list.contains("cron job"));
        assert!(list.contains("cron:"));
    }

    #[tokio::test]
    async fn test_cron_tool_add_at_schedule() {
        let tool = make_cron_tool();
        tool.set_context(&ToolContext { channel: "webui".into(), chat_id: "chat-1".into() });

        let result = tool.execute(serde_json::json!({
            "action": "add",
            "name": "one shot",
            "message": "once",
            "at": "2030-01-01T00:00:00Z"
        })).await.unwrap();
        assert!(result.contains("Created job"));

        // At-schedule jobs have delete_after_run=true
        let jobs = tool.cron_service.list_jobs();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].delete_after_run);
    }

    #[tokio::test]
    async fn test_cron_tool_add_at_invalid_date() {
        let tool = make_cron_tool();
        tool.set_context(&ToolContext { channel: "webui".into(), chat_id: "chat-1".into() });

        let result = tool.execute(serde_json::json!({
            "action": "add",
            "message": "bad date",
            "at": "not-a-date"
        })).await.unwrap();
        assert!(result.contains("invalid datetime format"));
    }

    #[tokio::test]
    async fn test_cron_tool_add_no_schedule_type() {
        let tool = make_cron_tool();
        tool.set_context(&ToolContext { channel: "webui".into(), chat_id: "chat-1".into() });

        let result = tool.execute(serde_json::json!({
            "action": "add",
            "message": "no timing",
            "name": "no timing"
        })).await.unwrap();
        assert!(result.contains("either every_seconds, cron_expr, or at"));
    }

    #[tokio::test]
    async fn test_cron_tool_remove_missing_job_id() {
        let tool = make_cron_tool();

        let result = tool.execute(serde_json::json!({
            "action": "remove"
        })).await.unwrap();
        assert!(result.contains("job_id is required"));
    }

    #[tokio::test]
    async fn test_cron_tool_no_action() {
        let tool = make_cron_tool();

        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(result.contains("Unknown action"));
    }

    #[test]
    fn test_format_schedule_every() {
        assert_eq!(format_schedule(&CronSchedule::every(60_000)), "every 1m");
        assert_eq!(format_schedule(&CronSchedule::every(3_600_000)), "every 1h");
        assert_eq!(format_schedule(&CronSchedule::every(1_000)), "every 1s");
        assert_eq!(format_schedule(&CronSchedule::every(500)), "every 500ms");
    }

    #[tokio::test]
    async fn test_cron_tool_add_job_sets_delivery_fields() {
        let tool = make_cron_tool();
        tool.set_context(&ToolContext { channel: "webui".into(), chat_id: "chat-42".into() });

        let result = tool.execute(serde_json::json!({
            "action": "add",
            "name": "remind me",
            "message": "go to sleep",
            "every_seconds": 60
        })).await.unwrap();
        assert!(result.contains("Created job"));

        // Verify the job payload has delivery fields set correctly
        let jobs = tool.cron_service.list_jobs();
        assert_eq!(jobs.len(), 1);
        let job = &jobs[0];
        assert!(job.payload.deliver, "deliver must be true so results are sent to user");
        assert_eq!(job.payload.channel, Some("webui".to_string()),
            "channel must be set so gateway knows where to route");
        assert_eq!(job.payload.to, Some("chat-42".to_string()),
            "to (chat_id) must be set so gateway knows where to route");
        assert_eq!(job.payload.message, "go to sleep",
            "payload message must contain the original instruction");
        assert_eq!(job.payload.kind, "agent_turn",
            "payload kind must be agent_turn for gateway execution");
    }

    #[tokio::test]
    async fn test_cron_tool_add_at_accepts_naive_local_time() {
        // Regression test: the model may output naive datetime without timezone
        // (e.g. "2030-01-01T10:30:00"). This should be accepted directly as local
        // time, not rejected and force the model to retry.
        let tool = make_cron_tool();
        tool.set_context(&ToolContext { channel: "webui".into(), chat_id: "chat-1".into() });

        let result = tool.execute(serde_json::json!({
            "action": "add",
            "name": "naive test",
            "message": "test",
            "at": "2030-01-01T10:30:00"
        })).await.unwrap();
        assert!(result.contains("Created job"));

        // Also verify UTC (Z) format is accepted and treated as local time
        let tool2 = make_cron_tool();
        tool2.set_context(&ToolContext { channel: "webui".into(), chat_id: "chat-1".into() });

        let result2 = tool2.execute(serde_json::json!({
            "action": "add",
            "name": "utc test",
            "message": "test",
            "at": "2030-01-01T10:30:00Z"
        })).await.unwrap();
        assert!(result2.contains("Created job"));

        // Both jobs should have been created on first call (no error-retry needed)
        assert_eq!(tool.cron_service.list_jobs().len(), 1);
        assert_eq!(tool2.cron_service.list_jobs().len(), 1);
    }
}
