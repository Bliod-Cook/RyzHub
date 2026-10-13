# RyzHub

按[架构方案](agent-platform-architecture-2026-09-25.md) §13 实现的 Rust + Flutter Agent 平台。已完成 M0 一致性、M1 Chat、M2 Codex Work、M3 第二条路径，以及 M4 搜索与记忆：PGroonga/pgvector、版本化索引、显式记忆、历史摘要、project_only 和临时对话。模型上游继续使用本地协议 fixture；Codex、Claude SDK、containerd、gVisor、数据库和浏览器实际运行。

项目代码与启动脚本均位于仓库根目录。最新目录调整和 M3 修复复核见[迁移与验证记录](docs/root-migration.md)。

## 运行服务端

需要 Rust ≥1.94（本次验证 1.98.1）和 Docker Compose。无需安装 PostgreSQL、NATS 或 protoc。

```sh
cd RyzHub
./scripts/dev.sh
```

REST/WS：`http://127.0.0.1:8080`，实例配置页：`/admin`。首次在客户端注册邮箱和密码，自动创建个人组织。`.env.example` 仅包含本地开发凭据；实际配置可复制为 `.env` 后使用 `RYZHUB_ENV_FILE=.env ./scripts/dev.sh`。

二进制支持 `all`、`gateway`、`owner`、`llm-gateway`、`migrate`、`maintain`、`reindex` 及 `migrate-secrets` 八个角色。Gateway 只需要 `DATABASE_URL` 和 `NATS_URL`；Owner 另需只读 BYPASSRLS 的 `WORKER_DATABASE_URL`。迁移和维护使用 `MIGRATION_DATABASE_URL`，不会授予运行进程 DDL 权限。运行进程拒绝超级用户或 BYPASSRLS 数据库账号。

管理员设置在 `config/development.toml`：监听地址、Origin、事件保留期、租约时长、模型/harness 白名单、组织并发上限等。修改后重启对应角色。实例管理员与组织 owner 分开；管理员可通过数据库运维会话设置 `users.is_instance_admin`，再用其登录 token 在 `/admin` 查看生效配置。

以迁移账号常驻运行 `ryzhub-server maintain --loop`（每 24 小时一次；也可由外部调度每日运行一次 `maintain`）：提前建立周/月分区，把落入 DEFAULT 分区的行迁回正式分区，按 `events_retention_days`（默认 30 天）删除整周 events 分区、按 `retention.audit_days`（默认 400 天）删除整月审计分区、按 `retention.usage_days`（默认 0，即不删除）删除整月用量分区，并清理过期 ticket。维护中断时写入落入 DEFAULT 分区，不会失败。整周、整月清理意味着实际保留期可能分别多出不到 7 天、不到一个月；items 不随事件清理。已处理命令、已确认的 guest 事件和失效网关密钥由 Owner 角色按 `[retention]` 配置分批删除。

## M1 本地协议联调

先停止占用 8080 的 M0 服务，再运行：

```sh
./scripts/dev-m1.sh --fixture
```

脚本启动 PostgreSQL/NATS、独立 containerd 执行节点、localhost:9081 协议 fixture、8080 API/WS 和 8081 LLM Gateway，自动迁移数据库。执行节点为名为 `ryzhub-m1-execution` 的 privileged 开发容器，只挂载本项目执行状态和二进制，不挂载 Docker socket；需要 Linux、Docker 及可用的 overlay/cgroup。默认镜像运行时为 runc，镜像按 digest 交给 host。

Web 客户端发“执行沙箱验收”会直接在实际容器中执行命令；“请向我提问”会出现回答卡片；“slow 慢速响应”可用于测试运行中的补充输入。“只读旁问”只读最近提交的主线前缀，不提供工具或写回主线；它和审批模式暂时不在输入框中提供，计划后续以命令方式调整。

本地配置在 `var/m1/config.toml`，vault 主密钥在权限 0600 的 `var/m1/secrets.env`，均已忽略。vault 主密钥需要保留。BYOK 凭据以信封加密存在 `secrets` 表：每个值有自己的数据密钥，数据密钥由 vault 主密钥包裹，凭据行只存 secret ID；撤销凭据会直接删除它的密钥。轮换时把旧密钥所在的环境变量名登记到 `llm.vault_retired_keys`（以旧 `vault_key_id` 为键），新密钥放入 `vault_key_env` 并递增 `llm.vault_key_id`，然后在带有新旧密钥的环境中以迁移账号运行 `ryzhub-server migrate-secrets`，把数据密钥改由新密钥包裹，之后即可移除旧密钥。升级到迁移 0019 后运行同一命令，把旧格式的凭据转入 `secrets`；转换前旧凭据照常可用。网关密钥是随机不透明令牌，数据库只存其 SHA-256，无需签名密钥；撤销立即生效。`/capabilities` 与 `/admin/settings` 反映生效模式；`llm.enabled=false` 时保留 M0 模拟模式。

配置真实上游时，在运行环境设置可用的 `RYZHUB_MODEL`，并配置 `ANTHROPIC_API_KEY` 或使用下述 BYOK API，再运行 `./scripts/dev-m1.sh`。可用 `RYZHUB_UPSTREAM_URL` 指定兼容的 Anthropic Messages 基础 URL；不要把 `/v1/messages` 后缀放入该值。此路径本轮未调用真实上游。

### BYOK 与 usage API

均使用用户的 Authorization Bearer，并检查实时成员关系与私有资源权限：

| 请求 | JSON / 说明 |
|---|---|
| `POST /orgs/{org}/credentials` | `{"name":"工作凭据","api_key":"…","organization":false}`；组织级凭据需 owner/admin |
| `GET /orgs/{org}/credentials` | 列出可用的本人/组织凭据元数据，不返回密钥 |
| `DELETE /orgs/{org}/credentials/{id}` | 撤销凭据及显式绑定它的 virtual key |
| `POST /orgs/{org}/conversations/{conv}/model-credential` | `{"credential_id":"uuid"}`；指定此对话后续新任务的凭据，`null` 恢复自动选择，运行中任务不变 |
| `GET /orgs/{org}/usage` | 本人的计量记录；owner/admin 可查看组织记录 |

自动选择顺序为个人 → 组织 → 平台；实例/组织策略可禁用个人或平台凭据。指定的凭据无权使用、已撤销或不可用时直接拒绝，不自动降级到平台 key。M1 暂无凭据管理 UI。

### 模型网关

网关说四种协议，请求原样转发（只替换认证、需要时替换模型 ID），用量从流里读出：

| 协议 | 路径 | 默认上游 |
|---|---|---|
| Anthropic Messages | `/v1/messages`，`/v1/messages/count_tokens`（只转发，不计量） | `llm.upstream_url`、`platform_key_env` |
| OpenAI Responses | `/v1/responses`、`/v1/responses/compact` | `llm.responses_url`、`responses_key_env` |
| OpenAI Chat Completions | `/v1/chat/completions`（流式请求会补上 `stream_options.include_usage`） | `llm.chat_completions_url`、`chat_completions_key_env` |
| Gemini | `/v1beta/models/{model}:generateContent`、`:streamGenerateContent` | `llm.gemini_url`、`gemini_key_env` |

`GET /v1/models` 列出这把密钥可用的模型：带 `anthropic-version` 头时用 Anthropic 的格式，否则用 OpenAI 的。

一个模型可以配置多个上游：`[[llm.models]]` 的 `upstreams` 每项有 `url`、`key_env`（空则用协议默认）、`model`（上游的模型 ID，不同时才填）、`priority`、`weight`。网关先试 `priority` 最小的，同一优先级内按权重随机。还没转发任何内容时，遇到连接失败、5xx 或 429 就换下一个；最后一个上游的回答原样返回。成员自己的凭据只发往协议的默认上游。

