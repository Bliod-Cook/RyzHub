# M4 Claude Code 审查与修复

2026-10-03。按用户要求调用本机 Claude Code 2.1.287，通过 `claude -p` 进行只读审查（Read/Glob/Grep，不编辑文件、不委派）。以下是实际返回的初审报告，之后附修复与验证记录。初审行号对应审查时的工作区。

# M4 搜索与记忆只读审查报告

**结论**：没有发现 P0。有 3 个 P1，都是代码上能确认的问题。另有 8 个已确认的 P2，3 个高概率但还没验证的 P2，以及 3 个需要你们拍板的问题。

审查期间工作区还在改，下面的行号以我最后读到的工作区为准。diff 快照里有几处问题在工作区里已经修了，所以不再列出：
- `context()` 生成快照时已按 `a.history` 过滤 `recent`；
- 组织策略清理时已清空 `memory_snapshot`；
- `project_file_search` 已加上冻结快照兜底，并带上 `file` 字段。

---

## P1（已确认）

### P1-1 配置 embedding 后，全文索引要等 embedding 成功才会写入；失败后新内容完全搜不到
- **位置**：
  - `crates/server/src/search/mod.rs:276-294`：`process()` 先逐批调用 `embed(...).await?`，成功后才调 `publish`；
  - `search/mod.rs:308-316`：失败后指数退避，超过 `max_attempts` 进入 `dead`，之后不会再自动重试；
  - `crates/server/src/search/retrieve.sql:4`：检索要求 `d.source_version=s.source_version`。
- **触发场景**：配置了 `embedding_url`，同时满足以下任一情况：
  - 按文档示例把 `embedding_key_env` 留空，而某个组织没有 OpenAI 凭据，或策略禁止平台凭据（`embedding.rs:46` 返回 `CREDENTIAL_UNAVAILABLE`/`PLATFORM_CREDENTIAL_DENIED`）；
  - 组织的 `policy.models` 里没有这个 embedding 模型（`embedding.rs:39-42`）；
  - 上游暂时宕机超过约 2 分钟（6 次退避）。
- **影响**：
  - 该组织之后的新消息、新标题以外的内容和新记忆永远不会生成 `search_docs`，全文模式也搜不到；`memory_search` 也一样搜不到。
  - 旧版本文档因为版本不一致被隐藏（这一点安全上没问题），但新内容不可见。
  - 这和文档写的"全文默认可用""回填期间全文检索可用"相矛盾。
  - M4 测试固定配置了 `embedding_key_env`，fixture 也总是成功，所以测试覆盖不到这个场景。
- **建议**：
  - 把全文发布和向量拆开：embedding 失败时仍然按版本发布 `search_docs`（不写 chunks），向量另作阶段或任务重试；
  - 或者在 `process` 里 embedding 失败时降级成 `vectors=vec![]` 后照常 publish，只对向量部分记错误并重试。

### P1-2 Chat 中内置记忆/搜索工具的大部分错误会让整个 run 失败
- **位置**：
  - `crates/server/src/chat/tools.rs:200-210`：只有白名单里的错误码会被记录成工具结果，其余一律 `return Err`；
  - `chat/tools.rs:260-261`：`prepare_search(...)?`；
  - `memory/api.rs:237-250`：配置了 embedding 时工具默认走 `hybrid`；
  - `memory/api.rs:252-283`：在这里调用 embedding；
  - `owner/actor.rs:375-385`：收到 Err 后把 run 标成 `failed`。
- **触发场景**（和 P1-1 是同一组条件）：模型调用 `conversation_search` 或 `memory_search` 时，embedding 返回下面任一错误：
  - `CREDENTIAL_UNAVAILABLE`、`PLATFORM_CREDENTIAL_DENIED`、`TEMPORARILY_UNAVAILABLE`（模型自己传 `mode:"vector"` 且没配 embedding 时）；
  - reqwest 上游错误，会变成 `INTERNAL_ERROR`。
  
  另外 `memory_save` 遇到 `REQUEST_ID_REUSED`、undo 冲突映射成的 `ConversationBusy`，以及 P2-H1 里的 PGroonga 语法错误，也都不在白名单里。
- **影响**：普通的一次检索就会让用户整轮对话失败。对没有 OpenAI 凭据的 BYOK 组织来说，这是每次必现的确定性失败。Work 走 MCP 路径只会返回 JSON-RPC 错误，不受影响。
- **建议**：
  - 内置记忆工具都是纯数据库操作，而且带幂等键，可以把除 `Fenced` 以外的所有错误都记录成 `{"error":code}` 返回给模型；
  - `prepare_search` 里 embedding 失败时降级成 `fulltext`。

