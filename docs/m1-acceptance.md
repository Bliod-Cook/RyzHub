# M1 Chat 路径与完整回归记录

日期：2026-09-25。依据[架构方案](../agent-platform-architecture-2026-09-25.md) §13，先完整回归 M0，再推进 M1。按用户选择，本轮模型上游使用本地 Anthropic Messages 协议 fixture；PostgreSQL、NATS、浏览器、containerd/runc 和沙箱命令均实际运行。没有调用真实模型服务。

## 已实现

| 路径 | 实现与边界 |
|---|---|
| LLM Gateway | Anthropic 原生 JSON/SSE 直通，版本与 beta 头透传，替换上游认证，保留 thinking 签名与 redacted blocks；不做供应商格式转换 |
| 身份与计量 | loop JWT 校验当前 exec_epoch/Owner 租约；sandbox virtual key 校验 lease_epoch，Owner 接管不使其失效；实时成员/策略检查、按组织和调用身份限流、usage 归因；中断记录标为估算 |
| BYOK | AES-256-GCM 密钥库，组织/个人凭据创建、列举、撤销及审计；新任务可继承对话指定的凭据，优先级为指定 → 个人 → 组织 → 平台；指定凭据失效时拒绝调用，不自动换用其他凭据 |
| Chat loop | 原生执行检查点 `loop_steps`、工具 `planned/started/completed`、持久输入队列、审批/提问、steer 边界注入、单轮无工具旁问；恢复不依赖 UI 文本 |
| 沙箱 | 独立 containerd 节点、digest 镜像、runc、主机 overlay 参数回读、内存/进程/CPU 限制、独占 workspace；终态后确认停止，清理可在 Owner 重启后继续 |
| guest daemon | 幂等 op_id、fsync/rename journal、单写者进程锁、exec/poll/write/kill、超时、进程组终止、有界输出与完整输出文件；gseq 持久重放，数据库提交后才 ACK |
| 本地凭据代理 | `127.0.0.1:8090/v1/messages` → 只读挂载的 gateway Unix socket；virtual key 仅在 daemon 内存，通过控制通道轮换，不放入 exec 环境、配置或 journal |
| Flutter | 显示实际 Chat 模型；运行中输入发送为 steer；只读旁问切换；持久提问回答；未采用的补充在取消时显示正确状态；Work 仍为模拟 Agent |

代码入口：[LLM](../crates/server/src/llm/gateway.rs)、[Chat](../crates/server/src/chat/mod.rs)、[执行节点](../crates/execution/src/host.rs)、[guest](../crates/execution/src/guest.rs)、[配置](../crates/server/src/config.rs)。新增数据库迁移 0004、0005。

## 本地验证

环境：Linux，Rust 1.98.1，Flutter 3.47.5 / Dart 3.13.4，Chromium 153.0.8010.12，PostgreSQL 18，NATS，Docker 内独立 containerd。执行节点没有挂载主机 Docker socket。

| 检查 | 结果 |
|---|---|
| Rust fmt / Clippy `-D warnings` | 通过 |
| `cargo test --workspace --locked` | 23 项通过；12 项数据库集成另行显式运行 |
| `./scripts/test-integration.sh` | M0 一致性 6 项通过 |
| `./scripts/test-m1.sh` | M1 协议/恢复 6 项通过 |
| Flutter analyze / widget tests | 无问题，5 项通过 |
| Protobuf STANDARD lint | 通过，buf 1.47.2 |
| Rust/WASM + Flutter Web release | 通过，静态资源自托管 |
| M0 浏览器回归 | 13 项通过 |
| M1 浏览器验收 | 9 项通过，无 JavaScript 页面错误 |
| Shell / Python 语法 | 通过 |

以上为本地执行结果；CI 已加入 M1 独立执行节点测试任务，未声称远程 CI 已运行。Rust/WASM 构建仍有上游 nightly `atomics` 和 workspace manifest 字段未继承的提示；当前 Web 使用 Dart JS + Rust WASM，不是 Dart WasmGC 构建。

M1 六个集成测试见 [m1.rs](../crates/server/tests/m1.rs)：

