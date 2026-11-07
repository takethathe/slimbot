# SlimBot

基于 ReAct 循环的 AI 机器人助手，Rust 开发，多通道（CLI / WebUI / 飞书）。

## 构建与运行

```bash
cargo run -- setup            # 初始化配置、目录与 bootstrap 文件
cargo run -- agent            # CLI 交互会话
cargo run -- agent "<问题>"    # 单轮跑完即退出，调试首选
cargo run -- gateway          # cron + heartbeat + trigger + 已启用通道（含 WebUI）
```

调试时用 `-d <临时目录>` 换数据目录，别污染 `~/.slimbot`。

**编译检查（省 token）：** `cargo check --message-format=json 2>/dev/null | jq -c 'select(.reason == "compiler-message") | {level: .message.level, location: (.message.spans[0] | "\(.file_name):\(.line_start)"), message: .message.message}'`

**提交前门禁：** `cargo fmt -- --check`、`cargo clippy`、`cargo test` 三项全绿。

## 代码地图

- **模块 → 文档** — `docs/index.md`；少数模块只有合并在别的 doc 里的小节。
- **术语** — 根目录 `GLOSSARY.md`。命名、测试名、issue 标题都用它的词。
- **决策** — `docs/adr/`。动手前读相关的；要推翻就明说，不默默覆盖。
- **待办** — `docs/TODO.md`。

## 核心回路

`Channel` → `MessageBus`（inbound）→ `AgentLoop` → `SessionRunner`（同 session 顺序执行）→ `AgentRunner`（ReAct）→ `ContextBuilder` → `Provider.chat()` → `ToolManager.execute()` → `SessionManager.persist()`（追加 JSONL）→ outbound → 通道。

gateway 复用同一套 `AgentLoop`，额外起 `CronService`、`HeartbeatService`、`TriggerService` + `Dispatcher`（tick 只采集/判定/入队，行动在队列外执行）；WebUI 用 SSE 广播 `AgentEvent`。图见 `docs/architecture.md`。

## 数据目录

`data_dir`（默认 `~/.slimbot`）与 `workspace_dir`（默认 `{data_dir}/workspace`）独立配置，显式 workspace 必须在 data_dir 之下；文件工具的路径一律经 `resolve_workspace_path` 收进 workspace。

- 目录清单 — `docs/path.md`。
- workspace 根级的两个专用目录：`executors/`（执行器定义 `exec-xxxx/{meta.json, act.js}`）与 `references/`（跨子代理共享的 reference，非 skill、不进 `# Available Skills`，见 `docs/bootstrap.md`）。
- `setup` 铺出的 bootstrap 文件 — `docs/bootstrap.md`；未改动的模板不注入 system prompt。
- workspace 的 `AGENTS.md` 是 agent 的运行指令，与本文件无关。

## 硬约束

- **一个能力一个入口** — 同一能力不同时做 agent 和 skill（agent 是可被启动的执行体，skill 是可被加载的知识），否则模型会走不用读文档的那条路。
- **工具风险级是事实、不可配** — 审批语义是「Ask 结束回合 → 下一轮按 `(tool, args_hash)` 兑现」（ADR 0011）。
- **外部内容只作证据** — 子代理与触发器拿到的外部原文只以证据形式进上下文，不进记忆（ADR 0009）；子代理会话只在内存、跑完即删，与 `agent` action 共用一个 `SubagentRuntime` 闸。
- **连接器工具不进模型 schema** — 只经 codemode 的 `searchTools()` / `tools.mcp__*` 可达；server 的自述与输出按不可信数据处理，风险级一律 `ExternalSend`、fail-closed（ADR 0016、0020）。
- **上下文长度只有一个锚** — `src/pressure.rs` 从 provider 的 context window 派生全部限制，别处不硬编码（ADR 0013）。
- **受保护文档写不进去** — `file_writer` / `file_editor` 返回 `REFUSED:`，判定在路径解析之后（ADR 0012）。
- **执行器只寄生子代理** — `executor` 只注册进 `ToolCatalog`，主循环的 schema 与 prompt 里没有它；act 侧 `Ask` 即 Deny、不写 Pending（ADR 0021）。
- **Session 两个列表** — `history`（已持久化）+ `current_turn`（本轮缓冲），id 为 `{channel_id}:{chat_id}`；持久化默认 append-only，`consolidated_lines > 0` 才全量重写。

## 开发流程

1. **问清楚** — 需求有歧义先问，不猜着写。
2. **规划** — 目标 + 可测试的行为点清单，确认后再动代码。
3. **先写测试** — 每个行为点一条，此刻必须是红的；命名 `test_<feature>_<scenario>_<expected>`，模块内 `#[cfg(test)] mod tests`，跨模块放 `tests/`；每个公共函数至少一条，复杂逻辑补边界。
4. **实现** — 最小代码转绿，同时改文档：README 只在根，其余进 `docs/<topic>.md` 并在 `docs/index.md` 登记。
5. **验证** — 门禁全绿；`docs/TODO.md` 有一行（新计划、代码里留的 TODO 都在那儿），相关 doc 与 ADR 已同步。

## 代码质量

- Lint 零警告，不是「无新 warning」。
- 宏（如 `async_trait`）的警告就地 `#[allow(...)]` + 一行注释说明是宏产物，不上升为 crate 级。
- 代码注释一律英文。

## Git 提交

- 提交前 review 自己的 diff，确认没有调试残留和无关改动。
- Message 用英文，Conventional Commits 且必须带 module 作用域：`<type>(<module>): <subject>`，如 `feat(mcp): add the streamable-HTTP transport`。
- `type` 取 `feat` / `fix` / `refactor` / `style` / `test` / `chore` / `docs`；`module` 取 `src/<module>` 那一段（`runner`、`session`、`mcp`、`trigger`、`config`…），跨模块取改动最大的一处，仓库级杂项用 `repo`。
- 复杂改动在 body 用 `- ` 列表写细节。
- Message 只署真人贡献者，`Co-Authored-By:` 不写 AI。硬性规则，无例外。

## Agent skills

- **Issue tracker** — issue 与 spec 是 `.scratch/<feature-slug>/` 下的 markdown，规则见 `docs/agents/issue-tracker.md`。
- **Triage labels** — issue 文件的 `Status:` 行用哪五个标签，见 `docs/agents/triage-labels.md`。
- **Domain docs** — skill 怎么消费术语表与 ADR，细则见 `docs/agents/domain.md`。