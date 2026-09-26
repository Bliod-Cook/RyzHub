# M3 review 修复

2026-09-26。修复本轮 review 中已复现的五项问题，无数据库迁移。

- 项目移动获取频道锁后重新校验对话状态。并发删除先提交时，移动被拒绝，对话保留项目关联并可随项目恢复。
- Codex 分叉只接受已完成轮次末尾的原生回复；同一轮较早的 assistant 消息不可分叉。命令目录返回 `fork_points`，客户端完成运行后刷新并仅显示这些入口，后端再次校验。
- Codex 每次 `turn/start` 显式发送 `plan` 或 `default`，关闭计划模式不再保留上一轮设置。
- Claude SDK 通过 `applyFlagSettings({effortLevel})` 更新推理强度，不再以 thinking token 上限代替 effort。
- 对话重命名及 Chat 归档与用户频道事件在同一事务提交；事件 revision 单调递增，在线与离线客户端均可应用连续修改。REST 元数据修改也与项目删除使用同一频道锁。

## 回归

新增 `crates/server/tests/m3/regressions.rs`，由 `scripts/test-m3.sh` 一并运行：

- 用数据库锁屏障重现删除和移出项目的竞态，断言移动被拒绝、项目恢复后对话恢复可用。
- 捕获实际 Claude/Codex 发往本地 fixture 的模型请求，验证 medium → high → low，以及 Plan → Default。
- 在同一 Codex 轮次产生工具前回复、工具结果和最终回复：拒绝早期回复的分叉，允许最终回复，并验证后续轮次未进入分支上下文。
- 两份 Rust 客户端投影模拟在线客户端与离线客户端，验证连续重命名和归档事件的即时回放、断线续传以及 revision 合并。

Flutter 增加分叉入口组件回归，验证同一轮只显示服务端确认的分叉位置。

最终验证通过：

- M3：runc、gVisor 各 6 项集成测试（原有 2 项 + 新增 4 项）。
- Rust workspace：28 项单元测试，fmt 与严格 Clippy 检查通过。
- Flutter：8 项组件测试，analyze 与 release Web 构建通过。
- Claude adapter：TypeScript 编译通过，更新镜像已重建并导入执行节点。

模型上游仅使用本地协议 fixture；实际运行固定版本的 Codex 与 Claude SDK。日志保存在 `var/m3-review-fixes/`。

## 镜像

更新后的 Claude adapter 需要运行 `scripts/prepare-m3.sh` 重建镜像，再重启使用新配置的服务。新镜像 digest 记录在 `var/m3/image.txt`。现有 Work 工作区继续固定原镜像；新建 Claude Work 对话才会使用更新后的 adapter，恢复或从旧工作区分叉不会自动切换镜像。

本轮验证镜像：`docker.io/library/ryzhub-m3-base@sha256:50b08e965bca2e45a6490e98c06a213f12adacfe2dfd530b75fcfddab5707ae2`。

后续在 RyzHub 根目录迁移时重新检查了以上实现，独立运行结果见[迁移与复核记录](root-migration.md)。