### P1-3 删除或按保留期清理后，记忆和摘要的正文仍长期保留在数据库和时间线里
- **位置与具体情况**：
  - **单条删除，以及删除对话时勾选"同时删除相关记忆"**：`memory/mod.rs:258` 和 `memory/api.rs:149` 只改 `status`，`content` 留着；`memory_events.before_state/after_state`（`mod.rs:222-224`、`262-263`）也留着完整正文。没有任何过期清理任务。
  - **组织关闭记忆，30 天后的清理**：`mod.rs:555` 带了 `AND status<>'deleted'`，用户此前已经删除的条目会**保留正文**，绕过了这次清理。
  - **项目过期回收**：`projects.rs:650-651` 只清空了 `memory_items.content`，没有删除 `memory_events`，事件里的项目记忆正文还在。
  - **快照副本**：`conversations.memory_snapshot` 保存了 saved 记忆正文和 recent 摘要正文（`mod.rs:345-369`）。单条删除、`delete_memories`、项目回收都不会清理其他对话里的这份副本；`delete_conversation`（`api.rs:136-147`）只清空了被删对话自己的快照。渲染时会过滤掉，但数据还在。
  - **`memory_context` 时间线条目**：`mod.rs:659-690` 把记忆正文、其他对话的标题和要点写进了 `items`/`events`。删除记忆、删除被引用的对话、组织清理之后，这些内容在"本轮引用的记忆"里依然可以展开看到。
- **影响**：和文档的承诺相冲突，包括"组织关闭记忆…30 天后清理保存记忆和摘要"、"删除对话…清理文本"，以及用户对"删除记忆"的预期。属于隐私和保留期问题。
- **建议**：
  - 快照只存 ID，渲染时再读取 active 条目的正文。条目更新是新建一行，所以冻结语义不变。
  - `memory_context` 也只存 ID 和标题，或者在清理时一并擦除。
  - 设一个撤销窗口（例如 7 到 30 天），过期后把 deleted/archived 行的 `content` 和事件里的 before/after 状态清空。
  - 策略清理去掉 `status<>'deleted'` 过滤；项目回收时一并删除相关 `memory_events`。

---

## P2（已确认）

### P2-1 "记忆已删除"提示和撤销按钮会出现在错误的对话里
- **位置**：`memory/mod.rs:580-599`，按 `m.source_conversation_id` 匹配事件；`mod.rs:203-208`，replace 时新条目沿用旧条目的来源对话。
- **场景一**：在对话 B 里让 agent `memory_forget` 一条在对话 A 中创建的记忆。B 里没有任何提示；等 A 下次运行时，A 里才会冒出"记忆已删除"。这违反了 §10.4 关于写入提示和撤销的要求。
- **场景二**：在设置页编辑或删除一条带来源对话的记忆，会在很久以后出现在那个来源对话里。
- **建议**：`memory_events` 增加一个"发起对话"字段，在调用工具时写入，REST 发起的写成 NULL，`project()` 按这个字段投递。

### P2-2 摘要收集器没有退避，失败的会话会长期占满处理名额
- **位置**：`memory/mod.rs:474-478`、`505-522`。
- **场景**：
  - 某些组织 `summarize` 会必然失败，例如没有对应 provider 的凭据，或 `policy.models` 不包含 `llm.model`。
  - 这些会话永远写不出摘要，每 30 秒都会被再次选中。查询带 `LIMIT 100` 但没有排序，超过 100 个时其他组织的会话可能一直轮不到。
  - 如果上游已经返回内容但被判为空，每 30 秒还会产生一次计费调用和一条 `usage_records`。
- **建议**：记录尝试次数和下次可执行时间并做退避，按最旧优先排序。

### P2-3 `search.enabled=false` 时，删除对话和创建临时对话仍然生效，但永远不会回收
- **位置**：`http/mod.rs:45-48` 的路由是无条件注册的；`http/orgs.rs:51-57` 接受 `temporary`；`main.rs:45-54` 只在开启搜索时才启动 `memory::collector`，而删除/过期回收就在这个 collector 里。
- **影响**：已删除对话的正文、沙箱和检查点，以及过期的临时对话，都不会被清理。
- **建议**：删除和过期回收单独启动，不依赖 search 开关；或者在搜索关闭时直接拒绝这两个请求。

