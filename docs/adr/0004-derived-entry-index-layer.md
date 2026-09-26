# Memory entry 是文档的派生视图，关键词优先且只读；不采用 zeroclaw 的 entry 权威模型

在文档级 category API（ADR 0003）之上叠加一层**派生的** Memory entry 检索层：`MemoryIndex` 从记忆文档解析出段落级 entry（`MemoryEntry`），`MemoryService::search` 按查询返回最相关的若干条。entry 与索引都是**派生视图，永不落盘**，记忆文档始终是唯一权威；索引惰性构建、按内容指纹失效重建，因此没有「索引陈旧」这一类 bug，也没有需要迁移或清理的索引文件。检索**只读**：不提供 `store` / `forget` / `supersede` / dedup / 衰减 / retention，写侧留作独立立项（新 ADR）。

这是对参照项目 zeroclaw `crates/zeroclaw-memory` 权威模型的**有意偏离**：zeroclaw 中 entry 行是一等公民、markdown 只是序列化格式、`brain.db` 为准。采纳 entry 权威会同时推翻 SlimBot 已有的四样契约——identity.md / SKILL.md / dream.md 钉住的**文件路径契约**、Dream 的**整文档工具**（`memory_read` / `memory_write` / `memory_edit`）、**markdown backend**，以及 ADR 0003 的**文档级 API**。因此 **ADR 0003 继续有效、不被取代**；0004 是在 0003 之上**叠加**一层派生检索，而非替代它。代价是失去逐条目去重与语义召回——若将来确实需要，派生的 `hash` 字段与可替换的 `MemoryIndex` trait 就是预留的接缝。

检索**关键词优先**且必须同时支持中文与英文。我们刻意不复用 SQLite FTS5 的默认 tokenizer：FTS5 的 `unicode61` 对中文**不**分词（需要 `trigram`），照抄 zeroclaw 的 Bm25 路径并不能解决中文检索。切片 1 自实现拉丁分词 + CJK bigram（单字回退 unigram），打分用 BM25-lite（idf + tf 饱和，heading 命中加权 ×2），并给出确定性 tie-break。可插拔性由 `MemoryIndex` trait 提供——等 embedding / hybrid 真正落地时，再引入 `SearchMode` 参数替换关键词实现，而不必在半成品阶段提前抽象。
