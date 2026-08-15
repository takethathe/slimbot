# 自研 span 模型 + RAII 生命周期，而非直接采用 tracing/opentelemetry crate

SlimBot 需要多 backend 的 trace 机制。我们决定自研最小 span 模型（`Span` / `SpanContext` / `TraceBackend` trait），用 RAII guard 管理 span 生命周期（构造即 start、Drop 即 end），span 在骨架调用点单独创建、不耦合 phase hook；backend 多路 fan-out 且不可失败；trace 与 `AgentEvent` 独立共存、互不派生；trace context 仅进程内维护。

评估过直接采用 `tracing` + `tracing-subscriber`（layer 即 backend）+ `tracing-opentelemetry`。放弃原因：file backend 在自研模型下退化为「写 JSONL」级别的琐事；`tracing` 的宏式埋点与 thread-local current span 在 async 下需额外处理；且会与现有手写 `log.rs` 的全局状态并存。代价是失去 `tracing` 生态的宏式埋点 —— 若未来需要再迁移。
