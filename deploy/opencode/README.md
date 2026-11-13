# OpenCode v2 接入

此插件为 OpenCode 注册 `herdex`、`herdex-fast` 和 `herdex-ultrafast` 三个 Provider：
启动时分别使用普通 Key、`<key>-fast`、`<key>-ultrafast` 读取 herdex 的 `/v1/models`，每 60 秒刷新一次，推理请求使用
`/v1/responses`。

- 模型清单以 herdex 为准，无硬编码模型列表；三个 Provider 使用相同的 canonical 模型 ID。
- `herdex` 使用普通 Key，显示所有可用模型；`herdex-fast` 使用派生 Fast Key，只显示
  有账号明确支持 Fast 的模型；`herdex-ultrafast` 使用派生 Ultrafast Key，仅显示有账号明确支持
  Ultrafast 的模型。速度由 Provider/API key 模式选择，不是模型 ID 后缀。
  若没有合资格账号，该 Provider 模型清单为空，不借用普通或 Fast 的清单。
  档位资格只取账号上游的原生 `service_tiers`，不兼容弃用的 `additional_speed_tiers`；
  OpenCode 或 models.dev 的静态能力描述不会给账号补上速度权限。
- 从 OpenCode 的 OpenAI 同名模型继承显示名、推理档位、能力、上下文限制及参考价格。
  优先使用当前可用模型的元数据，否则读取目录源定义；不复制直连端点或认证信息。
- 三个 Provider 都保留模型自身的推理档位，但剔除速度 variants 及推理 variants 中的速度覆盖。
  选择 `herdex-fast` / `herdex-ultrafast` 后，herdex 分别强制 `service_tier: "priority"` /
  `"ultrafast"`，不需要额外的 variant 或请求参数。冲突的显式速度选择会被网关拒绝，不静默降档。
- 未匹配目录的模型保留 OpenCode 默认能力/限制，显示名取端点的 `name` 或格式化 ID；
  不凭空添加推理档位。参考价格是目录元数据，不代表 ChatGPT
  订阅池的实际费用。
- 普通目录首次发现失败时清单为空；加速目录失败只使对应加速 Provider 暂时为空。
  后续普通目录刷新失败保留上一次整体结果；普通目录成功后，各加速目录独立处理，
  失败保留各自上一次有效清单，成功返回空列表则清空对应 Provider。
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

配置普通 Key 即可：插件不会要求面板新增 Key，而是为 Fast / Ultrafast Provider 自动使用
`<herdex API key>-fast` / `<herdex API key>-ultrafast`。这些后缀只是模式选择，
与普通 Key 共享启用、禁用和删除状态。

配置目录受 OpenCode 监视，文件变更会自动重载。打开 `/models`，选择 **Herdex Pool**
下的模型及推理档位即可，无需 `/connect`。插件已经提供 provider，配置中不必再写
`providers.herdex.models`。如果已有旧的 `plugins/herdex-models` 自动发现安装，只保留
一份插件，避免重复加载。

## 检查

```bash
opencode plugin list
opencode models
```

从列表选择实际存在的 `herdex/<模型 ID>`、`herdex-fast/<模型 ID>` 或
`herdex-ultrafast/<模型 ID>`；仅在该模型列出
对应档位时使用 `#high` 等后缀：

```bash
opencode run --model 'herdex/<模型 ID>#high' '只回复：正常'
```

选择 Fast Provider：

```bash
opencode run --model 'herdex-fast/<模型 ID>' '只回复：正常'
```

选择 Ultrafast Provider：

```bash
opencode run --model 'herdex-ultrafast/<模型 ID>' '只回复：正常'
```

速度资格由 herdex 的账号级模型目录决定，不把某个套餐名永久映射到 Fast 或 Ultrafast。
模拟测试可用 `npm test` 执行，不会连接实际网关或消耗订阅额度；尚未用有 Ultrafast 权限的真实订阅做端到端验收。

若模型存在但无推理档位，检查 OpenCode 的 OpenAI 同名模型元数据是否包含 `variants`。
显示名格式化只改变名称，请求始终使用 herdex 返回的真实模型 ID。
