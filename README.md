# herdex

codex 专用账号池网关（Rust）。把多个 ChatGPT 账号聚合成一个 codex API 入口：
请求按各账号的配额余量智能分配，会话粘性保持 prompt cache 命中，statusline 显示
整个号池的聚合用量。

**北极星：复刻 codex CLI 原版行为**——出站 TLS 栈与官方客户端同源，请求体逐字节透传。

## 特性

- **配额感知调度**：least usage（模型级与账号级取 max，防止模型级 0% 遮蔽账号级耗尽）→ LRU 排序；单账号 429 只冷却该账号+模型组合（短 TTL），永不熔断全池，全部冷却时仍按"上次失败时间最早者优先"返回候选而非报错
- **黏性让位可配**：`pin_yield_gap_pp`（面板可调）三档语义：`-1` 黏性优先（钉住的号永远先试，哪怕已耗尽）、`0` 纯水填、`>0` 滞后带（黏性在领先值 gap 内不让位）
- **池子口径 statusline**：CLI 的 usage 轮询返回**容量加权的池子聚合值**；turn 响应头同步改写为池子值——两条写入源一致，statusline 永远显示"整个号池还剩多少"而非单号
- **容量校准**：请求头实时探针 + 边界穿越记账（重放式，无运行时状态），
  解出每个账号每 1% 对应的 token 量；进一步按模型做最小二乘分离（`per_model`），
  校准收敛后池子估算从等权升级为 token 精确加权
- **PAT 虚拟账号**：CLI 用 codex 的 PersonalAccessToken 认证模式接入，whoami 由
  网关应答——无需任何真实 OAuth，账号身份（`pool@herdex.local`）只存在于网关侧
- **零成本用量探针**：定时探测 `wham/usage`（不消耗模型配额），各窗口全部入池
- **会话粘性**：Session-Id → 账号，TTL 1h 滑动窗口，失败自动重钉
- **TLS 指纹同源**：出站栈 pin 到 codex-rs Cargo.lock 的逐版本组合
  （reqwest 0.12.28 + rustls 0.23.36 + aws-lc-rs）
- **身份策略**：真 codex 客户端（CLI/App/IDE）自身 UA/Originator/Version 原样透传；非 codex 客户端套 `header-defaults` 伪装壳（防第三方 UA 触发 Cloudflare 挑战）
- **后端反代**：未匹配路径反向代理至 `chatgpt.com/backend-api`（含 `/api/codex/*`
  → `/wham/*` 重映射），CLI 的附属后端调用（遥测/插件/设置）有处可去
- **400 参数自愈**：上游拒绝的参数（如 max_output_tokens）自动剥离重试，学习结果落盘复用
- **可观测**：面板（`/manage/panel`）+ manage API；请求级 token 用量日志（按账号/模型/key 聚合），保留期可配
- **令牌自维护**：后台 proactive refresh，过期前自动续

## 构建

```bash
cargo build --release   # 需要 cmake（aws-lc-rs 构建）
```

## 配置

```toml
# /etc/herdex/herdex.toml
listen = "127.0.0.1:8319"
state-root = "/var/lib/herdex"        # SQLite 状态库目录

[manage]
# 面板认证密钥；支持 env 插值：{ env = "NAME" }、"env: NAME"、"{env:NAME}"
key = { env = "HERDEX_MANAGE_KEY" }

[oauth]
issuer = "https://auth.openai.com"
client-id = "app_EMoamEEZ73f0CkXaXp7hrann"

[upstream]
base-url = "https://chatgpt.com/backend-api/codex"

[header-defaults]                     # 仅对非 codex 客户端生效
user-agent = "codex_cli_rs/0.153.4"
originator = "codex_cli_rs"
beta-features = "multi_agent"

retention-days = 730                  # 请求日志保留天数；缺省 730，0 = 永久

[log]
level = "info"
```

## 运行

```bash
herdex --config /etc/herdex/herdex.toml   # 默认路径同上；-c 短选项
```

systemd 部署样例见 `deploy/herdex.service`（含完整沙箱加固）。

## CLI 接入（PAT 虚拟账号模式）

codex 端三处配置，即可让整个 CLI 无 OAuth 地跑在池子上，`/status` 显示池子聚合：

```toml
# ~/.codex/config.toml（片段）
model_provider = "herdex"
chatgpt_base_url = "http://<herdex-host>:8088"

[model_providers.herdex]
name = "herdex"
base_url = "http://<herdex-host>:8088/v1"
wire_api = "responses"
requires_openai_auth = true
experimental_bearer_token = "<herdex API key>"
```

```bash
# ~/.codex/auth.json —— PAT 模式，token 就是 herdex API key
{"auth_mode": "personalAccessToken", "personal_access_token": "<herdex API key>"}

# PAT 的 whoami 校验端点重定向到网关（env 变量，无 config 入口）
alias codex='CODEX_AUTHAPI_BASE_URL="http://<herdex-host>:8088" codex'
```

工作原理：codex 启动时向 `CODEX_AUTHAPI_BASE_URL/v1/user-auth-credential/whoami`
校验 PAT；herdex 应答一个虚拟身份（`pool@herdex.local`）。此后 codex 认为自己是
ChatGPT 账号会话（`/status` 限流卡片解锁），而所有后端流量——usage 轮询、
whoami、模型请求——全部落在 herdex 上，凭据始终是 herdex API key，
无任何真实 OAuth。

接口总览（Bearer 认证用 manage 面板里创建的 API key）：

- `POST /v1/responses`、`POST /backend-api/codex/responses` —— SSE 流式透传
- `GET  /v1/models`
- `WS   /v1/responses` —— WebSocket 透传
- `GET  /api/codex/usage`（及 `/v1/api/codex/usage`、`/wham/usage`）—— 池子聚合用量
- `GET  /v1/user-auth-credential/whoami` —— PAT 虚拟身份
- `*    /backend-api/*`（其余路径）—— 上游后端反代
- `POST /api/codex/ps/mcp` —— apps MCP 代理

注意：herdex 是 CLI 的认证依赖——网关不可达时 codex 启动会因 whoami 失败拒绝认证（fail-closed）。

## 测试

```bash
cargo test   # 32 个测试（单测 + HTTP 集成：假上游 + 真实 axum 链路）
```

核心语义不变式（逐条测试锁定）：
- 429 永不熔断全池；全部冷却时仍返回候选（按上次失败时间最早者优先），而不是直接报错
- 请求体逐字节透传
- 400 "not supported" 正确 failover、其它 400 原样返回
- spark 模型目录不泄漏给 plus
- 会话粘性（Session-Id → 账号，TTL 1h 滑动，失败重钉）
