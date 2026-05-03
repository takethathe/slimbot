# SlimBot 文档索引

## 入门

| 文档 | 说明 |
|------|------|
| [配置指南](config.md) | `config.json` 结构、验证规则、多 Provider 配置 |
| [内置工具](tools.md) | 6 个内置工具：shell、file_reader/writer/editor、list_dir、make_dir |
| [日志系统](logging.md) | 日志级别、CLI 参数、输出格式 |

## 核心模块

| 文档 | 对应模块 | 说明 |
|------|----------|------|
| [架构概览](architecture.md) | 整体 | 系统架构、数据流、组件交互 |
| [Agent 循环](agent_loop.md) | `agent_loop.rs` | 顶层编排、组件初始化 |
| [ReAct 执行器](runner.md) | `runner.rs` | ReAct 循环核心：迭代控制、工具调用流程 |
| [上下文构建器](context.md) | `context.rs` | System Prompt 构建流程 |
| [会话管理](session.md) | `session.rs` | 会话生命周期、任务队列、JSONL 持久化 |
| [会话压缩](consolidate.md) | `consolidate.rs` | Token 预算触发摘要、消息驱逐、上下文注入 |
| [消息总线](message_bus.md) | `message_bus.rs` | 请求分发、任务封装、结果路由 |
| [Provider](provider.md) | `provider/` | Provider 接口、OpenAI 兼容 API 集成 |
| [通道](channel.md) | `channel/` | 通道抽象、工厂模式、并发 I/O 循环 |

## 基础设施模块

| 文档 | 对应模块 | 说明 |
|------|----------|------|
| [日志系统](logging.md) | `log.rs`, `macros.rs` | 全局单例日志、级别过滤、彩色终端输出 |
| [路径管理](path.md) | `path.rs` | 路径解析、默认值、沙箱验证、波浪号展开 |
| [工具系统](tool.md) | `tool.rs`, `tools/` | Tool trait、ToolManager、工具结果处理 |
| [内存与历史](memory.md) | `memory.rs` | 长期记忆、历史记录、游标管理 |
| [工作池](worker.md) | `worker.rs` | 异步 Worker 与动态 WorkerPool |
| [I/O 调度器](io_scheduler.md) | `io_scheduler.rs` | 阻塞读取循环、stdin 处理 |
| [配置方案](config_scheme.md) | `config_scheme.rs` | 默认值、配置规范化、URL 派生 |
| [Bootstrap 模板](bootstrap.md) | `bootstrap.rs`, `embed.rs` | 嵌入文件、模板加载、技能文件 |
| [工具函数](utils.md) | `utils/mod.rs` | 文本截断、原子写入、持久化引用 |

## 快速链接

- [CLAUDE.md](../CLAUDE.md) — 开发规范、项目结构、编码标准
- [README](../README.md) — 项目概览、构建与运行说明
