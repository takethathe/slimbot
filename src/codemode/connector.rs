//! The tool bridge every codemode entry shares.
//!
//! `ctx.tools.<name>(args?)` and `ctx.searchTools(query, limit?)` are one
//! implementation, installed from a **catalog** and an **invoke** function the
//! entry supplies. What differs between entries is the subset of the callable
//! catalog they pass — the agent entry takes its own tools plus every online
//! connector, the collect entry takes only the connector tools a server claims
//! read-only (ticket 17), and the act entry splits the connector list in two
//! (ticket 15). A second install path would be a second safety boundary, which
//! is exactly what this module exists to prevent.
//!
//! Two properties belong to the bridge, not to an entry:
//!
//! - **Structural absence.** `tools` is assembled at install time from the
//!   catalog snapshot, so a name the catalog did not carry is `undefined` and a
//!   call is a `TypeError`, never a lookup that reaches the network.
//! - **Search reads the live catalog.** `searchTools` re-reads on every query,
//!   so a connector that goes offline disappears from search immediately
//!   (ADR 0016) even though its installed function outlives the run.
//!
//! Connector descriptions are server text and travel as data (ADR 0009); the
//! entries themselves stay the only place that decides which names exist. The
//! bridge never inspects the script.

use std::collections::HashMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use rquickjs::function::{Async, Opt};
use rquickjs::{Ctx, Function, Object, Value as JsValue};
use serde_json::{Value, json};

use super::host::host_object;
use crate::memory::index::tokenize;
use crate::tool::{ToolDefinition, TurnContext, with_turn_context};

/// The number of hits `searchTools` returns when no limit is given.
///
/// A bound on one tool result — how many rows a single search may put in the
/// model's next turn — not a context budget: it does not move with the
/// provider's window and does not belong in `pressure.rs` (ADR 0013), the same
/// way `docs/codemode.md` describes the 16KB output ceiling.
const DEFAULT_SEARCH_LIMIT: usize = 8;
/// The largest number of hits a script may ask for. Same kind of bound as
/// [`DEFAULT_SEARCH_LIMIT`]: one result set, not a share of the context window.
const MAX_SEARCH_LIMIT: usize = 32;

/// What a script may search: re-read on every call, so offline servers drop out.
pub(crate) type Catalog = Arc<dyn Fn() -> Vec<ToolDefinition> + Send + Sync>;

/// How one `tools.<name>` call is answered. Entries differ here too: the agent
/// entry routes through the approval gate, the collect entry has no gate and no
/// script-side task to attach (ticket 17 decision).
pub(crate) type Invoke = Arc<dyn Fn(String, Value) -> BoxFuture<'static, String> + Send + Sync>;

/// Install `ctx.tools.<name>(args?)` and `ctx.searchTools(query, limit?)` over
/// one entry's catalog subset.
///
/// `state` is whatever the entry needs to answer the bridge's two questions:
/// `catalog(&state)` is the subset a script may see, `invoke(state, name, args)`
/// answers one call. Only the wiring lives here — which names are in the
/// catalog and where a call goes stay in the closures, one entry each, so this
/// helper can never become a second place where the surface is decided.
pub(crate) fn install_tool_bridge<State, C, I>(
    ctx: &Ctx<'_>,
    state: State,
    catalog: C,
    invoke: I,
) -> rquickjs::Result<()>
where
    State: Clone + Send + Sync + 'static,
    C: Fn(&State) -> Vec<ToolDefinition> + Send + Sync + 'static,
    I: Fn(State, String, Value) -> BoxFuture<'static, String> + Send + Sync + 'static,
{
    let catalog: Catalog = {
        let state = state.clone();
        Arc::new(move || catalog(&state))
    };
    let invoke: Invoke = Arc::new(move |name, args| invoke(state.clone(), name, args));
    install_tools_namespace(ctx, catalog.clone(), invoke)?;
    install_search_tools(ctx, catalog)?;
    Ok(())
}

/// Run one nested call inside the turn's task-local scope.
///
/// The scope is rebuilt around **every** nested call, not opened once per run:
/// the call executes on the sandbox thread, where the runner's task-local is
/// unreachable, so a tool that resolves the turn's origin (message, cron,
/// trigger) would otherwise see an empty target. For the same reason `call`
/// builds the future itself — a future created outside the scope would run in
/// the wrong context, and the scope has to wrap the await, not the setup.
pub(crate) fn call_in_turn<T, F>(turn: Option<Arc<TurnContext>>, call: F) -> BoxFuture<'static, T>
where
    T: Send + 'static,
    F: FnOnce() -> BoxFuture<'static, T> + Send + 'static,
{
    Box::pin(async move {
        match turn {
            Some(turn) => with_turn_context(turn, call()).await,
            None => call().await,
        }
    })
}

/// Convert a JS argument to `serde_json` text on the spot: tool arguments are
/// JSON everywhere, and nothing borrowed may cross the await boundary.
fn js_to_json(v: &JsValue<'_>) -> rquickjs::Result<Value> {
    let ctx = v.ctx().clone();
    let Some(string) = ctx.json_stringify(v.clone())? else {
        return Err(rquickjs::Error::new_from_js("value", "json"));
    };
    let text = string.to_string()?;
    serde_json::from_str(&text).map_err(|_| rquickjs::Error::new_from_js("value", "json"))
}

