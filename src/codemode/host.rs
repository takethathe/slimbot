//! The whitelist side of the sandbox.
//!
//! A VM starts bare: no `require`, no `process`, no timers, no `console`. The
//! only capabilities a script has are the ones a [`HostBindings`]
//! implementation installs, and they all hang off one `__host` object which
//! the sandbox merges into the `ctx` argument. Safety therefore comes from the
//! host function list alone, never from scanning the script text.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use rquickjs::{Ctx, Function, Object};

use super::redact::Redactor;

/// Where a script's `ctx.log(...)` lines go. Already redacted when called.
pub type LogSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Per-run host environment shared by every installed function.
#[derive(Clone)]
pub struct HostEnv {
    redactor: Redactor,
}

impl HostEnv {
    pub fn new(redactor: Redactor) -> Self {
        Self { redactor }
    }

    pub fn redactor(&self) -> &Redactor {
        &self.redactor
    }

    pub fn redact(&self, text: &str) -> String {
        self.redactor.redact(text)
    }
}

impl Default for HostEnv {
    fn default() -> Self {
        Self::new(Redactor::empty())
    }
}

/// Process start, captured the first time anything asks for it.
static BOOT_AT: OnceLock<SystemTime> = OnceLock::new();

/// Epoch milliseconds of the first `boot_at()` call in this process.
pub fn process_boot_at_ms() -> i64 {
    let boot = BOOT_AT.get_or_init(SystemTime::now);
    boot.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// One set of host functions to install into a fresh VM.
///
/// Implementations must be cheap to share: the sandbox moves the `Arc` onto
/// its dedicated execution thread.
pub trait HostBindings: Send + Sync + 'static {
    /// Install functions onto the `__host` object. Called once per execution,
    /// before the script text is compiled.
    fn install(&self, ctx: &Ctx<'_>, env: &HostEnv) -> rquickjs::Result<()>;
}

/// A bindings set with no functions at all (pure computation scripts and
/// sandbox boundary tests).
#[cfg_attr(not(test), allow(dead_code))]
pub struct NoHost;

impl HostBindings for NoHost {
    fn install(&self, _ctx: &Ctx<'_>, _env: &HostEnv) -> rquickjs::Result<()> {
        Ok(())
    }
}

/// Fetch, or create, the `__host` object that hosts the whitelisted functions.
pub fn host_object<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let globals = ctx.globals();
    if let Ok(obj) = globals.get::<_, Object<'js>>("__host") {
        return Ok(obj);
    }
    let obj = Object::new(ctx.clone())?;
    globals.set("__host", obj.clone())?;
    Ok(obj)
}

/// Raise a catchable JS exception from inside a host function.
pub fn host_error<'js>(ctx: &Ctx<'js>, message: &str) -> rquickjs::Error {
    rquickjs::Exception::throw_message(ctx, message)
}

/// Turn a host-side failure message into the `Error` a script catches. Same
/// shape as [`host_error`], for host functions that have no `Ctx` at hand
/// (the JSON primitives below the shims).
pub fn host_failure(message: String) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("host", "string", message)
}

/// Install the three time functions every script gets: `now()`, `boot_at()`
/// and `uptime_s()`. All local time, no timezone configuration.
pub fn install_time<'js>(ctx: &Ctx<'js>, _env: &HostEnv) -> rquickjs::Result<()> {
    let host = host_object(ctx)?;

    let now = Function::new(
        ctx.clone(),
        |ctx: Ctx<'js>| -> rquickjs::Result<Object<'js>> {
            let now = chrono::Local::now();
            let obj = Object::new(ctx)?;
            obj.set("epoch_ms", now.timestamp_millis())?;
            obj.set("iso", now.to_rfc3339())?;
            obj.set("hour", now.format("%H").to_string())?;
            obj.set("minute", now.format("%M").to_string())?;
            obj.set("weekday", now.format("%A").to_string().to_ascii_lowercase())?;
            obj.set("date", now.format("%Y-%m-%d").to_string())?;
            Ok(obj)
        },
    )?;
    host.set("now", now)?;

    let boot = process_boot_at_ms();
    host.set(
        "boot_at",
        Function::new(ctx.clone(), move || -> i64 { boot })?,
    )?;

    // Process uptime, not per-execution elapsed time: scripts write conditions
    // like "one minute after boot", which only a process-wide clock answers.
    host.set(
        "uptime_s",
        Function::new(ctx.clone(), || -> i64 {
            let boot = BOOT_AT.get_or_init(SystemTime::now);
            SystemTime::now()
                .duration_since(*boot)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        })?,
    )?;

    Ok(())
}

/// Install `log(msg)`: redacted before it reaches the sink.
pub fn install_log<'js>(ctx: &Ctx<'js>, env: &HostEnv, sink: LogSink) -> rquickjs::Result<()> {
    let host = host_object(ctx)?;
    let env = env.clone();
    let log = Function::new(ctx.clone(), move |message: String| {
        sink(&env.redact(&message));
    })?;
    host.set("log", log)?;
    Ok(())
}

/// Install `secret(name)`: the value of one entry of `triggers.secrets`, or a
/// thrown error naming the config key when there is no such secret. Every
/// entry that reads secrets (collect, act) shares this one implementation.
pub fn install_secret<'js>(
    ctx: &Ctx<'js>,
    secrets: Arc<std::collections::BTreeMap<String, String>>,
) -> rquickjs::Result<()> {
    let host = host_object(ctx)?;
    let secret = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'_>, name: String| -> rquickjs::Result<String> {
            match secrets.get(&name) {
                Some(value) => Ok(value.clone()),
                None => Err(host_error(
                    &ctx,
                    &format!("no secret named '{name}'; add it to triggers.secrets in config"),
                )),
            }
        },
    )?;
    host.set("secret", secret)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_boot_at_is_stable() {
        let a = process_boot_at_ms();
        let b = process_boot_at_ms();
        assert_eq!(a, b);
        assert!(a > 0);
    }

    #[test]
    fn test_host_env_redacts_known_secret() {
        let env = HostEnv::new(Redactor::new(["tok"]));
        assert_eq!(env.redact("bearer tok"), "bearer ***");
    }

    #[test]
    fn test_host_env_default_has_no_secrets() {
        assert!(HostEnv::default().redactor().is_empty());
    }
}
