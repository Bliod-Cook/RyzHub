# M0 / M1 / M2 review 修复

2026-09-26。修复本轮 review 的 8 个问题；M1 的共享审批与密钥链路一起回归。真实模型上游继续使用本地协议 fixture。

| 优先级 | 问题 | 修复后的行为 |
|---|---|---|
| P1 | 检查点重放会 ACK 并丢弃下一轮事件 | 按 sandbox + native turn ID 找到所属 run；未登记的轮次保留待重放。恢复线程时的历史用量通知不阻塞后续事件。 |
| P1 | 恢复失败后下一条消息会创建空工作区 | 持久化 recovery_required，发送命令和创建 Work 沙箱均检查恢复状态。修复失败原因并恢复成功后才能继续。 |
| P1 | 恢复作业重试会停止自己刚建好的目标 | 固定 checkpoint、源 sandbox、目标 job ID 与 generation；失败作业重试复用原 ID。restore_applied 保证最终事务重试不重复失效时间线或追加通知。 |
| P1 | 同一 run 只有第一项并行审批能完成 | 仍有 pending action 时保持 requires_action；全部完成后才回到 running。 |
| P1 | 检查点复制持有 Host 全局锁，阻塞心跳 | 按 sandbox 串行化生命周期和 guest 调用；全局锁只保护状态。活动检查点中的暂停任务保持续租；遗留暂停任务可失效并被确认停止。 |
| P2 | 长时间 Work 的虚拟密钥一小时后失效 | 运行及等待用户期间每分钟检查，剩余不足 5 分钟时轮换；旧密钥不能发起新请求，已接收的流继续完成。 |
| P2 | Work 提问显示成审批按钮 | 使用统一的 __answer__ 类型标记，走 AnswerQuestion 并生成原生答案。 |
| P2 | 新设备只能看到最近 100 条消息 | Flutter 增加“加载更早消息”，REST 分页经 Rust core 合并并持久化；不推进实时 cursor、不覆盖较新 revision 或流式文本。 |

新增或扩展的回归包括：两项并行审批依次决议；缺失 blob 后禁止新 run；补回 blob 后复用恢复作业；Host Restore 后 Owner 被 fencing 再接管；重复应用恢复不追加事件；未登记 turn 延迟 ACK；跨轮次投影；历史用量通知；原生提问及答案；临近过期密钥刷新、复用与旧密钥拒绝；轮换不中断已接收流；阻塞的 guest 不占用全局状态或其他沙箱锁；历史合并的 revision、cursor、撤权和缓存约束。

浏览器分页测试使用独立测试数据库预置 225 条消息，验证首次 100 条、两次历史分页、刷新保留缓存及继续实时收发。脚本为 `scripts/browser-history.py`；要求 `RYZHUB_HISTORY_TEST_DB=ryzhub_test_...`，禁止对非测试数据库执行预置。

升级时先执行 `migrations/0008_restore_progress.sql`（通过现有 migrate 命令），再更新 Server、Host 和 Flutter/Web 客户端。迁移会为已有失效工作区补上恢复标记。

验证结果与限制见 [验收记录](m2-acceptance.md) 和 [机器可读汇总](m2-verification.json)。浏览器报告：[M0](browser-regression.json)、[历史分页](browser-history.json)、[M1](browser-m1.json)、[M2](browser-m2.json)。完整本地日志保存在 `var/m2-review/review-fixes/`。

最终验证通过：Rust 单元/组件 29 项；一致性与修复回归 9 项；M1 8 项；M2 7 项及 gVisor 往返 1 项；Flutter analyze 与组件 5 项；浏览器 35 个场景。另在真实 containerd/runc 上确认遗留暂停任务不续租，Stop 会先恢复调度再确认终止。未执行收费模型、真实云 S3/NAS、大容量工作区、物理多节点或 Windows/Android 真机验收。

## M3 阶段独立复核

后续对这 8 项修复重新检查实现并运行了 M0/M1/M2 回归。另补齐一致性脚本的测试签名 key、执行节点稳定二进制目录和 M1/M2 旧运行配置的升级回填。详细结果与 M3 新增场景见 [M3 验收](m3-acceptance.md)；本轮模型上游仍为本地协议 fixture。
