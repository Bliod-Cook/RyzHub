# RyzHub

按[架构方案](agent-platform-architecture-2026-09-25.md) §13 实现的 Rust + Flutter Agent 平台。已完成 M0 一致性、M1 Chat、M2 Codex Work 与 M3 第二条路径：官方 Claude Agent SDK、Responses Chat、统一命令和私有项目。模型上游继续使用本地协议 fixture；Codex、Claude SDK、containerd、gVisor、数据库和浏览器实际运行。

项目代码与启动脚本均位于仓库根目录。最新目录调整和 M3 修复复核见[迁移与验证记录](docs/root-migration.md)。

## 运行服务端

需要 Rust ≥1.94（本次验证 1.98.1）和 Docker Compose。无需安装 PostgreSQL、NATS 或 protoc。

```sh
cd RyzHub
./scripts/dev.sh
```

REST/WS：`http://127.0.0.1:8080`，实例配置页：`/admin`。首次在客户端注册邮箱和密码，自动创建个人组织。`.env.example` 仅包含本地开发凭据；实际配置可复制为 `.env` 后使用 `RYZHUB_ENV_FILE=.env ./scripts/dev.sh`。

二进制支持 `all`、`gateway`、`owner`、`llm-gateway`、`migrate`、`maintain` 六个角色。Gateway 只需要 `DATABASE_URL` 和 `NATS_URL`；Owner 另需只读 BYPASSRLS 的 `WORKER_DATABASE_URL`。迁移和维护使用 `MIGRATION_DATABASE_URL`，不会授予运行进程 DDL 权限。运行进程拒绝超级用户或 BYPASSRLS 数据库账号。

管理员设置在 `config/development.toml`：监听地址、Origin、事件保留期、租约时长、模型/harness 白名单、组织并发上限等。修改后重启对应角色。实例管理员与组织 owner 分开；管理员可通过数据库运维会话设置 `users.is_instance_admin`，再用其登录 token 在 `/admin` 查看生效配置。

以迁移账号常驻运行 `ryzhub-server maintain --loop`（每 24 小时一次；也可由外部调度每日运行一次 `maintain`）：提前建立周/月分区，把落入 DEFAULT 分区的行迁回正式分区，按 `events_retention_days`（默认 30 天）删除整周 events 分区、按 `retention.audit_days`（默认 400 天）删除整月审计分区，并清理过期 ticket。维护中断时写入落入 DEFAULT 分区，不会失败。整周清理意味着实际事件保留期可能多出不到 7 天；items 不随事件清理。已处理命令、已确认的 guest 事件、失效网关密钥和（可选的）用量记录由 Owner 角色按 `[retention]` 配置分批删除。

## M1 本地协议联调

先停止占用 8080 的 M0 服务，再运行：

```sh
./scripts/dev-m1.sh --fixture
```

脚本启动 PostgreSQL/NATS、独立 containerd 执行节点、localhost:9081 协议 fixture、8080 API/WS 和 8081 LLM Gateway，自动迁移数据库。执行节点为名为 `ryzhub-m1-execution` 的 privileged 开发容器，只挂载本项目执行状态和二进制，不挂载 Docker socket；需要 Linux、Docker 及可用的 overlay/cgroup。默认镜像运行时为 runc，镜像按 digest 交给 host。

Web 客户端发“执行沙箱验收”会请求执行审批，允许后在实际容器中执行命令；“请向我提问”会出现回答卡片；“slow 慢速响应”可用于测试运行中的补充输入。“只读旁问”只读最近提交的主线前缀，不提供工具或写回主线；它和审批模式暂时不在输入框中提供，计划后续以命令方式调整。

本地配置在 `var/m1/config.toml`，vault 主密钥在权限 0600 的 `var/m1/secrets.env`，均已忽略。vault 主密钥需要保留；轮换时把旧密钥所在的环境变量名登记到 `llm.vault_retired_keys`（以旧 `vault_key_id` 为键），新密钥放入 `vault_key_env` 并递增 `llm.vault_key_id`，旧 BYOK 密文仍可解密。网关密钥是随机不透明令牌，数据库只存其 SHA-256，无需签名密钥；撤销立即生效。`/capabilities` 与 `/admin/settings` 反映生效模式；`llm.enabled=false` 时保留 M0 模拟模式。

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