### P2-4 `reindex` 并不是低优先级，积压时新内容长时间不可见
- **位置**：`main.rs:171-178` 把 `available_at` 设成 `now()+1s`，`claim` 按 `available_at,created_at` 排序（`search/mod.rs:174-176`）；`search_jobs_ready` 索引（`migrations/0013:56`）不含 `org_id`。
- **场景**：大规模回填时，之后产生的新消息任务排在整个回填之后。由于检索要求版本完全一致，新消息在此期间全文也搜不到；每次 claim 还要在全局积压里过滤 `org_id`。
- **建议**：加一个 priority 列，或给回填任务一个靠后的 `available_at` 档位；索引改成 `(org_id, available_at) WHERE state IN (...)`。

### P2-5 Flutter 从搜索结果跳转后，时间线可能是空的或中间缺一段
- **位置**：`client/lib/controller.dart:629-648`；`crates/core/src/lib.rs:162-167`，收到 Snapshot 时整体替换 `state.items`。
- **场景一**：`select()` 只是发出订阅请求，WS 快照是异步到达的。如果快照晚于 REST 历史页的 `mergeHistory`，刚合并进来的旧消息会被覆盖。再叠加 `first_seq <= searchFocusSeq` 的过滤，视图会变成空的或只剩很少几条。
  - 现有的 `loadOlder` 用 `_historyVersion` 防住了这种情况（`controller.dart:184`、`203-209`），`selectSearch` 没有用。
- **场景二**：即使没有竞争，点"回到最新消息"后，命中点和快照窗口之间的消息也不会被加载，因为 `loadOlder` 只往最旧的一条之前翻页。这个缺口还会被写进本地缓存。
- **建议**：等频道初始化后再合并，并检查 `_historyVersion`；或者把聚焦模式做成一个独立的窗口视图，退出时重新加载。

### P2-6 记忆设置页在 SnackBar 里点"撤销"可能失效
- **位置**：`client/lib/ui/memories.dart:67-71`，`run()` 一进来就无条件 `setState`；`memories.dart:256-261`。
- **场景**：删除一条记忆后，在 SnackBar 还显示时切换到别的设置分区或关闭设置页，再点"撤销"。此时 `setState` 发生在 dispose 之后，会抛异常，撤销请求根本没有发出去。
- **建议**：SnackBar 回调里直接调用 `hub.request(...)`，之后 `if (mounted) load()`。

### P2-7 Work 指令按第一个 `<memory>` 切分，且大小上限和配置不一致
- **位置**：`crates/execution/src/guest.rs:742-747`；`config.rs:256`。
- **问题一**：项目指令是用户写的文本，如果其中出现字面量 `<memory>`，`project.md` 会在那里被截断，后半段写进 `memory.md`。记忆在渲染时已经转义过 `<`，所以应该用 `rfind`，或者把项目和记忆分成两个字段下发。
- **问题二**：配置允许 `memory_budget_chars` 最大 100,000。中文按 3 字节算，加上项目指令可能超过 guest 的 200,000 字节上限，触发 `INVALID_PROJECT_INSTRUCTIONS`，导致 Work mount 失败。

### P2-8 几个小问题
- **`items.created_at`**：`migrations/0013:12` 把历史消息的 `created_at` 全部填成了迁移时刻，`from/to` 过滤对旧消息不准。可以用对话创建时间或事件时间回填。
- **临时对话仍然可见**：`dialogs.dart:399-401` 的本地标题搜索，和 `controller.dart:124-125` 的归档列表，都没有过滤临时对话，和"不显示在历史列表"的说法不符。
- **`memory_context` 解析**：`messages.dart:187` 直接 `jsonDecode`，没有保护。如果正文超过 `MAX_ITEM_TEXT`（256 KiB）被截断，这张卡片在 build 时会抛异常。只在预算配得很大时可能出现。

---

## P2（高概率，但还没验证）

### P2-H1 PGroonga `&@~` 直接接收原始输入，语法错误会变成 500
- **位置**：`retrieve.sql:16`、`memory/api.rs:27`、`projects.rs:529`。
- **原因**：`&@~` 按 Groonga 查询语法解析输入。语法错误（例如未闭合的 `(`、`"`，或以 `OR` 结尾）会被 PGroonga 当作 ERROR 抛出。
- **影响**：REST 端返回 `INTERNAL_ERROR`；在 Chat 里还会通过 P1-2 的路径让整个 run 失败。
- **验证方法**：发一次 `GET /orgs/{org}/search?q=%28` 即可确认。
- **建议**：改用 `pgroonga_query_escape($3)` 或 `&@`，或者把这类错误映射成 `INVALID_ARGUMENTS`。

