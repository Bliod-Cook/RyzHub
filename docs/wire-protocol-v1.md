# 平台线协议 v1

唯一 schema 为 `proto/agentplatform/v1/wire.proto`；`prost + protox` 构建不依赖系统 protoc。协议只有主版本 1（`PROTOCOL_MAJOR`），没有小版本：新增的字段与命令都是可选的，旧客户端忽略即可；删除的字段在 proto 中 `reserved`。WebSocket 子协议为 `ryzhub.v1.protobuf`，每个 binary message 对应一个 Frame；另有压缩子协议 `ryzhub.v1.protobuf+zstd` 和 SSE 降级，见“传输”。

## 连接与身份

原生连接使用 Authorization Bearer；浏览器先 POST `/auth/ticket`，再连接 `/ws?ticket=...`。ticket 哈希落库、30 秒有效，兑换使用 DELETE RETURNING，只能用一次。跨域 Origin 必须在配置白名单内。服务本身只提供 HTTP/WS，部署时由反向代理终止 HTTPS/WSS。

10 秒内发送 Hello，必须包含 `protocol_major` = 1 和稳定的 `device_id`，否则收到 `INCOMPATIBLE_PROTOCOL`。Welcome 只带连接 ID。服务端不保存订阅集合：`resume_token` 恒为空、`resumed` 恒为 false，三个字段在 proto 中标为 deprecated，客户端不再发送。重连后客户端按本地连续游标重新订阅；只有客户端知道自己真正应用到了哪里。

服务端每 22 秒发 Ping，需回 Pong；超过 55 秒未收到任何客户端帧会关闭连接。客户端发来的单帧上限 1 MiB。会话到期前 60 秒发 AuthExpiring；AuthRefresh 仅允许同一用户的新有效会话，并触发订阅重新鉴权。会话被注销或撤销时，连接关闭前对全部订阅发 SubscriptionRevoked；会话只是到期时不撤销，客户端续期后带着原有频道和待发命令重连。移除成员撤销其在该组织的订阅，会话本身对其他组织仍然有效。撤销由数据库触发器通知、各进程单一监听连接即时推送，另有每 15 秒一次的兜底复核。

## 频道与订阅

- `conv:<uuid>`：私有对话，只有创建者可订阅；组织管理员不自动获得内容权限。
- `user:<org_uuid>:<user_uuid>`：组织内该用户的会话摘要、已读位置和草稿；该用户的 CreateRun 与 UndoMemoryUpdate 也发在这里。
- `org:<org_uuid>`：仅当前成员可订阅；预留给组织级变更，目前不发布事件。

Subscribe 的 `since_seq` 是 optional：缺失表示首次快照，0 是有效的重放起点。Gateway 先建立并 flush NATS 订阅，再在 REPEATABLE READ 事务内读取投影和 head_seq；缓冲中的旧事件会去重。已过期的游标或超前游标转快照。快照提供最近 100 个 Item，历史由 REST `items?before_seq=` 分页，每页 100 个，响应为 `{items, has_more}`，`has_more` 表示是否还有更早的 Item。Flutter 在时间线顶端进入视野时自动加载下一页，`has_more` 为 false 后停止；Rust core 按 Item revision 合并历史，不覆盖同 revision 的流式文本，不推进 live cursor，并将结果持久化到同一缓存。

订阅数据顺序是：快照或事件重放 → 当前流 ItemFold → SubscriptionSynced → 实时帧。Fold 请求失败时后续 Delta 缺口会触发再次订阅，最终内容仍由持久 ItemUpserted 修复。每个 Gateway 进程对每个频道只建立一个 NATS 订阅，由该进程内的订阅者共享；它每 5 秒对照数据库 head 校对尾部，NATS 丢失的持久事件从数据库补齐。

**未读**：主线 run 结束时，对话的 `ConversationDetail.activity_seq` 记为对话频道当时的 seq，并重新发布到用户频道。`activity_seq` 大于用户频道里该对话的已读位置（`ReadState.read_seq`）即为未读。客户端打开对话且停在最新处时发送 MarkRead。

**草稿**：`PUT /orgs/{org}/conversations/{conv}/draft {text, device_id}` 保存对话的未发送消息，后写入者胜；被接受的写入以 `DraftChanged(Draft)` 发到用户频道，用户频道快照的 `drafts` 带着所有未清空的草稿。空草稿即清除。其他设备只在输入框没有焦点时采用别处的草稿。

## 持久事件与流

EventEnvelope.seq 在频道内从 1 连续增长。客户端只推进最高连续已应用游标，乱序事件暂存并请求补齐。不认识的事件类型同样推进 seq。

Item 以 ID upsert，revision 严格增加。

