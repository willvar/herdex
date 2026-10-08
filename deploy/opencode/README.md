# OpenCode v2 接入

此插件为 OpenCode 注册 `herdex` provider：启动时读取 herdex 的 `/v1/models`，
每 60 秒刷新一次，推理请求使用 `/v1/responses`。

- 模型清单以 herdex 为准，无硬编码模型列表。
- 从 OpenCode 的 OpenAI 同名模型继承显示名、推理档位、能力、上下文限制及参考价格。
  优先使用当前可用模型的元数据，否则读取目录源定义；不复制直连端点或认证信息。
- 对 `gpt-*` 模型额外提供 `#fast`；该档位向 herdex 请求 `service_tier: "priority"`，
  对应账号池的 Fast 模式；已有推理档位还会生成组合档位，例如 `#max-fast`。
  `#fast` 使用模型默认推理强度，`#max-fast` 才是 max 推理加 Fast。
- 未匹配目录的模型保留 OpenCode 默认能力/限制，显示名取端点的 `name` 或格式化 ID；
  除上述 GPT Fast 档位外不凭空添加推理档位。参考价格是目录元数据，不代表 ChatGPT
  订阅池的实际费用。
- 首次发现失败时清单为空；后续刷新失败保留上一次结果；成功返回空列表则清空。
- 插件卸载时停止定时刷新。

使用 OpenCode v2；依赖固定为 `@opencode/plugin@2.0.16`。

## 安装

在 herdex 仓库根目录执行，安装到 OpenCode 配置目录下的 `extensions/`：

```bash
config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/opencode"
plugin_dir="$config_dir/extensions/herdex-models"
mkdir -p "$plugin_dir"
cp deploy/opencode/{index.js,package.json,package-lock.json} "$plugin_dir/"
npm ci --prefix "$plugin_dir" --omit=peer --ignore-scripts
```

将以下片段合并到该目录的 `opencode.json` / `opencode.jsonc`（保留其它配置）。
`baseURL` 改为实际 herdex 地址及 `listen` 端口，路径需包含 `/v1`。
`apiKey` 使用 herdex 面板创建的 API key：

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "plugins": [
    {
      "package": "./extensions/herdex-models",
      "options": {
        "baseURL": "http://127.0.0.1:8088/v1",
        "apiKey": "<herdex API key>"
      }
    }
  ]
}
```

`HERDEX_BASE_URL` / `HERDEX_API_KEY` 环境变量可覆盖以上两项，须由 **OpenCode 后台
服务**持有；仅在新终端里 export 不会改变已运行服务的环境。省略地址时默认
`http://127.0.0.1:8088/v1`，API key 没有默认值。

配置目录受 OpenCode 监视，文件变更会自动重载。打开 `/models`，选择 **Herdex Pool**
下的模型及推理档位即可，无需 `/connect`。插件已经提供 provider，配置中不必再写
`providers.herdex.models`。如果已有旧的 `plugins/herdex-models` 自动发现安装，只保留
一份插件，避免重复加载。

## 检查

```bash
opencode plugin list
opencode models
```

从列表选择实际存在的 `herdex/<模型 ID>`；仅在该模型列出对应档位时使用 `#high` 等后缀：

```bash
opencode run --model 'herdex/<模型 ID>#high' '只回复：正常'
```

GPT 模型还可选择 Fast 档位：

```bash
opencode run --model 'herdex/<GPT 模型 ID>#fast' '只回复：正常'
```

需要 max 推理与 Fast 同时启用时，选择组合档位：

```bash
opencode run --model 'herdex/<GPT 模型 ID>#max-fast' '只回复：正常'
```

若模型存在但无推理档位，检查 OpenCode 的 OpenAI 同名模型元数据是否包含 `variants`。
显示名格式化只改变名称，请求始终使用 herdex 返回的真实模型 ID。
