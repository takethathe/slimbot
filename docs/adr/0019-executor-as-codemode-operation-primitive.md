# 执行器：codemode 基础的按需动作原语，与采集器镜像

personal agent 光会“读与提醒”（触发器）不够，得能“对外部资源执行真实动作”。触发器已把“主动读”做成 codemode 里的采集器脚本（`collect.js`，其 HostBindings 刻意只读、无写无 shell）；对称地补一个“主动动”的原语——**执行器（Executor）**：一份 codemode 动作脚本（`act.js`），消费**连接器**的写侧，对外部资源执行一次真实动作。执行器与采集器镜像的是**结构**（目录 + codemode + subagent 创建/调用），**不镜像调度**：执行器是**按需执行的一次动作**（由 subagent、触发器 fire 或模型请求发起），无常驻 tick；周期性的“动”由触发器命中来驱动。

术语定位（连接器）：**连接器（Connector）= codemode 可直接调用的、非基础原语、非内置工具的外部能力封装**（同一连接器既能读又能动）；MCP server 是连接器的一个特例、v1 唯一实例。执行器依赖连接器动作、采集器依赖连接器取信息。基础原语（时间、`ctx.http` 网络）与内置工具（file/shell/message/web_fetch）都不是连接器；Channel（会话/传输面）与连接器正交；json-render UI 卡片属 Channel 展示、不是连接器。这放弃了“把动作工具直接注册进模型 schema 让模型 tool_call”的标准做法，换到的是：模型工具词汇表不因外部动作增多而膨胀（与 ADR 0016「MCP 不进 schema」同源）、一次脚本可编排“读—判断—动”多步、脚本本身充当动作的过滤器与护栏。

**Considered Options**

- **一等执行器原语（选定）**：与采集器目录结构镜像、复用 subagent/codemode/tick 全家桶。代价是多一个概念与一套带 actuation 的 HostBindings。
- **伞形术语**：不新增原语，把“动作能力”当作 MCP 工具 + 内置写工具的统称。省一层概念，但定不下“subagent 生成、codemode 内封装、按需执行、可作触发器动作目标”这组语义，也定不下审批与 evidence 边界。
- **模型级直接 tool_call 外部动作**：与 ADR 0016 冲突，一个外部系统就带来数十个动作工具、每轮 prompt 前缀线性膨胀。

**Consequences**

- MCP 归为“连接器的特例”（同 server 既读又动），不另立第二套外部集成机制；v1 的“一个真实集成”= 一个 GitHub 式 MCP 连接器 + 相应执行器。
- 执行器动作带副作用，默认风险档 `external_send`，走票 03 的“回合末兑现 / 文本清单无按钮”审批，**不在 codemode 执行中途逐调用弹窗**；无人值守（trigger 自动 fire）里的执行器动作一律 Deny（继承票 03/票 12）。审批粒度（整脚本 vs 逐动作）留给后续实现接缝，本决策不定。
- 执行器/采集器经连接器取回的外部内容若进入触发器采集路径，仍受 ADR 0009 的 evidence-only 约束，不因“走了连接器”绕过证据上限；连接器作为触发器源信息来源是单向的（collect 可用连接器读侧，连接器不反向依赖触发器）。
- 完整“镜像触发器”的执行器实现设计（`executors/<id>/{meta.json, act.js}`、actuation HostBindings、subagent 创建流程、codemode 调用面）另立一张触发器式子 effort；票 13 与本 ADR 只锁定位与接缝。**实现定案见 [ADR 0021](0021-executor-parasitic-unattended-subagent.md)**：执行器完全寄生一个 ephemeral 无人值守子代理（不进主循环、v1 Allow-only）；本 ADR 仍是「执行器是什么」，0021 是「它以什么形状存在、以什么姿态运行」。
- 连接器泛化（“凡 codemode 可调用的外部能力封装”）作为领域概念进 GLOSSARY/`docs/architecture.md`，不单开 ADR：它把 MCP 归入连接器，但不改变各连接器实例的既有机制。
- 由此边界推掉一批差异化项：卡片 actions 回传**出 v1**（审批已不需按钮，见票 03 #79）；技能供应链（安装/版本/签名）、数据导出/备份工具**出 v1**（数据即明文文件，复制 workspace 即备份）。