- `text`：显示文本，总是存在，不认识 `detail` 的客户端照此渲染；超过 256 KiB 时在字符边界截断并置 `truncated`。
- `detail`：结构化内容，每个字段各自限制在 64 KiB。`ToolDetail` 含工具名、参数 JSON、结果 JSON（完成后出现）和命令输出，插件工具另有 `plugin` 与 `app`（见“插件”）；`FileChangeDetail` 列出每个文件的路径、变更类型（add/update/delete）和 diff；`AnswerDetail` 是回复引用的来源和它发布的沙箱文件；`AttachmentsDetail` 是消息附带的文件；`ConversationDetail` 是用户频道里的对话（置顶时间、`activity_seq`）。
- `parent_item_id`：所属的父条目，目前是子 agent 的调用：Claude Code 的 Agent/Task 工具启动的子 agent 所做的事都挂在这次调用下面。客户端把它们缩进显示在父条目里；父条目不在时照常显示。

Delta 与 ItemFold 不占 seq、不入 events 表；base_seq 指向该 item 当前持久快照，revision 必须匹配。`stream` 为 `text` 或 `terminal_output`，两者偏移各自计算。命令运行期间，它的输出以 `terminal_output` 流挂在命令（工具）条目上，可能含 ANSI 转义，客户端按终端样式显示；条目完成后完整输出在 `ToolDetail.output` 里，流上的内容随之作废。Chat 的 `exec`、Codex 的命令都会这样推送。

偏移按 UTF-8 **字节**计算：

1. base_seq 大于已应用游标时暂存；最多 256 个或 1 MiB 文本，超过上限重订阅。
2. offset 等于已有字节长度时追加。
3. 重复/部分重叠时验证共同前缀，只追加未接收的后缀。
4. offset 超前、切在字符中间、内容冲突时请求重订阅和 Fold。
5. 较旧 revision/base_seq、会缩短现有内容的 Fold、已终结 Item 上的流帧全部忽略。
6. 最终 ItemUpserted 提供完整权威内容，替换临时流投影。

Owner 在频道行锁内完成事件发布、Delta 发布及 NATS flush，随后推进 published_seq。所有持久变更首先以同一事务提交。崩溃可能导致重复发布；Gateway 与客户端都按 seq 去重。锁与 writer_epoch/owner_id/lease_until 联合校验阻止旧发布者与新 Owner 交错。NATS 帧额外带 Frame.writer_epoch，Gateway 对照数据库校验；旧连接缓冲中迟到的帧也会被丢弃，遗漏的持久事件从数据库补齐。

## 命令

对话频道上的命令有 SendMessage、DecideApproval、Cancel、MarkRead、Steer、SideQuery、AnswerQuestion、InvokeMenuCommand（见“插件”），以及“运行设置与对话管理”中的 UpdateRunConfig、Compact、Fork。harness 专属命令（如 Claude 的 `/review`）以 `/name` 文本经 SendMessage 原样透传，由 harness 自己识别。

主线 run 活跃期间新输入使用 Steer，持久 Item 状态先为 `accepted`，在下一次模型请求前变为 `injected`；任务结束仍未注入的补充标为 `cancelled`。SideQuery 创建 `read_only` run，不带工具，不写回主线。

RunState 支持 `running/requires_action/completed/cancelled/failed`，`read_only` 区分旁问与主线。前端运行状态以 RunState 为准；工具卡片与助手消息可先完成，不能据此推断整个 run 已结束。

PendingAction 的 `kind` 为 `approval`（审批）或 `question`（提问）。审批要求 action_id、run_epoch、选项、未决状态与过期时间全部匹配，CAS 只允许一个决议。AnswerQuestion 携带 action_id、run_epoch 和 answer，使用相同的待决状态/代次 CAS。接管会更新待决项的执行代次并再次发布请求；旧设备的旧代次决议被拒。取消、到期和成员撤销会使待决项失效。

用户频道上的命令由 Gateway 自己受理：

- **CreateRun**：创建对话并排队它的第一条消息，是一条命令，所以可以离线排队。`conversation_id` 由客户端生成（UUIDv7）；`mode`、`run_config_json`、`project_id`、`temporary` 是对话的设置，`title` 为空时取自消息的第一行。
  - Gateway 在一个事务里创建对话、频道和 `commands` 行，提交后唤醒该对话的 Owner；Owner 按 SendMessage 执行它，结果帧照常推回。
  - 同一 cid 重发返回第一次的结果（还没有结果时继续等待）。这个 ID 已被别的对话（任何组织）占用时返回不可重试的 `CONVERSATION_ID_TAKEN`。
  - 对话与排队的首条消息原子创建，run 不是：Owner 万一拒绝首条消息，对话仍在、为空。
  - 客户端内核在 CreateRun 得到结果之前暂扣发往 `conv:<id>` 的 Subscribe 和命令（服务端此时还不认识这个频道），得到结果后一并发出。
