# Trace 机制

多 backend 的 span 埋点机制，用于观测一次 task run 的完整执行过程。术语定义见 [GLOSSARY.md](../GLOSSARY.md)，架构决策见 [ADR-0001](adr/0001-self-built-span-model.md)，「默认开启 + 正文不脱敏」的取舍见 [ADR-0022](adr/0022-trace-on-by-default.md)。

## 目标与非目标

**目标**

- 为一次 run（`AgentRunner::run()`）产出结构化 span 树，可导出到 file / OpenTelemetry 等 backend
- **覆盖所有模型调用**：主 ReAct 回路的 `chat`、子代理、Dream、卡片生成（请求 + 回复）、会话压缩（`consolidation → chat ×N`）；新增 `provider.chat` / `chat_stream` 调用点必须补对应 span
- 新建配置默认可用：`setup` 产物即可在本地 `/traces` 看到请求与回复（ADR 0022）
- 埋点对主流程零侵入：禁用时零开销，backend 失败不影响 run
- 与现有 `AgentEvent`（UI 实时通知）、`log.rs`（日志）完全独立

**非目标（v1）**

- 不做 W3C `traceparent` 跨进程传播（进程内维护 span 父子关系即可）
- 不做采样（`enabled` 即全量记录）
- 不做 metrics / `/health`、trace 文件轮转与保留期、正文脱敏或字段级过滤
- 不做 Phoenix / Langfuse 等 OpenInference 专属属性映射；`/traces` 不加「messages 列表」专用视图
- 不 trace cron / heartbeat tick 本身的内部步骤（它们没有模型调用）；consolidation、Dream、子代理均有自己的 span

## 模块布局

```
src/trace/
├── mod.rs          # Tracer、TraceBackend trait、SpanStatus、SpanAttributes、SpanRecorder
├── span.rs         # SpanContext、SpanGuard（RAII）
└── backends/
    ├── mod.rs      # 从 TraceConfig 构建 backend 列表
    ├── file.rs     # JSONL 文件 backend
    └── otel.rs     # OpenTelemetry OTLP HTTP backend
```

命名约定：**不**建 `src/trace/context.rs`，避免与 `src/context.rs`（ContextBuilder）混淆。

依赖方向：`trace` 是叶子模块，不依赖 provider / runner / session；provider 与 runner 依赖 `trace`。`trace` 不直接依赖 `config`，由上层（AgentLoop / gateway）根据配置构建 `Tracer` 再注入。

## 核心模型

### SpanContext

```rust
pub struct SpanContext {
    pub trace_id: String,          // 32 位 hex（16 字节，uuid v4）
    pub span_id: String,           // 16 位 hex（8 字节随机）
    pub parent_span_id: Option<String>,
}
```

- trace_id 用 `uuid::Uuid::new_v4()`（已有依赖）
- span_id 用 8 字节随机 hex（新增 `rand` 或 `getrandom` 依赖）
- 格式与 W3C/OTel 兼容，OTel backend 可直接映射

### SpanGuard（RAII）

- `Tracer::start_span(name, parent: Option<&SpanContext>) -> SpanGuard`
- 构造即 `on_span_start` 广播给所有 backend
- 存活期通过 `&self` 方法累积：`set_status(Ok | Error(msg))`、`set_attribute(key, value)`（属性在创建后按需设置，无初始 attributes 参数）
- 实现 `SpanRecorder`（见下），provider 用它对 `chat` span 写 body
- `Drop` 时把累积的 attributes + status + 结束时间戳一次性 `on_span_end` 广播
- 内部 `Arc<Mutex<...>>`（`parking_lot`）累积状态，`Send + Sync` 满足跨 `.await` 存活
- **无 tracer 或零 backend 时，`start_span` 返回廉价 no-op guard**（Drop 空操作），骨架无需 `if let` 判断

### TraceBackend trait