策略字段：`max_reasoning`（minimal/low/medium/high/xhigh/max）限制请求的推理强度。各协议的写法都会检查：effort 等级，或换算成等级的 thinking 预算。超过时返回 403 `REASONING_DENIED`。`retain_model_content_days` 开启后，每个请求的请求体与响应（响应最多 8 MiB）存进主存储的 `<org>/llm/<usage id>/`，按天数过期，由 retention 任务删除。owner/admin 用 `GET /orgs/{org}/usage/{id}/content` 查看；保留天数不超过 `retention.usage_days`。迁移 0026 新增 `usage_records.content_expires_at` 与 `retained_content`。

### Chat 上下文

请求的上下文估算超过 `llm.context_budget_tokens`（默认 120000，按约 4 字节一个 token 估算，0 关闭）时，最早的工具结果被移出上下文，直到低于预算的 60%。每个结果留一个占位：调用 ID、大小、开头 300 个字符。模型用 `read_tool_output` 工具按 ID 分页读回原文（原文在 `tool_calls` 表里）。本轮（最后一条用户输入之后）的结果不动；1 KiB 以下的结果也不动。占位是确定的，按从旧到新的顺序移出，所以上下文前缀只在跨过阈值时变化，不会每步都让提供方的缓存失效。

`llm.compaction_models` 里的 Anthropic 模型（按 ID 前缀，默认是支持上下文压缩的 Claude 系列）：
- 每个请求都带 `compact_20260112`：上下文超过 `compaction_trigger_tokens`（默认 150000，最小 50000）时由提供方自动压缩，压缩块留在回复里，此前的内容提供方不再读取。
- 对话的“压缩”使用提供方的按需压缩（`compaction: {type: "summarize"}`），压缩块之后成为上下文的开头。

提供方不能压缩时（模型不在列表中、请求失败、没有给出压缩块），改为请模型写一份摘要，由摘要代替之前的上下文。Responses 的 `/compact` 失败时也这样处理。所以 Chat 的两种协议都支持压缩。

沙箱内通过 `http://127.0.0.1:8090/v1/messages` 请求模型，由 guest 内存凭据代理经 Unix socket 转发。Chat 的沙箱工具静默执行，不请求审批。exec 工具支持后台运行、poll/stdin/kill、超时和截断输出；PTY 明确不支持。完整输出留在执行节点。仅 `/workspace` 为后续任务保留。

沙箱除回环外没有网卡，只能经出站代理访问白名单里的主机：沙箱进程的 `http(s)_proxy` 指向 `127.0.0.1:8091`，guest 把连接转给执行节点上该沙箱专用的 `control/egress.sock`。代理只接受 `CONNECT` 和 `http://` 请求，目标须在白名单上、端口为 80 或 443，并且解析结果全部是公网地址；连接直接用解析出的地址，防止 DNS 重绑定。白名单是 `[sandbox] egress_allow`（默认是常用软件包源与 GitHub，`*.example.com` 匹配子域名，空列表即关闭网络），组织策略的 `egress_hosts` 可以进一步收窄。每个连接（含被拒绝的）都由心跳收集进 `audit_logs`（`action='egress.connect'`，`detail` 记录拒绝原因和收发字节数）；执行节点按组织限制每分钟的连接数（环境变量 `RYZHUB_EGRESS_PER_MINUTE`，默认 600）。访问沙箱自身回环地址的请求即使被不认 `no_proxy` 的工具送进代理，guest 也会在本地直接处理。

磁盘配额：执行节点设置 `RYZHUB_QUOTA_VOLUME_SIZE`（字节，可写在环境变量或节点根目录的 `host.env` 中）后，host 首次启动时在 `<root>/volume.img` 创建并预分配 XFS 镜像（`RYZHUB_QUOTA_VOLUME_SPARSE=1` 改为稀疏镜像，但宿主磁盘写满时卷会出错），以 `prjquota` 挂载到 `<root>/volume`。新沙箱的可写层（upper/work）和 Chat 的 `/workspace` 各自是一个 project，上限为 `[sandbox] disk_bytes`（默认 10 GiB，组织策略 `sandbox_disk_bytes` 可降低）；写满时沙箱内得到 ENOSPC，节点不受影响。检查点暂存也放在卷上以便 reflink，但不计入沙箱配额。启用前创建的沙箱留在原位置、不受限制；启用后不要再关闭，否则卷上的沙箱将找不到。guest 以较低的 `oom_score_adj` 运行，内存不足时内核先杀沙箱里的命令，guest 与沙箱保持运行。

执行节点在 `[[sandbox.nodes]]` 中按名称配置（`name`、`host_socket` 或 `address`、`capacity`）；未配置时只有一个名为 `default`、位于 `sandbox.host_socket` 的节点。每个沙箱记录自己所在的节点，之后关于它的请求都发往该节点；删除工作区这类整体操作只发往它用过的节点。新沙箱放在 active、未失联且未满容量的节点上：优先对话上次所在的节点（工作区在那里），否则选负载最低的；`execution_nodes.state` 设为 `draining` 的节点不再接收新沙箱。另一台机器上的节点用 TLS 访问：节点设置 `RYZHUB_TLS_LISTEN`（如 `0.0.0.0:7443`）、`RYZHUB_TLS_CERT`/`RYZHUB_TLS_KEY`（节点证书）与 `RYZHUB_TLS_CLIENT_CA`（只接受该 CA 签发的客户端证书）；控制面在该节点的配置里写 `address`、`server_name`、`tls_ca` 与客户端的 `tls_cert`/`tls_key`。

Chat 沙箱在 run 结束后保留 `[sandbox] chat_idle_secs`（默认 600 秒）供同一对话的下一次 run 复用，之后销毁；`/workspace` 始终保留在节点上。`chat_checkpoints`（默认开启，需要存储）在每次用过沙箱的 run 结束后把 `/workspace` 检查点到存储；新沙箱落到没有该工作区的节点（换了节点或节点丢失）时从最新检查点恢复。闲置超过 `workspace_evict_after_secs`（默认 7 天）且已完整检查点的工作区会被移出节点，下次使用时恢复。挂起、归档或丢失的 Work 沙箱在 `reclaim_after_secs`（默认 1 天）后从节点销毁（恢复总是使用新沙箱）。运行中的 Work 工作区超过 20 分钟没有成功检查点时记录警告和审计（`workspace.checkpoint_stale`），工作区状态 API 返回 `stale: true`。

Work 对话在工作区运行时可以打开终端（工作区栏的“终端”按钮）：`/orgs/{org}/conversations/{conv}/terminal` 是一个独立的 WebSocket（与 `/ws` 相同的 ticket/Bearer 认证），二进制消息传终端字节，文本消息传 `{"resize":{...}}`、`{"close":true}` 与服务端的 `{"attached":{...}}`、`{"exit":code}`。终端是沙箱里的登录 shell，经执行节点直连 guest，不经过事件流；关闭面板或断线只是脱离，shell 继续运行，重连时按已收到的字节偏移续传，10 分钟无人连接才结束。连接期间沙箱不会被空闲挂起；执行代次变化（Owner 接管）会断开连接，客户端自动重连。

