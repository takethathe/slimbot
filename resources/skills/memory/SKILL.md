---
name: memory
description: Long-term memory: which documents Dream owns, and how to recall a stored fact or an older event.
always: true
---

# Memory

## Recall a stored fact — `memory_search`

SOUL, USER, TOOLS, AGENTS, `memory/MEMORY.md` and the newest history entries are already in this prompt — each document appears only once it differs from its template. `memory_search` covers the rest: a read-only search over those documents plus every `skills/<name>/SKILL.md` and custom `memory/<name>.md`, returning matching paragraphs with their category, enclosing heading and source path.

- `query` — keywords; Chinese and English both work
- `category` — optional filter: `agent`, `soul`, `tools`, `user`, `core`, `skill`, `custom` (`daily` is reserved and empty)
- `limit` — default 10, max 50

## Recall an older event

`memory_search` reads documents only, never the history log. The newest entries are already in this prompt; for anything older, search the log listed in your workspace notes with `grep`, or `file_reader` when it is small.

## Ownership

Dream rewrites `SOUL.md`, `USER.md` and `memory/MEMORY.md` from consolidated conversation history, and merges new workflows into `skills/<name>/SKILL.md`. State a correction in your reply and keep applying it yourself until it shows up in memory — consolidation decides when the next Dream run sees it.

## Skills vs agents

A skill is knowledge to load; an agent (`agents/<name>.md`) is an executable to launch. Give a capability one home only, never both — when both exist the model takes the shortcut of calling the agent and the document rots. Reusable *executors* with their own tools go to `agents/`; knowledge and conventions go to `skills/`.
