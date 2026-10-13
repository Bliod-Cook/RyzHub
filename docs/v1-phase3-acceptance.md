# v1 剩余工作 · 阶段 3：执行底座

2026-10-11。实现 v1 剩余工作计划的阶段 3。各项的配置与用法见 README 的沙箱部分。

## 变更

| 项 | 内容 |
|---|---|
| 3.1 节点路由 | 新增 `execution_nodes` 表与 `sandboxes.node` 列（迁移 0020）。每个沙箱记录所在节点，关于它的请求发往该节点。删除工作区等整体操作只发往它用过的节点，宕机的备用节点不会阻塞删除。只配置一个节点时不查库。`SandboxBackend` 增加 `connect()`，guest 通道统一经它获取 |
| 3.2 出站白名单 | 每个沙箱有自己的 `control/egress.sock`，guest 把 `127.0.0.1:8091` 转发过去，沙箱进程设有 `http(s)_proxy`。<br>代理只放行 CONNECT 与 `http://` 请求，且要求白名单主机、80/443 端口、解析结果全为公网地址，并用解析出的 IP 直连。默认白名单为常用软件包源与 GitHub，组织策略 `egress_hosts` 可以收窄。<br>每个连接都记入 `audit_logs`（`egress.connect`，`detail` 含拒绝原因与收发字节，迁移 0021），每个组织每分钟的连接数有上限 |
| 3.3 磁盘配额 | 节点级 XFS 卷，以 `prjquota` 挂载（host 用 loop 镜像自建，默认预分配）。沙箱可写层与 Chat 工作区各占一个 project quota，上限为 `sandbox.disk_bytes` ∩ 策略 `sandbox_disk_bytes`，写满时返回 ENOSPC。检查点暂存放在卷上以便 reflink，不计入配额 |
| 3.4 Chat 工作区 | Chat 沙箱在 run 结束后空闲保留 `chat_idle_secs` 供下一次 run 复用（迁移 0022 `idle_until`）。用过沙箱的 run 结束后，`/workspace` 检查点到存储（manifest `scope: "workspace"`）。新沙箱落到没有该工作区的节点时，先恢复到暂存目录，再 rename 到位 |
| 3.5 热盘回收 | Owner 每分钟回收一次：<br>• 挂起、归档或丢失的 Work 沙箱，过了宽限期后销毁；<br>• 已完整检查点且长期闲置的 Chat 工作区移出节点（`EvictWorkspace`，不留墓碑），下次使用时恢复；移出与新建沙箱靠对话租约行锁互斥。<br>host 为已销毁的沙箱保留 30 天墓碑，防止迟到的 Create 把它重新建出来 |
| 3.6 检查点时效 | 运行中的 Work 工作区超过 20 分钟没有已提交检查点时，写 WARN 与审计（每小时至多一次），状态 API 返回 `stale` |
| 3.7 环境与镜像 | 新增 `sandbox.deps` 与 `[[sandbox.profiles]]`，`RunConfig.env_profile` 选用 profile。deps 层作为第二个只读 lower（`deps:base`），检查点 manifest 记录 deps，恢复时核对。`scripts/build-deps.sh` 构建 deps 层并导入节点。<br>镜像回收：只回收本控制面用过、现已无人引用、且超过宽限期的 digest 镜像（迁移 0024 `node_images`） |
| 3.8 终端 | Work 工作区的交互式 shell。链路是 客户端 ↔ Gateway ↔ host ↔ guest，每个会话一条专用连接，不经过 Owner 与事件流。<br>guest 用 256 KiB 环形缓冲保存输出，重连时按字节偏移续传；脱离 10 分钟后结束会话。执行代次变化时连接被切断。<br>连接期间刷新 `terminal_seen_at`（迁移 0023），沙箱不会被空闲挂起。客户端用 xterm.dart 面板 |
| 3.9 文件工具 | Chat 新增 `fs_upload`（把附件复制进 `/workspace`）与 `fs_download`（把沙箱文件放进用户文件库和对话） |
| 3.10 多节点 | 新沙箱放在 active、未失联、未满容量的节点上：优先对话上次所在节点，否则选负载最低的。远程节点走 TCP + 双向证书 TLS（`RYZHUB_TLS_*` 与节点配置中的 `address`/`tls_*`）。`ryzhub-host` 也会读取 `<root>/host.env` |

## 与计划不同之处

- **终端传输**：计划是在主 WebSocket 上加 protobuf 帧，实际改为每个会话一个独立 WebSocket。终端字节不进入事件管道和客户端 Rust 内核，背压由 TCP 承担，不挤占事件流的发送队列。
- **环境 profile**：只放在实例配置中，没有建 `env_profiles` 表。组织级 profile 属于暂缓的组织功能；镜像回收需要的引用由配置、沙箱与检查点算出。
- **预热池（3.11）**：未实现。在 dev 节点上实测冷启动沙箱约 0.5 秒（三次分别为 0.50、0.58、0.49 秒），而 Chat 沙箱现已空闲复用，新建本就减少。实现预热池需要：
  - 向运行中的容器注入挂载（open_tree/move_mount，只有 runc 可用，gVisor 不行）；
  - 处理认领后未确认的孤儿沙箱；
  - 改造沙箱与容器的身份映射。

  风险大于约 0.5 秒的收益，留待有实际需求时再评估。

