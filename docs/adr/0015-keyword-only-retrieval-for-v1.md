# v1 维持派生关键词索引，向量 / sqlite / 增量 / transcript 均出 scope

ADR 0004 把检索定为"派生关键词索引、内存、永不落盘、只读"。票 07 调研了是否需要在 v1 升级这条路径，结论是**一个都不动**：无 embedding/hybrid、无 sqlite、无增量索引、transcript 不进语料。根本理由不是"向量不好"，而是**主召回不是检索**——固定记忆文档整篇注入 system prompt，检索只对不全注入的 skills/customs 有边际影响，且该影响在 v1 可接受。

**Considered Options**

- **hybrid（BM25 + 向量融合）**：能解决语义漏检，但需要引入 embedding provider（新端点 / 配置面 / 批量 / 缓存 / 隐私）、维度存储（需要落盘 = 拉回"索引陈旧"那整类 bug）、锁内或锁外做网络调用（SharedMemory 全程持锁、纯本地）。收益太小、代价太重。
- **sqlite FTS5 做关键词持久化**：中文不分词（ADR 0004 白纸黑字），换过去等于解决一个不存在的规模化问题（全量重建是微秒级）的同时把损坏恢复与迁移请回来。
- **只给 memory_search 加 `mode` 参数占位**：只有 `keyword` 合法、其余值报错——给模型一套只有一项可用的 enum，增加幻觉面（票 05 的"memory 操作指令不要太多"同一条理由）。
- **把 session transcript 纳入检索语料**（OpenClaw 做法）：与已定的边界冲突（history / snapshot 不进语料）；transcript 信息已被 consolidate→summary→history 链路处理，再建一条检索通路等于冗余。

**Consequences**

- v1 不新增任何检索/存储机件；现有 `MemoryIndex` trait + 预留 `SearchMode` 继续充当接缝，将来引入时替换实现而不触碰 caller。
- 本决定**挂在"检索是次级路径"这个前提上**：若文档预算（票 05/15）失效导致 MEMORY.md 长到整篇注入不经济，前提松动，keyword 够不够用要重估。最可能触发重开的实际场景是 **skill 数量增长**（唯一靠检索召回的语料面）。
- 不做离线评估集 → 将来加向量时没有 baseline，届时需补。
- 外部方案（OpenClaw SQLite hybrid、Letta archival memory、sqlite-vec）的**一手深度核查随重开一起做**，本票不预付。
