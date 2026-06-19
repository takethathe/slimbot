# Snip: Context Truncation

Snip is a stateless context truncation mechanism that runs before every LLM API call. It ensures the prompt never exceeds the model's token limit by dropping the oldest messages.

## How It Works

Before each `Provider.chat()` call, snip:

1. Estimates the total token count for all messages
2. If within budget, sends messages as-is
3. If over budget, truncates:
   - Preserves all system messages
   - Iterates backwards through non-system messages, keeping the most recent
   - Ensures the first non-system message is a User message (for compatibility)
   - Falls back to keeping the last 4 messages if nothing fits

## Budget Calculation

```
budget = context_window_tokens - max_output_tokens - 1024
```

Where:
- `context_window_tokens` — from `agent.context_window_tokens` in config
- `max_output_tokens` — from the active provider's `max_tokens` in config
- `1024` — safety buffer for tokenizer estimation drift

## Token Estimation

Snip uses the session's observed `char_per_token_ratio` (updated after each LLM call based on actual usage data):

```
tokens = ceil(message_chars / char_per_token_ratio)
```

Minimum: 4 tokens per message.

## Relationship to Consolidation

| Aspect | Snip | Consolidation |
|--------|------|---------------|
| **When** | Before every LLM call | After each ReAct turn |
| **Trigger** | Token estimate exceeds budget | LLM-reported tokens exceed budget |
| **Action** | Drop old messages (memory only) | LLM summarizes old messages |
| **State** | Stateless | Stateful (JSONL, meta.json) |
| **Cost** | Zero (no LLM call) | One LLM call per consolidation |
| **Purpose** | Safety net for immediate overflow | Long-term memory extraction |

Both can run in the same turn:
1. Snip runs first (before chat) — prevents immediate overflow
2. Turn executes
3. Consolidation runs after (if `prompt_tokens` > budget) — extracts long-term memory

## Configuration

Snip is always active. No configuration needed beyond the standard:
- `agent.context_window_tokens` — your model's context window
- `providers.<name>.max_tokens` — max output tokens for the provider

## Example

With `context_window_tokens = 32768` and `max_tokens = 4096`:

```
budget = 32768 - 4096 - 1024 = 27648 tokens
```

If your conversation exceeds 27648 tokens, snip will drop the oldest messages (after system messages) until it fits.