运行环境：`[sandbox] deps` 是可选的只读依赖层（按 digest 固定，挂在基础镜像之上），`scripts/build-deps.sh DIR` 或 `--dockerfile FILE` 构建并导入执行节点、输出引用。`[[sandbox.profiles]]`（`name`、`image`、`deps`、`memory_bytes`、`pids_limit`、`disk_bytes`）定义可选环境，对话或项目的运行设置 `env_profile` 选用；工作区沿用创建时的镜像与依赖层，检查点记录二者并在恢复时核对。控制面每小时回收它用过、现已无人引用（配置、未删除的沙箱、可能恢复的检查点）的镜像，未被引用满 `image_gc_after_secs`（默认 7 天）才删除；其他控制面或运维准备的镜像不会被删除。

`view_image` 让 Chat 模型查看沙箱里的图片（PNG、JPEG、GIF、WebP、BMP），相对路径从 `/workspace` 开始。文件在沙箱内由 guest 解码：符合要求的 PNG/JPEG/WebP 原样返回，其余的转正、缩放到长边不超过 2000 px 后重新编码（优先 PNG，过大时改用 JPEG），单张不超过 3 MiB。图片按对话存入 `chat_images`（迁移 0015），模型上下文和检查点只保存引用，发送请求时才内联。分叉会复制图片，删除对话时一并清除。LLM Gateway 请求体上限为 32 MiB。

## M2 Work 本地联调

需要 Linux、Docker、`uv`、`musl-tools`（提供 musl-gcc）、静态 Linux musl 版 **Codex 0.154.0**。可用 `RYZHUB_CODEX_BINARY=/absolute/path/to/codex` 指定二进制；不能使用 npm JavaScript 启动脚本。执行：

```sh
./scripts/dev-m2.sh
```

脚本准备执行节点及固定版本 gVisor、启动 PostgreSQL/NATS，执行迁移，启动本地 Anthropic/Responses fixture。API/WS 是 **8180**，LLM Gateway 是 8181。客户端服务器地址填 `http://127.0.0.1:8180`，在顶部切到 Work 后发送第一条消息即可创建对话并使用实际 Codex。默认沙箱 runtime 为 runc。

Work 支持原生流式输出、补充输入、停止、审批、提问与会话恢复。界面提供保存检查点、归档和恢复；归档前需结束当前运行。恢复保留历史，将检查点水位之后的操作标记失效。`side_query`、fork 和 PTY 尚不支持，`/capabilities` 明确返回能力。

每轮完成后自动保存检查点，运行中默认每 300 秒保存；归档也先提交检查点。RPO 取决于最后一个成功提交的检查点，上传失败保留旧恢复点。检查点以流式方式在执行节点、Server 与存储之间传输（S3 使用分段上传），不整体读入内存。主存储与可选二级存储都支持本地/NAS 目录或 S3 兼容接口。默认保留 30 天、每个对话最多 20 个检查点（`[work] checkpoint_retention_days`、`checkpoint_keep_last`，设为 0 关闭对应限制）；被淘汰的检查点先进入默认 86400 秒的延迟回收期，Owner 每 60 秒清理。最新恢复点与所有 restore pin 始终受到保护。空闲超过 `idle_suspend_secs`（默认 1800 秒，组织策略可收紧）的 Work 沙箱会先提交检查点再停止，下一条消息从该检查点恢复。

运行配置在 `var/m2/config.toml`，开发密钥复用 `var/m1/secrets.env`，都被忽略。`dev-m2.sh` 每次生成本地 fixture 配置；接真实上游时复制该文件，设置 `llm.responses_url`（例如 `https://api.openai.com`）、`llm.responses_key_env`（默认 `OPENAI_API_KEY`）、`work.model` 与模型白名单，保留密钥环境后通过 `RYZHUB_CONFIG=/absolute/config.toml target/debug/ryzhub-server all` 启动。BYOK 创建请求增加 `"provider":"openai"`；Anthropic 凭据不会用于 Responses 请求。真实模型调用本轮未验证。

工作区 API 使用用户 Bearer token：

| 请求 | 说明 |
|---|---|
| `GET /orgs/{org}/conversations/{conv}/workspace` | 当前检查点、水位、沙箱状态和最近作业 |
| `POST /orgs/{org}/conversations/{conv}/workspace` | `{"kind":"checkpoint"}`、`archive` 或 `restore`；返回后台作业 ID |

S3 配置、MCP catalog、故障场景、验证范围和部署限制见 [M2 验收与配置](docs/m2-acceptance.md)。

## M3 本地协议联调

先准备固定版本的 Codex 与执行环境（已有 M2 环境可复用）：

```sh
./scripts/prepare-m2.sh
./scripts/prepare-m3.sh
./scripts/dev-m3.sh
```

`prepare-m2.sh` 要求 Codex 0.154.0，可用 `RYZHUB_CODEX_BINARY` 指定二进制路径。M3 镜像锁定 Claude Agent SDK 0.3.283，并记录不可变镜像 digest。API/WS 为 `http://127.0.0.1:8280`，Gateway 为 8281，本地 Anthropic/Responses fixture 为 9181/9182。客户端连接地址填写 8280。脚本使用独立 `ryzhub_m3` 数据库，保留已有开发库。

本机 demo 在另一个终端构建并启动，默认连接 M3 后端：

```sh
./scripts/build-web.sh --dart-define=RYZHUB_SERVER_URL=http://127.0.0.1:8280 --output=../var/demo/web
python3 scripts/serve-web.py --directory var/demo/web
```

