# v1 剩余工作 · 阶段 6：M5 插件

2026-10-12。实现 v1 剩余工作计划的阶段 6（M5 插件）。用法见 README 的“插件（M5）”，线协议的变化见 [线协议](wire-protocol-v1.md) 的“v1.8 新增”。

## 变更

### 6A 后端与执行面

| 项 | 内容 |
|---|---|
| 1 数据模型 | 迁移 0027：<br>• 插件与安装：`plugins`、`plugin_versions`（不可变的解析结果、内容哈希、信任档、git 提交）、`plugin_installations`（v1 只开放成员范围；设置分明文与密钥库 id；`needs_reconfirm`；待确认的新版本）；<br>• 确认与工具：`plugin_grants`（确认过的权限摘要）、`plugin_tools`（工具定义、当前摘要与确认过的摘要）；<br>• 账号与开关：`plugin_connections`（OAuth）、`conversation_plugins`、`project_plugins`；<br>• 授权：`approval_grants`；<br>• 无组织上下文的表：`oauth_flows`、`plugin_blocks`、`plugin_app_pages`；<br>• 现有表新增：`runs.plugins`、`pending_actions.form`、`tool_calls.input`、`mcp_calls.installation_id/input`。 |
| 2 拉取与解析 | 来源：包地址、git（`http.curloptResolve` 固定地址，记录提交）、上传、官方目录、单独的 MCP 地址。<br>安全拉取（插件、MCP、OAuth 共用）：<br>• 只允许 HTTPS 公网地址；<br>• 解析后固定连接地址；<br>• 每次重定向都重新检查；<br>• 包体大小有上限。<br>解析在单独进程里进行（内存与 CPU 限制）：<br>• 包格式：zip / tar.gz / tar，去掉仓库归档的顶层目录，拒绝越出包的路径与链接；<br>• Agent Plugins 1.0.0 严格校验（顶层字段固定、`$schema`、名称、`mcp.json` 字段），`stdio` 服务只提示；<br>• 平台扩展 `app.ryzhub`：permissions、commands、ui、userConfig；<br>• Claude Code 插件导入。 |
| 3 信任与安装 | 三档信任：<br>• 已签名：用实例的受信 ed25519 公钥验证内容哈希上的签名；<br>• 官方审核：来自官方目录且内容一致；<br>• 未验证：其余，策略 `allow_unverified_plugins` 可以禁止。<br>安装确认：预览给出服务地址、权限、设置、技能与命令全文、当时能列出的工具；确认即安装。<br>机密设置存进密钥库，按字段声明的请求头只发给该插件的服务。<br>工具定义摘要变化或出现新工具时撤下这些工具，插件进入待确认；更新后若权限扩大或技能、命令文本改变，同样待确认。<br>实例管理员按名称或内容哈希停用，立即生效。 |
| 4 MCP Gateway 动态目录 | Work 的 `/mcp` 按 run 的插件列出 `<插件>__<工具>`，与记忆工具和实例的 `work.mcp_tools` 并列。<br>每次调用实时核对插件是否启用、未被停用，工具是否已确认、未被插件规则拒绝。<br>远程调用改用新的 MCP 客户端，见“与计划不同之处”。<br>`userConfig` 与账号令牌作为请求头注入。 |
| 5 必须审批 | 策略 `approval_required_tools` 中的工具，在 Work 中由 Gateway 强制审批：<br>• 成员在 harness 里批准过同一调用（同一工具与参数摘要、同一执行代次、1 小时内），平台签发一次性 `approval_grants`，Gateway 消费后直接放行；<br>• 没有授权的调用（自动模式，或绕过 harness 直接调用本地代理）由 Gateway 发起审批。<br>同一操作只问一次。Claude Code 的审批请求现在带工具名与参数。 |
| 6 Chat 接入 | 工具定义按 run 的插件动态生成，直接调用、不审批。<br>服务端的错误原文交给模型；未连接账号时返回 `account_not_connected`。 |
| 7 skills | Chat：系统提示只列名称与描述，`load_skill` 读全文或资源文件。<br>Work：harness 启动前由 guest 写进 `~/.codex/skills`、`~/.claude/skills`、`~/.config/opencode/skills`（目录名 `<插件>-<技能>`）。只替换平台写入的目录，用户自己的技能不动。 |
| 8 插件命令 | `InvokeMenuCommand`（proto 22）：服务端校验参数、填充模板，作为成员消息发出（首行 `/<插件>:<命令>`）。命令目录在 `…/commands` 的 `plugin_commands`。 |
| 9 MCP OAuth | 发现：401 的 `resource_metadata` 或 well-known 地址，然后读授权服务器元数据并核对 issuer。<br>注册：优先 CIMD，否则 DCR。<br>授权：PKCE（S256）、state、resource；回调时核对 `iss`，state 只能用一次，授权地址与令牌地址同样只允许公网 HTTPS。<br>令牌存密钥库，快过期时刷新；连接后第一次列出的工具视为安装时确认的。 |
| 10 项目与对话启用 | 插件默认对成员的所有对话生效。项目与对话可以单独开关，对话优先。<br>每个 run 记录开始时的插件集合；之后关闭或被停用的插件立即从它的工具中撤下。 |
| 11 按需加载 | 插件工具超过 `plugins.defer_tools_above` 个时，Anthropic 模型（`llm.tool_search_models`）把插件工具设为 `defer_loading`，并加上 BM25 tool search。 |
| elicitation（6B.5 的服务端） | 2026-07-28 协议的 `input_required` 中，elicitation 成为带 `form` 的提问。<br>Chat：调用暂停并保存请求与状态，回答后带 `inputResponses` 与 `requestState` 重试。<br>Work：Gateway 等待回答后重试。<br>sampling、roots 不提供。 |
| MCP Apps（6B.4 的服务端） | `ToolDetail.app/plugin`。<br>`POST …/apps` 经插件服务读取 `ui://` 页面，存成 10 分钟有效的令牌页，从 `plugins.content_origin` 提供。<br>CSP：只允许页面声明的 `connectDomains`，`frame-ancestors` 为客户端源。<br>`POST …/tool-calls` 供页面与声明式组件调用工具，规则与模型调用相同，必须审批的工具不能从界面调用。 |

