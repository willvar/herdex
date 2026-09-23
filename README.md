# herdex

codex 专用账号池网关（Rust）。把多个 ChatGPT 账号聚合成一个 codex API 入口：
请求按各账号的配额余量智能分配，会话粘性保持 prompt cache 命中，statusline 显示
整个号池的聚合用量。

**北极星：复刻 codex CLI 原版行为**——出站 TLS 栈与官方客户端同源，请求体逐字节透传。

## 特性

- **配额感知调度**：least usage（模型级与账号级取 max，防止模型级 0% 遮蔽账号级耗尽）→ LRU 排序；单账号 429 只冷却该账号+模型组合（短 TTL），永不熔断全池，全部冷却时仍按"上次失败时间最早者优先"返回候选而非报错
- **黏性让位可配**：`pin_yield_gap_pp`（面板可调）三档语义：`-1` 黏性优先（钉住的号永远先试，哪怕已耗尽）、`0` 纯水填、`>0` 滞后带（黏性在领先值 gap 内不让位）
- **池子口径 statusline**：CLI 的 usage 轮询返回**容量加权的池子聚合值**；turn 响应头同步改写为池子值——两条写入源一致，statusline 永远显示"整个号池还剩多少"而非单号
- **流内过载 failover**：chatgpt.com 可能在 200 SSE 流内携带
  `server_is_overloaded` 错误帧——缓冲至首个内容帧，错误先于内容到达时
  无痕换号重试；判定不依赖 HTTP 分块数量。SSE 前缀超过 2 MiB 仍无完整内容时，
  返回本地 `prefix_limit_exceeded` 错误，不冷却账号或自动换号；流尾错误标记入账
  内部计量最多保留 8 MiB 的 JSON 文档或单个 SSE 事件；超限仍原样透传，
  日志标记 `observer_limit_exceeded`（计量可能不完整），不冷却账号；SSE 从下一事件恢复计量。
- **容量校准**：请求头实时探针 + 边界穿越记账（重放式，无运行时状态），
  解出每个账号每 1% 对应的混合 token 量（输入已包含缓存，不重复累计），
  校准收敛后池子估算从等权升级为容量加权
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
cd web/panel && npm ci && npm run build && cd -   # 面板产物嵌入二进制（dist/ 随源码保存）
cargo build --release   # 需要 cmake（aws-lc-rs 构建）
```

面板开发：`cd web/panel && npm run dev`（热重载，/manage/api 代理到网关）。
改动 web/panel/src 后需 `npm run build` 再 `cargo build` 才会进入二进制。

## 配置

```toml
# /etc/herdex/herdex.toml
listen = "127.0.0.1:8319"
state-root = "/var/lib/herdex"        # SQLite 状态库目录
retention-days = 730                  # 请求日志保留天数；缺省 730，0 = 永久

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

[log]
level = "info"
```

## 运行

```bash
herdex --config /etc/herdex/herdex.toml   # 默认路径同上；-c 短选项
```

systemd 部署样例见 `deploy/herdex.service`（含完整沙箱加固）。

数据库启动时自动迁移，升级前应备份。新探针记录请求日志序号，按观察顺序归属
已完成请求，不再依赖同一秒内的时间比较；未知套餐不会作为套餐变更处理。
旧探针保留秒级估算，新旧顺序边界不混算；旧日志若缺少账号 ID 且邮箱对应多个账号，
不会强行分摊到容量校准中。容量及触顶时间仍是估算，上游额度更新可能有延迟；
面板日均消耗按所选的滚动 7/30 天计算，包含空闲日。

## CLI 接入（PAT 虚拟账号模式）

Codex 0.156.1 起的工作区路由要求 HTTPS 后端。herdex 内建 TLS：在配置中启用
`[tls]` 后首次启动会自动生成私有 CA 并签发叶子证书（SAN 覆盖配置的 hosts，
支持 IP 地址，无需域名）。CA 通过 `http://<host>:8088/ca.pem` 分发（无需认证），
客户端让 Codex 显式额外信任该 CA（`CODEX_CA_CERTIFICATE`），不必安装到系统
信任库，也不关闭证书校验。

