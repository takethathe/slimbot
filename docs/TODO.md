# TODO

## 缺失工具（对齐 nanobot）

### 核心工具

- [ ] **grep / glob** — 内容搜索和文件发现工具，支持 `output_mode`（`files_with_matches` / `count` / 默认带行号）、`glob` 过滤、`type` 过滤、`head_limit` / `offset` 分页、`context_before` / `context_after`
- [ ] **web_search / web_fetch** — 网络搜索和网页抓取工具，支持搜索关键词和 URL 获取，返回纯文本内容

### 高级工具

- [ ] **spawn** — 后台子代理创建，支持传入指令和等待结果，用于并行任务执行
- [ ] **mcp** — MCP 服务器客户端，连接外部 MCP 服务并将其工具包装为原生工具
- [ ] **self** — 运行时状态检查，返回当前 session 信息、配置、可用工具列表等
- [x] **message** — 向用户发送消息（用于 channel 推送场景，如 telegram/whatsapp）
- [ ] **notebook** — Jupyter notebook (`.ipynb`) 编辑工具，支持代码单元格和 markdown 单元格增删改
- [ ] **sandbox** — shell 命令沙箱后端，支持不同的执行隔离策略（docker/ssh/local）

### 可选 Skills（nanobot 内置）

- [ ] **github** — 使用 `gh` CLI 与 GitHub 交互（PR/Issue/repo 操作）
- [ ] **weather** — 通过 wttr.in 和 Open-Meteo 获取天气信息
- [ ] **summarize** — 摘要生成技能（URL、文件、YouTube 视频）
- [ ] **tmux** — 远程操控 tmux 会话
- [ ] **clawhub** — ClawHub 技能搜索和安装（公共技能注册表）
- [ ] **skill-creator** — 创建新技能的辅助工具

## 增强 config 模块

- [ ] 增强 `config_scheme.rs`，生成包含所有配置项的完整 config 文件
- [ ] `setup` 命令调用 config 模块生成 `config.json`，所有配置项使用默认值作为初始值
- [ ] 默认 config 包含完整的 `data_dir`、`workspace_dir`、`agent`、`providers`、`tools`、`channels` 配置
- [ ] 用户只需修改必填项（如 `api_key`）即可直接使用

## Heartbeat 机制

- [x] 实现 heartbeat 调度器，固定间隔时间运行任务
- [x] 支持 `config.json` 配置 `gateway.heartbeat.enabled` 和 `interval_s`
- [x] `AgentLoop` 初始化时启动 heartbeat 定时器（gateway 模式）
- [x] 定时器触发时读取 `workspace/HEARTBEAT.md`，通过 `AgentLoop.run_task()` 执行任务
- [x] 执行完成后通过 outbound 投递结果到默认 channel
- [ ] 支持 `workspace/heartbeat.jsonl` 任务格式（多任务调度）

## Cron 定时任务机制

- [x] 实现 `cron` 工具（add/list/remove actions），供 Agent 管理定时任务
- [x] Cron 服务实现：`CronService` 支持 `at`/`every`/`cron` 三种调度
- [x] JSON 持久化存储（`workspace/cron/jobs.json`）
- [x] 后台 cron 调度器，每秒 tick 检查并触发到期任务
- [x] 触发的任务通过 `AgentLoop.run_task()` 执行
- [x] 模型判断执行结果是否需要 deliver 到 channel（`payload.deliver` + `channel`/`to` 字段）

## Memory 记忆系统

