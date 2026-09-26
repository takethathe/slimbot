use serde::{Deserialize, Serialize};

/// The semantic kind of a memory document.
///
/// A memory category addresses a whole document (a markdown file in the
/// current backend). Dynamic names — skill names and custom category names —
/// are passed separately as the `name` argument at call time, so the enum
/// itself stays a clean closed set of unit variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCategory {
    /// Agent behavior rules, guardrails, interaction modes. → SOUL.md
    Soul,
    /// User attributes: identity, preferences, habits, communication style. → USER.md
    User,
    /// Agent working instructions. → AGENTS.md
    Agent,
    /// Tool usage guide. → TOOLS.md
    Tools,
    /// Long-term facts: project context, architecture, strategic decisions. → memory/MEMORY.md
    Core,
    /// Daily log (reserved; no writer yet). → memory/YYYY-MM-DD.md
    Daily,
    /// Reusable workflow template. → `skills/<name>/SKILL.md` (`name` required)
    Skill,
    /// User-defined category. → `memory/<name>.md` (`name` required)
    Custom,
}

impl MemoryCategory {
    /// Every fixed category kind, in canonical order.
    pub const ALL: [MemoryCategory; 8] = [
        MemoryCategory::Soul,
        MemoryCategory::User,
        MemoryCategory::Agent,
        MemoryCategory::Tools,
        MemoryCategory::Core,
        MemoryCategory::Daily,
        MemoryCategory::Skill,
        MemoryCategory::Custom,
    ];

    /// Canonical lowercase string form (matches serde snake_case).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Soul => "soul",
            Self::User => "user",
            Self::Agent => "agent",
            Self::Tools => "tools",
            Self::Core => "core",
            Self::Daily => "daily",
            Self::Skill => "skill",
            Self::Custom => "custom",
        }
    }

    /// Parse a canonical category string. Unknown strings return `None`.
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "soul" => Some(Self::Soul),
            "user" => Some(Self::User),
            "agent" => Some(Self::Agent),
            "tools" => Some(Self::Tools),
            "core" => Some(Self::Core),
            "daily" => Some(Self::Daily),
            "skill" => Some(Self::Skill),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }
}

impl std::fmt::Display for MemoryCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Validate a dynamic memory name (skill name or custom category name).
///
/// Constraint: 1..=64 chars, lowercase ASCII letters, digits, and hyphens.
/// `None` names are valid for fixed categories (tag semantics) but invalid
/// for `Skill` / `Custom`, which require a name to address a document.
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_variants_roundtrip_serde() {
        for cat in MemoryCategory::ALL {
            let json = serde_json::to_string(&cat).unwrap();
            assert_eq!(json, format!("\"{}\"", cat.as_str()));
            let parsed: MemoryCategory = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, cat);
        }
    }

    #[test]
    fn test_serde_accepts_snake_case_strings() {
        let parsed: MemoryCategory = serde_json::from_str("\"skill\"").unwrap();
        assert_eq!(parsed, MemoryCategory::Skill);
        let parsed: MemoryCategory = serde_json::from_str("\"core\"").unwrap();
        assert_eq!(parsed, MemoryCategory::Core);
    }

    #[test]
    fn test_as_str_matches_serde_names() {
        assert_eq!(MemoryCategory::Soul.as_str(), "soul");
        assert_eq!(MemoryCategory::User.as_str(), "user");
        assert_eq!(MemoryCategory::Agent.as_str(), "agent");
        assert_eq!(MemoryCategory::Tools.as_str(), "tools");
        assert_eq!(MemoryCategory::Core.as_str(), "core");
        assert_eq!(MemoryCategory::Daily.as_str(), "daily");
        assert_eq!(MemoryCategory::Skill.as_str(), "skill");
        assert_eq!(MemoryCategory::Custom.as_str(), "custom");
    }

    #[test]
    fn test_from_str_opt_known_and_unknown() {
        assert_eq!(
            MemoryCategory::from_str_opt("soul"),
            Some(MemoryCategory::Soul)
        );
        assert_eq!(
            MemoryCategory::from_str_opt("skill"),
            Some(MemoryCategory::Skill)
        );
        assert_eq!(MemoryCategory::from_str_opt("unknown"), None);
        assert_eq!(MemoryCategory::from_str_opt(""), None);
    }

    #[test]
    fn test_display_matches_as_str() {
        assert_eq!(MemoryCategory::Soul.to_string(), "soul");
        assert_eq!(MemoryCategory::Custom.to_string(), "custom");
    }

    #[test]
    fn test_valid_names_accepted() {
        assert!(is_valid_name("memory"));
        assert!(is_valid_name("my-skill-2"));
        assert!(is_valid_name("a"));
        assert!(is_valid_name("grep-workflow-v3"));
    }

    #[test]
    fn test_invalid_names_rejected() {
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("Uppercase"));
        assert!(!is_valid_name("has space"));
        assert!(!is_valid_name("has_underscore"));
        assert!(!is_valid_name("dot.name"));
        assert!(!is_valid_name(&"x".repeat(65)));
    }

    #[test]
    fn test_max_length_boundary() {
        assert!(is_valid_name(&"x".repeat(64)));
        assert!(!is_valid_name(&"x".repeat(65)));
    }
}
