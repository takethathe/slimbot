//! Memory-backed tools for the Dream sandbox.
//!
//! Dream edits long-term memory exclusively through the [`MemoryService`]
//! facade: `memory_read` / `memory_write` / `memory_edit` address documents by
//! category (`soul` / `user` / `core` / `skill`), with skill names passed via
//! the optional `name` argument. No filesystem tool is exposed, so the Dream
//! sandbox can only touch memory documents.

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::memory::SharedMemory;
use crate::memory::category::is_valid_name;
use crate::{EditOutcome, MemoryCategory};

/// Categories the Dream sandbox is allowed to touch.
pub const DREAM_CATEGORIES: [MemoryCategory; 4] = [
    MemoryCategory::Soul,
    MemoryCategory::User,
    MemoryCategory::Core,
    MemoryCategory::Skill,
];

/// JSON-schema fragment listing the allowed categories.
fn category_schema() -> Value {
    json!({
        "type": "string",
        "enum": DREAM_CATEGORIES
            .iter()
            .map(|c| c.as_str())
            .collect::<Vec<_>>(),
        "description": "Memory category to operate on"
    })
}

/// Properties shared by every memory tool: `category` and the optional `name`.
fn document_properties() -> serde_json::Map<String, Value> {
    let mut properties = serde_json::Map::new();
    properties.insert("category".to_string(), category_schema());
    properties.insert(
        "name".to_string(),
        json!({
            "type": "string",
            "description": "Skill name; required when category is 'skill'"
        }),
    );
    properties
}

/// Build a tool parameter schema from the shared document properties plus
/// `extra`, with `required` naming the mandatory arguments.
fn document_parameters(extra: &[(&str, Value)], required: &[&str]) -> Value {
    let mut properties = document_properties();
    for (key, schema) in extra {
        properties.insert((*key).to_string(), schema.clone());
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required
    })
}