- [x] 实现 memory 模块（独立模块，非工具），管理 `workspace/memory/MEMORY.md` 和 `history.jsonl`
- [x] 双层记忆：`MEMORY.md` 存储长期记忆（用户偏好、项目上下文、关键决策），`history.jsonl` 存储按时间追加的短期交互记录
- [x] `MemoryStore` 纯文件 I/O 层：读写 MEMORY.md、history.jsonl、SOUL.md、USER.md
- [x] `ContextBuilder` 构建时将 `MEMORY.md` 内容注入 system prompt，标记 `# Memory` 段落
- [x] history.jsonl 格式：每条包含 `cursor`（自增整数）、`timestamp`、`content`
- [x] Dream cursor：`.dream_cursor` 文件记录 Dream 已处理的最后一条 history 游标
- [x] Token-budget 触发 consolidate：LLM 返回 prompt_tokens 后检查预算（`context_window_tokens - max_completion_tokens - SAFETY_BUFFER`），超出时触发摘要
- [x] Consolidate 由模型对当前 session 的对话进行摘要，结果追加到 `history.jsonl`
- [x] Session 记录 `last_consolidated` 游标，已 consolidate 的 message 不再添加到上下文中
- [x] 用最后一次 consolidate 的摘要（`last_summary` 字段）代替所有已 evict 的 message 内容，注入到 system prompt 的 `[Resumed Session]` 段落
- [x] Consolidate 在 user-turn 边界进行消息驱逐（eviction），保证语义完整性
- [x] Consolidate 支持会话摘要持久化到 `SessionMeta`，重启后可恢复
- [x] Consolidate 使用 `char_per_token_ratio` 估算消息 token（`total_chars / prompt_tokens`，默认 4.0）
- [x] Consolidate 失败时降级处理（`archive` 返回错误时静默跳过，不中断主循环）
- [x] Session 持久化重构：meta 数据单独 `.meta.json` 文件，messages 使用 append-only 写入 JSONL，每条消息自增 ID，consolidation 游标基于 ID 而非数组索引
- [ ] Agent 可通过工具调用主动读取/搜索/更新记忆
- [x] 移除当前 `src/tools/memory.rs` 中的三个独立工具实现，改为 memory 模块内部管理

## Dream 定时记忆整理

- [ ] 实现 Dream 调度器，基于 cron 定时触发（如每天凌晨）
- [ ] Phase 1：分析未处理的 `history.jsonl` 记录和当前 `MEMORY.md`，生成修改计划
- [ ] Phase 2：使用 AgentRunner + file editor 工具执行有针对性的文件编辑
- [ ] 支持通过 git log 为记忆行添加年龄标注（line age annotation）
- [ ] Dream 可创建新 skill 文件到 `workspace/skills/`，基于历史中的重复模式
- [ ] Dream 编辑结果记录到 `history.jsonl` 作为审计日志

## Context Builder 对齐 nanobot

- [ ] **Identity 段落**：将当前 `fixed_intro()` 替换为结构化 identity 段落，包含 runtime 信息（OS、平台、工作区路径），参考 nanobot `identity.md` 模板
- [ ] **Platform Policy**：在 identity 中注入平台策略（POSIX/Windows），提示 file tool 优先于 shell 命令
- [ ] **Channel Format Hint**：根据当前 channel 类型（telegram/whatsapp/cli/email）注入格式提示，指导模型适配输出格式
- [ ] **Runtime Context 块**：在 user message 前注入 `[Runtime Context]` 元数据块（Current Time、Channel、Chat ID），而非拼入 system prompt
- [ ] **Session Summary 注入**：AutoCompact/Consolidate 后的摘要以 `[Resumed Session]` 标记注入 runtime context，而非 system prompt
- [ ] **Skills 加载区分 always/可选**：区分 always skills（始终加载完整内容）和可选 skills（仅显示名称列表，由 Agent 按需读取）
- [x] **Recent History 注入**：system prompt 末尾追加 `# Recent History` 段落，从 `history.jsonl` 读取未处理的最近 N 条（nanobot 默认 50 条）
- [ ] **Memory 跳过模板**：`MEMORY.md` 内容与模板一致时不注入 system prompt（对齐 `_is_template_content` 逻辑）
- [ ] **Message 合并**：当 history 最后一条消息与当前消息 role 相同时，合并内容而非追加同 role 消息（避免 provider 拒绝连续同 role 消息）
- [ ] **Media 支持**：`build_messages` 支持 base64 图片内联（多模态消息格式）
- [ ] **工具结果追加**：`add_tool_result` 辅助方法确保 tool message 格式正确（tool_call_id + name）
- [ ] **Thinking/Reasoning 支持**：assistant message 支持 `reasoning_content` / `thinking_blocks` 字段（为未来 reasoning 模型预留）

