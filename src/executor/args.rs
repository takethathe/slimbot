//! Caller arguments checked against an executor's `args_schema`.
//!
//! `args_schema` is a data source's output schema turned around (spec A): it
//! describes what a caller must pass in, and it is checked **before the script
//! starts**, so malformed arguments never run anything. The check owns the
//! "which field" answer: a refusal that cannot name the field is not actionable.
//!
//! **The schema is a closed contract.** The supported subset is what an action
//! actually needs — an object schema with typed `properties`, `required`,
//! `items` for arrays, `enum`, and `additionalProperties: false`, checked
//! recursively so an array element or a nested object gets the same treatment as
//! the top level. Everything else is **refused by name** ([`validate`]) instead
//! of being ignored: a keyword the host cannot honour would leave a
//! strong-looking declaration enforcing nothing, and the caller would never learn
//! that the schema is a no-op. `create` / `update` validate for that reason, and
//! [`check`] re-validates because the meta on disk is hand-editable.
//!
//! A schema the host *cannot use at all* (not an object, or declaring a
//! non-object args type) is a failure rather than a silent pass, for the same
//! reason.

use serde_json::Value;

/// Schema keywords the host enforces, for the error text.
const SUPPORTED_KEYWORDS: &[&str] = &[
    "type",
    "required",
    "properties",
    "items",
    "enum",
    "additionalProperties",
];

/// Keywords that cannot make a declaration stronger than it looks: they document
/// a field and change nothing the host checks, so they are accepted and dropped
/// (`schema_fields` never echoes them). Everything else is refused.
const ANNOTATION_KEYWORDS: &[&str] = &["$comment", "default", "description", "examples", "title"];

/// Type names the host knows how to check.
const SUPPORTED_TYPES: &[&str] = &[
    "string", "number", "integer", "boolean", "object", "array", "null",
];

/// Check `args` against `schema`.
///
/// `Ok(())` means the script may start. `Err` names the offending field, in
/// `args.<path>` form, and says what was expected.
pub(crate) fn check(schema: Option<&Value>, args: &Value) -> Result<(), String> {
    validate(schema)?;
    let Some(schema) = schema.filter(|s| !s.is_null()) else {
        return Ok(());
    };
    if !args.is_object() {
        return Err(format!(
            "`args` must be a JSON object, got {}",
            kind_of(args)
        ));
    }
    check_object(schema, args, "args")
}

/// Check that the host can enforce every part of `schema`.
///
/// This is the `create` / `update` gate: a keyword or type name outside the
/// supported subset is refused **by name and path**, so the caller can fix the
/// one thing that is wrong instead of discovering at `run` time that the schema
/// never did anything.
pub(crate) fn validate(schema: Option<&Value>) -> Result<(), String> {
    let Some(schema) = schema.filter(|s| !s.is_null()) else {
        return Ok(());
    };
    if !schema.is_object() {
        return Err("`args_schema` is not a JSON object; fix the schema and retry".to_string());
    }
    if let Some(declared) = schema.get("type")
        && !allows_type(declared, "object")
    {
        return Err(format!(
            "`args_schema.type` must be object, got {}; `args` is always an object",
            describe(declared)
        ));
    }
    validate_node(schema, "args_schema")
}

/// One schema node: every keyword has to be in the supported subset, and every
/// nested node (`properties.*` / `items`) is checked the same way.
fn validate_node(schema: &Value, path: &str) -> Result<(), String> {
    let Some(map) = schema.as_object() else {
        return Err(format!(
            "`{path}` must be a schema object; fix the schema and retry"
        ));
    };
    for property in map.keys() {
        if SUPPORTED_KEYWORDS.contains(&property.as_str())
            || ANNOTATION_KEYWORDS.contains(&property.as_str())
        {
            continue;
        }
        return Err(format!(
            "`{path}` uses `{property}`, which `args_schema` does not support (supported: {}); \
             remove it and retry",
            SUPPORTED_KEYWORDS.join(", ")
        ));
    }
    if let Some(declared) = map.get("type") {
        let names = declared_names(declared);
        if names.is_empty() {
            return Err(format!(
                "`{path}.type` must be a type name or a list of them, got {declared}; fix the \
                 schema and retry"
            ));
        }
        for name in names {
            if !SUPPORTED_TYPES.contains(&name.as_str()) {
                return Err(format!(
                    "`{path}.type` names `{name}`, which this host cannot check (supported: {}); \
                     fix the schema and retry",
                    SUPPORTED_TYPES.join(", ")
                ));
            }
        }
    }
    if let Some(required) = map.get("required")
        && !required
            .as_array()
            .is_some_and(|names| names.iter().all(Value::is_string))
    {
        return Err(format!(
            "`{path}.required` must be a list of field names; fix the schema and retry"
        ));
    }
    if let Some(properties) = map.get("properties") {
        let Some(properties) = properties.as_object() else {
            return Err(format!(
                "`{path}.properties` must be an object of field schemas; fix the schema and retry"
            ));
        };
        for (name, spec) in properties {
            validate_node(spec, &format!("{path}.properties.{name}"))?;
        }
    }
    if let Some(items) = map.get("items") {
        if items.is_array() {
            return Err(format!(
                "`{path}.items` must be one schema, not a list of them (tuple validation is not \
                 supported); fix the schema and retry"
            ));
        }
        validate_node(items, &format!("{path}.items"))?;
    }
    if let Some(additional) = map.get("additionalProperties")
        && !additional.is_boolean()
    {
        return Err(format!(
            "`{path}.additionalProperties` must be `false` (the only value this host enforces) or \
             left out; fix the schema and retry"
        ));
    }
    Ok(())
}