```rust
pub trait TraceBackend: Send + Sync {
    fn on_span_start(&self, ctx: &SpanContext, name: &str, start: SystemTime);
    fn on_span_end(
        &self,
        ctx: &SpanContext,
        end: SystemTime,
        status: SpanStatus,
        attributes: &BTreeMap<String, serde_json::Value>,
    );
}
```

- **同步 `&self`、不可失败**（`Drop` 是同步的）。backend 内部吞错、缓冲、后台 flush
- 采用 start/end 两段（OTel 原生形状），v1 不做 span event

### Tracer

```rust
pub struct Tracer {
    backends: Vec<Arc<dyn TraceBackend>>,  // fan-out 多路
}
```

- 显式 DI：`Option<Arc<Tracer>>` 注入 `RunSpec`（与 provider / consolidator 一致），不设全局态
- 每个 backend 独立、隔离：任一 backend 出错/变慢不影响其它 backend 与 run

## 埋点位置（骨架直插，不耦合 hook）

| 调用点 | span 名 | 父 | 说明 |
|---|---|---|---|
| `AgentRunner::run()` 进入、`AfterTurn` 退出前 | `invoke_agent slimbot` | 无（根） | 用 guard 持有跨整个 run；`gen_ai.operation.name=invoke_agent` |
| 每次 ReAct 循环迭代 | `iteration` | `invoke_agent slimbot` | 框架自定义 span，包裹 chat + 工具循环 |
| 骨架调用 `provider.chat()` 处 | `chat` | `iteration` | guard 传给 provider 写 body；`gen_ai.operation.name=chat` |
| 骨架调用 `tool.execute()` 处 | `execute_tool {name}` | `iteration` | 如 `execute_tool shell`；`gen_ai.operation.name=execute_tool` |
| `CardService::generate()` 一次卡片生成 | `json_render.generate` | 无（独立 trace） | 卡片生成的父 span；`json_render.patch_count` + token usage |
| 卡片调用 `provider.chat_stream()` 处 | `chat` | `json_render.generate` | 与 `iteration` 下的 `chat` 同一套语义；guard 传给 provider 写 body，成功时把完整回复写进 `gen_ai.response.content`；被取消/停滞时以 `error.type=cancelled` 结束 |
| 一次会话压缩（`Consolidator`） | `consolidation` | 无（独立 span） | slimbot 自定义层，不设 `gen_ai.operation.name`；`consolidation.attempts`（本轮模型调用次数）、`consolidation.summary_chars`；失败时 `SpanStatus::Error` |
| 压缩里的每次模型调用 | `chat` | `consolidation` | 初次摘要与每次压缩重试各一个；请求体、usage、finish reason 与回复正文同主回路口径 |
| `ApprovalHook` 每次决策 | `approval` | 无（独立 span） | 审批审计的唯一出口（ADR 0011）：`approval.decision`（`proposed`/`redeemed`/`expired`/`cleared`/`denied_by_policy`）+ `approval.decided_by`（`policy`/`redemption`/`ttl`/`round_end`）+ `approval.id`/`owner_id`/`session_id`/`run_id`/`turn_kind`/`tool`/`args`/`args_hash`/`risk`/`ts_ms`；一次性 span，不做父子挂载，避免被 snipping/consolidation 影响 |

根 span 的 guard 生命周期覆盖整个 `run()` 的骨架作用域；`chat` / `execute_tool {name}` 的 guard 只包裹各自调用。

> span 名与 attribute 遵循 OpenTelemetry GenAI 语义约定（`gen-ai-agent-spans.md` / `gen-ai-spans.md`）：
> 根 span 对应 `invoke_agent`（进程内 agent 调用），chat span 对应 Inference（`chat`），工具 span 对应 `execute_tool`。
> `iteration` 是 slimbot 特有的中间层，spec 允许自定义 span，故不设 `gen_ai.operation.name`。
> 因 runner 在调用 provider 前创建 chat span、不知道 model，span 名用固定 `chat`（spec 允许自定义格式），model 由 provider 写为 `gen_ai.request.model` 属性。

## Attribute 清单