---

## 需要确认语义 / 加固建议（不算 bug）

1. **两个开关是否独立**：`enabled=false` 时，`conversation_search`、历史注入和摘要都会一起关闭（`mod.rs:335`、`api.rs:184`、`mod.rs:433`）。架构里叫"记忆总开关"，这样做说得通；但 UI 写的是"使用保存的记忆"，验收文档写的是"独立开关"。需要统一一下措辞或行为。
2. **MCP 幂等键的作用域**：键的形式是 `mcp:{run}:{session}:{id}`，而服务端从不下发 session，所以 session 实际总是 `stateless`。现有远程 MCP 的键按 `sandbox_id` 隔离，记忆工具的键没有。如果同一个 run 内 harness 或沙箱重建、JSON-RPC id 从头计数，就可能出现误报的 `REQUEST_ID_REUSED`。建议把 `sandbox_id` 和 `lease_epoch` 加进键里。
3. **个人密钥可能发往非 OpenAI 地址**：`embedding_key_env` 为空时，`resolve(..., "openai")` 会优先取用户个人的 OpenAI BYOK 密钥，发给任意配置的 `embedding_url`（文档示例里就有指向本地服务的写法）。建议当 `embedding_url` 不是 OpenAI 时，强制要求配置 `embedding_key_env`，或者不使用个人凭据。

---

## 已核对、未发现问题的部分
- RLS 和 `security_invoker` 视图；
- 检索时实时核对所有者、删除状态和项目边界，`project_only` 边界没有越界；
- 工具路径的 `replace`/`forget` 做了作用域校验；
- 发布时先锁源、再锁任务，任务 token 和租约 fencing 正确；
- Chat 写入在 `runs::guard` 下执行，Work 时间线由 Owner 在 `fenced` 下投影；
- undo 的版本校验；
- 临时对话被排除在索引、摘要和上下文之外；
- 删除对话时的加锁顺序和 `idle` 检查。

## 初审问题处理

| 问题 | 处理 |
|---|---|
| P1-1 embedding 故障阻塞全文 | 故障时按相同版本发布全文并清除旧向量；后台继续退避重试向量。hybrid 查询自动使用全文结果。新增 503 fixture 验收。 |
| P1-2 检索错误终止 Chat | 内置记忆/搜索工具的非 fencing 错误写成工具结果，供模型处理。 |
| P1-3 正文副本与保留期 | saved 快照、引用时间线和变更提示只保存 ID/元数据。展开引用时通过认证 API 实时解析。删除历史来源清理 recent 快照副本；删除/失效记忆保留 30 天撤销窗口，之后清除正文和事件副本；组织清理包含已删除项，项目回收清理事件。 |
| P2-1 提示投递到来源对话 | 新增事件发起对话字段；工具事件投递到当前对话，设置页修改不投递到旧来源。 |
| P2-2 摘要无退避 | 保存尝试次数和下次执行时间，指数退避并按等待时间排序。 |
| P2-3 搜索关闭不回收 | 删除、临时过期与保留期 collector 独立于 search 开关启动。 |
| P2-4 回填优先级 | 显式 priority，实时任务为 0、回填为 -10；按组织和优先级建立领取索引。 |
| P2-5 搜索跳转竞争/历史缺口 | 搜索使用独立窗口，不合并到实时 core/cache；退出恢复实时窗口，撤销权限时清空搜索窗口。 |
| P2-6 SnackBar 撤销遇到 dispose | 请求继续执行，仅在 mounted 时更新界面。 |
| P2-7 Work memory 切分/大小 | 使用最后一个 memory 标记，Guest 上限提高到 600 KB。 |
| P2-8 时间戳/临时列表/截断 JSON | 历史消息按 run 或对话时间回填；所有历史入口过滤临时对话；引用卡片不解析持久正文，变更提示解析有保护。旧消息逐条精确时间不可恢复，回填时间为近似值。 |
| P2-H1 查询语法 | 使用 pgroonga_query_escape；实际数据库验收括号、引号和 OR 输入。 |
| 语义与加固 | enabled 明确为总开关；MCP 幂等键加入 sandbox/lease 身份；自定义 embedding 主机必须显式配置其自己的环境变量密钥。 |


