# MCP server 凭证在 Config editor 里以掩码 + 整表替换编辑，永不回显

Config editor 会把整棵 config schema 明文返回给浏览器，而 WebUI 按 [ADR 0010](0010-webui-loopback-only-no-auth.md) 只监听 loopback 且无认证；MCP server 的 `env` / `headers` 里放的是 token 与 `${secret:...}` 占位符。因此这两张 map 保持 `secret` 标记、value 恒不回显：owner 只看到「已配置 N 项、留空表示不改动」的裸 JSON 输入框，提交时按 object 校验后整表替换；`mcp` 标量段也只广告 server 名字，不内联 servers 数组。条目数 N 追加到字段的 `desc`（字段说明本就会渲染），因此无需新增 schema 字段，也不会把任何**已配置的**凭证值或键名写进状态响应；字段自带的静态说明里出现的 `Authorization` 只是文档示例。

考虑过的备选是去掉 `secret` 标记、渲染成正常的明文键值对编辑器（以及为 map 新增专用编辑 kind）：更好用，但凭证会进入状态响应，与配置文件的 0600 权限设计和此前修过的同类泄漏冲突。代价是 owner 无法在界面上查看已有条目、只能整表覆盖，且留空必须严格解释为「不改动」——这由 patch 组装跳过空 secret 字段来保证。v1 接受这个取舍；保存仍按 [ADR 0002](0002-webui-config-editor-persist-and-restart.md) 写盘、重启生效。
