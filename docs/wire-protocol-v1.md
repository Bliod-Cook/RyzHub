# 平台线协议 v1.1

唯一 schema 为 `proto/agentplatform/v1/wire.proto`；`prost + protox` 构建不依赖系统 protoc。WebSocket 子协议为 `ryzhub.v1.protobuf`，每个 binary message 对应一个 Frame。M0 支持 `text/part_index=0`，未宣告多流、压缩或真实 harness 能力。

## 连接与身份

原生连接使用 Authorization Bearer；浏览器先 POST `/auth/ticket`，再连接 `/ws?ticket=...`。ticket 哈希落库、30 秒有效，兑换使用 DELETE RETURNING，只能用一次。跨域 Origin 必须在配置白名单内。M0 HTTP/WS 由本地服务提供，部署时由反向代理终止 HTTPS/WSS。

10 秒内发送 Hello，必须包含 major=1 和稳定 device_id。Welcome 返回连接 ID、协议版本、22 秒心跳和 1 MiB 帧限制。服务端不保存订阅集合：`resume_token` 恒为空、`resumed` 恒为 false（字段保留以兼容旧客户端）。重连后客户端按本地连续游标重新订阅；只有客户端知道自己真正应用到了哪里。

服务端 Ping 需回 Pong；超过 55 秒未收到任何客户端帧会关闭连接。AuthRefresh 仅允许同一用户的新有效会话，并触发订阅重新鉴权。注销和会话到期撤销该连接的全部订阅；移除成员撤销其在该组织的订阅，会话本身对其他组织仍然有效。撤销由数据库触发器通知、各进程单一监听连接即时推送，另有每 15 秒一次的兜底复核。

## 频道与订阅

- `conv:<uuid>`：私有对话，只有创建者可订阅；组织管理员不自动获得内容权限。
- `user:<org_uuid>:<user_uuid>`：组织内该用户的会话摘要和已读。
- `org:<org_uuid>`：仅当前成员可订阅；M0 保留组织变更扩展点。

Subscribe 的 `since_seq` 是 optional：缺失表示首次快照，0 是有效的重放起点。Gateway 先建立并 flush NATS 订阅，再在 REPEATABLE READ 事务内读取投影和 head_seq；缓冲中的旧事件会去重。已过期的游标或超前游标转快照。快照提供最近 100 个 Item，历史由 REST `items?before_seq=` 分页，每页 100 个，响应为 `{items, has_more}`，`has_more` 表示是否还有更早的 Item。Flutter 在时间线顶端进入视野时自动加载下一页，`has_more` 为 false 后停止；Rust core 按 Item revision 合并历史，不覆盖同 revision 的流式文本，不推进 live cursor，并将结果持久化到同一缓存。

订阅数据顺序是：快照或事件重放 → 当前流 ItemFold → SubscriptionSynced → 实时帧。Fold 请求失败时后续 Delta 缺口会触发再次订阅，最终内容仍由持久 ItemUpserted 修复。每个 Gateway 进程对每个频道只建立一个 NATS 订阅，由该进程内的订阅者共享；它每 5 秒对照数据库 head 校对尾部，NATS 丢失的持久事件从数据库补齐。

## 持久事件与流

EventEnvelope.seq 在频道内从 1 连续增长。客户端只推进最高连续已应用游标，乱序事件暂存并请求补齐。未知可选事件内容仍推进 seq，保持协议小版本兼容。

Item 以 ID upsert，revision 严格增加。Delta 与 ItemFold 不占 seq、不入 events 表；base_seq 指向该 item 当前持久快照，revision 必须匹配。

偏移按 UTF-8 **字节**计算：

1. base_seq 大于已应用游标时暂存；最多 256 个或 1 MiB 文本，超过上限重订阅。
2. offset 等于已有字节长度时追加。
3. 重复/部分重叠时验证共同前缀，只追加未接收的后缀。
4. offset 超前、切在字符中间、内容冲突时请求重订阅和 Fold。
5. 较旧 revision/base_seq、会缩短现有内容的 Fold、已终结 Item 上的流帧全部忽略。
6. 最终 ItemUpserted 提供完整权威内容，替换临时流投影。

Owner 在频道行锁内完成事件发布、Delta 发布及 NATS flush，随后推进 published_seq。所有持久变更首先以同一事务提交。崩溃可能导致重复发布；Gateway 与客户端都按 seq 去重。锁与 writer_epoch/owner_id/lease_until 联合校验阻止旧发布者与新 Owner 交错。NATS 帧额外带 Frame.writer_epoch，Gateway 对照数据库校验；旧连接缓冲中迟到的帧也会被丢弃，遗漏的持久事件从数据库补齐。

## 命令

M0 实现 SendMessage、DecideApproval、Cancel、MarkRead。M1 增加 Steer、SideQuery、AnswerQuestion；`GET /capabilities` 返回 Chat 模式及模型，Work 仍为 mock。

