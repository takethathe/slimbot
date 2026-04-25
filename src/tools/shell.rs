use anyhow::Result;
use async_trait::async_trait;

use crate::tool::Tool;

/// Tool that executes shell commands via `sh -c`.
pub struct ShellTool {
    timeout_secs: u64,
}

impl ShellTool {
    pub fn new(timeout_secs: u64) -> Self {
        Self { timeout_secs }
    }
}

impl Default for ShellTool {
    fn default() -> Self {
        Self { timeout_secs: 30 }
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command. Returns stdout and stderr. Use with caution."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
        let command = args["command"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: command"))?;

        let timeout = tokio::time::Duration::from_secs(self.timeout_secs);
        let output = tokio::time::timeout(timeout, async {
            tokio::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .output()
                .await
        }).await;

        match output {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let mut result = String::new();
                if !stdout.is_empty() {
                    result.push_str(&format!("stdout:\n{}", stdout.trim_end()));
                }
                if !stderr.is_empty() {
                    if !result.is_empty() { result.push_str("\n---\n"); }
                    result.push_str(&format!("stderr:\n{}", stderr.trim_end()));
                }
                if result.is_empty() {
                    result = "(no output)".to_string();
                }
                if !output.status.success() {
                    result.push_str(&format!("\n\nExit code: {}", output.status.code().unwrap_or(-1)));
                }
                Ok(result)
            }
            Ok(Err(e)) => Err(anyhow::anyhow!("Failed to execute command: {}", e)),
            Err(_) => Err(anyhow::anyhow!("Command timed out after {} seconds", self.timeout_secs)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_shell_echo() {
        let tool = ShellTool::default();
        let result = tool.execute(serde_json::json!({"command": "echo hello"})).await.unwrap();
        assert!(result.contains("hello"));
    }

    #[tokio::test]
    async fn test_shell_missing_command() {
        let tool = ShellTool::default();
        let result = tool.execute(serde_json::json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_shell_timeout() {
        let tool = ShellTool::new(1);
        let result = tool.execute(serde_json::json!({"command": "sleep 10"})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }
}
