# Connector contract

A **connector** is one outside system the owner has wired in — GitHub, Slack, a
mailbox, a database — exposed to your script as tools with typed arguments. This
file is the single home of the connector rules: how a script finds one, what a
call answers, and which of them each entry can reach. How you use them in a
`collect` or `act` script is your own agent file's business.

## Find one

`await ctx.searchTools(query, limit?)` searches the live catalog and returns
`[{name, description}]`, best match first, 8 hits by default, at most 32. Search
with the verb and the object — `"create issue"`, `"post message"`,
`"list pull requests"`.

`name` is the callable id and carries the server:
`mcp__<server>__<tool>` — `mcp__github__create_issue`. The server half is what
you need for anything that asks *which* connector, never the bare tool name.

`searchTools` re-reads the catalog on every call: a connector that is offline
right now is not in the results, and one that just came online is.

## Call one

`await ctx.tools.mcp__github__create_issue({repo, title, body})` answers the
tool's result as **text**, and a failed call answers text starting with
`Error:` — a value you read, not an exception you catch. Two consequences:

- Parse the text when the tool answers JSON; a tool that answers prose is
  something to quote into `evidence`.
- `Error:` is the connector's own report (`connector is not available`,
  `no connector tool named …`). Read it whole: it names the server, the status,
  or the argument that was wrong.

A name the catalog did not carry is structurally absent: `ctx.tools.nope` is
`undefined`, and calling it throws `TypeError`. That is the one exception to
"failures are values" — so check `searchTools` first rather than guess a name.

## What each entry may reach

Both scripts reach the same pool; what differs is the slice of the catalog each
entry installs, fixed when the run starts — which is why a tool can be
searchable and still not callable.

| Your script | `ctx.tools` / `searchTools()` carry | Reaching past it |
|---|---|---|
| `collect(ctx)` — a data source | only connector tools the server claims **read-only**; the write half is invisible to a collector, in `ctx.tools` and in search | nothing to reach past — the collector's own base primitives stay as they are |
| `act(ctx)` — an executor | every connector tool, reads and writes | a **write** to a server outside the executor's `allow_connectors` ends the whole script on the spot — no `Error:` to catch; reads are never limited |

Read-only means the server claims it: `readOnlyHint` set and `destructiveHint`
not set. A server that claims neither counts as capable of writing, so it is
absent from a collector.

## Connector or base primitive

Both entries keep their base primitives, so the choice is per object:

- **A connector covers the system** — reach it through the connector. The owner
  configured that server, the arguments are typed, and for a change the connector
  call is the only route the approval gate can see.
- **No connector covers it** — a plain endpoint, a feed, a status page, or in a
  collector a local file — read it with the base primitives your entry installs:
  `ctx.http.get` in both, `ctx.fs.read` in a collector. Those stay exactly as
  they were; a missing connector is not a reason to stop reading the world.

Every outward **change** is one connector tool call. Raw http stays a read
primitive: a write composed out of it is invisible to the approval gate, and in
an unattended run that gate is the whole safety story.

## Search before you build

`searchTools` also answers "is this already a tool?". Before writing a step that
reaches outside, query the catalog for the effect you need: a connector tool that
already does it is one call with a typed receipt, while hand-rolling the same
effect over http is a second way into a system the owner already wired in — the
duplicate that drifts, and the one a repeat run will not find.

The store your entry creates into is the second place to look. A connector tool
that already exists is one call; a stored script that already exists is one
artifact, and a near-copy of it is the expensive duplicate — two scripts that
almost agree are two behaviours that drift apart, both of them still live. Look
first, add second: what is stored, what each one does, and only then a new one.
Your agent file names that store and the steps that go with it.

**Done when** you can name the queries you ran, the tool or stored script you are
reusing (or why none of the hits does the job), and the server each call will
land on.