**`invoke_agent slimbot`**（根）：
- GenAI：`gen_ai.operation.name`（`invoke_agent`）、`gen_ai.agent.name`（`slimbot`）、`gen_ai.conversation.id`（session_id）、`gen_ai.usage.input_tokens`、`gen_ai.usage.output_tokens`、`gen_ai.conversation.compacted`（bool，consolidation 是否触发）、`error.type`（失败时）
- 自定义：`channel`、`chat_id`、`max_iterations`、`max_tokens`、`iterations`

**`iteration`**：`iteration`（序号）

**`chat`**：
- GenAI：`gen_ai.operation.name`（`chat`）、`gen_ai.provider.name`（如 `openai`）、`gen_ai.request.model`、`gen_ai.request.max_tokens`、`gen_ai.usage.input_tokens`、`gen_ai.usage.output_tokens`、`gen_ai.usage.cache_read.input_tokens`、`gen_ai.response.finish_reasons`（string 数组）、`gen_ai.response.content`（string，完整 content；无 content 的纯工具调用响应不设置）、`error.type`（失败时，`provider_error`）
- HTTP：`http.request.body`（**完整 body，字符串 attribute，非嵌套 JSON**；HTTP 标准 semconv，保留以在传输失败时可见）

以上 `chat` 口径对主回路、卡片生成（`chat_stream`）与 consolidation 三处一致：请求体由 provider 写、回复正文由调用方写。

**`consolidation`**：`consolidation.attempts`（本轮模型调用次数，含压缩重试）、`consolidation.summary_chars`（落进 history 的摘要长度）。

**`execute_tool {name}`**：`gen_ai.operation.name`（`execute_tool`）、`gen_ai.tool.name`、`gen_ai.agent.name`（`slimbot`）、`gen_ai.tool.call.id`、`gen_ai.tool.call.arguments`（截断 200 字符）、`error.type`（失败时，`tool_error`）。

`error.type` 为低基数错误类别（`provider_error` / `max_iterations` / `cancelled` / `tool_error` / `_OTHER`），详细消息在 span status；`gen_ai.tool.call.arguments` 截断 200 字符、tool 错误消息截断 500 字符；body 全量不截断（按决策）；`gen_ai.response.content` 同样全量不截断（便于在 trace 中检视模型输出，前端对超长字符串折叠展示）。

## 最终 body 捕获

- `Provider::chat` 增加可选参数：`chat(&self, messages, tools, recorder: Option<&dyn SpanRecorder>)`
- `SpanRecorder` 是单方法 trait：`fn set_attribute(&self, key: &str, value: serde_json::Value)`
- `OpenAIProvider` 在 `.json(&body)` 之前调用 `recorder.set_attribute("http.request.body", serde_json::to_string(&body)?)`
- 失败路径也能捕获（body 在发请求前已写入）
- 其余调用方（tracing 关闭、mock、tests）传 `None`

## Backend

### file

- 每 span 结束（`on_span_end`）时同步 append 一行 JSONL：`{trace_id, span_id, parent_span_id, name, start, end, duration_us, status, attributes}`
- 写入用 `parking_lot::Mutex<File>` 串行化（已有依赖）
- **文件生命周期（进程内 append，跨进程截断）**：进程内保持 append-only（append 句柄在截断后仍然有效，O_APPEND 写入落在新末尾）；进程内崩溃只丢未结束的 span。默认在每次 Gateway 启动时截断（`tracing.reset_on_start`，默认 `true`），因此 trace 文件只保留当前进程运行期间的记录；CLI 模式从不截断，不会踩踏 Gateway 的 trace 数据。

### otel

