# herdex

codex 专用账号池网关（Rust）。把多个 ChatGPT 账号聚合成一个 codex API 入口：
请求按各账号的 5h/weekly 配额余量智能分配，会话粘性保持 prompt cache 命中，
配额探针零成本轮询。

**北极星：复刻 codex CLI 原版行为**——出站 TLS 栈与官方客户端同源，请求体逐字节透传。

## 特性

- **配额感知调度**：least 5h usage → least weekly usage → LRU 排序选账号；单账号 429 只冷却该账号+模型组合（短 TTL），永不熔断全池，全部处于冷却时仍按"上次失败时间最早者优先"返回候选而非报错
- **零成本用量探针**：定时探测 `wham/usage`（不消耗模型配额），5h/weekly 与模型级限流全部入池；支持消耗 banked reset credits 解锁账号
- **会话粘性**：Session-Id → 账号，TTL 1h 滑动窗口，失败自动重钉
- **TLS 指纹同源**：出站栈 pin 到 codex-rs Cargo.lock 的逐版本组合
  （reqwest 0.12.28 + rustls 0.23.36 + aws-lc-rs）
- **身份策略**：真 codex 客户端（CLI/App/IDE）自身 UA/Originator/Version 原样透传；非 codex 客户端套 `header-defaults` 伪装壳（防第三方 UA 触发 Cloudflare 挑战）；reqwest 原生 cookie store 对齐官方 CF cookie 行为
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

客户端入口（Bearer 认证用 manage 面板里创建的 API key）：

- `POST /v1/responses`、`POST /backend-api/codex/responses` —— SSE 流式透传
- `GET  /v1/models`
- `WS   /v1/responses` —— WebSocket 透传

## 测试

```bash
cargo test   # 28 个测试（单测 + HTTP 集成：假上游 + 真实 axum 链路）
```

核心语义不变式（逐条测试锁定）：
- 429 永不熔断全池；全部冷却时仍返回候选（按上次失败时间最早者优先），而不是直接报错
- 请求体逐字节透传
- 400 "not supported" 正确 failover、其它 400 原样返回
- spark 模型目录不泄漏给 plus
- 会话粘性（Session-Id → 账号，TTL 1h 滑动，失败重钉）
