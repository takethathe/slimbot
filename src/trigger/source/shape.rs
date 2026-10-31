//! Shape: what a source's data looks like without showing any of it.
//!
//! `trigger_source list` is a debugging surface the main loop reads through the
//! trigger agent. Echoing a real sample there would let external content slip
//! into a model context through a side door, so the listing reports field names
//! and types only — enough to write a correct condition script, useless as
//! content.

use serde_json::{Map, Value};

use super::normalize::Entry;

/// Largest number of fields reported per item. A pathological object must not
/// turn one listing into a wall of text.
const MAX_FIELDS: usize = 64;

/// Type of one JSON value, with array element types spelled out.
pub fn type_name(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(_) => "boolean".to_string(),
        Value::Number(_) => "number".to_string(),
        Value::String(_) => "string".to_string(),
        Value::Array(items) => {
            let mut element_types: Vec<String> = items.iter().map(type_name).collect();
            element_types.sort();
            element_types.dedup();
            if element_types.is_empty() {
                "array".to_string()
            } else {
                format!("array<{}>", element_types.join("|"))
            }
        }
        Value::Object(_) => "object".to_string(),
    }
}

/// Merge the observed shapes into `{field: type}`.
///
/// A field that appears with more than one type is reported as `a|b`: the
/// model needs to know it may need a guard, not which item won.
pub fn fields(entries: &[Entry]) -> Map<String, Value> {
    let mut merged: Vec<(String, Vec<String>)> = Vec::new();
    for entry in entries {
        if let Value::Object(map) = &entry.item {
            for (key, value) in map {
                let name = type_name(value);
                match merged.iter_mut().find(|(k, _)| k == key) {
                    Some((_, types)) => types.push(name),
                    None => merged.push((key.clone(), vec![name])),
                }
            }
        }
    }
    merged.sort_by(|a, b| a.0.cmp(&b.0));
    merged.truncate(MAX_FIELDS);

    let mut out = Map::new();
    for (key, mut types) in merged {
        types.sort();
        types.dedup();
        out.insert(key, Value::String(types.join("|")));
    }
    out
}

/// The `shape` value returned by `trigger_source list`.
///
/// `declared` is echoed when the source carries an `output_schema` (it is a
/// schema, not content); `observed` always describes the live snapshot.
pub fn report(
    declared: Option<&Value>,
    entries: &[Entry],
    observed_at: Option<i64>,
    ttl_items: usize,
) -> Value {
    let mut observed = Map::new();
    observed.insert("fields".to_string(), Value::Object(fields(entries)));
    observed.insert("items".to_string(), Value::from(ttl_items));
    observed.insert(
        "observed_at".to_string(),
        observed_at.map(Value::from).unwrap_or(Value::Null),
    );

    let mut out = Map::new();
    if let Some(schema) = declared {
        out.insert("declared".to_string(), schema.clone());
    }
    out.insert("observed".to_string(), Value::Object(observed));
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(item: Value) -> Entry {
        Entry {
            fingerprint: "f".into(),
            at: 1,
            item,
        }
    }

    #[test]
    fn test_type_name_covers_scalars_and_arrays() {
        assert_eq!(type_name(&json!("x")), "string");
        assert_eq!(type_name(&json!(1)), "number");
        assert_eq!(type_name(&json!(true)), "boolean");
        assert_eq!(type_name(&json!(null)), "null");
        assert_eq!(type_name(&json!(["a", "b"])), "array<string>");
        assert_eq!(type_name(&json!([1, "a"])), "array<number|string>");
        assert_eq!(type_name(&json!({})), "object");
        assert_eq!(type_name(&json!([])), "array");
    }

    #[test]
    fn test_fields_merge_types_across_items() {
        let entries = vec![entry(json!({"v": 1})), entry(json!({"v": "s"}))];
        let fields = fields(&entries);
        assert_eq!(fields["v"], "number|string");
    }

    #[test]
    fn test_report_never_contains_values() {
        let entries = vec![entry(json!({"title": "top secret headline", "n": 3}))];
        let shape = report(None, &entries, Some(42), 1);
        let text = shape.to_string();
        assert!(!text.contains("top secret headline"));
        assert_eq!(shape["observed"]["fields"]["title"], "string");
        assert_eq!(shape["observed"]["items"], 1);
        assert_eq!(shape["observed"]["observed_at"], 42);
        assert!(shape.get("declared").is_none());
    }

    #[test]
    fn test_report_echoes_declared_schema() {
        let declared = json!({"type": "object", "properties": {"a": {"type": "string"}}});
        let shape = report(Some(&declared), &[], None, 0);
        assert_eq!(shape["declared"], declared);
        assert_eq!(shape["observed"]["items"], 0);
        assert!(shape["observed"]["observed_at"].is_null());
    }

    #[test]
    fn test_fields_are_capped() {
        let mut map = Map::new();
        for i in 0..(MAX_FIELDS + 10) {
            map.insert(format!("f{i:03}"), json!(1));
        }
        let entries = vec![entry(Value::Object(map))];
        assert_eq!(fields(&entries).len(), MAX_FIELDS);
    }
}