- 依赖 `opentelemetry` / `opentelemetry_sdk` / `opentelemetry-otlp`（`http-proto` feature）
- `SpanExporter::builder().with_http().with_endpoint(endpoint)`，默认 `http://localhost:4318`
- **endpoint 补全**：OTLP HTTP 导出器对显式传入的 endpoint 原样使用（不像环境变量那样追加信号路径），`http://host:4318` 会 POST 到 `/` 被标准 collector 拒绝。`OtelBackend` 在 endpoint 无路径时补 `/v1/traces`，已带路径（含自定义前缀）时原样使用；由本地假 collector 的端到端测试钉住
- `on_span_start` 创建 SDK span（live 可见），`on_span_end` 写入 attributes + status 后 end
- 用 SDK 的 `BatchSpanProcessor` 批量导出；`Tracer` 析构时 flush/shutdown
- trace_id/span_id 复用 `SpanContext` 的 hex，映射为 OTel 标准 ID

## Config

`setup` 产物（新建安装）：

```json
{
  "tracing": {
    "enabled": true,
    "reset_on_start": true,
    "backends": [
      { "type": "file", "enabled": true, "path": "" },
      { "type": "otel", "enabled": false, "endpoint": "http://localhost:4318", "service_name": "slimbot" }
    ]
  }
}
```

- `tracing.enabled` 总开关；`backends` 数组天然支持同 type 多实例（fan-out 按顺序逐个构造）
- 每个 backend 有独立 `enabled`
- **默认值只存在于 `setup` 产物里**：`TraceConfig::default()` 仍是 `enabled: false` + 空 `backends`，因此既有 config.json 不会被偷偷打开，也不会因升级开始写盘（ADR 0022）；要开就重跑 `setup` 或手工加这一段
- 「`enabled` 为 true 但没有任何启用 backend」仍是零开销的 no-op：不隐式造 file backend
- `tracing.reset_on_start`（默认 `true`）：Gateway 启动时截断所有启用 file backend 的 trace 文件；`false` 时保留历史。仅 Gateway 模式读取，CLI 模式从不截断
- **启动可见性**：tracer 构建时为每个 trace 文件打一行 info 日志，形如 `[Trace] File backend: /…/workspace/traces.jsonl (12.3 KiB, CLI appends, gateway truncates on start)`，让增长在长跑之前被看见（不新增轮转或保留期）
- file backend 的 `path` 为空时，在接线层解析为 `{workspace_dir}/traces.jsonl`（默认，与其它 workspace 产物同处）；非空时原样使用（用户可自定义任意路径）。接线层的 `resolve_trace_file_paths` 是唯一解析函数，`build_tracer`（写）与 WebUI 查看器（读）共用，保证写入与读取指向同一文件
- 走 `define_config!` 宏生成 `TraceConfig` / `TraceBackendEntry`，挂到根 `Config`

## 本地接一个 OTel collector

`otel` backend 只是出口，不是一个必须常驻的组件；本地默认就是「不跑 collector、用 `/traces` 看」。想用标准 OTel 工具链时，任一 OTLP/HTTP 兼容后端均可（本仓库不安装、不依赖任何一个）：

```bash
# A) Phoenix（LLM 视角的 UI；本机已有 uv，无需容器）
uvx arize-phoenix serve          # UI http://localhost:6006，OTLP http 也在 6006

# B) Jaeger v2 单二进制（brew 无 formula，取自 GitHub releases；需要一份 otelcol 风格 config）
#    OTLP http receiver 默认 4318，UI 在 16686
```

切换只需把 `tracing.backends` 里 `otel` 项的 `enabled` 改为 `true`，endpoint 按需指向 collector（Phoenix 用 `http://localhost:6006/v1/traces`，Jaeger 用默认的 `http://localhost:4318` 即可，无路径时会自动补 `/v1/traces`）。

## WebUI Trace 查看器

