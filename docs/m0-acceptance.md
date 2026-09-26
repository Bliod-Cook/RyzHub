# M0 实现与验收记录

日期：2026-09-25。范围为[架构方案](../agent-platform-architecture-2026-09-25.md) §13 的 M0，按用户确认执行“先完成 M0，并验证一致性场景”。

M0 平台骨架、模拟 Agent、共享 Rust 内核与 Flutter 最小客户端已实现；本地自动验收通过。§11.2 的 Web PoC 已运行，Windows/Android 原生构建及真机 PoC 尚未验证，因此本记录不代表三端实机验收全部完成。

## 已交付路径

| 范围 | 实现 |
|---|---|
| 协议与内核 | Protobuf 二进制 WebSocket、连续 seq 游标、UTF-8 字节 Delta、ItemFold、订阅 diff、持久命令队列、断线重试与批量 UI diff |
| 服务端 | Axum Gateway、PostgreSQL 18、NATS、Owner 租约与 fencing、事务 outbox、模拟流式 Agent、审批 CAS、取消和已读 |
| 身份与隔离 | 邮箱密码登录、个人组织、集中 authorize、三层策略收紧、租户复合外键与 FORCE RLS、一次性浏览器 ticket、持久且轮换的 resume token |
| 运维 | 独立运行/迁移/维护角色、实例配置页、events 周分区、audit 月分区、保留期维护、Docker Compose、本地启动脚本和 CI 工作流 |
| 客户端 | 中文 Chat/Work 页面、Markdown 与代码复制、审批、IME 组字保护、连接恢复、SQLite/IndexedDB 缓存、自托管字体、Windows/Android/Web 工程及构建脚本 |

运行数据库账号拒绝超级用户和 BYPASSRLS；Owner 的全局待办发现使用独立只读账号。身份发现仅通过限定用途的 SECURITY DEFINER 函数查询当前用户的组织和对话。关键租约与 seq SQL 由 SQLx 编译期检查及 `.sqlx` 离线元数据覆盖，其余参数化 SQL 由实际数据库集成测试验证。

启动步骤见 [README](../README.md)，消息格式、恢复顺序和背压规则见[线协议](wire-protocol-v1.md)。

## 一致性场景

以下 D1–D6 对应[服务端集成测试](../crates/server/tests/consistency.rs)，C 对应[内核测试](../crates/core/src/tests.rs)。集成测试使用真实 PostgreSQL/NATS 和独立临时数据库；Gateway、Owner 重启用实际子进程终止与重新启动验证。

| 编号 | 测试 |
|---|---|
| D1 | `database_idempotency_fencing_rls_and_transaction_outbox` |
| D2 | `two_devices_approval_cas_and_stale_execution_epoch` |
| D3 | `websocket_reconnect_owner_process_restart_snapshot_and_isolation` |
| D4 | `gateway_process_restart_resumes_subscription_and_revocation_clears_it` |
| D5 | `member_removal_revokes_session_and_stops_waiting_run` |
| D6 | `delayed_nats_frames_from_retired_publisher_are_discarded` |

| 架构要求/补充场景 | 证据与结果 |
|---|---|
| ACK 丢失后重发 | D1、D3、C：忽略已收到的 ACK 并重发原 cid；返回原结果，命令只执行一次。序列化恢复后仍保留原 cid；相同 cid 携带不同 payload 被拒绝。通过。 |
| Delta 重复、部分重叠、缺口 | C：校验重叠前缀，只追加后缀；UTF-8 字节偏移正确；缺口触发补齐。通过。 |
| Delta 先于 Item、ItemFold 乱序 | C：base_seq 尚未应用时暂存；旧 Fold 不截断新内容；未来流缓冲有上限。通过。 |
| 提交后、发布前崩溃 | D1：先提交数据库且不调用发布，再使旧租约过期，模拟该故障边界；新 Owner 按 seq 发布遗留事件，清空待发布集合。通过。此用例没有在提交指令处注入进程 kill。 |
| Gateway 重启 | D4：实际重启 Gateway，通过数据库保存的 resume token 恢复订阅；撤销后清除投影。通过。 |
| Owner 重启与接管 | D3：流式输出期间终止 Owner，新进程接管后继续输出到完整终态。通过。 |
| 旧 Owner/执行代次被拒 | D1、D2：旧租约不能续约、追加事件或发布；过时的审批执行代次被拒。D6：接管后 NATS 中迟到的旧发布者帧被 Gateway 丢弃。通过。 |
| 两台设备同时审批 | D2：两个独立连接竞争决议，只允许一个 CAS 成功。通过。 |
| 流式输出期间切网 | D3：主动断开连接，恢复已保存状态并以新连接补齐，最终文本一致。Web 冒烟也验证重新连接。连接恢复机制通过；真实 Wi-Fi/蜂窝网络切换待真机验证。 |
| 快照与实时流衔接 | C、D3：快照后连续应用实时事件；旧快照不能覆盖新状态；事件清理后按权威 items 恢复快照。通过。 |
| 跨组织隔离 | D1、D3 及 domain 测试：RLS 隔离读取、复合外键拒绝跨租户关系、私有对话订阅被拒；组织管理员不自动获得私有内容。通过。 |
| 成员移除与订阅撤销 | D5、D4、C：撤销会话、终止等待中的模拟运行，清除私有投影和待发命令。通过。virtual key 的实际签发/调用属于 M1。 |
| 连接背压 | Gateway 单元测试：队列满时报告 ChannelLagged，保留可恢复的客户端数据，不误报权限撤销。通过。 |

## 自动验证结果