## 实现中发现并修复的问题

- **busybox wget 不认 `no_proxy`**：访问本地服务的请求被送进出站代理。guest 端现在会在本地直接处理目标是回环地址的请求。另外，busybox wget 走代理时会把 POST 发成 GET，测试里访问本地凭据代理的那条 wget 改用 `-Y off`。
- **出站日志被多个控制面共用**：最初的实现会因为别的控制面的记录而写入失败，而且某个读取方确认后其他读取方就丢失记录。改为环形缓冲、各控制面各自维护游标，只保存本库认识的沙箱。
- **guest 被 OOM 杀掉**：写大文件时页缓存压满内存，OOM killer 选中了 guest，整个沙箱随之死亡。现在 guest 的 `oom_score_adj` 设为 -998，它启动的命令和 harness 恢复为 0。
- **Work 检查点目录**：启用配额卷后，Work 检查点仍在旧位置创建 `artifacts` 目录，现已改到数据卷上。
- **节点进程耗尽**：测试中途失败时不会清理沙箱，执行节点上累积了 55 个运行中的沙箱，把容器的 991 个进程配额用光。已重启节点，并销毁 172 个 dev 库不认识的残留沙箱。测试 Harness 现在关闭时销毁本库的沙箱、移走工作区；启动时清扫一小时前遗留的测试库。
- **回收被 FENCED 拒绝**（部署 dev 时发现）：节点上沙箱的 epoch 可能比它的行新，原因有两个：之后的 Owner 用自己的 writer epoch fence 过它；0012 之前的旧运行时留下的 epoch 计数不同。回收原先用"行里的 epoch + 1"去 Destroy，dev 库里 24 个 lost 的 Work 沙箱因此每分钟报一次 FENCED。
  - 进入终止状态的沙箱行不会再被启用（恢复、续跑都会新建沙箱），宽限期过后也不存在需要防的旧写者，所以回收现在用最大 epoch 销毁；迟到的 Create 仍会被节点的墓碑拒绝。
  - 回收同时覆盖 `stopped` 状态。早期版本把 Chat 沙箱停在这个状态，overlay 仍挂着；原先只有用户再次打开该对话时才会清理。
  - `test-m1.sh` 的 Chat 复用测试增加了"节点上 epoch 更新的 lost 沙箱仍被回收"一项，确认改动前失败、改动后通过；回收测试增加了 `stopped` 的 Chat 沙箱。

## 验证

| 检查 | 结果 |
|---|---|
| fmt、clippy `-D warnings`、`cargo test --workspace --locked` | 通过，91 项单元测试，含出站规则与地址判定、guest 回环转发、拒绝与日志、配额 ABI、PTY 会话与续传、帧编解码、工作区检查点往返、销毁墓碑、mTLS 信任 |
| `test-integration.sh` | 17/17 |
| `test-m1.sh` | 22/22。新增节点路由与放置、出站白名单与审计（runc 与 gVisor 均通过，含直连失败）、配额 ENOSPC 与 OOM 下 guest 存活（runc 与 gVisor）、Chat 空闲复用与换节点恢复、工作区移出与 Work 沙箱回收及检查点时效、文件工具、profile + deps 层与镜像回收 |
| `test-m2.sh`（含 gVisor 往返） | 8/8 + 1/1。新增 Work 终端：命令、resize、脱离后续传、代次切断、关闭（runc 与 gVisor 均通过） |
| `test-m3.sh`（runc 与 gVisor） | 8/8、8/8 |
| `test-m4.sh` | 1/1 |
| `flutter analyze`、`flutter test` | 无问题，110 项（新增终端面板重连测试） |

## 部署

- **迁移**：0020–0024 只新增表、列与函数，不改写已有数据。
- **执行节点**：执行节点要随服务端一起更新，用 `scripts/start-execution-node.sh` 热替换。新增的 HostCommand 需要新版 host。
- **默认行为变化**：
  - 沙箱默认可以访问常用软件包源，不需要时设 `egress_allow = []`；
  - Chat 沙箱在 run 结束后空闲保留 10 分钟。
- **配额卷**：只在节点设置 `RYZHUB_QUOTA_VOLUME_SIZE` 后才启用，启用后不要关闭。dev 节点已经启用了 12 GiB 的预分配卷。

### dev 部署记录（2026-10-11）

1. 确认 `ryzhub_m3` 没有进行中的 run，备份到 `var/backups/0024-migration-20261011-172649/ryzhub_m3.dump`。
2. 构建后端，用 `scripts/dev-m4.sh` 重启，迁移到 24。`/healthz` 正常；`execution_nodes` 中有 `default` 一个节点（active，容量不限）。
3. 首次重启后，回收被 FENCED 拒绝（见上文）。修复后重新部署，dev 库中 37 个过期的 lost、stopped 沙箱全部回收：
   - 沙箱行现在全部为 `deleted`；
   - 执行节点上不再有 dev 库的沙箱；
   - 日志没有 WARN。
4. 执行节点和 Web 客户端此前已经更新到本阶段的版本。
