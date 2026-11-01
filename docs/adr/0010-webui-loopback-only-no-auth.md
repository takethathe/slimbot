# WebUI 仅 loopback、无认证，靠 Host/Origin/Content-Type 三重校验补偿

WebUI 是 slimbot 的**纯本机入口**：只监听 loopback，非 loopback 的 `host` 直接启动失败（无 override、无 `trusted_hosts`），远程访问一律走 IM 通道、用各通道自己的认证。我们刻意**不做** token、cookie、配对码、OAuth 或账户体系——这是对「Web UI 必须有登录」这一常识路径的偏离，之所以敢偏离，是因为真正的攻击面不是「谁连得上本机端口」，而是**用户自己的浏览器被第三方网站当跳板**：DNS rebinding 能让恶意页与本机服务同源，`no-cors` 简单请求能让恶意页免 preflight 发出 POST（`/config/api/update` 收裸 body、不校验 Content-Type，改 `providers.*.base_url` 即可把 `api_key` 送往攻击者服务器）。因此无认证的替代品不是「什么都不做」，而是四条全局中间件：Host 白名单（专杀 rebinding，否则 421）、变更请求必须携带同源 Origin（缺失即拒）、变更请求必须 `application/json`（强制 preflight）、`Sec-Fetch-Site: same-origin` 深度防御；同时零 CORS 头。代价是脚本与集成测试的变更请求必须显式带 header，现网写 `0.0.0.0` 的配置升级后起不来，且**本机任意进程仍可全权操作 WebUI（含写配置）——这一点显式接受、不做缓解**，因为本机进程本就能读配置与 keychain，加认证只会给出虚假安全感。

**Consequences**

- 用域名反代到 WebUI 会因 `Host` 不匹配被 421 拒绝；这是刻意的：远程入口只走 IM。
- `channels.webui.host` 白名单限定 `127.0.0.1` / `localhost` / `::1`，默认为 `127.0.0.1`（原默认 `0.0.0.0` 的 breaking change 需在 changelog 与 `docs/config.md` 记录）。
- 配置写入不进入审批流（审批只覆盖 agent 自主发起的动作），仅留审计日志，不设 UI 二次确认——确认框挡不住脚本，真正的门是 Origin/Host。
