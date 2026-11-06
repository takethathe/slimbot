# MCP 客户端自研最小实现，不引入官方 rmcp SDK

票 26 要接 MCP。我们决定在 `src/mcp/` 里自研最小客户端，分三层各测各的：`rpc.rs`（JSON-RPC 帧 + `initialize` / `tools/list` / `tools/call`，约 330 行实现）、`transport.rs`（stdio 子进程的 newline-delimited JSON-RPC，与 streamable-HTTP 的一请求一 POST——应答可能是 plain JSON 也可能是一帧 SSE，约 440 行）、`service.rs`（gateway 级连接池 + 每 server 工具缓存 + 退避重连的 supervisor，约 700 行）。协议版本只广告 `2025-03-26`，能力表是空对象。

换来的是三件事。**协议面和项目定位一样窄**：[ADR 0016](0016-mcp-through-codemode-only.md) 把连接器限定成"只给工具"，而 `rmcp` 最大的一块价值——把 server tools 转成 client 侧的 tool 注册与动态 schema——在本项目结构性用不上（MCP 工具绝不进模型 schema）。**错误文本与日志隐私是自己写的**：token 永不进 transport reason、URL 只报 host + path（query 剥掉，因为凭证常挂在那儿）、server 的 stderr 只计字节不 captured、HTTP 失败不回显 response body；这几条若走 SDK 就得逐处覆写它的 error 类型与 tracing 埋点。**依赖树不新增**：复用已在的 `tokio` / `reqwest`（`stream`）/ `serde_json` / `parking_lot`。

**Considered Options**

- **自研最小客户端（选定）**：约 1500 行含测试，全部语义在项目内可断言。
- **官方 `rmcp` crate**：省掉 handshake、帧与类型定义，代价是它的 client 构建走 tower service 一层（多一套 trait 与错误类型），而本项目真正难的部分——连接池、退避、离线摘除、风险映射、日志隐私——SDK 一个都不做，仍然要写这 700 行。它做得多的部分（resources / prompts / sampling / schema 注册）恰是 ADR 0016 排除的。
- **只借一个通用 JSON-RPC crate**：省不到东西。stdio 的 ambient reader task + oneshot 关联、HTTP 的 SSE 单帧解析都是 transport 的活，通用 crate 面向"一次 HTTP 请求一次响应"的模型，反而要在它外面再包一层。
- **现成 CLI adapter 把 HTTP server 转成 stdio**（`mcp-remote` 一类）：Rust 侧只写一条 transport，但给部署加了一个 Node 运行时和一条第三方供应链，与"单二进制的个人助手"冲突；而且多一跳就多一处能卡死 gateway 的地方。

**Consequences**

- MCP 协议演进要跟自己升级这块：streamable-HTTP 的 resumability / 服务端主动推流、`notifications/cancelled`、`tools/list_changed` 之外的动态性都不实现。server 发来我们无法关联的帧（含 stdout 上的非 JSON 噪声）一律忽略，不报错。
- 不实现 resources / prompts / sampling / roots / elicitation / progress / completion。`McpClient` 因此不保存协商到的版本号——只有 HTTP transport 需要在后续请求带上 `mcp-protocol-version` 头，版本归 transport 所有，client 里没有这个字段（也就没有一个只有测试会读的 pub 访问器）。
- 两个注入点决定可测性：`McpTransport`（一帧怎么变成答案）与 `Connector`（一条连接怎么开）。单元测试给内存假 peer，集成测试走真实链路——测试二进制自我重入当 stdio server，加一个手写 HTTP/1.1 假 server。
- 替换路径明确：将来若真需要标准生态（`tools/list_changed` 的动态 schema、或 SDK 的完整 compliance），换的是 `rpc.rs` + `transport.rs` 两个文件，接缝是 `McpService::definitions()` / `risk_for()` / `call()`，codemode 的 `AgentHost` 与审批闸不动。
- 隐私规则从约定变成可执行断言：`tests/mcp_integration.rs` 跑一条含真 token 的 HTTP 链路 + 一个把 token 打到 stderr 的子进程 + 一个回显 Authorization 的 401，断言 `status.label()` 与日志文件里三者都不出现。
