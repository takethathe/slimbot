# 记忆用文档级 category API + 可插拔 backend，而非 entry 级 API

SlimBot 的记忆模块对外只暴露**文档级** API：所有读写按 `MemoryCategory`（+ `Skill` / `Custom` 的动态 `name`）寻址整个文档，`MemoryBackend` trait 描述 `get` / `set` / `edit` / `remove` / `list_*_names` / `template`，当前唯一实现是 markdown 文件 backend；交互历史（`history.jsonl` + Dream 游标）作为独立的 `HistoryStore` 组合进 `MemoryService` facade，刻意不进入 trait。

评估过 zeroclaw 风格的 **entry 级 API**（按条目 store/recall，配 embedding 语义检索）。放弃原因：SlimBot 的实际消费形态是「整文档注入 system prompt」加 Dream 的外科手术式编辑，entry 级需要额外维护索引 / embedding 子系统却没有消费者；且「未修改模板不注入」在文档级可自然由 `get_effective` 表达，entry 级反而要重造该语义。代价是失去逐条目去重与语义召回 —— 若未来确需，可在 backend trait 之上叠加 entry 层，trait 就是预留的接缝。

另一处刻意的取舍：`MemoryCategory` 保持封闭单元变体，**不**把动态名字塞进枚举（如 `Skill(String)`）。动态名字改由调用时的 `name: Option<&str>` 传入并校验 `[a-z0-9-]{1,64}`；固定 category 的 `name` 作为 tag 元数据保留（markdown backend 忽略，仅文档标注），为将来 namespace / 标签过滤预留。代价是固定 category 存在一个「传了也不生效」的参数位，换来枚举保持干净封闭、serde 序列化稳定。该决策难以回退：system prompt 布局、Dream 工具 schema（`category` 白名单枚举）、以及 identity.md / SKILL.md / dream.md 依赖的文件路径契约，都已锚定在这套 category 寻址之上；配置里的 `memory.backend` 字段即为 backend 替换预留的入口。
