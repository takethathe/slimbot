# MCP 工具不进模型 schema，只通过 codemode 沙箱发现与调用

v1 的 MCP 集成把外部 server 的工具藏在 `codemode` 模型工具后面——模型的 `tools` 列表里只有一个 `codemode`，脚本用 `searchTools()` (BM25) 发现 MCP 工具、`tools.mcp__<server>__<tool>(args)` 调用。这放弃了 MCP 客户端的标准做法（把 server tools 注册进模型 schema 让模型直接 tool_call），换到的是：**模型工具词汇表不因第三方 server 而膨胀**、**一次脚本可编排多工具调用与过滤**（并行、聚合、条件分支），而标准做法需要多轮 tool_calls 逐条完成。

**Considered Options**

- **标准 MCP 客户端（直接注册进 schema）**：每个 MCP 工具都出现在 `tools` 声明里，模型直接调。优点：模型无需写脚本，调用更直观；缺点：一个 GitHub MCP server 就 30+ 工具，五个 server 轻松超 100，把模型的注意力从内置能力上拉走；且每轮 prompt 的 token 成本线性增长（schema 占 prompt 前缀 cache）。
- **混合：高频 direct、低频 codemode**：对每个 server 可配 `exposure: direct | codemode | hidden`（pi 的做法）。优点是精细，缺点是多一条配置面 + 多一套 schema 动态变更逻辑（direct 工具进出 tool_calls 触发 cache miss）+ 对 v1 "一个真实集成" 的需求过度。
- **deferred（tool_search 按需加载进 schema）**：工具不在初始声明里，模型调 `tool_search` → 下一轮加载匹配项进 schema。pi 的 `deferred` exposure。缺点：加载后 schema 就变了（cache 失效）；v1 只有少数 server 时不如统一走 codemode。

**Consequences**

- 前置基建：`codemode` 必须先从 trigger-internal 扩展为 model-facing tool（`src/tools/codemode.rs` + 新 `ENTRY_AGENT`），ADR 0008 的执行模型不变但 scope 扩大。
- `searchTools()` 只搜 MCP 工具、复用现有 `KeywordIndex` 的 BM25 核心；内置工具已在 schema 里可见不需要 searchTools。
- 断连处理：指数退避 + 从 searchTools 可搜面摘除离线 server 的工具 + 恢复后回添。
- MCP tool description 作为不可信文本：codemode 路径里「脚本是过滤器」，模型只看到脚本输出（加 `<codemode_output>` wrapper）；不受 ADR 0009 的 evidence-only 约束（那条限 trigger 路径）。
- 重开条件：若将来高频 MCP 工具需要被模型直接 tool_call（延迟敏感 / 简单调用），按 server/tool 粒度引入 `exposure: direct` 即可——接缝是 `McpService` 向 `ToolCatalog` 注册 factory，不改 codemode 路径。