访问 [http://127.0.0.1:3000](http://127.0.0.1:3000)，填写测试邮箱和至少 8 位密码注册即可。Chat 中发送普通文字可测试流式响应，发送“执行”或“提问”可测试沙箱执行与用户回答。当前回复由本地协议 fixture 生成，尚未接入真实模型；项目、会话、审批和沙箱执行链路实际运行。

前端只让用户选模型：输入框的“模型”选择器中，Chat 按提供方列出模型，Work 在 Codex / Claude Code 子菜单里选模型；上游格式（Anthropic Messages 或 OpenAI Responses）由服务端按模型所属提供方推断。对话开始后只能切换同一提供方（Work 为同一执行器）的模型，选择其他模型会用它打开新对话。在侧栏“项目”中管理项目指令和文本知识文件，在模型菜单中切换推理强度，Work 中切换计划模式，在消息上分叉，并通过对话菜单执行压缩、重命名、归档和导出。设置与项目内容在下一轮生效，设置变更不写入时间线，发送消息时会携带尚未确认的设置。Claude 的运行中补充会排入下一轮；界面分别显示“等待注入点”和“已转交执行器”，不声称模型已采纳。

### ACP harness

除 Codex 与 Claude Code 外，Work 可以接入说 [Agent Client Protocol](https://agentclientprotocol.com)（v1）的 agent。每个 `[[harness.acp]]` 是一个 harness，名字由实例决定：

| 字段 | 含义 |
|---|---|
| `name`、`label` | run 设置里的 harness 名（小写字母、数字、`-`），客户端显示的名字 |
| `command`、`args` | 镜像里的可执行文件与参数，在 `/workspace` 启动，经 stdio 通信 |
| `env` | 环境变量。值里的 `{model}`、`{models}`（以每个可用模型为键的 JSON 对象）、`{proxy}`（guest 模型代理 `http://127.0.0.1:8090`）、`{instructions}`（平台指令文件）会被替换 |
| `protocol` | agent 的提供方所用的网关协议，决定这个 harness 的模型 |
| `model` | 默认模型；其他模型在 `llm.models` 的 `harnesses` 里列出这个名字 |
| `model_option` | agent 会话选项 `model` 的取值，`{model}` 替换为模型 ID，用于切换模型 |
| `version` | 镜像固定的 agent 版本，检查点只能用同一版本恢复 |

guest 把平台请求翻译成 ACP。agent 不声明文件与终端能力，它在沙箱里自己读写文件、运行命令：
- 每一轮是一次 `session/prompt`。
- 中断是 `session/cancel`。
- 恢复工作区后用 `session/load` 接上原会话。
- 计划模式是会话的 `plan` 模式。
- 权限请求按审批模式处理：完全自动全部允许，允许编辑时编辑类直接允许，其余交给用户。
- 回复流式显示，思考、计划、命令（含输出）、文件 diff 都成为条目。
- 补充指令在下一轮生效。
- 不支持分叉、问题。
- 压缩以 `/compact` 发给 agent。

M3 镜像带 OpenCode 1.19.0（musl 版，另装 ripgrep），profile 见 `crates/server/tests/m3.rs` 的 `opencode_profile`，经 OpenCode 的 Anthropic provider 与 guest 代理使用平台模型。

升级先运行迁移 0009–0012，再更新 Server、Host/Guest、M3 镜像和 Web。已有 Work 工作区固定其镜像；新工作区使用新镜像。保留 `var/m1/secrets.env` 中的 vault 主密钥。项目删除可在 30 天内恢复，到期由 Owner 侧 collector 清理内容、工作区和未被引用的检查点；分支恢复 pin 仍受保护。

项目 API：`/orgs/{org}/projects`，`/{project}/files`、`/{project}/restore`；会话的 `/project` 用于移动，`/commands` 返回能力和配置，`/settings` 管理标题/归档，`/export` 导出。所有接口使用原有认证和租户边界。模型路由在 `llm.models` 中声明 `id/protocol/harnesses`，同时受组织策略限制。`GET /orgs/{org}/models` 按组织策略列出可选模型（Chat 列表，Work 按 harness 分组，含提供方、默认模型和推理等级）；创建对话、项目默认值或 `UpdateRunConfig` 只传 `model` 时，服务端从路由推断 protocol，Work 未指定 harness 时一并推断。完整配置和验收边界见 [M3 验收](docs/m3-acceptance.md)。

## 从 0011 升级到 0012

0012 把运行时改为事件驱动，并收紧了几处数据模型。升级顺序：

1. 停止所有 `owner` 与 `llm-gateway` 进程（`gateway` 可继续服务读请求）。
2. `ryzhub-server migrate`。迁移会把 runs 上的项目快照改写为按内容去重的 `project_revisions`，删除 `ws_resumes`，并为 events/audit_logs 建立 DEFAULT 分区。
3. 更新执行节点的 `ryzhub-host` 与 `ryzhub-guest`，并停止节点上现有的沙箱容器。guest 是沙箱的 init 进程，已在运行的沙箱会继续使用旧 guest；而 guest 日志格式、统一 harness 事件协议和流式检查点传输都已改变，新旧 Server/guest 不能混用。升级前让运行中的任务结束：每轮结束会自动提交检查点。
4. 启动新版 Server 各角色，并以迁移账号常驻 `maintain --loop`。节点租约过期后，Owner 会从最新检查点恢复每个 Work 工作区（检查点之后的操作按 RPO 规则标记失效）；Chat 沙箱是一次性的，下次使用时重建。
5. 客户端无需同时升级：线协议只做了向后兼容的新增（见[线协议](docs/wire-protocol-v1.md) v1.1）。

行为变化：
- 已签发的旧版 JWT 网关密钥全部失效。Chat 每次模型调用都会签发新密钥；Work 沙箱在恢复或下一轮开始时换发。配置中的 `llm.signing_key_env` 与 `RYZHUB_SIGNING_KEY` 可以删除。
- WebSocket 不再在服务端保存订阅集合；`Welcome.resume_token` 恒为空，客户端按本地游标重新订阅（现有客户端本来就这样做）。
- 移除组织成员只撤销其在该组织的访问（订阅、网关密钥、运行中的任务），不再注销其全部会话。
- 新增配置：`approval_ttl_secs`（默认 24 小时）、`[retention]`、`[work] checkpoint_keep_last` / `idle_suspend_secs`、`llm.vault_key_id` / `vault_retired_keys`。检查点保留期默认从「不清理」改为 30 天。

## M4 搜索与记忆

侧栏搜索支持消息内容与项目文件，结果可定位消息；设置页“记忆”管理个人/项目记忆、优先级和两个独立开关。Chat/Work 提供历史搜索与显式记忆工具，写入提示支持撤销。临时对话不进入搜索或记忆；删除对话可同时删除相关记忆。检索实时核对源权限，删除和项目边界变更不等待索引更新。

升级需要先更新 Compose PostgreSQL 镜像（包含 PGroonga 4.0.4 与 pgvector 0.8.2），再运行迁移 0013。全文默认可用；向量检索配置真实 embedding 服务后启用，更换模型用 `ryzhub-server reindex` 回填。配置、接口、验收和部署边界见 [M4 验收](docs/m4-acceptance.md)，两轮 Claude Code 审查及修复见 [审查记录](docs/m4-code-review.md)。

M4 接真实上游时，将实例配置保存为 `var/m4/config.toml`，将模型密钥环境变量保存为权限 0600 的 `var/m4/secrets.env`，运行 `./scripts/dev-m4.sh`。此脚本保留这两份文件，复用 `var/m1/secrets.env` 的 vault 主密钥和已有 `ryzhub_m3` 数据库，不启动协议 fixture。可用 `RYZHUB_M4_CONFIG`、`RYZHUB_M4_SECRETS` 和 `RYZHUB_M4_DATABASE` 指定其他本地文件及已有数据库。

Responses 服务设置 `llm.responses_url`（服务端自动追加 `/v1/responses`，基础地址不要重复包含 `/v1`）、`llm.responses_key_env`、`llm.responses_model` 和 `work.model`。将模型加入 `policy.models` 及 `llm.models`（`protocol = "responses"`，`harnesses = ["chat", "codex"]`）；`llm.model` 也可设为这个模型，使新建 Chat 和历史摘要默认走 Responses。未配置兼容的 1024 维 embedding 服务时，全文搜索与记忆仍可使用。

**本地 embedding**：用 `--features local-embedding` 构建后，设置 `search.embedding_provider = "local"`，服务端在进程内用 candle 跑 Qwen3-Embedding（`search.local_embedding_model`，默认 `Qwen/Qwen3-Embedding-0.6B`），向量截成 1024 维后重新归一化，与外部服务的向量可以互换。模型首次使用时下载到 `HF_HOME`（约 1.2 GB），不能联网的服务器预先填好这个缓存。本地向量不经过凭据、策略模型列表和用量记录。CPU 上一次只算一批。

## 用其他账号登录

实例可以配置任意多个 OpenID Connect 提供方（`[[auth.oidc]]`：`name`、`label`、`issuer`、`client_id`、`client_secret_env`、`scopes`）和 GitHub（`[auth.github]`），登录页按 `/capabilities` 的 `auth_providers` 显示按钮。服务端的回调地址是 `<public_url>/auth/oauth/<name>/callback`（GitHub 为 `github`）。

- 流程：`GET /auth/oauth/{provider}/start?return_to=` 带 PKCE、state、nonce 跳到提供方；回调时 OIDC 用 JWKS 校验 ID token（签名、issuer、audience、nonce、有效期），GitHub 从 API 取已验证的主邮箱。然后带一次性的 `login_code` 回到 `return_to`，客户端用 `POST /auth/oauth/exchange {code}` 换会话。`return_to` 必须以 `auth.return_urls` 中的某一项开头（默认是本机的网页地址和原生端的回环地址）。
- 账号：已绑定的身份直接登录；提供方确认过的邮箱已有账号、且 `auth.link_verified_email`（默认开）时绑定到该账号；否则新建用户和个人组织。
- 设置 › 账户 › 登录方式列出已绑定的身份，可以绑定其他提供方（`start` 带 `link=<WS ticket>` 证明是本人）或解绑；最后一种登录方式不能解绑。
- 原生端在系统浏览器里登录，回到本机的回环地址；网页端整页跳转。

## 自动归档与保留期

策略 `auto_archive_after_secs` 与 `archive_retention_secs`（实例配置 `[policy]`，组织策略只能更严）开始生效，Owner 每 5 分钟检查一次：

- 闲置超过 `auto_archive_after_secs` 的对话自动归档：期间没有运行、没有排队的命令或工作区作业。置顶和临时对话不自动归档。运行中或挂起的 Work 工作区通过归档作业先提交检查点、再停止沙箱；其余直接归档。
- 归档超过 `archive_retention_secs`（从归档或之后最后一次运行结束算起）的对话，删除它存下的工作区：检查点全部退役并回收，对象只在没有其他检查点共用、也没有恢复钉住时删除；执行节点上留着的 Chat 工作区一并清除。时间线保留。取消归档会重新计时。
- `workspace_heads.storage_state` 记录工作区文件在哪里：`hot`（执行节点上）、`archiving`、`cold`（只在检查点存储里）、`restoring`、`purging`、`deleted`，`GET …/workspace` 返回 `storage_state`。被删除的 Work 工作区不能恢复（`WORKSPACE_PURGED`），对话仍可查看；Chat 对话取消归档后从空的工作区开始。
- 两种操作都写审计（`conversation.auto_archive`、`workspace.purge`）。

## Android 通知

需要用户处理的事件（审批、提问）和运行结束时，服务端通过 FCM HTTP v1 发高优先级的可见通知，点击后打开对应对话。

- 服务端：`[push] enabled = true`，`fcm_credentials` 指向 Firebase 服务账号的 JSON 密钥文件。事件在同一事务里写入 `push` 作业，由 Owner 的作业队列投递；只通知对话创建者的设备，有活跃 WebSocket 连接的设备不通知（它已经看到了），已失效的设备令牌自动删除，审批在投递前已被处理的不再通知。
- 设备：`POST /devices {device, platform:"android", token, locale}` 登记，绑定当前登录；退出登录后不再通知，`DELETE /devices/{device}` 取消登记。通知标题和正文按设备语言（`zh`/`en`）生成。
- 客户端：Android 构建需要 Firebase 项目的 `client/android/app/google-services.json`（不提交）；没有这个文件时照常构建，只是没有通知。登录后请求通知权限（Android 13 起）、取得令牌并登记。

## 联网搜索

联网搜索使用模型服务商自带的 web search，平台不做额外限制：Chat 请求始终附带内置工具（Anthropic 为 `web_search_20260318`，Responses 为 `web_search`），因此上游必须支持服务端工具；Codex 配置为 `web_search = "live"`；Claude Code 保留自带的 WebSearch。Anthropic 的 `pause_turn` 会把原样的 assistant 内容再发送一次来续写。回合未结束时（暂停中，或服务端工具排在客户端工具之后），补充指令会等到回合结束再发送。搜索在 Chat 和 Work 中都显示为 `web_search` 工具步骤。Chat 的流式回复遇到搜索会在当场拆分：已输出的文字单独成条，接着是搜索步骤，后续文字写入新的条目。暂停的回合中，搜索的结果会在下一步补上。重试时，失败那次尝试新增的条目标记为 `discarded`，客户端不显示。每段文字引用的网页显示在该段下方（`ItemDetail.answer`，协议 minor 2）。

## 附件与文件库

每个成员有自己的文件库。文件属于上传者，不属于某个对话；同一个文件可以在上传者的多个对话里引用。删除对话只移除引用，文件留在库里；在文件库里删除文件会把它从所有对话中移除：时间线显示“已删除”，模型下一次请求里换成提示文字，沙箱下次同步时文件消失。

- 上传：`POST /orgs/{org}/attachments?name=<文件名>`，请求体是文件原始字节。服务端边收边算 sha256，不信任客户端声明的类型或哈希。单个文件默认 50 MB（策略 `attachment_bytes`，平台上限 64 MiB），每个成员文件库总量由策略 `attachment_quota_bytes` 限制（默认不限）。每条消息最多 10 个文件。
- 分片上传：超过 8 MiB（`/capabilities` 的 `attachments.part_bytes`）的文件分片上传，每片可以单独重试，中断后接着传。
  - `POST /orgs/{org}/uploads {name,size}` 开始，返回分片数、已收到的分片，主存储是 S3 时还有每个缺失分片的预签名 PUT 地址（1 小时有效，再次 `GET /orgs/{org}/uploads/{id}` 换新）。
  - 经服务端：`PUT …/uploads/{id}/parts/{n}`，边收边算该片的 sha256，存进主存储；同一片再传一次会替换。
  - 直传 S3：客户端把分片 PUT 到预签名地址，再用 `POST …/parts/{n}/sent {etag}` 报告。网页版直传要求桶的 CORS 允许 PUT 并暴露 `ETag`。
  - `POST …/uploads/{id}/complete`：服务端按顺序把整份文件读回来，计算整份的 sha256，之后与普通上传一样进入文件库。重复完成返回同一个文件；`DELETE …/uploads/{id}` 放弃。分片在完成或放弃后删除，24 小时未完成的上传由回收任务清理。
- 类型按文件头判断：图片（PNG/JPEG/GIF/WebP/BMP）由独立的 `ryzhub-server render-image` 子进程解码，限制内存和 CPU，生成长边不超过 2000 px、不超过 3 MiB 的渲染版；PDF 发给模型时从存储读取；不超过 32 KiB 的 UTF-8 文本直接内联；其余文件只告诉模型路径。
- 存储：内容写入 `BlobStore` 的 `<org>/attachments/<sha256>`（主存储与可选二级存储），同组织内相同内容共用一个对象，最后一个引用它的条目删除后由 Owner 每 60 秒的回收任务清理；中断的上传 24 小时后放弃。迁移 0016 新增 `attachments`、`conversation_files` 和 `pending_inputs.attachment_ids`。
- 接口：`GET /orgs/{org}/attachments?q=&kind=`（文件库列表，可按文件名搜索、按 `image,pdf,text,file` 过滤）、`…/{id}/content`（原文件，总是作为下载返回）、`…/{id}/preview`（图片渲染版）、`DELETE …/{id}`；只有上传者能访问。`SendMessage.attachment_ids` / `Steer.attachment_ids` 携带文件，消息条目的 `ItemDetail.attachments` 列出文件（协议 minor 3），有文件时正文可以为空。
- Chat：用户消息里依次是图片/PDF 引用、一段列出 `/attachments/<名字>` 路径的说明、内联的文本文件、正文。上下文和检查点只存引用，发请求时才内联；每次请求最多带最近 20 张图、4 个 PDF，合计 16 MiB，其余换成提示文字；压缩上下文时只发提示文字。`llm.models` 的 `inputs`（默认 `["image","pdf"]`）声明模型能直接读的类型，其他类型只给路径。
- 沙箱：Chat 与 Work 沙箱都以只读方式挂载 `/attachments`，内容是该对话当前引用的全部文件，按需从存储上传到执行节点（执行节点上按对话保存一份，删除工作区时清除）。Chat 每次执行工具前同步，Work 在每次运行开始以及发送带文件的输入前同步。
- Work：本轮正文末尾附上文件路径列表；原样可用的图片同时作为图片输入（Codex 为 `localImage`，Claude 适配层读成图片块）。执行器自己复制进 `/workspace` 的副本在文件删除后无法收回。
- 模型交给用户的文件：模型（Chat 和 Work 的 Codex、Claude 都一样）在回答里用链接引用沙箱里的文件，如 `[报告](sandbox:/workspace/report.pdf)`，也可以引用 `/attachments/<名字>`。回答完成时服务端把链接指向的文件复制进对话创建者的文件库（`direction=output`，计入配额，每条回答最多 20 个），绑定到本对话，之后的沙箱在 `/attachments` 下还能拿到；同名同内容的文件再次引用时复用同一条目。读取走执行节点的 `WorkspaceFile` 命令：用 `openat2(RESOLVE_IN_ROOT)` 把路径解析限定在 `/workspace` 内，符号链接和 `..` 都出不去，只接受普通文件。结果记在回答条目的 `AnswerDetail.files`（协议 minor 4）：每个链接对应一个文件，或一个错误码（`FILE_NOT_FOUND`、`NOT_A_FILE`、`FILE_TOO_LARGE`、`ATTACHMENT_QUOTA_EXCEEDED` 等）。客户端把这类链接显示为带文件图标的链接，点击打开预览；文件库有“AI 生成”标签页。
- 客户端：输入框“+”菜单里的“上传文件”和“从文件库选择”，也可以拖放文件到对话区域，或在网页版直接粘贴截图。消息里的图片显示缩略图。点击任何文件（消息里、输入框里或文件库里）都打开预览。从对话里打开的预览浮在整个页面上（包括侧边栏），底下的页面模糊显示，点击模糊处等于关闭；底部输入框在当前对话里带着这个文件继续提问（回答进行中时作为补充指令），发出后预览关闭。从文件库打开的预览盖住侧边栏右侧的整个区域，底部提问会开一个带着该文件的新对话。图片先显示服务端的渲染版、再换成原图（原图不超过 20 MB 时；全分辨率、动图会动，服务端没渲染成功的图片也能显示），可缩放，PDF 由 pdfrx（PDFium，网页版是 WASM，首次打开 PDF 时才加载）渲染，文本和代码按原样显示、Markdown 渲染显示，其他类型显示“无法预览”和下载按钮；同一组文件可用 ← → 或两侧按钮切换。渲染器在 `client/lib/ui/preview.dart` 的 `previewers` 里按顺序登记，以后加 HTML、SVG 预览只需插入新的渲染器。构建 Windows 版需要开启开发者模式（pdfrx 的要求）。侧边栏的“文件库”页（网页地址 `#/library`）可以按网格或列表浏览、搜索、按类型筛选、上传（也可拖放或粘贴到该页）、下载、删除文件，或把文件带进一个新对话。

## 插件（M5）

插件为对话带来远程 MCP 工具、技能（skills）、命令和界面。成员在“设置 › 插件”中安装：先看到插件要求和带来的全部内容，确认后才安装。

**来源**（`POST /orgs/{org}/plugins/preview {source}`）：
- `url`：zip 或 tar.gz 包的地址；
- `git`：仓库与分支、标签或提交，记录拉取到的提交；
- `upload`：上传的包，base64 放在 `archive`；
- `catalog`：实例配置的官方目录 `plugins.catalog_url` 里的条目；
- `mcp`：单独一个远程 MCP 地址，作为只有一个服务的最小插件。

**拉取**：只连 HTTPS 的公网地址。
- 域名在服务端解析，连接固定到检查过的地址；每次重定向都重新检查。
- git 也连到检查过的地址。
- 包最多 16 MiB、1000 个文件，在单独的受限进程（`ryzhub-server parse-plugin`）里解包和解析。
- `plugins.insecure_hosts` 只用于本机开发和测试，列出的主机可以用 HTTP 和内网地址。

**包格式**：Agent Plugins 1.0.0。
- `plugin.json`（字段固定）、`mcp.json`（`streamable-http` 的服务；`stdio` 只提示、不运行）、`skills/<名>/SKILL.md`。
- 平台的内容放在扩展命名空间 `plugins.namespace`（默认 `app.ryzhub`）下：
  - `extensions.app.ryzhub` 里写：
    - `permissions.tools`：`default`、`readOnly`、按工具的 `allow`/`ask`/`deny`；
    - `commands`：`name`、`title`、`prompt`（命名空间目录里的文件，用 `{{参数}}`）、`params`（JSON schema）；
    - `ui`：`declarative`、`mcpApps`；
    - `userConfig`：`type`、`secret`、`required`，以及 `header`、`prefix`，表示填进对插件服务的请求头。
  - 包里的 `app.ryzhub/signature.json`（`{key, signature}`）是发布者对内容哈希的 ed25519 签名。
- Claude Code 插件（`.claude-plugin/plugin.json`）会自动转换：`commands/*.md` 成为命令（`$ARGUMENTS` 成为参数 `arguments`），`.mcp.json` 的 HTTP 服务、`skills/` 照用，agents 与 hooks 只提示。

**信任**：
- 已签名：签名用 `plugins.trusted_keys`（`key id → base64 公钥`）验证通过。
- 官方审核：来自官方目录，内容与目录记录的 `content_hash` 一致（目录不写哈希时不核对）。
- 未验证：其余。策略 `allow_unverified_plugins = false` 时不能安装。

**安装与变更**：
- 安装（`POST /orgs/{org}/plugins {version_id, config, secrets}`）即确认：
  - 当时列出的工具都算已确认；
  - 机密设置存进密钥库，只发给该插件的服务。
- 之后列出的新工具，或描述、schema、注解变了的工具，不再提供给模型，插件进入“有变更待确认”，直到成员确认。
- 更新（`/update`）重新拉取来源。新版本权限扩大，或技能、命令的文本有变，也要确认。
- 实例管理员用 `POST /admin/plugin-blocks {name | content_hash}` 停用插件，所有正在进行的 run 立即失去它的工具。
- 成员关闭插件（`PATCH …/plugins/{id} {enabled}`）也立即生效。

**启用范围**：插件默认对成员的所有对话生效。项目（`PUT /orgs/{org}/projects/{p}/plugins/{id}`）与对话（`PUT …/conversations/{c}/plugins/{id}`，输入框“+”菜单）可以单独开关，对话优先于项目。每个 run 记录开始时启用的插件。

**使用**：
- 工具名是 `<插件>__<工具>`。
- Chat 直接调用工具，不审批；插件规则为 `deny` 的工具不提供。
- Chat 系统提示列出技能的名称与描述，模型用 `load_skill` 读全文。
- 插件工具超过 `plugins.defer_tools_above`（默认 30）个时，Anthropic 模型改为按需搜索工具（`llm.tool_search_models`）。
- Work 的 harness 经 MCP Gateway 看到同样的工具；技能在 harness 启动前写进 Codex、Claude Code、OpenCode 的技能目录。
- 命令在输入框 `/` 菜单的“插件命令”分组，填写参数后以 InvokeMenuCommand 发出。

**必须审批的工具**：策略 `approval_required_tools`（`<插件>__<工具>`，可用 `<插件>__*`）中的工具，在 Work 里由 Gateway 强制审批。
- 成员在 harness 里批准过同一调用（同一工具、同样的参数），平台就签发一次性授权，Gateway 消费后放行，不再询问。
- 绕过 harness 的调用由 Gateway 自己发起审批。

**账号连接（OAuth）**：插件服务返回 401 时，插件页显示“连接账号”。
- 发现：受保护资源元数据、授权服务器元数据（核对 issuer）。
- 注册：优先 client ID metadata document（`<public_url>/plugins/oauth/client.json`），否则动态注册。
- 授权：PKCE、state、`resource`，返回时核对 `iss`；回调地址 `<public_url>/plugins/oauth/callback`，完成后回到 `<plugins.client_url>/#/plugins`。
- 令牌存在密钥库，快过期时刷新。

**提问**：插件服务在调用中提问（MCP elicitation）时，对话里出现表单（或要打开的页面），回答后调用继续。Chat 与 Work 都支持。

**界面**：
- 声明式组件：工具结果的 `_meta["app.ryzhub/ui"]` 里写卡片、文本、列表、表格、按钮、表单、图片、柱状图、代码、diff，按钮与表单调用插件的工具。图片只显示结果自带的 `data:`，网络图片显示为链接。
- MCP Apps：工具声明 `_meta.ui.resourceUri`（且插件声明 `ui.mcpApps`、策略未禁止 `allow_mcp_apps`）时，工具卡片可以打开它的页面。
  - 页面由服务端经插件服务读取，从单独的源 `plugins.content_origin`（监听 `plugins.content_listen`）提供，链接 10 分钟内有效。
  - CSP 只允许页面声明的 `connectDomains`，只能被 `allowed_origins` 嵌入。
  - Web 用 `sandbox="allow-scripts allow-forms"` 的 iframe；Android 与 Windows 用独立数据目录的 WebView，导航限于该源。
  - 页面只拿到本次调用的输入和结果，能调用工具、打开 HTTPS 链接、请求发消息（需成员确认）、调整高度。

迁移 0027 新增插件的各表、`runs.plugins`、`pending_actions.form`、`tool_calls.input`、`mcp_calls.installation_id/input`。

## 项目文档

项目的知识文件除了直接编辑文本，也可以上传文档：`POST /orgs/{org}/projects/{project}/files/upload?name=<文件名>`，请求体是原始字节（最大 32 MiB）。

- 支持 PDF、DOCX 和 UTF-8 文本，按文件内容判断。PDF 与 DOCX 的文字在单独的 `ryzhub-server extract-text` 进程里抽取（限制内存 1 GiB、CPU 30 秒），抽出的文字作为知识文件内容（超过 1 MiB 时截断并标出），参与搜索与项目上下文，计入项目的文件大小上限。
- 原文件按内容地址存进组织的文件存储（与文件库同一处，相同内容共用对象），`GET …/files/{file}/source` 下载；删除或替换知识文件、删除项目时，没有其他知识文件或文件库条目引用的原文件随之删除。
- 不支持的类型返回 `UNSUPPORTED_PROJECT_FILE`，读不出文字的文档返回 `DOCUMENT_UNREADABLE`。
- 客户端：项目对话框的“上传文档”，知识文件列表标出来源类型，可以下载原文件。

迁移 0028 新增外部身份的邮箱、`login_flows`、`login_codes` 与 `workspace_heads.storage_state`；迁移 0029 新增 `project_files` 的原文件列、`uploads`、`upload_parts` 与 `push_devices`。

## 运行 Flutter 客户端

本次使用 Flutter 3.47.5 / Dart 3.13.4、flutter_rust_bridge 2.11.1。Rust 生成的绑定已提交。原生 UI 的 SQLite 缓存和 Web 的 IndexedDB 缓存都按服务器、用户、组织隔离；访问 token 不写磁盘。缓存按记录保存：一条 `engine`（resume token、未确认的命令）加每个频道一条，一次更新只写它改动的频道，同一批写入在一个本地事务里提交；旧版的整份缓存在首次保存时改写成记录，未确认的命令保留。命令写入成功之后才会发送，定时重发和重连重发也只发已写入的命令。服务端在访问 token 到期前一分钟发 `AuthExpiring`，客户端随即续期并在原连接上发 `AuthRefresh`，频道和待发命令都不受影响；token 自然过期只会断开连接，只有会话被吊销或用户被停用才会撤销频道（`SubscriptionRevoked`）。续期失败时回到登录页。服务端拒绝的消息会在页面上提示原因，可以“恢复编辑”，把原文和附件放回输入框。Web 登录后，服务端把设备的 refresh token 写入 HttpOnly Cookie（`Path=/auth`、`SameSite=Strict`，HTTPS 下加 `Secure`，30 天滑动有效）。页面刷新或访问 token 过期时，用它换取新的访问 token，并轮换 refresh token；被替换的旧 token 在 60 秒内仍可使用，供同时刷新的多个标签页共用，超过 60 秒再出现就视为重放，吊销整个设备。退出登录会吊销该设备签发的所有 token。页面和 API 都在本机回环地址上时，客户端会让 API 使用和页面相同的主机名，保证 Cookie 同站。打开的对话写在地址里（`#/c/<id>`），刷新、前进后退和书签都能回到它。原生端把同一个 refresh token 存在系统的安全存储里（flutter_secure_storage：Android Keystore、Windows 凭据管理器），按服务器分开，只发给服务端的 `/auth/` 接口，所以重启后不用重新登录；退出登录时一并删除。原生端会记住每个服务器和组织上次打开的对话。Windows 从睡眠唤醒时（`WM_POWERBROADCAST`）立即重连，不等旧连接超时。

Web 构建准备：

```sh
cargo install flutter_rust_bridge_codegen --version 2.11.1 --locked
cargo install cargo-expand wasm-pack
rustup toolchain install nightly-2026-09-25 --profile minimal --component rust-src --target wasm32-unknown-unknown
./scripts/build-web.sh
python3 scripts/serve-web.py
```

`build-web.sh` 还会把代码高亮用的语法集写到 `client/web/syntaxes.bin`（约 960 KB，不提交）：网页在第一次显示代码时才下载它，之前代码先显示为纯文本。原生端的语法集随 Rust 库内置。

打开 `http://127.0.0.1:3000`。界面参照 ChatGPT 网页版：侧栏包含新建对话、已归档、项目和最近对话，底部账户菜单进入设置；顶部切换 Chat/Work；“新建对话”只是草稿，发送第一条消息时才在服务端创建对话并以其作为标题。设置页提供语言、外观（浅色/深色/强调色）、新对话默认值、快捷键、账户与退出登录、用量、已归档对话、模型凭据和服务器策略。界面支持中文和英文：首次进入按浏览器或设备语言选择，识别不到时使用英文，选择只保存在本机（Web 为 IndexedDB `ryzhub-prefs`）。对话可以置顶（侧栏行或对话顶部的图钉按钮，Chat 和 Work 都可以，临时对话除外）：置顶的对话按置顶时间排在侧栏“已置顶”分组，归档期间暂不显示；置顶状态随用户频道同步，其他设备即时更新。删除和临时对话（右上角按钮）在服务端启用搜索、`/capabilities` 声明 `conversation_delete` / `conversation_temporary` 后才会出现；临时对话创建时发送 `temporary: true`，列表中带该标记的对话不显示。输入框右侧显示所选模型及其推理强度（0xProto 字体），点击后在同一菜单中切换。

静态服务器必须发送 COOP `same-origin` 和 COEP `require-corp`，FRB 的共享 Wasm 内存需要 `crossOriginIsolated`。脚本提供这些头，并将 CanvasKit 与字体保存在本站。Rust 内核产物在 `client/web/pkg`，可重建、不提交二进制。

- Windows：在装有 Visual Studio C++ 工具链的 Windows 上运行 `scripts/build-windows.ps1`，它编译并复制 Rust DLL 到 Flutter 输出目录。
- Android：安装 SDK/NDK、`cargo-ndk` 和三个 Rust Android target 后运行 `scripts/build-android.sh`。Debug manifest 允许本地 HTTP；release 使用 HTTPS。
- 原生开发时可用 `--dart-define=RYZHUB_RUST_LIBRARY=<Rust 动态库绝对路径>` 显式指定库。Flutter 外壳通过 Dart 注入传输和平台缓存，三端使用同一个 bridge 和 Rust 状态机。

消息的 Markdown、代码高亮、diff 与终端输出（ANSI）都在 Rust 内核里解析，Flutter 只按块渲染：流式回复中已经写完的块不再变化，每批只重新解析并重建最后一块。时间线（消息与折叠的步骤）由内核维护，以按位置的增删改下发；帧到达后内核攒 32ms 再交给界面，空闲后的第一批立即交出。

服务端支持时（`/capabilities` 的 `create_run`），新对话由 `CreateRun` 命令创建，离线时也能新建：对话立即出现在侧栏，命令排在本地队列里，联网后发出。侧栏用圆点标出有新回复、尚未看过的对话；打开并停在最新处即算已读，各设备同步。每个对话有自己的草稿，停止输入 1.5 秒后同步到其他设备，其他设备在输入框没有焦点时采用。输入 `/` 会按组列出可用命令（通用命令、对话操作、Agent 命令），上下键选择、回车或 Tab 选定、Esc 关闭。Work 对话的输入框里有审批模式（每次询问、自动允许编辑、全部自动；策略禁止全自动时不出现），下一轮生效。由平台模拟的能力（`commandCatalog.capabilities` 为 `emulated`，如 Claude 与 ACP agent 的补充指令）旁边有说明图标。工作区栏显示最近一次检查点的保存时间，使用中超过 20 分钟没有新检查点时给出提示；归档或按保留期删除后显示工作区的存储状态。客户端优先协商 `ryzhub.v1.protobuf+zstd` 子协议，WebSocket 连续三次连不上时改用事件流（`GET /stream` + `POST /stream/{connection}`），见[线协议](docs/wire-protocol-v1.md)的“传输”。

M0 模式输入 `/approve 任意内容` 会打开模拟审批，可用两个浏览器窗口登录同一账号竞争决议。普通消息流式输出；侧栏顶部的连接状态在异常（黄色）时可点击重连，网络变化和应用恢复前台也会触发按游标补齐。Enter 发送、Shift+Enter 换行，Ctrl+Enter 也可发送（可在设置中改为只用 Ctrl+Enter 发送）；IME 组字期间不会误发送。浏览器测试脚本以 `zh-CN` locale 运行。

## 验证

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
./scripts/test-integration.sh
./scripts/test-m1.sh
./scripts/test-m2.sh
./scripts/test-m3.sh
./scripts/test-m4.sh
TEST_RUNTIME=runsc ./scripts/test-m3.sh
./scripts/test-client.sh
```

`test-client.sh` 先构建本机的 Rust 内核（`cargo build -p ryzhub-bridge --release`），Flutter 测试用它渲染消息，再检查设计 token 的生成文件是否最新，然后运行 `flutter analyze` 与 `flutter test`。`client/test/golden_test.dart` 用打包的字体渲染一段固定的时间线（Markdown、代码、diff、终端输出，浅色与深色）和审批卡片、插件组件，与 `client/test/goldens/*.png` 逐像素比对；图片在 Linux 上生成，界面有意改动后用 `flutter test --update-goldens test/golden_test.dart` 重新生成并检查差异。

颜色、间距、圆角与字号来自 `client/tokens/tokens.json`。改动后运行 `python3 scripts/gen-tokens.py` 重新生成 `client/lib/tokens.g.dart`（`theme.dart` 导出它）：颜色在 `LightTokens` / `DarkTokens`，间距在 `Space`，圆角在 `Radii`，字号在 `FontSizes`。`test-m1.sh` 和 `test-m2.sh` 会重建执行节点：本机没有 musl-gcc 时加 `CC_x86_64_unknown_linux_musl=gcc`。`test-m2.sh` 要求 codex 0.154.0，本机版本不同时用 `RYZHUB_CODEX_BINARY` 指向该版本。

集成测试标记为 ignored，必须执行脚本才会运行。它们在独立临时数据库里验证，成功后删除测试数据库；测试不会删除开发数据。测试需要有 CREATE DATABASE 权限的本地管理员，以及 `scripts/init-db.sql` 中的角色。CI 显式运行集成测试。

承载一致性保证的 SQL（租约接管/续期/fencing、seq 分配、发布、命令幂等、成员关系、run 守卫与网关密钥查找）使用 SQLx 编译期检查，`.sqlx` 提供离线元数据；其余参数化 SQL 用真实数据库集成测试验证。修改 schema 或这些查询后，对一个迁移后的数据库运行 `DATABASE_URL=... ./scripts/prepare-sql.sh` 并提交 `.sqlx` 的变化。

[M0 验收](docs/m0-acceptance.md) · [M1 与完整回归记录](docs/m1-acceptance.md) · [M2 验收](docs/m2-acceptance.md) · [M3 验收](docs/m3-acceptance.md) · [M4 验收](docs/m4-acceptance.md) · v1 剩余工作 [阶段 1](docs/v1-phase1-acceptance.md)、[2](docs/v1-phase2-acceptance.md)、[3](docs/v1-phase3-acceptance.md)、[4](docs/v1-phase4-acceptance.md)、[5](docs/v1-phase5-acceptance.md)、[6](docs/v1-phase6-acceptance.md)、[7](docs/v1-phase7-acceptance.md)、[8](docs/v1-phase8-acceptance.md) · [真机验证清单](docs/v1-device-poc.md) · [线协议](docs/wire-protocol-v1.md)

## 代码导航

| 位置 | 职责 |
|---|---|
| `proto/`、`crates/proto` | Protobuf schema、构建与编解码 |
| `crates/domain` | 共享 RBAC/资源归属检查、三层策略收紧、三条生命周期 |
| `crates/server` | REST/WS（`http`、`ws`）、RLS 存储（`store`）、Owner actor 与发布（`owner`）、run 生命周期（`runs`）、Chat loop（`chat`）、原生 harness 驱动（`work`）、LLM/MCP Gateway（`llm`、`mcp`）、插件（`plugins`：包、拉取、MCP 客户端、OAuth、界面）、执行节点访问（`sandbox`）、检查点/恢复（`workspace`）、撤销推送（`revocation`）、保留与维护 |
| `crates/execution` | containerd host、overlay 校验、guest journal（快照 + 追加日志）、exec、统一 harness 协议与 Codex/Claude 适配、ACP 桥（`acp`）、upper tar.zst 流式归档与本地凭据代理 |
| `crates/core` | sans-IO seq/Delta/Fold 状态机、订阅集合、离线命令、重连退避；时间线投影（`timeline`）、Markdown 块（`markdown`）、代码高亮（`highlight`）、diff 与 ANSI 解析 |
| `crates/bridge`、`client/lib/rust` | flutter_rust_bridge、32ms 合批与时间线 diff |
| `client` | Windows/Android/Web UI、按块渲染（`ui/markdown.dart`）、输入法、平台 IO 与传输（WebSocket、事件流） |
| `crates/server/tests/consistency.rs`、`m1.rs`、`m2.rs` | PostgreSQL/NATS/containerd 与真实进程故障验收 |

暂不提供 OAuth 登录和组织管理 UI。Windows 输入法、Android 切网与硬件帧率不能由 Linux 的自动测试替代，见验收记录。