### 6B 客户端

| 项 | 内容 |
|---|---|
| 1 插件管理页 | 设置 › 插件。<br>列表：信任档、版本、工具（只读、新工具、描述已变更）、提示、开关。<br>操作：连接/断开账号、设置（机密字段不回显）、刷新工具、检查更新、查看并确认变更、卸载。<br>安装对话框：五种来源，预览后展示全部内容与设置表单，“信任并安装”。<br>OAuth 完成后回到 `#/plugins`，自动打开该页。输入框“+”菜单可以开关本对话的插件。 |
| 2 `/` 菜单插件命令 | “插件命令”分组，有参数的先填表单，再发 InvokeMenuCommand。 |
| 3 声明式组件 | 工具结果 `_meta["app.ryzhub/ui"]` 中的组件：card、text、list、table、button、form、image、chart（柱状）、code、diff，其余忽略。按钮与表单经服务端调用插件工具，结果显示在组件下方。网络图片只显示为链接。未采用 genui（alpha），组件目录较小，用 Flutter 直接实现。 |
| 4 MCP Apps | 工具卡片上的“打开界面”。<br>Web：`sandbox="allow-scripts allow-forms"` 的 iframe，只接受来自该 frame 的消息。<br>Android：webview_flutter，清除 cookie，导航限于内容源。<br>Windows：webview_windows，独立数据目录。<br>宿主桥（JSON-RPC）：`ui/initialize`、初始化后送出本次输入与结果、`tools/call`、`ui/message`（需成员确认）、`ui/open-link`（仅 HTTPS）、`ui/notifications/size-changed`，其余返回 unsupported。 |
| 5 elicitation | 带 `form_json` 的提问显示为表单（字符串、数字、布尔、枚举，标出必填），也可以拒绝；url 模式可打开页面，然后点“已完成”。 |

## 与计划不同之处

- **MCP 客户端自己实现**：计划是 rmcp 3.x，但它依赖 reqwest 0.13。平台的拉取需要固定解析地址的 reqwest 0.12 客户端，同时引入两套 HTTP 栈不划算。
  - 新客户端支持 2026-07-28（`server/discover`、每个请求携带 `_meta`、`input_required`）与此前的握手协议（会话、SSE 回复）。
  - 旧协议服务在 SSE 中途发来的请求一律拒绝。
- **运行快照**：没有内容寻址的 `plugin_revisions`，插件集合记在 `runs.plugins`。运行中按安装的当前版本取工具，所以成员确认更新后，进行中的 run 也用新版本。
- **原文不进 BlobStore**：包解析后的内容就是存下来的版本；原始归档不另存。
- **OAuth 刷新**：在使用令牌时进行，不经 jobs。
- **MCP Apps 只在 Chat**：Work 的工具条目来自 harness 的投影，暂不关联插件页面。
- **授权签发只覆盖 Claude Code**：Claude 的审批请求带工具名与参数，可以签发授权。Codex 与 ACP agent 不带，它们的 MCP 调用由 Gateway 自己审批，同一操作同样只审批一次。
- **组织级安装不开放**：表结构保留 `scope`，界面与接口只做成员范围（与已定决策一致）。
- **原生端未实机验证**：Android 与 Windows 的 WebView 代码只经过静态分析，在本机无法构建运行；真机验证随阶段 8 一起做。

