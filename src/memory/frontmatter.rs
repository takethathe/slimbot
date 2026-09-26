//! Skill document frontmatter parsing.
//!
//! Skill documents (`skills/<name>/SKILL.md`) may begin with a YAML
//! frontmatter fence carrying `name` / `description` / `always`. The
//! available-skills list in the system prompt is built from this metadata, and
//! the derived entry index strips the fence before indexing (frontmatter is
//! metadata, not searchable content). Both consumers share this parser so the
//! split is defined in exactly one place.

/// Parsed YAML frontmatter from a skill file.
#[derive(Debug, Clone)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub always: bool,
    pub content: String,
}

/// Parse YAML frontmatter from a skill file.
/// Returns None if the file has no valid `---` delimiters.
pub fn parse_skill_frontmatter(content: &str) -> Option<SkillMeta> {
    if !content.starts_with("---\n") {
        return None;
    }
    let rest = &content[4..];
    let end = rest.find("\n---\n")?;
    let front = &rest[..end];
    let body = &rest[end + 5..];

    let mut name = String::new();
    let mut description = String::new();
    let mut always = false;

    for line in front.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("name:") {
            name = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("description:") {
            description = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("always:") {
            always = rest.trim() == "true";
        }
    }

    Some(SkillMeta {
        name: if name.is_empty() {
            "unknown".to_string()
        } else {
            name
        },
        description,
        always,
        content: body.trim().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frontmatter_full() {
        let input = r#"---
name: memory
description: Two-layer memory system.
always: true
---

# Memory content here
"#;
        let meta = parse_skill_frontmatter(input).unwrap();
        assert_eq!(meta.name, "memory");
        assert_eq!(meta.description, "Two-layer memory system.");
        assert!(meta.always);
        assert_eq!(meta.content, "# Memory content here");
    }

    #[test]
    fn test_parse_frontmatter_no_always() {
        let input = r#"---
name: custom
description: A custom skill.
---

Skill body.
"#;
        let meta = parse_skill_frontmatter(input).unwrap();
        assert_eq!(meta.name, "custom");
        assert_eq!(meta.description, "A custom skill.");
        assert!(!meta.always);
        assert_eq!(meta.content, "Skill body.");
    }

    #[test]
    fn test_parse_no_frontmatter_returns_none() {
        assert!(parse_skill_frontmatter("# Just a markdown file").is_none());
        assert!(parse_skill_frontmatter("").is_none());
    }

    #[test]
    fn test_parse_no_closing_delimiter_returns_none() {
        assert!(parse_skill_frontmatter("---\nname: test\nno closing").is_none());
    }

    #[test]
    fn test_parse_skill_frontmatter_empty_name_falls_back() {
        let input = r#"---
name:
description: A skill.
---

Body
"#;
        let meta = parse_skill_frontmatter(input).unwrap();
        assert_eq!(meta.name, "unknown");
        assert_eq!(meta.content, "Body");
    }

    #[test]
    fn test_parse_skill_frontmatter_empty_content_skipped() {
        let input = "---\nname: test\ndescription: desc\n---\n";
        let meta = parse_skill_frontmatter(input).unwrap();
        assert_eq!(meta.content, "");
    }

    #[test]
    fn test_parse_skill_frontmatter_with_always_false() {
        let input = r#"---
name: optional-skill
description: An optional skill.
always: false
---

Skill content here.
"#;
        let meta = parse_skill_frontmatter(input).unwrap();
        assert_eq!(meta.name, "optional-skill");
        assert!(!meta.always);
        assert_eq!(meta.content, "Skill content here.");
    }

    #[test]
    fn test_parse_skill_frontmatter_with_always_true() {
        let input = r#"---
name: required-skill
description: A required skill.
always: true
---

Required skill content.
"#;
        let meta = parse_skill_frontmatter(input).unwrap();
        assert_eq!(meta.name, "required-skill");
        assert!(meta.always);
        assert_eq!(meta.content, "Required skill content.");
    }

    #[test]
    fn test_parse_skill_frontmatter_multiline_content() {
        let input = r#"---
name: multi
description: Multi-line content.
---

# Heading

Paragraph 1.

Paragraph 2.

```rust
let x = 42;
```
"#;
        let meta = parse_skill_frontmatter(input).unwrap();
        assert!(meta.content.contains("# Heading"));
        assert!(meta.content.contains("Paragraph 1."));
        assert!(meta.content.contains("Paragraph 2."));
        assert!(meta.content.contains("```rust"));
    }
}