- **UndoMemoryUpdate(event_id)**：撤销一条记忆变更，由 Gateway 直接执行并保存结果；同一 cid 只撤销一次。已撤销或记忆已再次变化时返回不可重试的 `MEMORY_UNDO_UNAVAILABLE`。

cid 必须是 UUID；同一用户内全局唯一。重复请求必须有相同的目标对话和 protobuf payload，返回第一次保存的完整结果；复用 cid 携带不同内容返回 CID_REUSED。两个设备重发同一个 cid 不改变原始执行设备归因。

命令先持久排队并唤醒持有该对话的 Owner，Owner 排序执行。命令结果、投影、事件和 seq 在同一事务提交后才返回 Ack；结果经 NATS 推回等待的 Gateway，不依赖轮询。Ack 仅代表平台接收/应用了命令，实际完成通过事件体现。Owner 不可达时返回可重试拒绝，保留原 cid。客户端每 5 秒重试未确认队列，重连也使用相同 cid。

## 插件

`GET /capabilities` 的 `plugins` 宣告插件可用，`plugin_content_origin` 是 MCP Apps 页面所在的源。

- **InvokeMenuCommand(installation_id, command, params_json, attachment_ids)**：运行插件命令，发在对话频道上。
  - 服务端按命令声明的参数校验 `params_json`（JSON 对象），把参数填进 prompt 模板。
  - 结果以 `/<插件>:<命令>` 开头，作为成员的消息发出，之后与 SendMessage 相同。
  - 插件在本对话未启用、命令不存在时返回 `PLUGIN_NOT_FOUND`，参数不符时返回 `INVALID_ARGUMENTS`。
  - 可用的命令在 `GET …/commands` 的 `plugin_commands` 里：`[{installation, plugin, name, title, description, params}]`。
- **PendingAction.form_json**：插件服务的提问（MCP elicitation），`kind` 为 `question`。
  - `{"mode":"form","message","requestedSchema"}`：按 JSON schema 生成表单，回答是字段组成的 JSON 对象的文本。
  - `{"mode":"url","message","url"}`：成员打开页面，回答 `accept` 或 `decline`。
  - 两种都可以回答 `decline`。回答仍用 AnswerQuestion。
- **ToolDetail.plugin / ToolDetail.app**：插件工具的插件名，以及它的 MCP App 页面（`ui://…`）。
  - 有 `app` 时，客户端可以用 `POST /orgs/{org}/conversations/{conv}/apps {item_id}` 取得页面地址和这次调用的输入、结果，在沙箱化的 frame 里显示。
  - 页面里的工具调用走 `POST …/tool-calls {item_id, tool, arguments}`，规则与模型调用相同。必须审批的工具不能从界面调用。

## capabilities 与错误码

`GET /capabilities` 宣告服务端启用的功能：`chat`/`model`（Chat 的协议与默认模型）、`work`/`work_model`/`claude_model`/`harnesses`、`search`、`memory`、`conversation_temporary`、`conversation_delete`、`plugins`、`plugin_content_origin`，以及：

- `registration`：为 `false` 时实例不开放注册，`POST /auth/register` 与外部账号的首次登录都返回 `REGISTRATION_CLOSED`，已有成员照常登录。
- `auth_providers`：`[{id, label, kind}]`（`kind` 为 `oidc` 或 `github`），可用的外部登录。登录走 `/auth/oauth/{id}/start?return_to=`，回来时地址带 `login_code`（用 `POST /auth/oauth/exchange` 换会话）、`login_linked` 或 `login_error`。
- `push`：服务端会向 Android 设备发通知；客户端登录后 `POST /devices` 登记 FCM 令牌，WebSocket 的 Hello 里的 `device_id` 与登记时的 `device` 相同，连接期间这台设备不收通知。
- `attachments`：`max_bytes`、`max_per_message`；比 `part_bytes` 大的文件经 `/orgs/{org}/uploads` 分片上传。

REST 与命令拒绝的错误码及其 HTTP 状态见 `crates/server/src/error.rs`。

## 背压与存储

每连接最多 16 个订阅、32 个并行待处理命令、256 个待发送帧。队列满时发送 ChannelLagged 并结束该频道的发送；客户端用自己的持久游标重新订阅。ChannelLagged 不删除投影或离线命令，SubscriptionRevoked 才清除对应私有数据。

WebSocket 与本地缓存由 Flutter 外壳实现并注入 Rust 内核（原生使用 SQLite，Web 使用 IndexedDB），协议、游标与命令队列逻辑全部在内核。状态按服务器 × 用户 × 组织分区，待发队列和投影游标在同一次写入保存；先保存再发出。访问令牌仅保留在当前客户端进程内存。

