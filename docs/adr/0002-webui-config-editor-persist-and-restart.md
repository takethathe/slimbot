# WebUI 配置编辑持久化到文件、重启生效（v1 不做进程内热重载）

WebUI 的 Config editor 通过基于路径的 patch 写回 `config.json`，改动仅在 Gateway 重启后生效；我们刻意不接线当时的热重载设施（`Config::init` / `notify` watcher / `Config::subscribe`）。原因：这些设施当时是死代码——`main.rs` 与 `gateway.rs` 都直接把配置读进本地 `Arc<Config>`，且 cron/heartbeat/dream/ToolManager/AgentLoop 都在启动时用初始快照构建，让「保存即生效」需要逐组件重放配置，那是独立的工程。重启生效诚实且低风险。代价是编辑后必须重启。

**2026-11-11 补注（架构深化实现票 02）**：决策不变，两处事实随实现更新。① 真实启动入口是 `Config::load_resolved`（`read → resolve_secrets → validate`），CLI 与 gateway 都走它；`Config::load_for_preview` 是给 setup / WebUI 编辑器看的、刻意不校验的磁盘视图，不是生效入口。② 从未被调用的 `Config::init` / watcher / `subscribe` / `compute_diff` / `ConfigChange` 整套热重载设施已删除，`notify` 依赖一并移除；进程改为在启动时 `Config::install` 一份**不可变快照**，供只拿到 workspace 路径的消费方（`web_fetch` 工具、快照保留策略）读取。因此原文「若未来需要热生效，`compute_diff` 变更语义已就位」不再成立：热生效仍需重新设计，本 ADR 的重启生效约束不受影响。
