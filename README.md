# herdex

codex 专用账号池网关（Rust）。把多个 ChatGPT 账号聚合成一个 codex API 入口：
请求按各账号的配额余量智能分配，会话粘性保持 prompt cache 命中，statusline 显示
整个号池的聚合用量。

**北极星：复刻 codex CLI 原版行为**——出站 TLS 栈与官方客户端同源，请求体逐字节透传。

## 特性

- **配额感知调度**：least usage（模型级与账号级取 max，防止模型级 0% 遮蔽账号级耗尽）→ LRU 排序；单账号 429 只冷却该账号+模型组合（短 TTL），永不熔断全池，全部冷却时仍按"上次失败时间最早者优先"返回候选而非报错
- **黏性让位可配**：`pin_yield_gap_pp`（面板可调）三档语义：`-1` 黏性优先（钉住的号永远先试，哪怕已耗尽）、`0` 纯水填、`>0` 滞后带（黏性在领先值 gap 内不让位）
- **池子口径 statusline**：usage 轮询与 turn 响应头按实际窗口时长分别聚合，
  周额度与月额度不混算；每组百分比、容量权重与最早重置时间都来自同一组账号。
  前两个周期按时长递增放入主、次窗口，其余周期通过附加额度返回；
  未知百分比或周期不当作 0%，没有已知窗口时 usage 返回 502。
  容量权重沿用已有主窗口校准，未校准窗口在各自组内回退，不改历史计量
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
- **会话粘性**：Responses 入口将 `session-id`／`session_id` 及 `thread-id`／`thread_id` 明确归一化；重复、冲突或越界的路由头返回 400，不发往上游。原有选号粘性仍为 TTL 1h 滑动窗口，失败自动重钉，不保证缓存命中。
- **回合路由隔离**：`x-codex-turn-state` 绑定调用方 key、会话、模型、后端及实际服务账号／凭据。同一归属保留首个状态；未知、过期或换号后的旧值不直接转发。换号取得新状态后，可将客户端仍保留的旧别名映射到新账号自己的状态，不把旧账号的不透明值发给新账号。
  绑定仅在内存保存，最多 256 项，空闲 30 分钟过期，单个状态最多 8 KiB；重启不恢复旧值。客户端负责在新的用户回合清理自己的状态，网关不根据提示词内容猜回合边界。
- **路由诊断边界**：静态 `header-defaults` 不注入会话／回合路由头；限额日志不打印不透明路由状态。请求体及原生回放内容不因该机制而截断或改写。
- **TLS 指纹同源**：出站栈 pin 到 codex-rs Cargo.lock 的逐版本组合
  （reqwest 0.12.28 + rustls 0.23.36 + aws-lc-rs）
- **身份策略**：真 codex 客户端（CLI/App/IDE）自身 UA/Originator/Version 原样透传；非 codex 客户端套 `header-defaults` 伪装壳（防第三方 UA 触发 Cloudflare 挑战）
- **后端反代**：未匹配路径反向代理至 `chatgpt.com/backend-api`（含 `/api/codex/*`
  → `/wham/*` 重映射），CLI 的附属后端调用（遥测/插件/设置）有处可去
- **400 参数自愈**：上游拒绝的参数（如 max_output_tokens）自动剥离重试，学习结果落盘复用
- **可观测**：面板（`/manage/panel`）+ manage API；请求级 token 用量日志（按账号/模型/key 聚合），保留期可配
- **失败诊断**：每次上游尝试（含 HTTP 错误、连接失败、参数重试和断流）单独入日志，
  用 `diagnostics.request_id` 与递增的 `attempt` 关联；响应头 `x-herdex-request-id` 可用于对照。
  面板展开「诊断」可查看失败阶段、上游请求编号、累计/单次耗时、已接收字节数和底层传输错误链。
  `status=0` 表示尚未收到 HTTP 响应；流中断可能仍为 HTTP 200，面板按错误标红。
  诊断不保存请求正文、认证头或上游错误消息正文；旧记录的诊断保持未知。
  流内错误事件另以「字段名 + 白名单 code/type/status 值」单独记一行服务日志，
  便于定位上游拒绝原因，同样不含上游消息正文；错误码按顶层 `code`、嵌套
  `error.code`、`error.type` 依次提取（兼容新旧上游格式）。
  请求计数包含每次尝试，失败行只保留实际观察到的 token 用量。