/// Parse and validate the `category` argument against the Dream whitelist.
fn parse_category(args: &Value) -> Result<MemoryCategory> {
    let raw = args["category"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: category"))?;
    match MemoryCategory::from_str_opt(raw) {
        Some(cat) if DREAM_CATEGORIES.contains(&cat) => Ok(cat),
        _ => bail!("unknown category '{raw}'; allowed: soul, user, core, skill"),
    }
}

/// Resolve the optional `name` argument. `skill` requires a valid name and
/// permits brand-new names; fixed categories ignore `name` (tag semantics).
fn parse_name(args: &Value, category: &MemoryCategory) -> Result<Option<String>> {
    match category {
        MemoryCategory::Skill => match args["name"].as_str() {
            Some(n) if is_valid_name(n) => Ok(Some(n.to_string())),
            Some(n) => bail!("invalid skill name '{n}': use 1..=64 chars of [a-z0-9-]"),
            None => bail!("category 'skill' requires a name"),
        },
        _ => Ok(None),
    }
}

/// Human-readable label for the document a call targets.
fn target_label(category: &MemoryCategory, name: Option<&str>) -> String {
    match name {
        Some(n) => format!("{category}:{n}"),
        None => category.to_string(),
    }
}

/// "existing skills" hint injected into every tool description, so Dream can
/// merge into an existing skill instead of creating a duplicate.
fn skills_hint(skill_names: &[String]) -> String {
    if skill_names.is_empty() {
        "Existing skills: (none).".to_string()
    } else {
        format!("Existing skills: {}.", skill_names.join(", "))
    }
}

/// `memory_read` — read one memory document.
pub struct MemoryReadTool {
    memory: SharedMemory,
    description: String,
}

impl MemoryReadTool {
    pub fn new(memory: SharedMemory, skill_names: &[String]) -> Self {
        Self {
            memory,
            description: format!(
                "Read a memory document by category (soul, user, core, skill). \
                 For category 'skill', pass the skill name via `name`. {}",
                skills_hint(skill_names)
            ),
        }
    }
}

#[async_trait]
impl crate::tool::Tool for MemoryReadTool {
    fn name(&self) -> &str {
        "memory_read"
    }

    /// Risk class (ADR 0011): Reads one memory document.
    fn risk_level(&self) -> crate::approval::RiskLevel {
        crate::approval::RiskLevel::ReadOnly
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        document_parameters(&[], &["category"])
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let category = parse_category(&args)?;
        let name = parse_name(&args, &category)?;
        let content = self
            .memory
            .lock()
            .await
            .get(&category, name.as_deref())
            .await?;
        match content {
            Some(c) if !c.trim().is_empty() => Ok(c),
            _ => Ok(format!(
                "No content in {}.",
                target_label(&category, name.as_deref())
            )),
        }
    }
}

/// `memory_write` — create or overwrite one memory document.
pub struct MemoryWriteTool {
    memory: SharedMemory,
    description: String,
}

impl MemoryWriteTool {
    pub fn new(memory: SharedMemory, skill_names: &[String]) -> Self {
        Self {
            memory,
            description: format!(
                "Create or overwrite a memory document by category \
                 (soul, user, core, skill). For category 'skill', pass the skill \
                 name via `name`; creating a new skill is allowed. {}",
                skills_hint(skill_names)
            ),
        }
    }
}

#[async_trait]
impl crate::tool::Tool for MemoryWriteTool {
    fn name(&self) -> &str {
        "memory_write"
    }

    /// Risk class (ADR 0011): Writes one memory document inside the workspace.
    fn risk_level(&self) -> crate::approval::RiskLevel {
        crate::approval::RiskLevel::WorkspaceWrite
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        document_parameters(
            &[(
                "content",
                json!({
                    "type": "string",
                    "description": "Full document content to write"
                }),
            )],
            &["category", "content"],
        )
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let category = parse_category(&args)?;
        let name = parse_name(&args, &category)?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: content"))?;

        let label = target_label(&category, name.as_deref());
        let existed = {
            let ms = self.memory.lock().await;
            ms.get(&category, name.as_deref())
                .await?
                .is_some_and(|c| !c.trim().is_empty())
        };
        self.memory
            .lock()
            .await
            .set(&category, name.as_deref(), content)
            .await?;

        let verb = if existed { "Updated" } else { "Created" };
        Ok(format!("{verb} {label}."))
    }
}

/// `memory_edit` — replace an exact old_string with new_string.
pub struct MemoryEditTool {
    memory: SharedMemory,
    description: String,
}

impl MemoryEditTool {
    pub fn new(memory: SharedMemory, skill_names: &[String]) -> Self {
        Self {
            memory,
            description: format!(
                "Edit a memory document by replacing an exact old_string with \
                 new_string (old_string must appear exactly once). Categories: \
                 soul, user, core, skill. For category 'skill', pass the skill \
                 name via `name`. {}",
                skills_hint(skill_names)
            ),
        }
    }
}

#[async_trait]
impl crate::tool::Tool for MemoryEditTool {
    fn name(&self) -> &str {
        "memory_edit"
    }

    /// Risk class (ADR 0011): Edits one memory document inside the workspace.
    fn risk_level(&self) -> crate::approval::RiskLevel {
        crate::approval::RiskLevel::WorkspaceWrite
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        document_parameters(
            &[
                (
                    "old_string",
                    json!({
                        "type": "string",
                        "description": "Exact text to replace; must appear exactly once"
                    }),
                ),
                (
                    "new_string",
                    json!({
                        "type": "string",
                        "description": "Replacement text"
                    }),
                ),
            ],
            &["category", "old_string", "new_string"],
        )
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let category = parse_category(&args)?;
        let name = parse_name(&args, &category)?;
        let old = args["old_string"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: old_string"))?;
        let new = args["new_string"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: new_string"))?;

        let label = target_label(&category, name.as_deref());
        let outcome = self
            .memory
            .lock()
            .await
            .edit(&category, name.as_deref(), old, new)
            .await?;

        Ok(match outcome {
            EditOutcome::Replaced => format!("Edited {label}."),
            EditOutcome::NotFound => {
                format!("old_string not found in {label}; no changes made.")
            }
            EditOutcome::Ambiguous(n) => {
                format!("old_string appears {n} times in {label}; must be unique, no changes made.")
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryService;
    use crate::tool::Tool;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::Mutex;

    async fn make_memory() -> (SharedMemory, TempDir) {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let memory = Arc::new(Mutex::new(MemoryService::new(&workspace)));
        memory.lock().await.init().unwrap();
        (memory, tmp)
    }

    fn args(value: Value) -> Value {
        value
    }

    // -- category / name parsing -------------------------------------------

    #[test]
    fn test_parse_category_accepts_whitelist() {
        for raw in ["soul", "user", "core", "skill"] {
            let cat = parse_category(&args(json!({ "category": raw }))).unwrap();
            assert_eq!(cat.as_str(), raw);
        }
    }

    #[test]
    fn test_parse_category_rejects_non_whitelist() {
        for raw in ["agent", "tools", "daily", "custom", "nope", ""] {
            assert!(
                parse_category(&args(json!({ "category": raw }))).is_err(),
                "category '{raw}' must be rejected"
            );
        }
    }

    #[test]
    fn test_parse_category_requires_argument() {
        assert!(parse_category(&args(json!({}))).is_err());
    }

    #[test]
    fn test_parse_name_skill_requires_name() {
        let err = parse_name(&args(json!({})), &MemoryCategory::Skill).unwrap_err();
        assert!(err.to_string().contains("requires a name"));
    }

    #[test]
    fn test_parse_name_skill_rejects_invalid_name() {
        for bad in ["Uppercase", "has space", "has_underscore", "dot.name", ""] {
            let err =
                parse_name(&args(json!({ "name": bad })), &MemoryCategory::Skill).unwrap_err();
            assert!(
                err.to_string().contains("invalid skill name"),
                "name '{bad}' must be rejected"
            );
        }
        let too_long = "x".repeat(65);
        assert!(parse_name(&args(json!({ "name": too_long })), &MemoryCategory::Skill).is_err());
    }

    #[test]
    fn test_parse_name_skill_allows_new_name() {
        let name = parse_name(
            &args(json!({ "name": "brand-new" })),
            &MemoryCategory::Skill,
        );
        assert_eq!(name.unwrap().as_deref(), Some("brand-new"));
    }

    #[test]
    fn test_parse_name_fixed_category_ignores_name() {
        let name = parse_name(&args(json!({ "name": "ignored" })), &MemoryCategory::Core);
        assert_eq!(name.unwrap(), None);
    }

    // -- memory_read --------------------------------------------------------

    #[tokio::test]
    async fn test_read_returns_content() {
        let (memory, _tmp) = make_memory().await;
        memory
            .lock()
            .await
            .set(&MemoryCategory::User, None, "likes tea")
            .await
            .unwrap();

        let tool = MemoryReadTool::new(memory, &[]);
        let out = tool
            .execute(args(json!({ "category": "user" })))
            .await
            .unwrap();
        assert_eq!(out, "likes tea");
    }

    #[tokio::test]
    async fn test_read_missing_returns_message() {
        let (memory, _tmp) = make_memory().await;
        let tool = MemoryReadTool::new(memory, &[]);
        let out = tool
            .execute(args(json!({ "category": "core" })))
            .await
            .unwrap();
        assert!(out.contains("No content in core"));
    }

    #[tokio::test]
    async fn test_read_skill_requires_name() {
        let (memory, _tmp) = make_memory().await;
        let tool = MemoryReadTool::new(memory, &[]);
        let err = tool
            .execute(args(json!({ "category": "skill" })))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("requires a name"));
    }

    #[tokio::test]
    async fn test_read_rejects_non_whitelist_category() {
        let (memory, _tmp) = make_memory().await;
        let tool = MemoryReadTool::new(memory, &[]);
        let err = tool
            .execute(args(json!({ "category": "agent" })))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown category"));
    }

    #[tokio::test]
    async fn test_read_existing_skill() {
        let (memory, _tmp) = make_memory().await;
        memory
            .lock()
            .await
            .set(&MemoryCategory::Skill, Some("grep-workflow"), "step 1")
            .await
            .unwrap();

        let tool = MemoryReadTool::new(memory, &["grep-workflow".to_string()]);
        let out = tool
            .execute(args(
                json!({ "category": "skill", "name": "grep-workflow" }),
            ))
            .await
            .unwrap();
        assert_eq!(out, "step 1");
    }

    // -- memory_write -------------------------------------------------------

    #[tokio::test]
    async fn test_write_creates_document() {
        let (memory, _tmp) = make_memory().await;
        let tool = MemoryWriteTool::new(memory.clone(), &[]);
        let out = tool
            .execute(args(json!({ "category": "core", "content": "hello" })))
            .await
            .unwrap();
        assert!(out.starts_with("Created core"));

        let stored = memory
            .lock()
            .await
            .get(&MemoryCategory::Core, None)
            .await
            .unwrap();
        assert_eq!(stored.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn test_write_overwrites_document() {
        let (memory, _tmp) = make_memory().await;
        memory
            .lock()
            .await
            .set(&MemoryCategory::Core, None, "old")
            .await
            .unwrap();

        let tool = MemoryWriteTool::new(memory.clone(), &[]);
        let out = tool
            .execute(args(json!({ "category": "core", "content": "new" })))
            .await
            .unwrap();
        assert!(out.starts_with("Updated core"));

        let stored = memory
            .lock()
            .await
            .get(&MemoryCategory::Core, None)
            .await
            .unwrap();
        assert_eq!(stored.as_deref(), Some("new"));
    }

    #[tokio::test]
    async fn test_write_new_skill_is_allowed() {
        let (memory, _tmp) = make_memory().await;
        let tool = MemoryWriteTool::new(memory.clone(), &[]);
        let out = tool
            .execute(args(json!({
                "category": "skill",
                "name": "new-skill",
                "content": "---\nname: new-skill\n---\nbody"
            })))
            .await
            .unwrap();
        assert!(out.starts_with("Created skill:new-skill"));

        let stored = memory
            .lock()
            .await
            .get(&MemoryCategory::Skill, Some("new-skill"))
            .await
            .unwrap();
        assert!(stored.is_some());
    }

    #[tokio::test]
    async fn test_write_skill_missing_name_errors() {
        let (memory, _tmp) = make_memory().await;
        let tool = MemoryWriteTool::new(memory, &[]);
        let err = tool
            .execute(args(json!({ "category": "skill", "content": "x" })))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("requires a name"));
    }

    #[tokio::test]
    async fn test_write_invalid_skill_name_errors() {
        let (memory, _tmp) = make_memory().await;
        let tool = MemoryWriteTool::new(memory, &[]);
        let err = tool
            .execute(args(json!({
                "category": "skill",
                "name": "Bad Name",
                "content": "x"
            })))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid skill name"));
    }

    #[tokio::test]
    async fn test_write_requires_content() {
        let (memory, _tmp) = make_memory().await;
        let tool = MemoryWriteTool::new(memory, &[]);
        let err = tool
            .execute(args(json!({ "category": "user" })))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("content"));
    }

    // -- memory_edit --------------------------------------------------------

    #[tokio::test]
    async fn test_edit_replaced_maps_to_success() {
        let (memory, _tmp) = make_memory().await;
        memory
            .lock()
            .await
            .set(&MemoryCategory::Soul, None, "be concise")
            .await
            .unwrap();

        let tool = MemoryEditTool::new(memory.clone(), &[]);
        let out = tool
            .execute(args(json!({
                "category": "soul",
                "old_string": "be concise",
                "new_string": "be very concise"
            })))
            .await
            .unwrap();
        assert_eq!(out, "Edited soul.");

        let stored = memory
            .lock()
            .await
            .get(&MemoryCategory::Soul, None)
            .await
            .unwrap();
        assert_eq!(stored.as_deref(), Some("be very concise"));
    }

    #[tokio::test]
    async fn test_edit_not_found_maps_to_message() {
        let (memory, _tmp) = make_memory().await;
        memory
            .lock()
            .await
            .set(&MemoryCategory::Soul, None, "be concise")
            .await
            .unwrap();

        let tool = MemoryEditTool::new(memory, &[]);
        let out = tool
            .execute(args(json!({
                "category": "soul",
                "old_string": "absent",
                "new_string": "x"
            })))
            .await
            .unwrap();
        assert!(out.contains("old_string not found in soul"));
    }

    #[tokio::test]
    async fn test_edit_ambiguous_maps_to_message() {
        let (memory, _tmp) = make_memory().await;
        memory
            .lock()
            .await
            .set(&MemoryCategory::User, None, "tea tea")
            .await
            .unwrap();

        let tool = MemoryEditTool::new(memory, &[]);
        let out = tool
            .execute(args(json!({
                "category": "user",
                "old_string": "tea",
                "new_string": "coffee"
            })))
            .await
            .unwrap();
        assert!(out.contains("appears 2 times in user"));
    }

    #[tokio::test]
    async fn test_edit_skill_targets_name() {
        let (memory, _tmp) = make_memory().await;
        memory
            .lock()
            .await
            .set(&MemoryCategory::Skill, Some("s1"), "old step")
            .await
            .unwrap();

        let tool = MemoryEditTool::new(memory, &["s1".to_string()]);
        let out = tool
            .execute(args(json!({
                "category": "skill",
                "name": "s1",
                "old_string": "old step",
                "new_string": "new step"
            })))
            .await
            .unwrap();
        assert_eq!(out, "Edited skill:s1.");
    }

    // -- descriptions -------------------------------------------------------

    #[tokio::test]
    async fn test_description_lists_existing_skills() {
        let (memory, _tmp) = make_memory().await;
        let skills = vec!["alpha".to_string(), "beta".to_string()];

        for tool_desc in [
            MemoryReadTool::new(memory.clone(), &skills)
                .description()
                .to_string(),
            MemoryWriteTool::new(memory.clone(), &skills)
                .description()
                .to_string(),
            MemoryEditTool::new(memory.clone(), &skills)
                .description()
                .to_string(),
        ] {
            assert!(tool_desc.contains("Existing skills: alpha, beta."));
        }
    }

    #[tokio::test]
    async fn test_description_with_no_skills() {
        let (memory, _tmp) = make_memory().await;
        let desc = MemoryReadTool::new(memory, &[]).description().to_string();
        assert!(desc.contains("Existing skills: (none)."));
    }

    #[tokio::test]
    async fn test_tool_names_are_memory_tools() {
        // The three tool names are the only memory tools Dream exposes.
        let (memory, _tmp) = make_memory().await;
        assert_eq!(
            MemoryReadTool::new(memory.clone(), &[]).name(),
            "memory_read"
        );
        assert_eq!(
            MemoryWriteTool::new(memory.clone(), &[]).name(),
            "memory_write"
        );
        assert_eq!(MemoryEditTool::new(memory, &[]).name(), "memory_edit");
    }

    #[tokio::test]
    async fn test_parameters_expose_whitelist_enum() {
        let (memory, _tmp) = make_memory().await;
        let tool = MemoryReadTool::new(memory, &[]);
        let params = tool.parameters();
        let allowed: Vec<&str> = params["properties"]["category"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(allowed, vec!["soul", "user", "core", "skill"]);
    }
}