1. **网关协议与 BYOK**：请求体、beta 头、ping/原生块保留，上游认证替换，累计 usage 取最终值；密钥密文落库；旧执行代次与被篡改 JWT 拒绝。
2. **检查点与交互**：提问回答持久化、工具卡片完成、thinking/signature 原样用于后续请求、steer 被消费；只读旁问没有工具且不写回后续主线；取消使未注入补充失效。
3. **工具完成前杀死 Owner**：真实执行 `printf once >> marker; sleep 1; …`，dispatch 后终止 Owner，等 guest 完成，再重启 host agent 和 Owner。工具结果恢复成功，数据库只接收一个 gseq，文件副作用只有一个 `once`。
4. **virtual key 生命周期**：Owner exec_epoch 变化不影响有效 sandbox lease；轮换使旧 key 失效；lease/过期/策略/限流拒绝；SSE 中断记录估算 usage。
5. **实际 guest 代理与在线 host 重启**：沙箱内 wget 调本地代理，Owner 保持运行时重启 host，最终获得 SSE 与带 virtual key 的 usage；journal 无凭据。模拟终态发布后 stop 回执/数据库更新丢失，新 Owner 仅凭残留 sandbox 状态发现并完成清理。
6. **指定凭据与租户边界**：个人和组织凭据同时存在时，显式选择组织凭据；撤销后返回不可用；其他组织不能设置此对话凭据或读取 usage。

[执行测试](../crates/execution/tests/guest.rs)覆盖重复 exec、变更参数复用 op_id 拒绝、旧 epoch 拒绝、重启后 gseq 重放、超时、有界输出、stdin 去重和 key 不进入 journal。[host 单元测试](../crates/execution/src/host.rs)覆盖错误 overlay 参数拒绝，以及多租户配置下 runc 拒绝；真实 runc 创建路径也回读挂载参数。

## 浏览器证据

实际使用 browser 工具进行页面操作、无障碍树检查和截图；Playwright 运行完整回归。脚本通过可访问控件输入/点击，并检查 Rust/WASM 写入 IndexedDB 的投影及后台 usage。

- [M0 报告](browser-regression.json)：注册、中文流与 Markdown、代码复制、拒绝审批、第二设备审批同步、取消、新建/切换 Work、离线队列恢复不重复、重载登录与缓存、390px 导航、错误密码、页面错误与外部资源检查。截图：[桌面](browser-desktop.png)、[窄屏](browser-mobile.png)。
- [M1 报告](browser-m1.json)：注册、Anthropic 流、真实沙箱审批与输出、提问回答、steer、旁问、仅旁问运行时启动新主线、重连、usage。截图：[桌面](browser-m1-desktop.png)、[窄屏](browser-m1-mobile.png)、[browser 工具检查](browser-m1-tool-review.png)。

复现：先构建/启动 Web，分别使用 M0 服务或 `./scripts/dev-m1.sh --fixture`，然后运行对应脚本：

```sh
uv run --with playwright playwright install --with-deps chromium
uv run --with playwright python scripts/browser-regression.py  # M0
uv run --with playwright python scripts/browser-m1.py          # M1 fixture
```

浏览器脚本会创建本地测试账号，报告不包含访问 token。测试只支持本地 fixture 的确定性响应，不能用其输出断言来测试真实模型质量。

## 尚未验证与后续范围

- 真实 Anthropic 上游、真实 token 计费和真实模型工具选择未联调，按用户要求留待配置后验证。
- Windows/Android 原生构建、输入法、物理切网、移动端后台与硬件帧率仍需实机 PoC；390px Chromium 不是 Android 验收。
- gVisor 只有配置入口与约束检查，本机未安装 runsc、未做创建或归档恢复测试。当前可验收执行路径为可信 runc 模式。
- PTY、文件上传/下载、复杂上下文压缩/附件、供应商路由/故障转移与额度管理尚未实现。BYOK 目前提供 REST API，无客户端凭据管理页。
- 沙箱无外网网卡；模型只能经本地代理的 gateway socket 访问。普通网络工具或软件包下载需要后续出站策略实现。
- Owner/host 进程重启已测试；整节点/guest 崩溃不能等同于此结论。guest 重启时未完成进程标记 `outcome_unknown`，不盲目重放副作用。跨节点调度、二级存储、RPO 归档恢复属于后续工作。
- Work harness、OpenAI Responses、MCP、插件、搜索、记忆等 M2–M5 不在本轮交付范围。