M1 活跃主线期间新输入使用 Steer，持久 Item 状态先为 `accepted`，在下一次模型请求前变为 `injected`；任务结束仍未注入的补充标为 `cancelled`。SideQuery 创建 `read_only` run，不带工具，不写回主线。AnswerQuestion 携带 action_id、run_epoch 和 answer，与审批使用相同的待决状态/代次 CAS。客户端问题卡片通过 `PendingAction.options=["__answer__"]` 识别；该标记是 v1 的暂行约定。

RunState 支持 `running/requires_action/completed/cancelled/failed`，`read_only` 区分旁问与主线（缺省 false，兼容旧缓存）。前端运行状态以 RunState 为准；工具卡片与助手消息可先完成，不能据此推断整个 run 已结束。

cid 必须是 UUID；同一用户内全局唯一。重复请求必须有相同的目标对话和 protobuf payload，返回第一次保存的完整结果；复用 cid 携带不同内容返回 CID_REUSED。两个设备重发同一个 cid 不改变原始执行设备归因。

命令先持久排队并唤醒持有该对话的 Owner，Owner 排序执行。命令结果、投影、事件和 seq 在同一事务提交后才返回 Ack；结果经 NATS 推回等待的 Gateway，不依赖轮询。Ack 仅代表平台接收/应用了命令，实际完成通过事件体现。Owner 不可达时返回可重试拒绝，保留原 cid。客户端每 5 秒重试未确认队列，重连也使用相同 cid。

审批要求 action_id、run_epoch、选项、未决状态与过期时间全部匹配，CAS 只允许一个决议。接管会更新待审批项的执行代次并再次发布请求；旧设备的旧代次决议被拒。取消、到期和成员撤销会使待审批失效。

## v1.1 新增字段

`Welcome.minor_version` 为 1。新增字段均为可选，旧客户端忽略即可：

- `Item.truncated`：显示文本超过 256 KiB 时在字符边界截断并置 true。
- `Item.detail`：工具与文件变更的结构化内容。`ToolDetail` 含工具名、参数 JSON、结果 JSON（完成后出现）和命令输出；`FileChangeDetail` 列出每个文件的路径、变更类型（add/update/delete）和 diff。每个字段各自限制在 64 KiB。`Item.text` 保留 v1.0 的渲染，供不认识 detail 的客户端使用。
- `PendingAction.kind`：`approval` 或 `question`。v1.0 的 `options=["__answer__"]` 约定继续保留。

## 背压与存储

每连接最多 16 个订阅、32 个并行待处理命令、256 个待发送帧。队列满时发送 ChannelLagged 并结束该频道的发送；客户端用自己的持久游标重新订阅。ChannelLagged 不删除投影或离线命令，SubscriptionRevoked 才清除对应私有数据。

Rust 原生驱动使用 SQLite WAL；Flutter 外壳原生使用 SQLite，Web 使用 IndexedDB。状态按服务器 × 用户 × 组织分区，待发队列和投影游标在同一次写入保存；先保存再发出。访问令牌仅保留在当前客户端进程内存。

## 边界

M0 不协商 permessage-deflate/zstd、JSON 子协议、SSE 降级或 RFC 8441。这些传输优化不改变上述一致性规则。客户端主动断开/重连覆盖连接迁移机制；真实 Windows/Android 系统回调和 IME 表现仍需要实机 PoC。

## M3 命令与状态

新增 `UpdateRunConfig(config_json)`、`Compact`、`Fork(item_id)`，沿用 CID 幂等、Owner fencing 和提交后 ACK。模型/推理/审批/计划设置保存到对话，下一轮复制到 run；已有运行记录（或分叉带来的上下文种子）后 harness/protocol 不可变。设置变更不再写入时间线通知；`SendMessage.config_json` 可携带客户端尚未确认的设置，服务端在同一事务内先应用再创建 run，避免与单独的 UpdateRunConfig 乱序。客户端只传 `model` 时，服务端按模型路由推断 protocol，Work 未给 harness 时一并推断；切换到另一提供方或执行器的模型返回 `HARNESS_PROTOCOL_IMMUTABLE`，客户端改为新建对话。fork 是持久化后台作业，`GET /conversations/{id}/commands` 可查询结果。

补充输入区分 accepted/waiting/handed；handed 只代表转交。Claude 在当前原生轮结束后启动下一轮，仍属于同一平台 run；所有原生 turn ID 持久映射，迟到事件按原始 turn 路由。

`GET /orgs/{org}/conversations/{id}/commands` 提供三态能力（native/emulated/unsupported）、可路由模型、推理等级、审批模式和 harness 专属命令。前端隐藏 unsupported 控件，明确标识 emulated 行为。

Codex 的 `fork_points` 返回可分叉的 Item ID，仅包括已完成轮次末尾的原生 assistant 回复；轮次中间的回复或之后仍有工具操作的回复不可选择，服务端同样拒绝。客户端在 run 完成后刷新该列表。其他 harness 的 `fork_points` 为 null，沿用其消息边界。

重命名和 Chat 归档在同一事务内更新对话并向用户频道写入 `ItemUpserted(kind=conversation)`；其 revision 使用所分配的用户频道 seq，保证连续修改可被客户端合并。标题和状态既支持在线推送，也支持按游标续传。