## Claude Code 第二轮复查

再次调用 Claude Code 进行只读复查，实际报告如下。

# M4 修复复审（只读，基于当前工作区）

**结论**：原报告里已确认的问题（P1-1、P1-2、P1-3，P2-1 到 P2-8，P2-H1）在当前代码里都已修复。这次复审发现 1 个由隐私清理修复引入的 P1 回归，和 1 个发布重试中较轻的 P2。没有 P0。

我读代码期间，`memory/mod.rs` 和 `search/mod.rs` 还在改：新增了组织解冻后重排记忆任务（`mod.rs:575-584`）、`purge_memories` 里删除 `search_docs`（`mod.rs:702-706`）、`memory_allowed`（`search/mod.rs:282-298`）。下面的行号以我最后一次读到的为准。

---

## P1（新回归）：删除对话时的快照清理会改写并锁住整个组织的会话行

- **位置**：
  - `crates/server/src/memory/mod.rs:715`：`purge_history_references` 的条件只有 `WHERE org_id=$1 AND memory_snapshot IS NOT NULL`。没有按创建者过滤，也不检查快照里是否真的引用了这个对话。所以组织内**所有用户**、所有带快照的会话行都会被重写并加行锁，直到事务提交。
  - 调用方：
    - `memory/api.rs:190`：REST 删除；
    - `projects.rs:696`：`purge_conversation`，经由 `memory/mod.rs:682` 由 collector 调用，删除对话和**过期临时对话**都会走到；
    - `projects.rs:598-648`：项目回收时每个对话调用一次。
- **后果**：
  1. **并发删除会死锁**。REST 删除先在 `api.rs:178-184` 锁住自己那一行，再在 190 行扫描全组织。两个同组织的删除事务只要在时间上重叠，就会互相等待对方先锁住的那一行。collector 和 REST 删除重叠时也一样。PostgreSQL 会中止其中一个，REST 那边就返回 500。
  2. **项目回收会阻塞整个组织**。第一次循环就锁住了全组织带快照的会话行，之后还要逐个调用沙箱的 `Destroy`/`PurgeWorkspace`（`projects.rs:622`、`640`），直到 684 行才提交。在这段时间里，组织内已有快照的会话要开始 run 时，会卡在 `memory/mod.rs:340` 的 `SELECT memory_snapshot … FOR UPDATE`。改名、归档等写会话行的操作也会被阻塞。
  3. **写放大**：每删除一个对话，包括从来不会出现在 recent 里的临时对话，都要重写整个组织的会话行。
- **同类问题**：组织策略过期后，`mod.rs:559` 的 `UPDATE conversations SET memory_snapshot=NULL WHERE org_id=$1` 没有加 `AND memory_snapshot IS NOT NULL`。只要还处于冻结状态，每 30 秒就会重写并锁住该组织的全部会话行。`mod.rs:563-568` 每轮也会对全部 id 重新跑一遍 purge。
- **建议**：
  - 条件改成只匹配被删对话的创建者，并且只匹配 `memory_snapshot->'recent' @> jsonb_build_array(jsonb_build_object('id',conv))` 的行；
  - 临时对话直接跳过；
  - 项目回收时在循环结束后一次性传入全部 id，并放到沙箱调用之后执行；
  - 冻结清理加上 `IS NOT NULL` 过滤，并只执行一次。

## P2（较轻）：embedding 失败时，"发布全文"和"重新排队"不在同一个事务里

- **位置**：
  - `search/mod.rs:307-311`：失败分支里调用 `publish`；
  - `publish` 在 274-276 行无条件把任务标成 `completed` 并提交；
  - 重新排队（`queued`/`dead`）在 `worker` 的另一个事务里完成（335-341 行），只按 `claim_token` 匹配。
- **触发场景**：两个事务之间如果出现数据库错误（335/340/341 行的 `?` 会让 worker 退出，由 supervise 重启）或者进程崩溃，任务就会一直停在 `completed`：没有向量，没有 `error`，也不会进入 `dead`。这样它不会再自动重试，也很难被发现。
- **fencing 本身没有问题**：被重新领取的旧 worker 在 `publish` 时校验不过 token，排队的 UPDATE 也匹配不上。
- **建议**：给 `publish` 传一个"向量待补"的标记，在同一个事务里直接写入 `state='queued'`、退避时间和 `error`。
- **附带一点**：用户关闭"保存记忆"期间被处理的记忆任务（例如撤销一次删除，`undo` 不检查开关），会走 `memory_allowed=false`，不带向量发布，同版本的旧 chunks 也会被删掉。只有组织解冻会重排这些任务（`mod.rs:575-584`）；`set_preferences`（`api.rs:97-116`）在用户重新开启时不会重排。之后 hybrid 查询仍能通过全文命中，但这些记忆一直没有向量，要等手动 reindex。

