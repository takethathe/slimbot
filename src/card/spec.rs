//! Rust port of the `@json-render/core` spec-stream compiler.
//!
//! The model emits a JSONL stream where each line is an RFC 6902-style patch
//! operation (`add`/`replace`/`remove`/`move`/`copy`/`test`) against a single
//! JSON document (the Card spec). Feeding text chunks into [`SpecCompiler`]
//! assembles that document incrementally, tolerating:
//!
//! - lines split across chunk boundaries (a partial line is buffered until a
//!   newline completes it),
//! - provider re-sends of the same line (identical trimmed lines apply once),
//! - `replace` against a not-yet-existing path (treated as a set, so mid-stream
//!   reordering only costs smoothness, never correctness),
//! - array index `-` meaning append.
//!
//! The compiled result is a generic [`serde_json::Value`]; the server never
//! defines a strongly-typed Spec — the client renderer is the authority. A
//! spec with a non-string `root` or an empty/missing `elements` object is
//! treated as "no card".

use std::collections::HashSet;

use serde_json::{Map, Value};

/// Incrementally compiles a JSONL patch stream into a Card spec document.
pub struct SpecCompiler {
    /// Buffer for the current, not-yet-complete line.
    buf: String,
    /// Trimmed lines already applied, so provider re-sends are no-ops.
    seen: HashSet<String>,
    /// The document assembled so far.
    doc: Value,
}

impl Default for SpecCompiler {
    fn default() -> Self {
        Self::new()
    }
}

impl SpecCompiler {
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            seen: HashSet::new(),
            doc: Value::Object(Map::new()),
        }
    }

    /// Feed a text chunk. Complete lines (terminated by `\n`) are applied
    /// immediately; a trailing partial line is buffered until the next chunk
    /// or [`finish`](Self::finish). Returns the operations applied by this
    /// chunk, in order, so callers can forward them as streaming deltas.
    pub fn push(&mut self, chunk: &str) -> Vec<Value> {
        self.buf.push_str(chunk);
        let mut applied = Vec::new();
        while let Some(nl) = self.buf.find('\n') {
            let line = self.buf[..nl].to_string();
            self.buf.drain(..nl + 1);
            if let Some(op) = self.apply_line(&line) {
                applied.push(op);
            }
        }
        applied
    }

    /// Flush any trailing (unterminated) line and return the compiled spec.
    /// Returns `None` when the result is "no card": `root` is missing or not a
    /// string, or `elements` is missing or an empty object.
    pub fn finish(&mut self) -> Option<Value> {
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            self.apply_line(&line);
        }

        let root_ok = self.doc.get("root").map(|v| v.is_string()).unwrap_or(false);
        let elements_ok = self
            .doc
            .get("elements")
            .and_then(|v| v.as_object())
            .map(|o| !o.is_empty())
            .unwrap_or(false);
        if root_ok && elements_ok {
            Some(self.doc.clone())
        } else {
            None
        }
    }

    /// Apply one JSONL line. Returns the operation object when it was applied
    /// (i.e. line was non-empty, unseen, and parsed as a JSON object), or
    /// `None` when it was skipped.
    fn apply_line(&mut self, line: &str) -> Option<Value> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        // Dedup: a repeated identical line is applied only once. Mark it seen
        // even if it fails to parse, so re-sends of malformed text stay cheap.
        if !self.seen.insert(trimmed.to_string()) {
            return None;
        }

        let parsed: Value = serde_json::from_str(trimmed).ok()?;
        if !parsed.is_object() {
            return None;
        }

        let op = parsed.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let path = parse_pointer(parsed.get("path").and_then(|v| v.as_str()));
        match op {
            "add" | "replace" => {
                let value = parsed.get("value").cloned().unwrap_or(Value::Null);
                set_at(&mut self.doc, &path, value);
            }
            "remove" => {
                remove_at(&mut self.doc, &path);
            }
            "move" => {
                let from = parse_pointer(parsed.get("from").and_then(|v| v.as_str()));
                if let Some(val) = remove_at(&mut self.doc, &from) {
                    set_at(&mut self.doc, &path, val);
                }
            }
            "copy" => {
                let from = parse_pointer(parsed.get("from").and_then(|v| v.as_str()));
                if let Some(val) = get_at(&self.doc, &from).cloned() {
                    set_at(&mut self.doc, &path, val);
                }
            }
            "test" => {
                // `test` validates but does not mutate the document.
            }
            _ => {
                // Unknown ops do not mutate the document.
            }
        }
        Some(parsed)
    }
}

/// Decode an RFC 6901 JSON pointer into unescaped tokens. `""` and `"/"`-only
/// resolve to the document root (empty token list).
fn parse_pointer(path: Option<&str>) -> Vec<String> {
    let path = match path {
        Some(p) => p,
        None => return Vec::new(),
    };
    if path.is_empty() {
        return Vec::new();
    }
    let body = path.strip_prefix('/').unwrap_or(path);
    if body.is_empty() {
        return Vec::new();
    }
    body.split('/')
        .map(|tok| tok.replace("~1", "/").replace("~0", "~"))
        .collect()
}

