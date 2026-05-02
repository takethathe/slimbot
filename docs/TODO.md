# TODO

## 缺失工具（对齐 nanobot）

### 核心工具

- [ ] **grep / glob** — 内容搜索和文件发现工具，支持 `output_mode`（`files_with_matches` / `count` / 默认带行号）、`glob` 过滤、`type` 过滤、`head_limit` / `offset` 分页、`context_before` / `context_after`
- [ ] **web_search / web_fetch** — 网络搜索和网页抓取工具，支持搜索关键词和 URL 获取，返回纯文本内容

### 高级工具

- [ ] **spawn** — 后台子代理创建，支持传入指令和等待结果，用于并行任务执行
- [ ] **mcp** — MCP 服务器客户端，连接外部 MCP 服务并将其工具包装为原生工具
- [ ] **self** — 运行时状态检查，返回当前 session 信息、配置、可用工具列表等
- [ ] **message** — 向用户发送消息（用于 channel 推送场景，如 telegram/whatsapp）
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

## 缺失工具（对齐 nanobot）

### 核心工具

- [ ] **grep / glob** — 内容搜索和文件发现工具，支持 `output_mode`（`files_with_matches` / `count` / 默认带行号）、`glob` 过滤、`type` 过滤、`head_limit` / `offset` 分页、`context_before` / `context_after`
- [ ] **web_search / web_fetch** — 网络搜索和网页抓取工具，支持搜索关键词和 URL 获取，返回纯文本内容

### 高级工具

- [ ] **spawn** — 后台子代理创建，支持传入指令和等待结果，用于并行任务执行
- [ ] **mcp** — MCP 服务器客户端，连接外部 MCP 服务并将其工具包装为原生工具
- [ ] **self** — 运行时状态检查，返回当前 session 信息、配置、可用工具列表等
- [ ] **message** — 向用户发送消息（用于 channel 推送场景，如 telegram/whatsapp）
- [ ] **notebook** — Jupyter notebook (`.ipynb`) 编辑工具，支持代码单元格和 markdown 单元格增删改
- [ ] **sandbox** — shell 命令沙箱后端，支持不同的执行隔离策略（docker/ssh/local）

## Heartbeat 机制

- [ ] 实现 heartbeat 调度器，固定间隔时间运行任务
- [ ] 支持 `config.json` 配置 `agent.heartbeat_interval_minutes`，默认 30 分钟
- [ ] 定义 `workspace/heartbeat.jsonl` 任务格式（任务描述、调度信息、状态）
- [ ] `AgentLoop` 初始化时启动 heartbeat 定时器
- [ ] 定时器触发时读取 `heartbeat.jsonl`，通过 `MessageBus` 提交任务
- [ ] 执行完成后标记任务状态（completed/failed）

## Cron 定时任务机制

- [ ] 实现 `cron_add` / `cron_list` / `cron_remove` 工具，供 Agent 管理定时任务
- [ ] 定义 `workspace/cron.jsonl` 格式（cron 表达式、任务描述、创建者 channel_id/chat_id、创建时间、状态）
- [ ] 实现后台 cron 调度器，每分钟检查并匹配应执行的任务
- [ ] 触发的任务通过 `MessageBus` 提交到 `AgentRunner` 作为后台任务运行
- [ ] 模型判断执行结果是否需要 deliver 到创建任务的 channel，需要则投递

## Memory 记忆系统

- [x] 实现 memory 模块（独立模块，非工具），管理 `workspace/memory/MEMORY.md` 和 `history.jsonl`
- [x] 双层记忆：`MEMORY.md` 存储长期记忆（用户偏好、项目上下文、关键决策），`history.jsonl` 存储按时间追加的短期交互记录
- [x] `MemoryStore` 纯文件 I/O 层：读写 MEMORY.md、history.jsonl、SOUL.md、USER.md
- [x] `ContextBuilder` 构建时将 `MEMORY.md` 内容注入 system prompt，标记 `# Memory` 段落
- [x] history.jsonl 格式：每条包含 `cursor`（自增整数）、`timestamp`、`content`
- [x] Dream cursor：`.dream_cursor` 文件记录 Dream 已处理的最后一条 history 游标
- [ ] Token-budget 触发 consolidate：session 结束交互时检查 token 使用量，超出 `context_window_limit`（config 可配置，默认 6k）则启动 consolidate
- [ ] Consolidate 由模型对当前 session 的对话进行摘要，结果追加到 `history.jsonl`
- [x] Session 记录 `last_consolidated` 游标，已 consolidate 的 message 不再添加到上下文中
- [ ] 用最后一次 consolidate 的摘要代替所有已 evict 的 message 内容，注入到 system prompt
- [ ] Consolidate 在 user-turn 边界进行消息驱逐（eviction），保证语义完整性
- [ ] Consolidate 支持多轮循环（nanobot 默认最多 5 轮），每轮重新估算 token 数
- [ ] Consolidate 失败时降级为 raw archive（直接写入 history.jsonl 带 `[RAW]` 标记）
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
