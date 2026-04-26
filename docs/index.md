# SlimBot 文档索引

## 入门

| 文档 | 说明 |
|------|------|
| [配置指南](config.md) | `config.json` 结构、验证规则、多 Provider 配置 |
| [内置工具](tools.md) | 6 个内置工具：shell、file_reader/writer/editor、list_dir、make_dir |

## 核心模块

| 文档 | 对应模块 | 说明 |
|------|----------|------|
| [架构概览](architecture.md) | 整体 | 系统架构、数据流、组件交互 |
| [Agent 循环](agent_loop.md) | `agent_loop.rs` | 顶层编排、组件初始化 |
| [ReAct 执行器](runner.md) | `runner.rs` | ReAct 循环核心：迭代控制、工具调用流程 |
| [上下文构建器](context.md) | `context.rs` | System Prompt 构建流程 |
| [会话管理](session.md) | `session.rs` | 会话生命周期、任务队列、JSONL 持久化 |
| [消息总线](message_bus.md) | `message_bus.rs` | 请求分发、任务封装、结果路由 |
| [Provider](provider.md) | `provider/` | Provider 接口、OpenAI 兼容 API 集成 |
| [通道](channel.md) | `channel/` | 通道抽象、工厂模式、并发 I/O 循环 |

## 快速链接

- [CLAUDE.md](../CLAUDE.md) — 开发规范、项目结构、编码标准
- [README](../README.md) — 项目概览、构建与运行说明