## 传输

一个连接无论走哪种传输，Hello/Welcome、订阅、命令和背压规则都相同。

- **压缩**：服务端 WebSocket 栈（axum 0.8 / tungstenite 0.29）不支持 permessage-deflate，压缩以子协议 `ryzhub.v1.protobuf+zstd` 提供，客户端在 `ryzhub.v1.protobuf` 之前提供它。
  - 只压缩服务端到客户端方向：不小于 256 字节的帧作为一个完整的 zstd frame 发送（压缩后不更小时照原样发），更小的帧照原样发。
  - protobuf Frame 不会以 zstd 的魔数开头（那是 Frame 没有的 5 号字段的键），所以客户端按开头 4 字节区分，不需要标记。客户端内核用纯 Rust 的 ruzstd 解压，解压上限 4 MiB。
  - 不在消息之间共享压缩上下文：ruzstd 在一个 zstd frame 结束前保留最后一个窗口的数据不交出，整条连接一个流就无法逐条解码。真正占流量的大帧（快照、历史、长回复）各自压缩；流式 Delta 帧很小，压缩本来无益。
- **SSE 降级**：WebSocket 连续 3 次握手失败时，客户端改用事件流，事件流失败后再试 WebSocket。
  - `GET /stream?ticket=&device_id=` 打开连接（鉴权同 `/ws`）。第一个事件 `connection` 给出连接 ID（随机，也是 Welcome 的 `connection_id`），之后每个事件的 data 是一个 base64 编码的 protobuf Frame。
  - 客户端帧逐个 `POST /stream/{connection}`（protobuf 正文，带访问令牌，只能投递到自己的连接），按顺序串行发送。
  - 连接只存在于打开它的进程：多个 Gateway 时需要会话粘滞。
- **RFC 8441**：服务端开启了 HTTP/2 extended CONNECT，`/ws` 可以经 HTTP/2 承载 WebSocket（已用 hyper 的 h2 客户端测试）。只在 TLS 由本服务终止（或反向代理以 HTTP/2 转发 extended CONNECT）时有意义。

这些传输优化不改变上述一致性规则。

## 运行设置与对话管理

`UpdateRunConfig(config_json)`、`Compact`、`Fork(item_id)` 沿用 CID 幂等、Owner fencing 和提交后 ACK。模型/推理/审批/计划设置保存到对话，下一轮复制到 run；已有运行记录（或分叉带来的上下文种子）后 harness/protocol 不可变。设置变更不写入时间线；`SendMessage.config_json` 可携带客户端尚未确认的设置，服务端在同一事务内先应用再创建 run，避免与单独的 UpdateRunConfig 乱序。客户端只传 `model` 时，服务端按模型路由推断 protocol，Work 未给 harness 时一并推断；切换到另一提供方或执行器的模型返回 `HARNESS_PROTOCOL_IMMUTABLE`，客户端改为新建对话。fork 是持久化后台作业，结果见 `GET …/commands` 的 `operations`。

补充输入区分 accepted/waiting/handed；handed 只代表转交。Claude 在当前原生轮结束后启动下一轮，仍属于同一平台 run；所有原生 turn ID 持久映射，迟到事件按原始 turn 路由。

`GET /orgs/{org}/conversations/{id}/commands` 提供三态能力（native/emulated/unsupported：`steer`、`fork`、`compact`、`plan_mode`、`thought_level`、`terminal`）、可路由模型、推理等级、审批模式、harness 专属命令和插件命令。前端隐藏 unsupported 控件，明确标识 emulated 行为。

`GET /orgs/{org}/models` 的 `work` 分组带 `label`（显示名）。除 `codex`、`claude` 外，实例可以配置经 ACP 接入的 agent（如 `opencode`），harness 名由实例决定，客户端不应写死列表；它们的补充指令在下一轮生效（`steer: emulated`），不能分叉。

Codex 的 `fork_points` 返回可分叉的 Item ID，仅包括已完成轮次末尾的原生 assistant 回复；轮次中间的回复或之后仍有工具操作的回复不可选择，服务端同样拒绝。客户端在 run 完成后刷新该列表。其他 harness 的 `fork_points` 为 null，沿用其消息边界。

重命名和 Chat 归档在同一事务内更新对话并向用户频道写入 `ItemUpserted(kind=conversation)`；其 revision 使用所分配的用户频道 seq，保证连续修改可被客户端合并。标题和状态既支持在线推送，也支持按游标续传。

## 边界

客户端以主动断开、重连处理连接迁移；Windows/Android 的系统回调与输入法表现需要在真机上验证，见[真机验证清单](device-checklist.md)。