- **Banked reset 消耗顺序**：面板消耗前读取券明细，明确选择 `expires_at` 最早的可用券；
  永不过期券排在有明确过期时间的券之后，同一过期时间保持上游顺序；无法解析过期时间时拒绝消耗。
  点击后须确认账号及消耗 1 张券，执行中阻止重复提交，结果常驻账号详情。
  操作结果仅由消耗重置请求决定；成功后独立刷新额度。
  面板额度查询同时拉取逐张券明细，在上游提供时显示状态、发放时间 `granted_at`
  和到期时间 `expires_at`；详情接口暂时不可用时仍保留数量和额度结果。
- **请求级自动 Banked reset（默认关闭）**：实际请求收到明确的
  `usage_limit_reached` 且首个内容尚未发出时，先完成普通账号 failover；全部候选都未成功时，
  在账号级 single-flight 锁内重新确认额度，消耗最早到期的可用券并将原请求重试一次。
  同一账号/额度窗口的并发请求共享一次重置结果；列表失败、额度无法确认、重置失败或已输出内容时不继续扣卡。
- **请求档位记录**：请求日志保存客户端的 `service_tier`，`priority` 表示请求 Fast，
  `ultrafast` 表示请求 Ultrafast；
  历史未记录与新请求未指定档位分开表示。不保存请求正文，也不据此推定实际消费倍率
  或调整原始 token 数量、容量校准
- **令牌自维护**：后台约每 2 分钟检查一次，剩余有效期不超过 10 分钟时自动刷新，
  已过期令牌也会尝试恢复。启用和禁用账号均参与，刷新不会自动启用账号；
  禁用账号仍不参与请求调度、后台额度查询或模型目录采集
- **搜索并发调度**：standalone `alpha/search` 使用账号级 in-flight lease，
  并发请求优先分散到空闲的可用账号；每次尝试记录首字节、完整响应、字节数和上游
  request ID，不记录查询正文。单次搜索超时后在有限候选内 failover，避免一个账号永久占住请求。
- **模型目录自动更新**：复用后台轮询，从 Codex 官方稳定版发布频道获取
  `client_version`，按账号采集上游完整目录；`/models` 返回 Codex 原生
  `{"models": [...]}` 模型元数据，`/v1/models` 返回 OpenAI-compatible 的
  `{"object":"list","data":[...]}` canonical 模型 ID 视图。无 `models`
  配置过滤时，两者均以启用账号目录的并集为基础，禁用或删除账号立即不参与合并；
  `models` 配置只缩小清单，不能凭空赋予模型或速度权限。
  `-fast` / `-ultrafast` Key 分别筛选有合资格账号支持的模型，并强制
  `service_tier: "priority"` / `"ultrafast"`，原生目录合并同名模型的速度能力。
  旧的 `gpt-*-fast` 请求仍作为兼容入口接受。
  调度跳过目录明确不支持模型的账号；启动时先从磁盘恢复上次已采集目录，
  首个刷新周期前路由与两个目录接口即保持正确。
  版本源不可用时两个目录接口返回 503，不清除已有目录、用量或中断现有推理请求
- **账号级档位准入**：尊重目录的 `available_in_plans` 套餐范围，并按每个账号对应模型的
  `service_tiers` 判断速度权限；不按模型名前缀、Plus/Pro 名称或套餐价格猜测。
  只认原生 `service_tiers` 对象；弃用的 `additional_speed_tiers` 不参与准入，也不对外输出。
  缺失、空值、空数组或无效的档位对象均不能授予加速权限；未知速度能力不进入加速池，
  不能因普通池里有同名模型而放行。
  模型发现、推理与独立搜索选号、会话粘性、失败重试和重置券重试共用能力过滤；
  普通 Key 显式请求速度档位同样受约束。派生 Key 的池级用量和响应头仅聚合对应速度池。
  不剥离明确请求的速度字段来重试普通模式；无合资格账号时返回 503
  `no_eligible_account`，冲突的速度选择器返回 400 `conflicting_service_tiers`。
  权限以上次成功采集的账号目录为准，新目录撤回能力、账号禁用/删除会更新清单和调度；
  成功的空目录会清除缓存，格式错误或网络失败仍保留上次有效目录。

## 构建

```bash
cd web/panel && npm ci && npm run build && cd -   # 面板产物嵌入二进制（dist/ 随源码保存）
cargo build --release   # 需要 cmake（aws-lc-rs 构建）
```