## AutoCompact 空闲会话压缩

- [ ] 实现 AutoCompact 调度器，TTL-based 空闲会话检测（config 可配置 idle TTL，默认 24h）
- [ ] 定时扫描所有 session，识别超过 TTL 未活动的会话
- [ ] 对空闲会话执行压缩：分离未 consolidate 的尾部消息，保留最近 N 条消息（默认 8 条）
- [ ] 使用 Consolidator 对剩余消息进行 LLM 摘要
- [ ] Session 重新加载时注入摘要到 system prompt，代替已归档的消息
- [ ] 压缩结果持久化到 session JSONL 文件

- [ ] **时间戳注入**：在构造 context 时，注入的 history 消息中，每条 `role=user` 的消息需在开头增加时间戳，用于处理相对时间引用（如"昨天xxx"、"刚才"等）
- [ ] **WebUI 空消息过滤**：WebUI 显示时，过滤掉 trim 后内容为空的消息，避免空白气泡占用界面

## 用户命令处理机制

- [x] 实现三层命令处理架构
- [x] **Channel 层**：`/quit`、`/exit` 在 IoScheduler 中拦截，绕过 MessageBus 直接触发退出
- [x] **AgentLoop 层**：`/stop`（停止所有运行中任务）、`/clear` 或 `/new`（清空当前会话）、`/status`（显示会话状态）
- [x] **AgentRunner 层**：其他斜杠命令（如 `/help`）作为正常任务流经 ReAct 循环，由模型处理
- [x] `/stop` 使用 per-session CancellationToken 机制取消当前和队列中的所有任务
- [x] `/stop` 取消后 enqueue 哨兵 `/stop` User 消息，等待所有 pending task 排空后返回 "all tasks cancelled"
- [x] AgentRunner 在运行开始、LLM 请求前、每个 tool call 前检查取消状态
- [x] 退出命令直接触发：cancel 所有 task、停止所有 channel、shutdown thread pool，然后正常退出

## CLI Markdown 输出支持

- [ ] 增加 CLI 输出的 Markdown 格式处理
- [ ] 解析 LLM 返回的 Markdown 内容并在终端中渲染（加粗、代码块、列表等）
- [ ] 可选使用 `comfy-table` / `termimad` / `bat` 等库实现终端 Markdown 渲染
- [ ] 保持与非 Markdown 通道（如 Telegram）输出兼容性

## LLM Cache 功能

- [x] `ProviderConfig` 新增 `prompt_cache_enabled` 字段（默认 true），通过 serde 自动反序列化
- [x] OpenAI provider 在最后一条 system message 的 content 上注入 `cache_control: {"type": "ephemeral"}`
- [x] `setup.rs` `merge_provider` 支持 partial config 中的 `prompt_cache_enabled` 字段

## Gateway 模式

- [x] `slimbot gateway` 命令启动 cron、heartbeat 和 enabled channels
- [x] CLI channel 不在 config.channels 中配置，仅在 CLI/agent 模式隐式启用
- [x] Channel 配置 key 即为 type，无需 `type` 字段
- [x] WebUI channel：axum HTTP server，SSE 流式输出，嵌入 index.html
- [x] Gateway 主线程为 ChannelManager 监听 output message
- [x] Cron 与 Heartbeat 集成到 AgentLoop 通过 callback 触发任务
- [x] 完整的单元测试覆盖：cron service (38 tests), heartbeat service (12 tests), webui channel (11 tests), channel manager (9 tests), message tool (6 tests), cron tool (13 tests)
