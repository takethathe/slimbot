# WebUI 配置编辑持久化到文件、重启生效（v1 不做进程内热重载）

WebUI 的 Config editor 通过基于路径的 patch 写回 `config.json`，改动仅在 Gateway 重启后生效；我们刻意不接线现有的热重载设施（`Config::init` / `notify` watcher / `Config::subscribe`）。原因：这些设施目前是死代码——`main.rs` 与 `gateway.rs` 都直接 `Config::load()` 到本地 `Arc<Config>`，且 cron/heartbeat/dream/ToolManager/AgentLoop 都在启动时用初始快照构建，让「保存即生效」需要逐组件重放配置，那是独立的工程。重启生效诚实且低风险。代价是编辑后必须重启；若未来需要热生效，path-patch 写入接口与 `compute_diff` 变更语义都已就位，可在其上叠加订阅回调。