面板开发：`cd web/panel && npm run dev`（热重载，/manage/api 默认代理到
`http://127.0.0.1:8317`）。远程网关或自定义端口通过启动环境变量指定：
`HERDEX_DEV_PROXY="http://<herdex-host>:<listen 端口>" npm run dev`。
改动 web/panel/src 后需 `npm run build` 再 `cargo build` 才会进入二进制。

## 配置

```toml
# /etc/herdex/herdex.toml
listen = "127.0.0.1:8317"
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
standalone-search-timeout-seconds = 45  # 单次 alpha/search 上游尝试上限

[header-defaults]                     # 仅对非 codex 客户端生效
user-agent = "codex_cli_rs/0.153.4"
originator = "codex_cli_rs"
beta-features = "multi_agent"

[log]
level = "info"
```

启动时立即查询 `https://releases.openai.com/codex/channels/latest`；成功后约每小时
检查版本（账号模型目录仍每约 2 分钟采集），失败后下一轮重试。模型目录查询只使用
官方版本，不读取旧配置中的 `header-defaults.version`，也不以旧版本兜底；取不到
版本时 `GET /models` 和 `GET /v1/models` 均返回 503，恢复后自动重新采集。两个
接口（无 `models` 配置覆盖时）同源：返回当前存在且启用账号的已采集目录并集。
使用以 `-fast` 结尾的 Bearer Key 时，目录改为该并集中支持 Fast 的 canonical 模型子集。

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

Codex 0.156.1 起的工作区路由要求 HTTPS 后端。herdex 内建 TLS：首次启动自动生成
私有 CA 并签发叶子证书（SAN 自动覆盖本机全部非回环 IP 与主机名，或由 `[tls]`
配置指定，支持 IP 地址，无需域名）。CA 通过面板头部「下载 CA 证书」或
`http://<host>:<listen 端口>/ca.pem` 直链分发（该端口同时说 HTTP 和 HTTPS，
见下）。客户端让 Codex 显式额外信任该 CA（`CODEX_CA_CERTIFICATE`），不必安装到
系统信任库，也不关闭证书校验。

**同一端口双协议**：codex 是唯一强制 HTTPS 的客户端；opencode、curl 等直接
用 `http://` 访问同一端口即可，**完全不需要任何证书操作**。

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

