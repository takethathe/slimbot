# Config Scheme

对应模块：`src/config_scheme.rs`

## 概述

`ConfigScheme` 持有所有默认值和验证规则，负责配置的规范化（填充缺失默认值、修正无效值）。

## 默认值常量

| 常量 | 值 |
|------|-----|
| `DEFAULT_BASE_URL` | `https://api.openai.com` |
| `DEFAULT_API_URL` | `https://api.openai.com/v1/chat/completions` |
| `DEFAULT_PROVIDER_TYPE` | `openai` |
| `DEFAULT_MODEL` | `gpt-4o` |
| `DEFAULT_TEMPERATURE` | `0.7` |
| `DEFAULT_MAX_TOKENS` | `4096` |
| `DEFAULT_MAX_ITERATIONS` | `40` |
| `DEFAULT_TIMEOUT` | `120` |
| `DEFAULT_CONTEXT_WINDOW_TOKENS` | `8192` |

## 核心方法

### `default_config`

生成完整的默认配置，包含一个名为 `"default"` 的 Provider。

### `normalize`

规范化已有配置：
- 填充缺失的默认值（如空的 `provider` → `"default"`）
- 修正无效值（如 `temperature > 2.0` → `0.7`）
- **URL 派生**：`api_url` 为空时，从 `base_url + "/v1/chat/completions"` 派生
- **不规范化 `api_key`**：必须由用户显式设置
- 移除空的 tools/channels 条目

### URL 派生逻辑

```
if api_url 不为空 → 保持不变
else if base_url 不为空 → api_url = base_url + "/v1/chat/completions"
else → api_url = DEFAULT_API_URL
```

### `write_default_config`

将默认配置写入指定路径的 JSON 文件。

### `config_exists`

检查配置文件是否存在。

## 与 `config.rs` 的关系

- `config.rs` 定义数据结构和 serde 默认值（用于反序列化时填充）
- `config_scheme.rs` 提供运行时规范化逻辑和验证规则
- 两者结合确保无论用户传入什么配置，都能得到完整有效的配置对象
