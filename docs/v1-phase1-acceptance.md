# v1 剩余工作 · 阶段 1：文档对齐与小修

2026-10-11。对照架构方案复核实现后的第一阶段，内容见 v1 剩余工作计划的阶段 1。

## 变更

| 项 | 内容 |
|---|---|
| 文档对齐 | 架构方案按 2026-10-11 的决定修订：§11.3 改为 IO 由 Flutter 注入；§12.3 删除 resume_token，改为按游标重订阅；§12.2 压缩改为 zstd 子协议；§5.6 与 §12.4 写明 harness 命令按文本透传、`InvokeMenuCommand` 只用于插件命令；§14 记录 Chat 不审批、组织功能暂缓。`docs/wire-protocol-v1.md` 与 README 同步 |
| 删除 driver-native | 未被任何代码使用的 `crates/driver-native` 已删除；workspace 的 sqlx 去掉 `sqlite` feature |
| resume_token | proto 中 `Hello.resume_token`、`Welcome.resume_token`、`Welcome.resumed` 标为 deprecated；内核不再缓存或回传 token，旧缓存里的字段读取时被忽略 |
| 能力声明 | guest 侧 Codex `fork` 改为 native（实际已用 `thread/fork`）；Claude 的 `subagent` 在实现子 agent 树之前改为 unsupported |
| Chat 沙箱挂载泄漏 | Chat run 结束后原先只 Stop，overlay 与镜像挂载一直留在执行节点上。现改为 Destroy：卸载挂载、删除沙箱目录，`/workspace` 目录保留；数据库状态记为 `deleted`。之前遗留为 `stopped` 的 Chat 沙箱会在其对话下次运行清理时一并销毁 |
| usage_records 按月分区 | 迁移 `0018_usage_partitions.sql` 把用量表改为按 `created_at` 月分区（主键改为 `(id, created_at)`，带 DEFAULT 分区），叶子分区不授权。`maintain` 预建分区、迁回 DEFAULT 中的行，并按 `retention.usage_days` 删除整月分区。原先 Owner 按 ctid 分批删除的规则已移除：分区表上 ctid 不唯一，会误删其他分区的行 |

## 验证

| 检查 | 结果 |
|---|---|
| `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -D warnings` | 通过 |
| `cargo test --workspace --locked` | 通过 |
| `./scripts/test-integration.sh` | 16/16，新增 `usage_partitions_move_stranded_rows_and_retention_drops_whole_months` |
| `./scripts/test-m1.sh` | 14/14；`guest_proxy_live_host_restart_and_terminal_cleanup_recovery` 现断言沙箱为 `deleted`，且节点上目录与挂载都已移除。guest 日志不含密钥的检查改在 exec 进行中执行 |
| `./scripts/test-m3.sh`（runc） | 8/8 |
| `flutter analyze`、`flutter test` | 无问题，109 项通过 |
| 迁移 0018 在开发库副本上执行 | 322 行全部进入 2026-09、2026-10 分区；`ryzhub_app` 在 RLS 下读、改、插入正常，直接读叶子分区被拒绝；副本已删除 |

## 部署

- 迁移 0018 会重命名并重建 `usage_records`，持有排他锁直到复制完成。执行前停止 `owner` 与 `llm-gateway` 角色，避免进行中的模型请求写入失败；数据量小时只需数秒。
- 执行节点：`scripts/start-execution-node.sh` 已在 m1 测试中热替换 host 与 guest。
- Web 客户端已用 `scripts/build-web.sh` 重建到 `var/m4/web`。

## 本地部署记录

2026-10-11，dev server（M4 栈，数据库 `ryzhub_m3`）：

- 迁移前确认没有进行中的 run，并用 `pg_dump -Fc` 完整备份到 `var/backups/0018-migration-20261011-141300/`（目录 0700，文件 0600）。
- 编译通过后停止旧后端，经 `scripts/dev-m4.sh` 启动新版并应用迁移 0018；`/healthz` 正常，日志无 WARN 或 ERROR。
- 322 条用量记录全部保留：2026-09 分区 185 条，2026-10 分区 137 条；已预建 2026-11、2026-12 分区和 DEFAULT 分区，叶子分区都不授权。