OpenCode v2 的动态模型接入见下方[安装说明](#opencode-v2-接入)。

`[tls]` 段仅用于自定义证书 SAN：

```toml
[tls]
hosts = []   # 自动探测本机 IP 与主机名；也可显式指定服务器的 IP 或域名
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
- `POST /v1/alpha/search`（及 `/api/codex/alpha/search`）—— Codex Responses Lite
  的独立 `web.run` 搜索代理，使用池内账号凭据访问上游
- `GET  /v1/models` —— 启用账号的 canonical 模型并集；`-fast` / `-ultrafast` Key 分别筛出对应档位模型
- `GET  /models` —— 启用账号完整 Codex 模型目录并集（codex CLI 使用）
- `GET  /api/codex/usage`（及 `/v1/api/codex/usage`、`/wham/usage`）—— 池子聚合用量
- `GET  /v1/user-auth-credential/whoami` —— PAT 虚拟身份
- `GET  /api/codex/accounts/check`（及 `/backend-api/wham/accounts/check`）—— 认证后的虚拟工作区发现
- `*    /backend-api/*`（其余路径）—— 上游后端反代
- `POST /api/codex/ps/mcp` —— apps MCP 代理

使用自定义 Codex provider 时，若要启用 Lite 模型的原生搜索，还需在 provider
配置中设置 `supports_standalone_web_search = true`；`web_search = "live"` 仍由
Codex 全局配置控制。Lite 模型的搜索请求会单独访问上面的 `alpha/search`，不会伪装成
一次 Responses 完成。

注意：herdex 是 CLI 的认证依赖；网关不可达、API key 无效或禁用、HTTPS 证书不受
信任时，codex 会在身份获取或工作区发现阶段拒绝启动（fail-closed）。

Fast / Ultrafast Provider 不需要额外数据库 Key：若普通 Key 为 `<key>`，下游 Provider
分别使用 `<key>-fast` / `<key>-ultrafast` 即可。三种形式共享同一 Key 的启用/禁用和删除状态；
后缀只是 Provider 模式选择，不是独立权限边界。没有对应能力的账号不会被选中；
订阅名称本身不授予速度档位。Ultrafast 路径有模拟上游回归，尚未用具备权限的真实订阅做端到端验收。

### 速度协议依据

- 上游每个模型的 `service_tiers` 为对象数组，元素的 `id`、`name`、`description` 均为字符串。
  Fast 的请求 ID 是 `priority`，Ultrafast 是 `ultrafast`；名称和描述保留该账号上游返回的内容，
  不根据套餐或客户端目录补写权限。`default_service_tier` 是可省略的字符串/空值，只选择默认项，
  不单独授予任何档位能力。普通模式不要求目录中出现一个虚构的 `standard` 档位。
- Codex 官方 [`ModelServiceTier` 定义](https://github.com/openai/codex/blob/1a604d58da344c84528fecd5b5154105f597d70b/codex-rs/protocol/src/openai_models.rs#L240-L245)
  给出目录结构；同文件明确标记 `additional_speed_tiers` 为弃用字段。
- OpenCode V2 的 [OpenAI 选项](https://github.com/anomalyco/opencode/blob/394d49d487584fbd6cd1d0184973b2420c4c3fd9/packages/ai/src/providers/openai-options.ts#L8-L16)
  接受 `serviceTier: "ultrafast"`，并在 [Responses 请求构造](https://github.com/anomalyco/opencode/blob/394d49d487584fbd6cd1d0184973b2420c4c3fd9/packages/ai/src/protocols/open-responses.ts#L835-L840)
  中映射为 `service_tier`。其[公开模型目录](https://models.opencode.ai/experimental/v2.0/api.json?type=chat)
  的 GPT-6 Astra / GPT-6.1 Sol 均有 `experimental.modes.ultrafast.body.service_tier = "ultrafast"`。
  这些信息确认客户端协议，不是某个 Pro $500 账号的完整目录回包或授权证明。
- 模型目录由后台逐启用账号查询，每轮工作结束后等待 120 秒；请求读取内存缓存，
  重启先恢复磁盘缓存。上游查询失败保留最近有效目录，不保证权限变动被即时发现。

## OpenCode v2 接入

配套插件位于 [`deploy/opencode/`](deploy/opencode/README.md)：
  启动时从 herdex 拉取普通、Fast、Ultrafast 三套模型清单，每分钟刷新，继承 OpenCode 目录中的
  同名模型显示名、推理强度、能力与限制；模型 ID 始终使用 canonical ID。选择
  `herdex-fast` Provider 后，herdex 通过 `<key>-fast` 自动使用
  `service_tier: "priority"`；`herdex-ultrafast` 使用 `<key>-ultrafast` 和
  `service_tier: "ultrafast"`。推理强度与速度选择分开，不继承会覆盖速度的 variants。
  使用 Responses API，可通过
 `http://<herdex-host>:<listen 端口>/v1` 连接，无需证书配置或手动维护模型列表。
安装与配置步骤见插件目录的 README。

只支持 `model` 字段的 OpenAI-compatible 客户端使用普通 Key 请求 canonical 模型 ID；
需要 Fast / Ultrafast 时，将 Bearer Key 写成 `<key>-fast` / `<key>-ultrafast`，模型 ID 保持不变。旧的
`gpt-*-fast` 请求 ID 仍会被兼容处理。

## 测试

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked   # 单测 + HTTP 集成：假上游 + 真实 axum 链路
npm ci --prefix deploy/opencode --omit=peer --ignore-scripts
npm test --prefix deploy/opencode
cd web/panel && npm ci && npm run lint && npm test && npm run build
```

前端回归覆盖设置保存与轮询的并发、额度查询失败/重试、重置券确认/防重复提交/结果反馈、同邮箱账号容量隔离、
空闲日预测和图表卸载；后端覆盖重置券消耗结果、慢速 SSE 分块、前缀预算、同秒请求归属、
未知套餐、旧数据库迁移、断连收尾，以及历史清理后的容量统计。
速度回归覆盖 Plus 权限撤回、混合套餐、未知能力、同名模型档位合并、缓存重载、
显式速度透传、参数自愈不降档、重试前权限复核，以及三个 Provider 的发现/刷新/卸载。

核心语义不变式（逐条测试锁定）：
- 429 永不熔断全池；全部冷却时仍返回候选（按上次失败时间最早者优先），而不是直接报错
- 请求体逐字节透传
- 400 "not supported" 正确 failover、其它 400 原样返回
- spark 模型目录不泄漏给 plus
- 会话粘性（Session-Id → 账号，TTL 1h 滑动，失败重钉）
- 内容前流内错误（如 server_is_overloaded）：候选耗尽后对同账号重试一次，永不无界重试