---

## 原有问题核对：已全部修复

| 原编号 | 当前状态 |
|---|---|
| P1-1 | embedding 失败时先发布全文，再重试向量（`search/mod.rs:307-311`）；hybrid 降级为全文（`search/mod.rs:114-125`）。fencing 正确。 |
| P1-2 | `chat/tools.rs:198`：除 fenced 以外的内置工具错误都记录为工具结果；`prepare_search` 中 hybrid 降级为全文。 |
| P1-3 | saved 快照、`memory_context`、`memory_notice` 都只存 ID；展开时通过新接口 `timeline_context` 按当前权限解析。30 天后清理正文和事件（`mod.rs:688-708`），组织清理包含已删除的行，项目回收会清理事件。 |
| P2-1 | 通知按 `origin_conversation_id` 投递（`mod.rs:600-603`）。 |
| P2-2 | 摘要有退避，并按最旧优先排序（`mod.rs:521`、`529`）。 |
| P2-3 | collector 不再依赖搜索开关（`main.rs:51-54`）。 |
| P2-4 | 有了 priority 列，索引带 org（迁移文件第 60、63 行，`claim` 的排序）。 |
| P2-5 | 搜索聚焦使用独立窗口（`controller.dart:651-676`）。 |
| P2-6 | SnackBar 撤销在页面销毁后仍会发出请求（`memories.dart:69-87`）。 |
| P2-7 | guest 改用 `rfind`，上限 600 KB，足够容纳预算上限加项目指令。 |
| P2-8 | `items.created_at` 已回填；各历史列表已过滤临时对话；时间线解析不会再崩溃。 |
| P2-H1 | 三处都已用 `pgroonga_query_escape`；测试覆盖了 `(`、`"`、`OR`。 |
| 原"待确认" #2、#3 | MCP 幂等键已包含 sandbox/lease；自定义 embedding 地址必须配置自己的 `embedding_key_env`（`config.rs:271-275`）。 |

**仍需你们拍板（不计级别）**：原"待确认" #1。`docs/m4-acceptance.md:43` 现在明确写着"保存记忆与引用历史是独立开关"，但代码里关闭"保存记忆"仍会连带关闭历史注入、`conversation_search` 和摘要（`memory/mod.rs:336`、`memory/api.rs:227`、`mod.rs:516`）。需要改文档或改代码中的一个。

## 复查后修复与核对

- 复查新增 P1：历史快照清理从 REST 删除和项目/对话回收事务移出，改为独立的短后台事务；仅改写实际存在失效引用的快照，并按旧 recent 值校验避免覆盖并发重建。该事务不持有 channel 锁，不调用执行节点。组织冻结清理增加 cleaned_at 标记，只执行一次，清空快照也仅匹配非空行。新增实际数据库验收：锁住无关会话后清理仍能完成，已删除来源的摘要副本被移除。
- 复查新增 P2：全文发布与 queued/dead、退避时间、错误状态在同一事务提交。停止在 process 返回之后也不会丢失向量重试；验收直接检查此时的持久状态。
- 用户重新开启总开关时重排该用户记忆索引已在最终代码实现；复查报告读取的是该段变更前的版本。组织解冻同样重排。
- 文档统一总开关与历史开关的语义。没有请求用户作常规实现选择。
- 最后两项修复由针对性的集成测试与 Clippy 验证；没有进行第三轮 Claude Code 审查。


## 最终验证

Rust workspace 测试、一致性 13/13、M3 7/7、M4 集成验收、Flutter 25 项测试和静态检查、Claude adapter 构建均通过。复查后的最终清理改动另行通过 M4 的无关行锁/原子重试验收，以及 M3 的并发项目删除/移动回归。`cargo fmt --all --check`、全 workspace/all-targets Clippy（warnings as errors）、`git diff --check` 通过。embedding 故障测试中的 503 是有意注入，验证了全文可用性和持久重试。