codex 端三处配置如下，`/status` 显示池子聚合：

```toml
# ~/.codex/config.toml（片段）
model_provider = "herdex"
chatgpt_base_url = "https://<herdex-host>:<listen 端口，默认同 8088>"

[model_providers.herdex]
name = "herdex"
base_url = "https://<herdex-host>:<listen 端口，默认同 8088>/v1"
wire_api = "responses"
requires_openai_auth = true
experimental_bearer_token = "<herdex API key>"
```

```bash
# ~/.codex/auth.json —— PAT 模式，token 就是 herdex API key
{"auth_mode": "personalAccessToken", "personal_access_token": "<herdex API key>"}

# PAT 的 whoami 校验端点重定向到网关（env 变量，无 config 入口）
codex() {
  CODEX_AUTHAPI_BASE_URL="https://<herdex-host>:<listen 端口，默认同 8088>" \
  CODEX_CA_CERTIFICATE="$HOME/.codex/herdex-ca.pem" command codex "$@"
}
```

HTTPS 是唯一形态——herdex 服务的是 codex，而 codex 0.156+ 只接受 HTTPS
后端（工作区路由拒绝明文 HTTP）。无需配置段，`listen` 端口即 HTTPS-only；
`[tls]` 段仅用于自定义证书 SAN：

```toml
[tls]
enabled = true
port = 8443
hosts = ["192.168.168.254"]   # 叶子证书 SAN：IP 或域名均可
```

函数放入 `~/.bashrc` 后，在已有终端执行 `source ~/.bashrc`。若使用系统已信任的
公有 CA 证书，可省略 `CODEX_CA_CERTIFICATE`。

工作原理：codex 启动时向 `CODEX_AUTHAPI_BASE_URL/v1/user-auth-credential/whoami`
获取虚拟身份（`pool@herdex.local`），再经 `/api/codex/accounts/check` 验证 API key
并发现同一个虚拟工作区。发现响应的 `NO_CONSTRAINT` 让 CLI 继续使用配置中的
HTTPS 后端 origin，不暴露池内真实账号。此后 codex 认为自己是 ChatGPT 账号会话
（`/status` 限流卡片解锁），usage 轮询、工作区发现、whoami、模型请求均落在
herdex 上，凭据始终是 herdex API key，无客户端 OAuth。

接口总览（Bearer 认证用 manage 面板里创建的 API key）：

- `POST /v1/responses`、`POST /backend-api/codex/responses` —— SSE 流式透传
- `GET  /v1/models`
- `GET  /api/codex/usage`（及 `/v1/api/codex/usage`、`/wham/usage`）—— 池子聚合用量
- `GET  /v1/user-auth-credential/whoami` —— PAT 虚拟身份
- `GET  /api/codex/accounts/check`（及 `/backend-api/wham/accounts/check`）—— 认证后的虚拟工作区发现
- `*    /backend-api/*`（其余路径）—— 上游后端反代
- `POST /api/codex/ps/mcp` —— apps MCP 代理

注意：herdex 是 CLI 的认证依赖；网关不可达、API key 无效或禁用、HTTPS 证书不受
信任时，codex 会在身份获取或工作区发现阶段拒绝启动（fail-closed）。

## 测试

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked   # 单测 + HTTP 集成：假上游 + 真实 axum 链路
cd web/panel && npm ci && npm run lint && npm test && npm run build
```

前端回归覆盖设置保存与轮询的并发、额度查询失败/重试、同邮箱账号容量隔离、
空闲日预测和图表卸载；后端覆盖慢速 SSE 分块、前缀预算、同秒请求归属、
未知套餐、旧数据库迁移、断连收尾，以及历史清理后的容量统计。

核心语义不变式（逐条测试锁定）：
- 429 永不熔断全池；全部冷却时仍返回候选（按上次失败时间最早者优先），而不是直接报错
- 请求体逐字节透传
- 400 "not supported" 正确 failover、其它 400 原样返回
- spark 模型目录不泄漏给 plus
- 会话粘性（Session-Id → 账号，TTL 1h 滑动，失败重钉）