/// Set `value` at `tokens`, creating intermediate containers on the way. A
/// missing path is therefore a plain insertion (streaming tolerance).
fn set_at(doc: &mut Value, tokens: &[String], value: Value) {
    if tokens.is_empty() {
        *doc = value;
        return;
    }
    let key = &tokens[0];
    match doc {
        Value::Object(map) => {
            if tokens.len() == 1 {
                map.insert(key.clone(), value);
            } else {
                let child_needs_array = tokens[1] == "-" || tokens[1].parse::<usize>().is_ok();
                let entry = map.entry(key.clone()).or_insert_with(|| {
                    if child_needs_array {
                        Value::Array(Vec::new())
                    } else {
                        Value::Object(Map::new())
                    }
                });
                set_at(entry, &tokens[1..], value);
            }
        }
        Value::Array(arr) => {
            if tokens.len() == 1 {
                if key == "-" {
                    arr.push(value);
                } else if let Ok(i) = key.parse::<usize>() {
                    if i >= arr.len() {
                        arr.push(value);
                    } else {
                        arr.insert(i, value);
                    }
                }
            } else if let Ok(i) = key.parse::<usize>()
                && i < arr.len()
            {
                set_at(&mut arr[i], &tokens[1..], value);
            }
        }
        _ => {
            // A scalar sits where a container is expected: replace and retry.
            *doc = Value::Object(Map::new());
            set_at(doc, tokens, value);
        }
    }
}

/// Remove the value at `tokens`, returning it. Missing paths are a no-op.
fn remove_at(doc: &mut Value, tokens: &[String]) -> Option<Value> {
    if tokens.is_empty() {
        return Some(std::mem::replace(doc, Value::Object(Map::new())));
    }
    let key = &tokens[0];
    match doc {
        Value::Object(map) => {
            if tokens.len() == 1 {
                map.remove(key)
            } else {
                map.get_mut(key)
                    .and_then(|child| remove_at(child, &tokens[1..]))
            }
        }
        Value::Array(arr) => {
            if key == "-" {
                if tokens.len() == 1 { arr.pop() } else { None }
            } else {
                let i = key.parse::<usize>().ok()?;
                if i >= arr.len() {
                    return None;
                }
                if tokens.len() == 1 {
                    Some(arr.remove(i))
                } else {
                    remove_at(&mut arr[i], &tokens[1..])
                }
            }
        }
        _ => None,
    }
}