- `GET /traces` 提供独立 HTML 页面（`resources/webui/traces.html`，嵌入 `webui/traces.html`），与聊天页共用 `webui/common.css`（视觉语言单一来源）
- `GET /traces/api/traces`：解析 trace JSONL，按 trace_id 分组，返回按开始时间降序（新→旧）的 trace 摘要（trace_id 短前缀、开始时间、状态、span 数量、总时长、根 span 名、running 标记）与 `startup_ms`。`startup_ms` 是 Gateway 启动时实际执行 trace 文件截断/创建的时刻（接线层 `run_gateway` 在 `reset_trace_files` 成功后记录），**不是文件 mtime**（mtime 每次追加都会更新，不能代表启动时间）；`tracing` 关闭或 `reset_on_start: false` 时为 `None`（无明确的启动边界可展示）
- `GET /traces/api/trace/{id}`：单个 trace 的完整 span 树（按 `parent_span_id` 嵌套），每个 span 携带 name/start/end/duration_us/status/error/完整 attributes；无此 trace 返回 404
- 进行中 trace（根 span 未结束 / 无根）标记为 `running`；trace 开始时间取根 span 的 start，无根时取最小 span start
- 大文件保护：超过 `MAX_TRACE_FILE_BYTES`（50 MiB）的文件仍读取，但通过 `too_large` 提示并告警
- 解析/分组/排序/建树在 `src/trace_viewer.rs` 纯函数中（可单测），HTTP handler 为薄壳
- 前端：列表按新→旧、trace 展开为 span 树、span 展开显示 attributes 键值表；大 JSON（如 `http.request.body`）默认折叠点击展开，超长字符串（>200 字符）截断可展开；空态 / 禁用态文案；聊天页 ↔ trace 页双向互链

## 状态语义

根 span `set_status` 映射（与 `AgentResult.success` 对齐）：

| 结束条件 | status | `error.type` |
|---|---|---|
| 模型返回无 tool_calls | Ok | — |
| 超出 `max_iterations` | Error | `max_iterations` |
| LLM 调用错误 | Error | `provider_error` |
| hook 返回 `Abort` | Error | `_OTHER` |
| 取消（`/stop` / 预取消） | Error | `cancelled` |

## 边界

- **与 AgentEvent**：完全独立，各自在埋点处发出，互不派生
- **与 log.rs**：完全独立，v1 不互相引用、不注入 trace_id；需要关联时靠 `session_id` 事后 join
- **隐私**：`http.request.body` 含完整 system prompt、记忆注入与工具结果，`gen_ai.response.content` 含完整模型输出；落盘后的 trace 文件等同会话正文，勿外传、勿当作可公开的调试样本。v1 不做脱敏，靠 `reset_on_start` 与 50 MiB 告警兜体积

## 测试点（TDD）

- `SpanContext`：trace_id 32 hex / span_id 16 hex / 父子链接
- `SpanGuard`：构造触发 `on_span_start`；`Drop` 触发 `on_span_end` 且携带累积的 attributes/status；`set_attribute`/`set_status` 累积正确；跨 `.await`（`Send`）
- `Tracer`：`start_span` 父子 `parent_span_id` 正确；零 backend 时 no-op 廉价
- fan-out：所有 backend 都收到 start/end；单 backend 报错不影响其它 backend 与 run
- file backend：每 span 结束写一行 JSONL，字段完整
- `setup`：新建配置的 `tracing` 段为 `enabled: true` + file + otel（关闭）；既有配置不被改写
- consolidation：`consolidation → chat` 父子结构、`chat` 带完整请求体、压缩重试多一个子 span、失败时父 span 为 error
- 卡片：`chat` span 带 `gen_ai.response.content`（完整回复）；取消/失败不写正文
- otel backend：构造 + 导出 smoke；本地假 collector 的端到端断言（`POST /v1/traces`、请求体与回复的标记串、bare endpoint 补全路径）
- 启动日志：CLI 打出 trace 路径、大小与追加语义
- runner 集成（mock backend 捕获）：span 树层级 `invoke_agent slimbot → iteration → chat / execute_tool {name}` 正确、attribute 齐全（`gen_ai.*`）；失败 run → 根 span `Error` + `error.type`、成功 → `Ok`
- body 捕获：provider 单测（mock recorder）断言 `http.request.body` 为完整序列化 body
- otel backend：数组 attribute（如 `gen_ai.response.finish_reasons`）导出为 OTel array
- config：解析 `tracing` 段、同 type 多实例、默认值
