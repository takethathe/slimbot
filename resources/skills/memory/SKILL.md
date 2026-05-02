---
name: memory
description: Two-layer memory system with Dream-managed knowledge files.
always: true
---

# Memory

## Structure

- `SOUL.md` — Bot personality and communication style. **Managed by Dream.** Do NOT edit.
- `USER.md` — User profile and preferences. **Managed by Dream.** Do NOT edit.
- `memory/MEMORY.md` — Long-term facts (project context, important decisions). **Managed by Dream.** Do NOT edit.
- `memory/history.jsonl` — Append-only JSONL, not loaded into context automatically. Use `grep` to search it.

## Search Past Events

`memory/history.jsonl` is JSONL format — each line is a JSON object with `cursor`, `timestamp`, `content`.

- For broad searches, start with `shell: grep "pattern" memory/history.jsonl` before requesting full file content
- Use exact timestamps or keywords as search patterns
- Read `memory/history.jsonl` directly with `file_reader` for small files

## Important

- **Do NOT edit SOUL.md, USER.md, or MEMORY.md.** They are automatically managed by the Dream process.
- If you notice outdated information, note it in your response — Dream will correct it on its next run.
- Memory content is automatically injected into your context when relevant.
