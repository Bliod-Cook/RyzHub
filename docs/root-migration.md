# RyzHub 根目录迁移与 review 复核

2026-09-26。项目正式名称为 **RyzHub**，原 `agent-platform` 内的 Rust workspace、Flutter 客户端、配置、脚本、文档、构建缓存和本地数据已移到仓库根目录。根目录 README 合并了原入口的架构链接与完整启动说明；CI 工作目录、缓存和 Web 产物路径、文档引用及本地开发配置同步更新。Android 应用名称和 Windows 窗口/产品名称统一为 RyzHub，包标识和可执行文件名保持兼容。

## 复核另一对话的修复

逐项检查了 [M3 review 修复记录](m3-review-fixes.md) 的实现及回归：

| 修复 | 独立验证内容 |
|---|---|
| 删除项目与移出对话竞态 | 频道锁屏障使删除先提交；移动被拒绝，恢复项目后对话仍可恢复。 |
| Codex 分叉位置 | 工具前回复被拒绝，最终回复可分叉；分支不包含后续轮次。前端只使用服务端确认的 `fork_points`。 |
| 退出 Codex 计划模式 | 每轮显式发送 `plan/default`，捕获下一次模型请求确认已回到 Default。 |
| Claude 推理强度 | 使用 SDK `applyFlagSettings`；捕获上游请求确认 medium → high → low。 |
| 重命名/归档同步 | 元数据与用户频道事件原子提交，revision 单调递增；在线与离线投影一致。 |

## 迁移完整性

- 迁移前源码备份：`/tmp/ryzhub-before-root-move-20260926.tar.gz`。目录在同一文件系统中直接移动，保留原文件内容与 inode；没有保留旧目录软链接。
- 原有 review 的业务代码和数据库迁移保持原样。本轮只调整项目入口、路径与说明；构建时重新生成客户端绑定和本地缓存。
- 保留 PostgreSQL 命名卷及 `var/` 中的密钥、沙箱、镜像、工作区和检查点。开发执行容器已重建，持久状态和运行二进制从新路径挂载；PostgreSQL 初始化脚本挂载也已更新。
- Docker Compose 项目名及现有服务标识保持兼容，继续使用原数据库卷。产品名称、Rust crates 与 Flutter 应用均为 RyzHub。
- 运行命令以仓库根目录为工作目录，详见 [README](../README.md)。API 预览为 `127.0.0.1:8280`，Web 为 `127.0.0.1:3000`。

## 验证记录

最终全部通过：Rust 单元/组件 31 项、一致性与升级 10 项、M3 runc/gVisor 各 6 项，共 53 项后端测试；Flutter 8 项组件测试；Chromium 15 个 M3 场景。fmt、严格 Clippy、Flutter analyze、根目录 release Web/Wasm 构建均通过。浏览器无页面错误和外部请求，桌面与手机截图已复核，测试工作区已归档。本地 Web/API 预览保持运行。

结果和日志摘要见[机器可读报告](root-migration-verification.json)。Windows/Android 的名称修改经过静态检查，本轮未执行对应原生构建或真机验收。

日志位于 `var/root-migration/`。模型上游仍为本地协议 fixture；实际运行固定版本的 Codex、Claude SDK、runc/gVisor 和 Chromium。本次复核不替代商业上游、物理多节点或真机验收。