## 实现中发现并修复的问题

- 测试里 `localhost` 指向本机。拉取以解析出的地址为准，所以按名字访问也会被拒绝；测试覆盖了这一点。

## 验证

| 检查 | 结果 |
|---|---|
| fmt、clippy `-D warnings`、`cargo test --workspace --locked` | 通过，143 项单元测试。新增的有：<br>• 包解析：Agent Plugins 与扩展、严格字段、越界路径、Claude Code 导入、签名不计入内容哈希、版本权限扩大的判断、命令模板；<br>• 安全拉取：私网、链路本地、元数据地址、IPv6 ULA、带凭据的地址、`localhost` 都被拒绝；<br>• ed25519 签名验证、工具名规则、OAuth well-known 地址、WWW-Authenticate 解析、elicitation 的问与答 |
| `test-integration.sh` | 19/19 |
| `test-m1.sh` | 28/28。新增 3 项（本地 MCP 服务兼作包主机与授权服务器）：<br>• 安装与 Chat：<br>&nbsp;&nbsp;– 官方目录信任档、预览内容、机密设置只发给插件服务且不进模型与数据库明文；<br>&nbsp;&nbsp;– 工具调用、技能按需读取、提问表单与带状态重试、插件命令；<br>&nbsp;&nbsp;– MCP App 页面（内容源、CSP、界面调用工具，被拒绝的工具仍被拒绝）；<br>&nbsp;&nbsp;– 描述变化后撤下与重新确认、关闭与实例停用立即生效、对话开关、卸载删除密钥。<br>• 拉取与 OAuth：<br>&nbsp;&nbsp;– 元数据地址、私网、`localhost`、`file://` 被拒绝；<br>&nbsp;&nbsp;– 旧协议服务（会话、SSE）；<br>&nbsp;&nbsp;– OAuth（PKCE、resource、iss、state 只能用一次、令牌只在密钥库）；<br>&nbsp;&nbsp;– 授权服务器指向私网时拒绝。<br>• Work Gateway：<br>&nbsp;&nbsp;– 插件工具列表与规则；<br>&nbsp;&nbsp;– 必须审批的工具绕过 harness 调用时由 Gateway 审批；<br>&nbsp;&nbsp;– 一次性授权被消费后不再询问。 |
| `test-m2.sh`（含 gVisor 往返） | 8/8 + 1/1 |
| `test-m3.sh`（runc 与 gVisor） | 8/8、8/8（Work 启动时安装技能） |
| `test-m4.sh` | 1/1 |
| `flutter analyze`、`flutter test` | 无问题，122 项通过（另有 2 项按条件跳过）。新增：声明式组件与工具调用、提问表单、App 宿主桥（只放行允许的方法、链接只限 HTTPS、发消息需确认）、安装预览的全文展示 |
| dev 真实服务 | 新账号经 API 安装公开的远程 MCP 服务 DeepWiki（`mcp`，未验证），列出 3 个工具。<br>在 Web 客户端里让真实模型（Responses）用它查询 `tokio-rs/axum`：模型调用了 `deepwiki__ask_wiki_question`，根据结果回答；工具卡片标出插件名。<br>`#/plugins` 打开插件设置页，显示信任档与工具数。控制台无错误 |

## 部署

- **迁移**：0027 只新增表与列，不改写数据。
- **执行节点**：guest 新增 `InstallSkills`，Work 启动时调用，需要热替换（测试脚本已替换）。
- **配置**：新增 `[plugins]`。
  - 生产需设置 `public_url`（浏览器访问 API 的地址，OAuth 回调用；阶段 7 起移到配置顶层，与登录共用）、`client_url`。
  - `content_origin` 与 `content_listen` 必须是不同于客户端的源。
  - 可选 `trusted_keys`、`catalog_url`。
  - `insecure_hosts` 只用于开发。
- **客户端**：Web 需用 `scripts/build-web.sh`（proto、bridge 有改动）。新增 webview_flutter、webview_windows 依赖。

### dev 部署记录（2026-10-12）

1. 确认 `ryzhub_m3` 没有进行中的 run，备份到 `var/backups/0027-migration-20261011-234401/ryzhub_m3.dump`。
2. `var/m4/config.toml` 加上 `[plugins]`（`public_url` 8280、`client_url` 3000、内容源 127.0.0.1:8282），原文件留在 `config.toml.before-phase6`。
3. 构建后端，用 `scripts/dev-m4.sh` 重启，迁移到 27。`/healthz` 正常，`/capabilities` 宣告 `plugins` 与内容源，8282 在监听，日志无 WARN。
4. 用 `scripts/build-web.sh` 构建 Web 到 `var/m4/web`。