本地环境：Linux，Rust 1.98.1，Flutter 3.47.5 / Dart 3.13.4，flutter_rust_bridge 2.11.1，PostgreSQL 18 与 NATS。Web 使用 2026-09-25 的 Rust nightly 工具链。

| 检查 | 结果 |
|---|---|
| `cargo fmt --all --check` | 通过 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过，无警告 |
| `cargo test --workspace --locked` | 15 项通过：core 9、domain 3、proto 1、native SQLite 缓存 1、Gateway 背压 1 |
| `./scripts/test-integration.sh` | D1–D6 共 6 项通过 |
| `flutter analyze` | 通过，无问题 |
| `flutter test` | 3 项通过：中文组字期间 Ctrl+Enter 不发送、未闭合流式代码块与复制、一万条消息惰性构建 |
| `buf lint`（STANDARD） | 通过 |
| Rust WASM + Flutter Web release 构建 | 通过 |
| Chromium Web 冒烟 | 注册、Rust/WASM Protobuf 流、IndexedDB 投影保存、审批、重连、无外部字体/资源请求，全部通过 |
| Shell/Python 脚本语法 | 通过 |

共 **24 项自动测试通过**，另有上述静态检查、构建和浏览器冒烟。集成测试标记为 ignored，普通 `cargo test` 会跳过它们，必须显式运行集成测试脚本。CI 已配置 Rust、数据库集成、Flutter 和 Web 构建任务；这里记录的是本地结果，尚无远程 CI 执行记录。

复现基础检查见 [README](../README.md#验证)。启动服务和 Web 页面后，可在具备 Chromium 系统依赖的环境运行：

```sh
uv run --with playwright playwright install chromium
uv run --with playwright python scripts/poc-web-smoke.py
```

脚本会创建一个本地测试账号，并更新截图与指标文件；报告不包含登录凭据。

## Web PoC 数据

原始数据：[poc-web-metrics.json](poc-web-metrics.json)。实际页面：[poc-web.png](poc-web.png)。测量环境为 Linux headless Chromium 153.0.8010.12、1280×900、localhost、冷浏览器上下文、未压缩的 Python 静态服务器。

| 项目 | 测量值 |
|---|---|
| 首次绘制 / FCP | 2,689.7 ms |
| Rust WASM | 588,456 字节，约 575 KiB |
| Dart JavaScript | 3,008,151 字节，约 2.87 MiB |
| 中文字体 | 2,336,468 字节，约 2.23 MiB |
| Chromium CanvasKit WASM | 5,428,806 字节，约 5.18 MiB |
| Emoji fallback shard | 205,180 字节，约 200 KiB |
| Web 导出目录 | 约 43.6 MiB，包含多个渲染器的备选资源，不等于首屏下载量 |
| `crossOriginIsolated` | `true` |
| 测试路径的外部字体/资源请求 | 0 |

这是一次本地样本，不是公网性能指标或 SLA。FCP 没有单独区分中文首次显示时间；首次中文延迟、实际设备帧率及弱网分位数仍需专项测量。

中文字体子集包含 GB2312、ASCII、中日韩标点和全角符号，共 7,726 个请求码点。测试路径用到的 Emoji shard 同站托管；不覆盖完整 Unicode。来源与许可证见[字体说明](../client/assets/fonts/README.md)。

## Windows / Android 实机待验收

以下项目本次均未在实际设备验证。Widget 测试与 Linux 浏览器测试只覆盖其中的程序逻辑。

- [ ] Windows：运行 `scripts/build-windows.ps1`，确认 Rust DLL 加载、注册、流式回复、审批和重连。
- [ ] Windows：逐一验证微软拼音、搜狗、注音的组字、候选框定位、换行、切换输入框与 Ctrl+Enter；组字期间不得误发。
- [ ] Windows：验证睡眠/唤醒、网络变化后恢复；检查字体、剪贴板和长列表中的文字选择。
- [ ] Android：按 README 安装 SDK/NDK 与 Rust target，运行 `scripts/build-android.sh` 并安装到设备，确认原生桥及 SQLite 可用。
- [ ] Android：输出期间 Wi-Fi → 蜂窝网络 → Wi-Fi；断网发送后恢复，确认命令不重复执行、文本不缺失。
- [ ] Android：锁屏/后台期间完成模拟任务，回到前台后补齐终态；检查输入法、选择和复制。
- [ ] 两端：在长列表与流式 Markdown/代码块下记录帧率，验证拖选、双击选词及流式更新期间选择的稳定性。
- [ ] 两端：验证中英混排、代码中的中文与超出当前字体子集的字符，记录缺字与 fallback 情况。

## 实现边界

Chat/Work 当前均为明确标注的模拟 Agent。真实模型、LLM Gateway、OCI/guest、harness adapter、搜索、记忆和插件属于 M1–M5，本次未实现。

Flutter 通过 Dart 注入传输及平台存储，共享 Rust 协议、投影与命令状态机；独立 Rust 原生驱动提供 tokio/tungstenite + SQLite。Markdown 当前在 Flutter 层渲染，Rust 增量 AST/高亮流水线留待后续优化。

线协议只开放文本 `part_index=0` 和 SendMessage、DecideApproval、Cancel、MarkRead 四个命令；OAuth、压缩、JSON 子协议、SSE 降级及 RFC 8441 尚未实现。未完成 Windows/Android 原生二进制和真机验证。

## 后续完整回归

同日新增的 13 项浏览器回归和 M1 实现/测试见 [M1 与完整回归记录](m1-acceptance.md)。上表保留 M0 首次验收时的数量；当前工作区测试数量以新记录为准。
