//! Host functions available to a condition script (`condition.js`).
//!
//! A judgment runs unattended inside the tick's millisecond budget, so its
//! capability list is shorter than a collector's: the frozen source views come
//! in as plain data, and the only host calls are the private store, a log line
//! and the clock. There is no HTTP, no filesystem and no tool — "go fetch more
//! data" is the source layer's job, and a condition that could call a tool
//! could also spend ten seconds inside a tick.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use rquickjs::{Ctx, Function};

use crate::codemode::host::{
    HostBindings, HostEnv, LogSink, host_error, host_object, install_log, install_time,
};

use super::state::TriggerState;

/// Everything a condition may call. Reads see the state as of this judgment;
/// writes accumulate and are committed by the layer once the script returns.
pub struct JudgeHost {
    base: BTreeMap<String, serde_json::Value>,
    writes: Arc<Mutex<BTreeMap<String, serde_json::Value>>>,
    logs: Arc<Mutex<Vec<String>>>,
}

impl JudgeHost {
    pub fn new(base: BTreeMap<String, serde_json::Value>) -> Self {
        Self {
            base,
            writes: Arc::new(Mutex::new(BTreeMap::new())),
            logs: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// What the script asked to remember.
    pub fn writes(&self) -> BTreeMap<String, serde_json::Value> {
        self.writes.lock().map(|m| m.clone()).unwrap_or_default()
    }

    /// Log lines the script produced, already redacted.
    pub fn logs(&self) -> Vec<String> {
        self.logs.lock().map(|l| l.clone()).unwrap_or_default()
    }

    fn merged(&self) -> BTreeMap<String, serde_json::Value> {
        let mut merged = self.base.clone();
        if let Ok(writes) = self.writes.lock() {
            for (key, value) in writes.iter() {
                merged.insert(key.clone(), value.clone());
            }
        }
        merged
    }

    fn get(&self, key: &str) -> serde_json::Value {
        let merged = self.merged();
        merged.get(key).cloned().unwrap_or(serde_json::Value::Null)
    }

    /// Refuse a write that would push the store past 16KB, and say so in the
    /// log as well as in the thrown error: the script may catch it, and the
    /// tick's warning list is where a human looks.
    fn set(&self, key: &str, value: &serde_json::Value) -> std::result::Result<(), String> {
        let probe = {
            let mut merged = self.merged();
            merged.insert(key.to_string(), value.clone());
            TriggerState {
                store: merged,
                ..Default::default()
            }
        };
        if probe.store_bytes() > super::state::STORE_MAX_BYTES {
            let message = format!(
                "store.set('{key}') would exceed the {}KB private-store limit; the value was not saved",
                super::state::STORE_MAX_BYTES / 1024
            );
            if let Ok(mut logs) = self.logs.lock() {
                logs.push(message.clone());
            }
            return Err(message);
        }
        if let Ok(mut writes) = self.writes.lock() {
            writes.insert(key.to_string(), value.clone());
        }
        Ok(())
    }
}

/// JavaScript shim: `ctx.store.get/set` over JSON strings.
const SHIM: &str = r#"
__host.store = {
    get: (key) => JSON.parse(__host.store_get(String(key))),
    set: (key, value) => { __host.store_set(String(key), JSON.stringify(value)); return true; },
    keys: () => JSON.parse(__host.store_keys()),
};
"#;

impl HostBindings for JudgeHost {
    fn install(&self, ctx: &Ctx<'_>, env: &HostEnv) -> rquickjs::Result<()> {
        install_time(ctx, env)?;

        let logs: LogSink = {
            let logs = self.logs.clone();
            Arc::new(move |line: &str| {
                if let Ok(mut guard) = logs.lock() {
                    guard.push(line.to_string());
                }
            })
        };
        install_log(ctx, env, logs)?;

        let host = host_object(ctx)?;

        {
            let store = self.clone_store();
            host.set(
                "store_get",
                Function::new(
                    ctx.clone(),
                    move |key: String| -> rquickjs::Result<String> {
                        Ok(store.get(&key).to_string())
                    },
                )?,
            )?;
        }
        {
            let store = self.clone_store();
            host.set(
                "store_keys",
                Function::new(ctx.clone(), move || -> rquickjs::Result<String> {
                    let keys: Vec<String> = store.merged().keys().cloned().collect();
                    Ok(serde_json::Value::from(keys).to_string())
                })?,
            )?;
        }
        {
            let store = self.clone_store();
            host.set(
                "store_set",
                Function::new(
                    ctx.clone(),
                    move |ctx: Ctx<'_>, key: String, payload: String| -> rquickjs::Result<()> {
                        let value: serde_json::Value =
                            serde_json::from_str(&payload).map_err(|e| {
                                host_error(&ctx, &format!("store value is not JSON: {e}"))
                            })?;
                        store.set(&key, &value).map_err(|e| host_error(&ctx, &e))?;
                        Ok(())
                    },
                )?,
            )?;
        }

        ctx.eval::<(), _>(SHIM)?;
        Ok(())
    }
}

impl Clone for JudgeHost {
    fn clone(&self) -> Self {
        Self {
            base: self.base.clone(),
            writes: self.writes.clone(),
            logs: self.logs.clone(),
        }
    }
}

impl JudgeHost {
    /// The sandbox hands the `Arc` to its own thread; cloning through the
    /// shared interior keeps writes visible to [`JudgeHost::writes`].
    fn clone_store(&self) -> Self {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codemode::{ENTRY_JUDGE, Limits, Redactor, RunRequest, execute};
    use serde_json::json;

    fn judge(script: &str, base: BTreeMap<String, serde_json::Value>) -> serde_json::Value {
        let host = Arc::new(JudgeHost::new(base));
        let env = HostEnv::new(Redactor::empty());
        let request = RunRequest::new(script, ENTRY_JUDGE)
            .with_ctx(json!({"sources": {}}))
            .with_limits(Limits::judge());
        execute(request, host, env).expect("script must run")
    }

    fn seeded(key: &str, value: serde_json::Value) -> BTreeMap<String, serde_json::Value> {
        BTreeMap::from([(key.to_string(), value)])
    }

    #[test]
    fn test_store_get_reads_seeded_state() {
        let out = judge(
            "function judge(ctx) { return {fire: ctx.store.get('cursor') === 'abc'}; }",
            seeded("cursor", json!("abc")),
        );
        assert_eq!(out["fire"], true);
    }

    #[test]
    fn test_store_set_then_read_in_the_same_run() {
        let out = judge(
            "function judge(ctx) { ctx.store.set('seen', 7); return {fire: ctx.store.get('seen') === 7}; }",
            BTreeMap::new(),
        );
        assert_eq!(out["fire"], true);
    }

    #[test]
    fn test_store_set_is_visible_to_the_host_after_the_run() {
        let host = Arc::new(JudgeHost::new(BTreeMap::new()));
        let env = HostEnv::new(Redactor::empty());
        let request = RunRequest::new(
            "function judge(ctx) { ctx.store.set('cursor', 'next'); return {fire: false}; }",
            ENTRY_JUDGE,
        )
        .with_limits(Limits::judge());
        execute(request, host.clone(), env).unwrap();
        assert_eq!(host.writes()["cursor"], json!("next"));
    }

    #[test]
    fn test_oversized_store_write_fails_and_is_logged() {
        let host = Arc::new(JudgeHost::new(BTreeMap::new()));
        let env = HostEnv::new(Redactor::empty());
        let request = RunRequest::new(
            "function judge(ctx) { \
                 let ok = true; \
                 try { ctx.store.set('a', 'x'.repeat(9000)); ctx.store.set('b', 'y'.repeat(9000)); } \
                 catch (e) { return {fire: false, detail: String(e.message).slice(0, 40)}; } \
                 return {fire: ok}; \
             }",
            ENTRY_JUDGE,
        )
        .with_limits(Limits::judge());
        let out = execute(request, host.clone(), env).unwrap();
        // The second write is refused, and the script can see why.
        assert!(out["detail"].as_str().unwrap().contains("16KB"), "{out}");
        assert!(host.writes().contains_key("a"));
        assert!(!host.writes().contains_key("b"));
        assert!(
            host.logs().iter().any(|l| l.contains("16KB")),
            "the refusal must also reach the log: {:?}",
            host.logs()
        );
    }

    #[test]
    fn test_condition_has_no_network_or_file_functions() {
        let out = judge(
            "function judge(ctx) { return {fire: false, detail: Object.keys(ctx).sort().join(',')}; }",
            BTreeMap::new(),
        );
        let detail = out["detail"].as_str().unwrap();
        for forbidden in ["http", "fs", "secret", "subagent", "shell"] {
            assert!(
                !detail.contains(forbidden),
                "ctx leaks '{forbidden}': {detail}"
            );
        }
        assert!(detail.contains("store"), "{detail}");
    }

    #[test]
    fn test_time_functions_are_available() {
        let out = judge(
            "function judge(ctx) { const n = ctx.now(); return {fire: n.epoch_ms > 0 && typeof n.hour === 'string'}; }",
            BTreeMap::new(),
        );
        assert_eq!(out["fire"], true);
    }
}
