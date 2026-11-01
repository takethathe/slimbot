---
name: memory
description: Long-term memory: which documents Dream owns, how to write a durable fact, and how to recall a stored one.
always: true
---

# Memory

## Write a durable fact — `file_editor` on `memory/MEMORY.md`

Memory has one landing document: `memory/MEMORY.md`, written with the ordinary file tools (`file_editor` to add a bullet, `file_writer` only to rewrite the whole document). There is no memory-write tool and no category argument: which document a fact finally belongs to is Dream's later decision.

Every other memory document is **protected by code**. `AGENTS.md`, `SOUL.md`, `USER.md`, `TOOLS.md`, `skills/*/SKILL.md` and custom `memory/*.md` writes return `REFUSED:` — that refusal is final, do not retry it through `shell` or another path. Put the fact in `MEMORY.md` and tell the owner what you would have changed.

## The write convention (read by humans, not parsed)

1. **One fact per bullet, digested in your own words.** `MEMORY.md` is your understanding, never a transcript.
2. **Every bullet carries provenance: source + date**, as `(source · YYYY-MM-DD)`. Never invent a source or a date — use the date you were given, and mark inferred facts `(unknown source · YYYY-MM-DD)`.

## Raw external content never goes into memory

Fetched pages, feed items and trigger evidence reach you as bounded evidence; only the **digest** belongs in `MEMORY.md`. Do not paste page text, quoted paragraphs, JSON payloads or snapshots. The original is already at its reference path (`tool-results/<id>.txt`) — cite the path instead of copying the body. Dream deletes entries that look copied rather than understood.

## Recall a stored fact — `memory_search`

SOUL, USER, TOOLS, AGENTS, `memory/MEMORY.md` and the newest history entries are already in this prompt — each document appears only once it differs from its template. `memory_search` covers the rest: a read-only search over those documents plus every `skills/<name>/SKILL.md` and custom `memory/<name>.md`, returning matching paragraphs with their category, enclosing heading and source path.

- `query` — keywords; Chinese and English both work
- `category` — optional filter: `agent`, `soul`, `tools`, `user`, `core`, `skill`, `custom` (`daily` is reserved and empty)
- `limit` — default 10, max 50

## Recall an older event

`memory_search` reads documents only, never the history log. The newest entries are already in this prompt; for anything older, search the log listed in your workspace notes with `grep`, or `file_reader` when it is small.

## Ownership

Dream rewrites `SOUL.md`, `USER.md` and `memory/MEMORY.md` from consolidated history, promotes facts out of `MEMORY.md` into `skills/<name>/SKILL.md` or custom `memory/<name>.md`, and merges new workflows into existing skills. State a correction in your reply and keep applying it yourself until it shows up in memory — consolidation decides when the next Dream run sees it.

## Skills vs agents

A skill is knowledge to load; an agent (`agents/<name>.md`) is an executable to launch. Give a capability one home only, never both — when both exist the model takes the shortcut of calling the agent and the document rots. Reusable *executors* with their own tools go to `agents/`; knowledge and conventions go to `skills/`.