fn check_object(schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    let Some(map) = value.as_object() else {
        return Err(format!("`{path}` must be object, got {}", kind_of(value)));
    };
    let properties = schema.get("properties").and_then(|p| p.as_object());

    if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
        for name in required.iter().filter_map(|n| n.as_str()) {
            if !map.contains_key(name) {
                return Err(format!("`{path}.{name}` is required"));
            }
        }
    }

    if schema.get("additionalProperties") == Some(&Value::Bool(false))
        && let Some(properties) = properties
    {
        for name in map.keys() {
            if !properties.contains_key(name) {
                return Err(format!(
                    "`{path}.{name}` is not allowed by `args_schema` (allowed: {})",
                    properties.keys().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }

    let Some(properties) = properties else {
        return Ok(());
    };
    for (name, spec) in properties {
        let Some(field) = map.get(name) else {
            continue;
        };
        check_value(spec, field, &format!("{path}.{name}"))?;
    }
    Ok(())
}

/// Check one value against one field schema.
///
/// Recursion lives here so an object property, an array element and a nested
/// array element are all enforced by the same rules — the depth the caller
/// writes is the depth the host checks.
fn check_value(spec: &Value, value: &Value, path: &str) -> Result<(), String> {
    if let Some(declared) = spec.get("type")
        && !declared_types(declared).is_empty()
        && !declared_types(declared)
            .iter()
            .any(|expected| matches_type(value, expected))
    {
        return Err(format!(
            "`{path}` must be {}, got {}",
            describe(declared),
            kind_of(value)
        ));
    }
    if let Some(allowed) = spec.get("enum").and_then(|e| e.as_array())
        && !allowed.contains(value)
    {
        return Err(format!(
            "`{path}` must be one of {}, got {value}",
            allowed
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if value.is_object() && spec.get("properties").is_some() {
        check_object(spec, value, path)?;
    }
    if let (Some(items), Value::Array(values)) = (spec.get("items"), value) {
        for (index, item) in values.iter().enumerate() {
            check_value(items, item, &format!("{path}[{index}]"))?;
        }
    }
    Ok(())
}

/// Whether a `type` declaration (string or array of strings) permits `name`.
fn allows_type(declared: &Value, name: &str) -> bool {
    let types = declared_types(declared);
    types.is_empty() || types.contains(&name)
}

/// The type names in a `type` declaration. Empty means "nothing to enforce".
fn declared_types(declared: &Value) -> Vec<&str> {
    match declared {
        Value::String(name) => vec![name.as_str()],
        Value::Array(names) => names.iter().filter_map(|n| n.as_str()).collect(),
        _ => Vec::new(),
    }
}

/// The names in a `type` declaration, trusting only a declaration that is shaped
/// like one.
fn declared_names(declared: &Value) -> Vec<String> {
    match declared {
        Value::String(name) => vec![name.clone()],
        Value::Array(names) if names.iter().all(Value::is_string) => names
            .iter()
            .filter_map(|n| n.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

fn matches_type(value: &Value, expected: &str) -> bool {
    match expected {
        "string" => value.is_string(),
        "number" => value.is_number(),
        // JSON Schema counts 1.0 as an integer; `serde_json` keeps the float.
        "integer" => {
            value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|n| n.fract() == 0.0)
        }
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        // `validate` refuses an unknown type name, so this arm is unreachable
        // through a stored schema; it stays total rather than panicking.
        _ => true,
    }
}

/// The type as it should read in a message: `string`, or `string|array`.
fn describe(declared: &Value) -> String {
    let types = declared_types(declared);
    if types.is_empty() {
        "any".to_string()
    } else {
        types.join("|")
    }
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "required": ["title"],
            "properties": {
                "title": {"type": "string"},
                "state": {"type": "string", "enum": ["open", "closed"]},
                "labels": {"type": ["array", "null"]},
                "count": {"type": "integer"},
                "meta": {
                    "type": "object",
                    "required": ["kind"],
                    "properties": {"kind": {"type": "string"}}
                }
            }
        })
    }

    #[test]
    fn test_no_schema_accepts_any_object() {
        assert!(check(None, &json!({"anything": 1})).is_ok());
        assert!(check(Some(&Value::Null), &json!({})).is_ok());
    }

    #[test]
    fn test_matching_args_pass() {
        let args = json!({
            "title": "a bug",
            "state": "open",
            "labels": ["bug"],
            "count": 3,
            "meta": {"kind": "issue"}
        });
        assert!(check(Some(&schema()), &args).is_ok());
    }

    #[test]
    fn test_missing_required_field_is_named() {
        let err = check(Some(&schema()), &json!({"state": "open"})).unwrap_err();
        assert_eq!(err, "`args.title` is required");
    }

    #[test]
    fn test_wrong_type_names_the_field_and_the_expectation() {
        let err = check(Some(&schema()), &json!({"title": 42})).unwrap_err();
        assert_eq!(err, "`args.title` must be string, got integer");
    }

    #[test]
    fn test_union_type_accepts_either_and_reports_both() {
        let ok = json!({"title": "t", "labels": null});
        assert!(check(Some(&schema()), &ok).is_ok());
        let err = check(Some(&schema()), &json!({"title": "t", "labels": "bug"})).unwrap_err();
        assert_eq!(err, "`args.labels` must be array|null, got string");
    }

    #[test]
    fn test_enum_violation_lists_the_allowed_values() {
        let err = check(Some(&schema()), &json!({"title": "t", "state": "pending"})).unwrap_err();
        assert!(err.starts_with("`args.state` must be one of "), "{err}");
        assert!(err.contains("\"open\""), "{err}");
        assert!(err.contains("pending"), "{err}");
    }

    #[test]
    fn test_integer_rejects_a_fractional_number_but_accepts_a_whole_one() {
        assert!(check(Some(&schema()), &json!({"title": "t", "count": 3})).is_ok());
        assert!(check(Some(&schema()), &json!({"title": "t", "count": 3.0})).is_ok());
        let err = check(Some(&schema()), &json!({"title": "t", "count": 3.5})).unwrap_err();
        assert_eq!(err, "`args.count` must be integer, got number");
    }

    #[test]
    fn test_nested_object_is_checked_with_a_dotted_path() {
        let err = check(Some(&schema()), &json!({"title": "t", "meta": {}})).unwrap_err();
        assert_eq!(err, "`args.meta.kind` is required");
        let err = check(Some(&schema()), &json!({"title": "t", "meta": {"kind": 7}})).unwrap_err();
        assert_eq!(err, "`args.meta.kind` must be string, got integer");
    }

    #[test]
    fn test_additional_properties_false_rejects_an_unknown_key() {
        let strict = json!({
            "type": "object",
            "properties": {"title": {"type": "string"}},
            "additionalProperties": false
        });
        let err = check(Some(&strict), &json!({"title": "t", "extra": 1})).unwrap_err();
        assert!(err.contains("`args.extra` is not allowed"), "{err}");
        assert!(err.contains("title"), "{err}");
        assert!(check(Some(&strict), &json!({"title": "t"})).is_ok());
    }

    #[test]
    fn test_a_schema_the_host_cannot_use_is_refused_not_ignored() {
        let err = check(Some(&json!("nonsense")), &json!({})).unwrap_err();
        assert!(err.contains("not a JSON object"), "{err}");
        let err = check(Some(&json!({"type": "string"})), &json!({})).unwrap_err();
        assert!(err.contains("always an object"), "{err}");
    }

    #[test]
    fn test_non_object_args_are_refused() {
        let err = check(Some(&schema()), &json!("title")).unwrap_err();
        assert_eq!(err, "`args` must be a JSON object, got string");
    }

    #[test]
    fn test_a_supported_schema_with_array_items_passes() {
        let ok = json!({
            "type": "object",
            "required": ["labels"],
            "properties": {
                "labels": {"type": "array", "items": {"type": "string"}},
                "meta": {
                    "type": "object",
                    "properties": {"tags": {"type": "array", "items": {"type": "integer"}}}
                }
            }
        });
        assert!(validate(Some(&ok)).is_ok(), "{:?}", validate(Some(&ok)));
        assert!(
            check(
                Some(&ok),
                &json!({"labels": ["bug"], "meta": {"tags": [1, 2]}})
            )
            .is_ok()
        );
    }

    #[test]
    fn test_array_items_are_checked_element_by_element() {
        let schema = json!({
            "type": "object",
            "properties": {"labels": {"type": "array", "items": {"type": "string"}}}
        });
        assert!(check(Some(&schema), &json!({"labels": ["bug", "ui"]})).is_ok());
        let err = check(Some(&schema), &json!({"labels": ["bug", 7]})).unwrap_err();
        assert_eq!(err, "`args.labels[1]` must be string, got integer");
    }

    #[test]
    fn test_nested_array_items_are_checked_at_every_depth() {
        // An array of objects borrows the object rules, and an array of arrays
        // borrows the item rules — the same treatment at every depth.
        let schema = json!({
            "type": "object",
            "properties": {
                "rows": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["name"],
                        "properties": {
                            "name": {"type": "string"},
                            "ids": {"type": "array", "items": {"type": "integer"}}
                        }
                    }
                }
            }
        });
        let ok = json!({"rows": [{"name": "a", "ids": [1]}]});
        assert!(check(Some(&schema), &ok).is_ok());
        let err = check(Some(&schema), &json!({"rows": [{"ids": [1]}]})).unwrap_err();
        assert_eq!(err, "`args.rows[0].name` is required");
        let err = check(
            Some(&schema),
            &json!({"rows": [{"name": "a", "ids": [1.5]}]}),
        )
        .unwrap_err();
        assert_eq!(err, "`args.rows[0].ids[0]` must be integer, got number");
    }

    #[test]
    fn test_every_unsupported_keyword_is_refused_by_name() {
        // A schema the host cannot honour must not pretend it can: a silent
        // no-op would leave a strong-looking declaration enforcing nothing.
        for keyword in [
            "anyOf",
            "oneOf",
            "allOf",
            "not",
            "$ref",
            "definitions",
            "pattern",
            "patternProperties",
            "format",
            "minimum",
            "$schema",
            "minItems",
            "uniqueItems",
        ] {
            let schema = json!({
                "type": "object",
                "properties": {"title": {"type": "string", keyword: 1}},
            });
            let err = validate(Some(&schema)).unwrap_err();
            assert!(err.contains(keyword), "{keyword} not named: {err}");
            assert!(err.contains("args_schema.properties.title"), "{err}");
            // The run path refuses it too: a hand-edited meta cannot smuggle a
            // keyword back in behind the create/update check.
            let at_run = check(Some(&schema), &json!({"title": "t"})).unwrap_err();
            assert!(at_run.contains(keyword), "{keyword} not named: {at_run}");
        }
    }

    #[test]
    fn test_an_unsupported_type_name_is_refused_by_name() {
        let schema = json!({"type": "object", "properties": {"at": {"type": "timestamp"}}});
        for err in [
            validate(Some(&schema)).unwrap_err(),
            check(Some(&schema), &json!({"at": "yesterday"})).unwrap_err(),
        ] {
            assert!(err.contains("timestamp"), "{err}");
            assert!(err.contains("args_schema.properties.at"), "{err}");
        }
    }

    #[test]
    fn test_additional_properties_must_be_a_boolean() {
        // Only `false` is enforced, so a schema-valued declaration is refused
        // rather than read as "anything goes".
        let schema = json!({"type": "object", "additionalProperties": {"type": "string"}});
        let err = validate(Some(&schema)).unwrap_err();
        assert!(err.contains("additionalProperties"), "{err}");
        assert!(
            validate(Some(
                &json!({"type": "object", "additionalProperties": false})
            ))
            .is_ok()
        );
    }

    #[test]
    fn test_a_schema_the_host_cannot_use_is_refused_at_the_top_level() {
        assert!(validate(Some(&json!("nonsense"))).is_err());
        assert!(validate(Some(&json!({"type": "string"}))).is_err());
        assert!(validate(Some(&json!({"type": "object", "required": "title"}))).is_err());
        assert!(validate(Some(&json!({"type": "object", "properties": []}))).is_err());
        assert!(
            validate(Some(
                &json!({"type": "object", "items": [{"type": "string"}]})
            ))
            .is_err()
        );
    }
}