/// Borrow the value at `tokens`, if it exists.
fn get_at<'a>(doc: &'a Value, tokens: &[String]) -> Option<&'a Value> {
    if tokens.is_empty() {
        return Some(doc);
    }
    let key = &tokens[0];
    match doc {
        Value::Object(map) => map.get(key).and_then(|child| get_at(child, &tokens[1..])),
        Value::Array(arr) => {
            if key == "-" {
                arr.last().and_then(|child| get_at(child, &tokens[1..]))
            } else {
                let i = key.parse::<usize>().ok()?;
                arr.get(i).and_then(|child| get_at(child, &tokens[1..]))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A minimal but valid card: a root id + a non-empty elements map.
    fn valid_lines() -> Vec<String> {
        vec![
            json!({"op": "add", "path": "/root", "value": "card1"}).to_string(),
            json!({"op": "add", "path": "/elements", "value": {}}).to_string(),
            json!({"op": "add", "path": "/elements/card1", "value": {"type": "Card", "props": {}}})
                .to_string(),
        ]
    }

    #[test]
    fn test_spec_compiler_applies_add_root_and_elements() {
        let mut c = SpecCompiler::new();
        for line in valid_lines() {
            c.push(&line);
            c.push("\n");
        }
        let spec = c.finish().expect("should compile a card");
        assert_eq!(spec["root"], json!("card1"));
        assert_eq!(spec["elements"]["card1"]["type"], json!("Card"));
    }

    #[test]
    fn test_spec_compiler_skips_duplicate_lines() {
        let mut c = SpecCompiler::new();
        c.push(&format!(
            "{}\n",
            json!({"op": "add", "path": "/root", "value": "r"})
        ));
        c.push(&format!(
            "{}\n",
            json!({"op": "add", "path": "/elements", "value": {}})
        ));
        // Feed the same element-add line three times; it must apply only once.
        let dup =
            json!({"op": "add", "path": "/elements/a", "value": {"type": "Text"}}).to_string();
        let applied = c.push(&format!("{dup}\n{dup}\n{dup}\n"));
        assert_eq!(applied.len(), 1, "identical lines collapse to one apply");
        let spec = c.finish().unwrap();
        assert_eq!(spec["elements"].as_object().unwrap().len(), 1);
        assert!(spec["elements"].get("a").is_some());
    }

    #[test]
    fn test_spec_compiler_buffers_incomplete_line_until_newline() {
        let mut c = SpecCompiler::new();
        let full = json!({"op": "add", "path": "/root", "value": "card1"}).to_string();
        // Split mid-token; nothing should apply before the newline arrives.
        let applied1 = c.push(&full[..10]);
        assert!(applied1.is_empty(), "partial line must not apply early");
        let applied2 = c.push(&full[10..]);
        assert!(
            applied2.is_empty(),
            "still no newline, so still nothing applied"
        );
        let applied3 = c.push("\n");
        assert_eq!(applied3.len(), 1, "completing the newline applies it");
    }

    #[test]
    fn test_spec_compiler_remove_and_array_append() {
        let mut c = SpecCompiler::new();
        c.push(&format!(
            "{}\n",
            json!({"op": "add", "path": "/root", "value": "r"})
        ));
        c.push(&format!(
            "{}\n",
            json!({"op": "add", "path": "/elements", "value": {"a": {"type":"Text"}, "b": {"type":"Text"}}})
        ));
        // remove an element
        c.push(&format!(
            "{}\n",
            json!({"op": "remove", "path": "/elements/a"})
        ));
        // array append with '-'
        c.push(&format!(
            "{}\n",
            json!({"op": "add", "path": "/list", "value": []})
        ));
        c.push(&format!(
            "{}\n",
            json!({"op": "add", "path": "/list/-", "value": {"k": 1}})
        ));
        c.push(&format!(
            "{}\n",
            json!({"op": "add", "path": "/list/-", "value": {"k": 2}})
        ));
        let spec = c.finish().unwrap();
        assert!(spec["elements"].get("a").is_none());
        assert!(spec["elements"].get("b").is_some());
        assert_eq!(spec["list"], json!([{"k": 1}, {"k": 2}]));
    }

    #[test]
    fn test_spec_compiler_finish_returns_none_for_empty_spec() {
        // Missing root.
        let mut c = SpecCompiler::new();
        c.push(&format!(
            "{}\n",
            json!({"op": "add", "path": "/elements", "value": {"x": {"type":"Text"}}})
        ));
        assert!(c.finish().is_none(), "no root -> no card");

        // Empty elements.
        let mut c2 = SpecCompiler::new();
        c2.push(&format!(
            "{}\n",
            json!({"op": "add", "path": "/root", "value": "r"})
        ));
        c2.push(&format!(
            "{}\n",
            json!({"op": "add", "path": "/elements", "value": {}})
        ));
        assert!(c2.finish().is_none(), "empty elements -> no card");

        // root not a string.
        let mut c3 = SpecCompiler::new();
        c3.push(&format!(
            "{}\n",
            json!({"op": "add", "path": "/root", "value": 5})
        ));
        c3.push(&format!(
            "{}\n",
            json!({"op": "add", "path": "/elements", "value": {"x": {"type":"Text"}}})
        ));
        assert!(c3.finish().is_none(), "non-string root -> no card");
    }

    #[test]
    fn test_spec_compiler_handles_cjk_in_values() {
        let mut c = SpecCompiler::new();
        c.push(&format!(
            "{}\n",
            json!({"op": "add", "path": "/root", "value": "卡片"})
        ));
        c.push(&format!(
            "{}\n",
            json!({"op": "add", "path": "/elements/h", "value": {"type": "Heading", "props": {"text": "本周花费统计"}}})
        ));
        let spec = c.finish().unwrap();
        assert_eq!(spec["root"], json!("卡片"));
        assert_eq!(
            spec["elements"]["h"]["props"]["text"],
            json!("本周花费统计")
        );
    }

    #[test]
    fn test_spec_compiler_replace_missing_path_sets_value() {
        let mut c = SpecCompiler::new();
        // `replace` on a path that does not exist yet must behave like a set.
        c.push(&format!(
            "{}\n",
            json!({"op": "add", "path": "/root", "value": "r"})
        ));
        c.push(&format!(
            "{}\n",
            json!({"op": "replace", "path": "/elements/new", "value": {"type": "Text"}})
        ));
        let spec = c.finish().unwrap();
        assert_eq!(spec["elements"]["new"]["type"], json!("Text"));
    }

    #[test]
    fn test_spec_compiler_chunk_split_equals_single_feed() {
        let lines = valid_lines();
        let whole = lines.iter().map(|l| format!("{l}\n")).collect::<String>();

        // Reference: one-shot feed.
        let mut single = SpecCompiler::new();
        single.push(&whole);
        let expected = single.finish().expect("single-feed spec");

        // Compare: arbitrary char-level chunking must yield the identical spec.
        let mut split = SpecCompiler::new();
        let chars: Vec<char> = whole.chars().collect();
        for pair in chars.chunks(2) {
            let s: String = pair.iter().collect();
            split.push(&s);
        }
        let got = split.finish().expect("char-split spec");
        assert_eq!(expected, got, "streaming assembly equals one-shot");
    }
}