/// Install `ctx.tools.<name>(args)` for every name in the catalog snapshot.
/// The arguments object is optional (`Opt`): calling a tool with no arguments is
/// legal, and becomes `{}`.
fn install_tools_namespace<'js>(
    ctx: &Ctx<'js>,
    catalog: Catalog,
    invoke: Invoke,
) -> rquickjs::Result<()> {
    let host = host_object(ctx)?;
    let tools = Object::new(ctx.clone())?;
    for def in catalog() {
        let invoke = invoke.clone();
        let name = def.name.clone();
        let func = Function::new(
            ctx.clone(),
            Async(move |args: Opt<JsValue<'js>>| {
                let invoke = invoke.clone();
                let name = name.clone();
                // Convert on the JS thread: nothing borrowed may cross the await.
                let json = match args.0 {
                    Some(ref v) => js_to_json(v).map_err(|e| e.to_string()),
                    None => Ok(Value::Object(serde_json::Map::new())),
                };
                async move {
                    // An unconvertible argument object must not become "no
                    // arguments": the tool would run with the wrong payload.
                    match json {
                        Err(msg) => Ok::<String, rquickjs::Error>(format!(
                            "Error: `tools.{name}` arguments must be a JSON-serializable \
                             object ({msg})"
                        )),
                        Ok(json) => Ok(invoke(name, json).await),
                    }
                }
            }),
        )?;
        tools.set(def.name.as_str(), func)?;
    }
    host.set("tools", tools)?;
    Ok(())
}

/// Install `ctx.searchTools(query, limit?)`, returning a JSON array string of
/// `{name, description}` ranked by the same BM25-lite scoring the memory
/// keyword index uses (latin tokens + CJK bigrams, name hits weighted like
/// headings). Connector tools are searchable; their descriptions are server text
/// and stay data for the script (ADR 0009).
fn install_search_tools<'js>(ctx: &Ctx<'js>, catalog: Catalog) -> rquickjs::Result<()> {
    let host = host_object(ctx)?;
    let func = Function::new(
        ctx.clone(),
        Async(move |query: String, limit: Opt<i32>| {
            let defs = catalog();
            let mut limit = limit.0.unwrap_or(DEFAULT_SEARCH_LIMIT as i32);
            if limit <= 0 {
                limit = DEFAULT_SEARCH_LIMIT as i32;
            }
            if limit > MAX_SEARCH_LIMIT as i32 {
                limit = MAX_SEARCH_LIMIT as i32;
            }
            let hits = search_tools(&defs, &query, limit as usize);
            async move {
                Ok::<String, rquickjs::Error>(
                    serde_json::to_string(&hits).unwrap_or_else(|_| "[]".into()),
                )
            }
        }),
    )?;
    host.set("searchTools", func)?;
    Ok(())
}

fn term_counts(tokens: Vec<String>) -> HashMap<String, u32> {
    let mut counts = HashMap::new();
    for token in tokens {
        *counts.entry(token).or_insert(0) += 1;
    }
    counts
}

/// Rank tool definitions against `query`. Deterministic: terms are visited in
/// sorted order and score ties break on the tool name, so repeated searches
/// over the same catalog return the same order (float addition is not
/// associative).
pub(crate) fn search_tools(defs: &[ToolDefinition], query: &str, limit: usize) -> Vec<Value> {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;

    let mut terms = tokenize(query);
    if terms.is_empty() || defs.is_empty() {
        return Vec::new();
    }
    terms.sort();
    terms.dedup();

    struct Doc {
        name: String,
        description: String,
        counts: HashMap<String, u32>,
        len: usize,
    }
    let docs: Vec<Doc> = defs
        .iter()
        .map(|def| {
            let name = term_counts(tokenize(&def.name));
            let description = term_counts(tokenize(&def.description));
            let mut counts: HashMap<String, u32> = HashMap::new();
            for (token, c) in name {
                *counts.entry(token).or_insert(0) += 2 * c;
            }
            for (token, c) in description {
                *counts.entry(token).or_insert(0) += c;
            }
            let len = counts.values().sum::<u32>() as usize;
            Doc {
                name: def.name.clone(),
                description: def.description.clone(),
                counts,
                len: len.max(1),
            }
        })
        .collect();

    let n = docs.len() as f64;
    let avg_len = (docs.iter().map(|d| d.len).sum::<usize>() as f64 / n).max(1.0);
    let mut scores = vec![0.0f64; docs.len()];

    for token in &terms {
        let df = docs.iter().filter(|d| d.counts.contains_key(token)).count() as f64;
        if df == 0.0 {
            continue;
        }
        let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
        for (idx, doc) in docs.iter().enumerate() {
            let Some(&tf) = doc.counts.get(token) else {
                continue;
            };
            let tf = tf as f64;
            let denom = tf + K1 * (1.0 - B + B * doc.len as f64 / avg_len);
            scores[idx] += idf * (tf * (K1 + 1.0)) / denom;
        }
    }

    let mut hits: Vec<(f64, &Doc)> = scores
        .iter()
        .enumerate()
        .filter(|(_, score)| **score > 0.0)
        .map(|(idx, score)| (*score, &docs[idx]))
        .collect();
    hits.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.name.cmp(&b.1.name))
    });
    hits.truncate(limit);
    hits.into_iter()
        .map(|(_, doc)| json!({"name": doc.name, "description": doc.description}))
        .collect()
}
