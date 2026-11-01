# 记忆写侧：主模型只用 file 工具写 MEMORY.md，分类与晋升归 Dream

v1 要让 agent「聊完不忘」，主循环必须能写记忆。我们没有新增任何 memory 写工具（曾设计 `memory_store` / `memory_supersede` / `memory_forget` 三个条目级工具 + `category`/`name` 参数），而是让主模型用**已有的 `file_writer` / `file_editor` 编辑唯一的落地区文档 `memory/MEMORY.md`**：工具数量本身是幻觉预算，多一个工具与一组枚举参数就多一类错法；而"这条事实该放 soul 还是 user、是不是操作细节"这类分类判断需要全量上下文，交给 Dream 的整理过程比交给正在对话的模型更可靠。由此形成漏斗：**主模型即时写 core，Dream 分类并晋升（Promotion）到其它文档**。

**Considered Options**

- 条目级 memory 工具三件套：写入语义清晰、可校验参数哈希，但把分类责任压到对话现场，且与"模型直接编辑文本"这一既有可能冲突（它照样能用 file 工具改）。
- 主模型可写全部类别（沿用 Dream 那三个工具扩注册）：需要 provenance 由系统注入校验，而写入路径一旦不经系统控制，这个承诺就守不住。

**Consequences**

- 主模型的可写集是**唯一一篇**文档；`SOUL.md` / `USER.md` / `AGENTS.md` / `TOOLS.md` / `skills/**/SKILL.md` / `memory/*.md`（MEMORY.md 除外）是 **Protected document**，由 file 工具**代码强制**拒写（不是提示词约束）——人格与"对主人的描述"被误改是永久且静默的错误。workspace 内的运行文件（`cron/jobs.json`、`sessions/*.jsonl`、`HEARTBEAT.md`）不受此保护，避免把功能阉割当成安全。
- provenance 降级为**写法约定**（每条 bullet 自带来源与日期），派生索引不解析、不做门禁；"外部内容与他人消息禁写 MEMORY.md"只能靠提示词 + Dream 审查兜住，这是明写的残余风险。
- 记忆写入属 `WorkspaceWrite` → 默认 Allow，**不进审批流**：给"记东西"加审批会把成本抬到模型干脆不记。
- 每篇文档有 token 预算：Dream 侧硬拒超限（它是唯一能腾空间的人），主模型侧只附一行容量回执、绝不拒写（写不进即永久丢信息，容量不足本该由压缩解决）。超限只置标记，由 Dream 的下一次 tick 提前触发整理。
- MEMORY.md 模板原有的 "Managed by Dream. Do NOT edit directly." 与新方向自相矛盾，必须改写；连带 `context.rs` 的"未修改模板跳过注入"判定需要处理旧 workspace。