沙箱内通过 `http://127.0.0.1:8090/v1/messages` 请求模型，由 guest 内存凭据代理经 Unix socket 转发。exec 工具支持后台运行、poll/stdin/kill、超时和截断输出；PTY 明确不支持。沙箱默认无外网，完整输出留在执行节点。仅 `/workspace` 为后续任务保留。

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

访问 [http://127.0.0.1:3000](http://127.0.0.1:3000)，填写测试邮箱和至少 8 位密码注册即可。Chat 中发送普通文字可测试流式响应，发送“执行”或“提问”可测试工具审批与用户回答。当前回复由本地协议 fixture 生成，尚未接入真实模型；项目、会话、审批和沙箱执行链路实际运行。

前端只让用户选模型：输入框的“模型”选择器中，Chat 按提供方列出模型，Work 在 Codex / Claude Code 子菜单里选模型；上游格式（Anthropic Messages 或 OpenAI Responses）由服务端按模型所属提供方推断。对话开始后只能切换同一提供方（Work 为同一执行器）的模型，选择其他模型会用它打开新对话。在侧栏“项目”中管理项目指令和文本知识文件，在模型菜单中切换推理强度，Work 中切换计划模式，在消息上分叉，并通过对话菜单执行压缩、重命名、归档和导出。设置与项目内容在下一轮生效，设置变更不写入时间线，发送消息时会携带尚未确认的设置。Claude 的运行中补充会排入下一轮；界面分别显示“等待注入点”和“已转交执行器”，不声称模型已采纳。

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

## 运行 Flutter 客户端

本次使用 Flutter 3.47.5 / Dart 3.13.4、flutter_rust_bridge 2.11.1。Rust 生成的绑定已提交。原生 UI 的 SQLite 缓存和 Web 的 IndexedDB 缓存都按服务器、用户、组织隔离；访问 token 不写磁盘。

Web 构建准备：

```sh
cargo install flutter_rust_bridge_codegen --version 2.11.1 --locked
cargo install cargo-expand wasm-pack
rustup toolchain install nightly-2026-09-25 --profile minimal --component rust-src --target wasm32-unknown-unknown
./scripts/build-web.sh
python3 scripts/serve-web.py
```

打开 `http://127.0.0.1:3000`。界面参照 ChatGPT 网页版：侧栏包含新建对话、已归档、项目和最近对话，底部账户菜单进入设置；顶部切换 Chat/Work；“新建对话”只是草稿，发送第一条消息时才在服务端创建对话并以其作为标题。设置页提供语言、外观（浅色/深色/强调色）、新对话默认值、快捷键、账户与退出登录、用量、已归档对话、模型凭据和服务器策略。界面支持中文和英文：首次进入按浏览器或设备语言选择，识别不到时使用英文，选择只保存在本机（Web 为 IndexedDB `ryzhub-prefs`）。置顶、删除和临时对话（右上角按钮）只预留了界面，服务端在 `/capabilities` 声明 `conversation_pin` / `conversation_delete` / `conversation_temporary` 后才会启用；临时对话创建时发送 `temporary: true`，列表中带该标记的对话不显示。输入框右侧显示所选模型及其推理强度（0xProto 字体），点击后在同一菜单中切换。

静态服务器必须发送 COOP `same-origin` 和 COEP `require-corp`，FRB 的共享 Wasm 内存需要 `crossOriginIsolated`。脚本提供这些头，并将 CanvasKit 与字体保存在本站。Rust 内核产物在 `client/web/pkg`，可重建、不提交二进制。

- Windows：在装有 Visual Studio C++ 工具链的 Windows 上运行 `scripts/build-windows.ps1`，它编译并复制 Rust DLL 到 Flutter 输出目录。
- Android：安装 SDK/NDK、`cargo-ndk` 和三个 Rust Android target 后运行 `scripts/build-android.sh`。Debug manifest 允许本地 HTTP；release 使用 HTTPS。
- 原生开发时可用 `--dart-define=RYZHUB_RUST_LIBRARY=<Rust 动态库绝对路径>` 显式指定库。Flutter 外壳通过 Dart 注入传输和平台缓存；独立 `driver-native` crate 提供 tokio/tungstenite + SQLite 驱动。两者使用相同 Rust 协议与状态机。

M0 模式输入 `/approve 任意内容` 会打开模拟审批，可用两个浏览器窗口登录同一账号竞争决议。普通消息流式输出；侧栏顶部的连接状态在异常（黄色）时可点击重连，网络变化和应用恢复前台也会触发按游标补齐。Ctrl+Enter 发送（可在设置中改为 Enter 发送）；IME 组字期间不会误发送。浏览器测试脚本以 `zh-CN` locale 运行。

## 验证

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
./scripts/test-integration.sh
./scripts/test-m1.sh
./scripts/test-m2.sh
./scripts/test-m3.sh
TEST_RUNTIME=runsc ./scripts/test-m3.sh
cd client
flutter analyze
flutter test
```

集成测试标记为 ignored，必须执行脚本才会运行。它们在独立临时数据库里验证，成功后删除测试数据库；测试不会删除开发数据。测试需要有 CREATE DATABASE 权限的本地管理员，以及 `scripts/init-db.sql` 中的角色。CI 显式运行集成测试。

承载一致性保证的 SQL（租约接管/续期/fencing、seq 分配、发布、命令幂等、成员关系、run 守卫与网关密钥查找）使用 SQLx 编译期检查，`.sqlx` 提供离线元数据；其余参数化 SQL 用真实数据库集成测试验证。修改 schema 或这些查询后，对一个迁移后的数据库运行 `DATABASE_URL=... ./scripts/prepare-sql.sh` 并提交 `.sqlx` 的变化。

[M0 验收](docs/m0-acceptance.md) · [M1 与完整回归记录](docs/m1-acceptance.md) · [M2 验收](docs/m2-acceptance.md) · [M3 验收](docs/m3-acceptance.md) · [线协议](docs/wire-protocol-v1.md)

## 代码导航

| 位置 | 职责 |
|---|---|
| `proto/`、`crates/proto` | Protobuf schema、构建与编解码 |
| `crates/domain` | 共享 RBAC/资源归属检查、三层策略收紧、三条生命周期 |
| `crates/server` | REST/WS（`http`、`ws`）、RLS 存储（`store`）、Owner actor 与发布（`owner`）、run 生命周期（`runs`）、Chat loop（`chat`）、原生 harness 驱动（`work`）、LLM/MCP Gateway（`llm`、`mcp`）、执行节点访问（`sandbox`）、检查点/恢复（`workspace`）、撤销推送（`revocation`）、保留与维护 |
| `crates/execution` | containerd host、overlay 校验、guest journal（快照 + 追加日志）、exec、统一 harness 协议与 Codex/Claude 适配、upper tar.zst 流式归档与本地凭据代理 |
| `crates/core` | sans-IO seq/Delta/Fold 状态机、订阅集合、离线命令、重连退避 |
| `crates/driver-native` | tokio WebSocket + SQLite 驱动，支持网络变化输入 |
| `crates/bridge`、`client/lib/rust` | flutter_rust_bridge 与批量 UI diff |
| `client` | Windows/Android/Web UI、输入法、Markdown/代码复制、平台 IO |
| `crates/server/tests/consistency.rs`、`m1.rs`、`m2.rs` | PostgreSQL/NATS/containerd 与真实进程故障验收 |

M0 尚不协商 WebSocket 压缩、SSE 降级或多流内容；暂不提供 OAuth 登录和组织管理 UI。客户端 Markdown 在 Flutter 层渲染，Rust Markdown AST/高亮流水线留待后续优化。Windows 输入法、Android 切网与硬件帧率不能由 Linux 的自动测试替代，见验收记录。
