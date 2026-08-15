# Trace 机制

多 backend 的 span 埋点机制，用于观测一次 task run 的完整执行过程。术语定义见 [CONTEXT.md](../CONTEXT.md)，架构决策见 [ADR-0001](adr/0001-self-built-span-model.md)。

## 目标与非目标

**目标**

- 为一次 run（`AgentRunner::run()`）产出结构化 span 树，可导出到 file / OpenTelemetry 等 backend
- 埋点对主流程零侵入：禁用时零开销，backend 失败不影响 run
- 与现有 `AgentEvent`（UI 实时通知）、`log.rs`（日志）完全独立

**非目标（v1）**

- 不做 W3C `traceparent` 跨进程传播（进程内维护 span 父子关系即可）
- 不做采样（`enabled` 即全量记录）
- 不 trace cron / heartbeat / dream / consolidation（consolidation 仅作为根 span 的 attribute）

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
- 实现 `SpanRecorder`（见下），provider 用它对 `provider.chat` span 写 body
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
| `AgentRunner::run()` 进入、`AfterTurn` 退出前 | `agent.run` | 无（根） | 用 guard 持有跨整个 run |
| 每次 ReAct 循环迭代 | `iteration` | `agent.run` | 包裹 provider.chat + 工具循环 |
| 骨架调用 `provider.chat()` 处 | `provider.chat` | `iteration` | guard 传给 provider 写 body |
| 骨架调用 `tool.execute()` 处 | `tool.{name}` | `iteration` | 如 `tool.shell` |

根 span 的 guard 生命周期覆盖整个 `run()` 的骨架作用域；`provider.chat` / `tool.{name}` 的 guard 只包裹各自调用。

## Attribute 清单

**`agent.run`**（根）：`session_id`、`channel`、`chat_id`、`max_iterations`、`max_tokens`、`total_tokens`、`prompt_tokens`、`completion_tokens`、`prompt_cache_hit_tokens`、`iterations`、`consolidated`（bool）

**`iteration`**：`iteration`（序号）

**`provider.chat`**：`provider`（type）、`model`、`prompt_tokens`、`completion_tokens`、`total_tokens`、`prompt_cache_hit_tokens`、`finish_reason`、`http.request.body`（**完整 body，字符串 attribute，非嵌套 JSON**）

**`tool.{name}`**：`tool_call_id`、`args`（截断 200 字符）、`error`（失败时，截断 500 字符）。不存 `tool.name`（已在 span 名中）。

敏感内容截断：`args` 200 字符、`error` 500 字符；body 全量不截断（按决策）。

## 最终 body 捕获

- `Provider::chat` 增加可选参数：`chat(&self, messages, tools, recorder: Option<&dyn SpanRecorder>)`
- `SpanRecorder` 是单方法 trait：`fn set_attribute(&self, key: &str, value: serde_json::Value)`
- `OpenAIProvider` 在 `.json(&body)` 之前调用 `recorder.set_attribute("http.request.body", serde_json::to_string(&body)?)`
- 失败路径也能捕获（body 在发请求前已写入）
- 其余调用方（`consolidate.rs` 两处、mock、tests）传 `None`

## Backend

### file

- 每 span 结束（`on_span_end`）时同步 append 一行 JSONL：`{trace_id, span_id, parent_span_id, name, start, end, duration_us, status, attributes}`
- 写入用 `parking_lot::Mutex<File>` 串行化（已有依赖）
- 进程崩溃不丢已结束的 span（未结束的本就该丢）

### otel

- 依赖 `opentelemetry` / `opentelemetry_sdk` / `opentelemetry-otlp`（`http-proto` feature）
- `SpanExporter::builder().with_http().with_endpoint(endpoint)`，默认 `http://localhost:4318`
- `on_span_start` 创建 SDK span（live 可见），`on_span_end` 写入 attributes + status 后 end
- 用 SDK 的 `BatchSpanProcessor` 批量导出；`Tracer` 析构时 flush/shutdown
- trace_id/span_id 复用 `SpanContext` 的 hex，映射为 OTel 标准 ID

## Config

```json
{
  "tracing": {
    "enabled": false,
    "backends": [
      { "type": "file", "enabled": true, "path": "~/.slimbot/traces.jsonl" },
      { "type": "otel", "enabled": true, "endpoint": "http://localhost:4318", "service_name": "slimbot" }
    ]
  }
}
```

- `tracing.enabled` 总开关；`backends` 数组天然支持同 type 多实例（fan-out 按顺序逐个构造）
- 每个 backend 有独立 `enabled`
- 走 `define_config!` 宏生成 `TraceConfig` / `TraceBackendEntry`，挂到根 `Config`

## 状态语义

根 span `set_status` 映射（与 `AgentResult.success` 对齐）：

| 结束条件 | status |
|---|---|
| 模型返回无 tool_calls | Ok |
| 超出 `max_iterations` | Error |
| LLM 调用错误 | Error |
| hook 返回 `Abort` | Error |
| 取消 | Error |

## 边界

- **与 AgentEvent**：完全独立，各自在埋点处发出，互不派生
- **与 log.rs**：完全独立，v1 不互相引用、不注入 trace_id；需要关联时靠 `session_id` 事后 join

## 测试点（TDD）

- `SpanContext`：trace_id 32 hex / span_id 16 hex / 父子链接
- `SpanGuard`：构造触发 `on_span_start`；`Drop` 触发 `on_span_end` 且携带累积的 attributes/status；`set_attribute`/`set_status` 累积正确；跨 `.await`（`Send`）
- `Tracer`：`start_span` 父子 `parent_span_id` 正确；零 backend 时 no-op 廉价
- fan-out：所有 backend 都收到 start/end；单 backend 报错不影响其它 backend 与 run
- file backend：每 span 结束写一行 JSONL，字段完整
- otel backend：构造 + 导出 smoke（可选：本地 mock OTLP 服务）
- runner 集成（mock backend 捕获）：span 树层级 `agent.run → iteration → provider.chat / tool.{name}` 正确、attribute 齐全；失败 run → 根 span `Error`、成功 → `Ok`
- body 捕获：provider 单测（mock recorder）断言 `http.request.body` 为完整序列化 body
- config：解析 `tracing` 段、同 type 多实例、默认值
