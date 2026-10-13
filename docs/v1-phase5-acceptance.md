# v1 剩余工作 · 阶段 5：Harness 与 LLM Gateway

2026-10-11。实现 v1 剩余工作计划的阶段 5。线协议的变化见 [线协议](wire-protocol-v1.md) 的“v1.7 新增”，配置见 README 的“模型网关”“Chat 上下文”“ACP harness”。

## 变更

| 项 | 内容 |
|---|---|
| 5.1 `terminal_output` 流 | 命令运行期间的输出挂在命令条目上推送（`Delta.stream = "terminal_output"`），与回复文本各自计偏移。<br>来源：Codex 的 `item/commandExecution/outputDelta`；Chat 的 `exec` 每秒取一次 guest 的输出。guest 的 fold 增加 `outputs`，Owner 的实时流按（条目，流）区分，只把 `text` 写回条目。<br>客户端在步骤卡片里用内核的 ANSI 片段显示（最高 200 px，跟随末尾），完成后显示完整输出 |
| 5.2 子 agent 树 | `HarnessEvent::Item.parent_native_id` → `items.parent_item_id`（迁移 0026）→ `Item.parent_item_id`（协议小版本 7）。<br>Claude adapter 读 `parent_tool_use_id`，并开启 `forwardSubagentText`。Codex 的 `collabAgentToolCall` 显示为工具步骤，它带 `parentItemId` 时同样嵌套。<br>内核的行不再单列有父条目的条目，客户端把它们缩进显示在调用卡片里。Claude 的 `subagent` 能力恢复为 native |
| 5.3 回合 diff | Codex 的 `turn/diff/updated` 映射为每回合一个文件变更条目（`turn-diff:<turn>`），随 diff 增长替换，按 `diff --git` 拆成文件。文件变更行可展开，用内核解析的 hunk 按增删着色 |
| 5.4 Codex 命令发现 | initialize 之后发送 `skills/list`，用保留 ID 截获响应，启用的 skill 成为 Capabilities 里的命令（进入 `/` 菜单的 Agent 命令分组）。Capabilities 不带模型列表时保留已发现的模型 |
| 5.5 Gateway 协议 | 新增 OpenAI Chat Completions（流式请求强制 `stream_options.include_usage`，用量取最后一个 chunk）与 Gemini `generateContent` / `streamGenerateContent`（`usageMetadata` 累计，输出 = candidates + thoughts）。<br>`/v1/messages/count_tokens` 只转发、不计量；`GET /v1/models` 按密钥与策略列出模型，按请求头选 Anthropic 或 OpenAI 格式。<br>成员可以登记 Gemini 凭据。guest 代理同步增加这些路由，支持 GET |
| 5.6 多上游故障转移 | 每个模型可配置多个上游（`url`、`key_env`、`model`、`priority`、`weight`）：按优先级，同级按权重随机。还没转发任何内容时，连接失败、5xx、429 换下一个；最后一个上游的回答原样返回。<br>成员自己的凭据只发往协议的默认上游 |
| 5.7 推理上限与内容留存 | `Policy.max_reasoning` 检查各协议的写法：Anthropic `output_config.effort` 或 thinking 预算，Responses `reasoning.effort`，Chat Completions `reasoning_effort`，Gemini `thinkingLevel` 或 `thinkingBudget`。预算按 1K/4K/16K/32K/64K 换算成等级，超过上限返回 403 `REASONING_DENIED`。<br>`Policy.retain_model_content_days` 开启后，请求与响应（最多 8 MiB）存进主存储 `<org>/llm/<usage id>/`，按内容寻址；`usage_records.retained_content` 记录地址，`content_expires_at` 记录到期时间，retention 任务到期删除。owner/admin 用 `GET /orgs/{org}/usage/{id}/content` 查看。<br>Anthropic 压缩的用量按 `usage.iterations` 合计 |
| 5.8 ACP v1 通用 adapter | 实例配置 `[[harness.acp]]`，每个 profile 是一个 Work harness：命令、环境模板、协议、默认模型、版本。<br>guest 的 ACP 桥（`crates/execution/src/acp.rs`）是纯状态机，对 guest 的其余部分扮演 Codex 方言：<br>• 平台请求 → `session/new`、`load`、`prompt`、`cancel`；<br>• `session/update` → 条目与 Delta；<br>• `request_permission` → 审批；<br>• 计划模式与模型切换走会话选项。<br>服务端：补充指令在下一轮生效（与 Claude 相同），`/compact` 作为一轮发出，分叉不可用，检查点记录 profile 的版本。<br>M3 镜像带 OpenCode 1.19.0（musl 版）与 ripgrep，经它的 Anthropic provider 与 guest 代理使用平台模型 |
| 5.9 Chat 上下文管理 | 请求估算超过 `llm.context_budget_tokens` 时，最早的工具结果换成占位，直到低于 60%；模型用 `read_tool_output` 分页读回。<br>`llm.compaction_models` 里的 Anthropic 模型：每个请求带阈值压缩（`compact_20260112`）；`/compact` 用按需压缩（`compaction: {type: "summarize"}`）。<br>提供方不能压缩时请模型写摘要，摘要代替之前的上下文。Responses 的 compact 失败时同样回退。Chat 的 Messages 路径从此支持压缩 |

## 与计划不同之处

- **ACP 桥放在 guest 里，以 Codex 方言对接**：ACP 的 prompt 在整轮结束时才返回，更新也不带回合。桥自己生成回合 ID，立即应答“开始回合”，guest 的 journal、replay 与服务端的投影都不用改。桥不做 IO，单元测试直接重放从 OpenCode 录下的消息形状。
- **ACP 的能力**：
  - 不支持：分叉（OpenCode 的 fork 仍是 unstable）、提问、推理强度。
  - 补充指令：排到下一轮。
  - 审批模式：由桥按模式应答权限请求，不依赖 agent 的配置项。
  - `terminal_stream`：标为不支持。桥会转发进行中的工具内容，但 OpenCode 只在完成时给出输出。
- **模型切换要求 profile 声明全部模型**：OpenCode 拒绝切到配置里没有的模型。环境模板因此提供 `{models}`（以每个可用模型为键的 JSON 对象），由服务端按目录给出。
- **移出的工具结果不另存 BlobStore**：原文本来就完整保存在 `tool_calls.result` 里，`read_tool_output` 直接从那里读（限同一用户的对话，迁移 0026 加了 `(org_id, call_id)` 索引）。
- **阈值压缩保留压缩块之前的消息**：文档说提供方会忽略压缩块之前的内容，也允许客户端删掉。这里没有删：dev 环境没有 Anthropic 上游可以核对删除后的消息格式，删错会让后续每次请求都 400。体积由工具结果移出控制。按需压缩按文档要求删掉被概括的内容，压缩块成为上下文开头。
- **未用真实 Anthropic 核对压缩**：字段名与 beta 头（`compact-2026-01-12`、`compact-2026-09-04`）依据官方文档，用 fixture 验证。流式压缩块的 delta 字段文档未写明，按 `content` 读取，也接受 `text`。
- **留存不超过用量记录**：保留天数取策略值与 `retention.usage_days` 的较小者。否则用量月分区被删后，留存的内容就没有记录可以找到了。
- **Gemini 的动态思考不受上限约束**：`thinkingBudget` 为 -1（模型自定）或不写时，请求没有指明强度，网关放行。
- **Claude 关闭后台任务**（`CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1`）：后台运行的子 agent 与命令会在 run 结束后继续请求模型，网关以 403 拒绝（run 已结束）。关闭后它们在本轮内完成，每次模型请求都属于一个 run。

## 实现中发现并修复的问题

- **Claude 子 agent 的条目缺失**：Agent SDK 默认不转发子 agent 的文字，开启 `forwardSubagentText` 后才有。
- **留存内容的键**：BlobStore 按内容寻址（64 位十六进制、读取时校验），不能用路径式的键。改为每条记录一个命名空间，按哈希存放，地址记在 `retained_content`。
- **guest 重启测试**：命令的实时输出使 fold 版本递增，版本只在进程内有效，重启后从 0 开始。replay 以“版本不同”判断有无新 fold，所以重启后仍能送达。测试改为只比较持久的部分。
- **OpenCode 在 fixture 上一直循环**：fixture 在 `message_delta` 之后又发了一个只有用量的 `message_delta`，OpenCode 用的 AI SDK 把结束原因读成 unknown，于是继续这一轮。真实 API 只发一个，所以只为 OpenCode 去掉了这个多余事件。但兼容 Anthropic 的上游如果也这样发，OpenCode 会出同样的问题。
- **gVisor 下停止沙箱偶尔失败**（OpenCode 在 gVisor 上恢复工作区时发现）：host 停止沙箱时先发 SIGKILL，任务列表显示已停止后立即删除任务。但 containerd 有时仍把它当作运行中（`failed precondition` 或 `cannot delete a running container`）。停止因此报错，恢复以 `OLD_WRITER_NOT_FENCED` 失败。
  - 改为删除失败且原因是“仍在运行”时，每 100 ms 重试，最多 10 秒，仍不行再强制删除，并以任务从列表消失确认退出。
  - 恢复时旧沙箱停不下来的原因现在会写进日志。
- **磁盘写满**：重建镜像时根分区只剩 757 MB。清理了重复的测试可执行文件与 incremental 缓存。

## 验证

| 检查 | 结果 |
|---|---|
| fmt、clippy `-D warnings`、`cargo test --workspace --locked` | 通过，131 项单元测试。新增的有：<br>• ACP 桥 7 项：回复流式、审批与命令输出、模式与模型切换、审批模式、中断、`session/load`、思考、diff 与计划、初始化失败；<br>• Gateway：上游排序与权重分布、各协议的推理强度、用量换算（含压缩的 iterations）；<br>• Chat：工具结果移出（顺序、稳定性、本轮不动、两种协议）、压缩与摘要接续、流式压缩块；<br>• 运行设置：ACP harness 的推断、协议、能力与配置校验；<br>• execution 与 Codex 的输出、diff、skills、子 agent 映射 |
| `test-integration.sh` | 19/19 |
| `test-m1.sh` | 25/25。新增：<br>• 命令输出流（`streamcheck`）；<br>• Chat Completions 与 Gemini 的用量、上游故障转移、推理上限、内容留存与到期删除、`count_tokens`、`/v1/models`；<br>• Chat 上下文：工具结果移出并读回、阈值压缩请求、按需压缩、自动压缩块、提供方不能压缩时回退到摘要 |
| `test-m2.sh`（含 gVisor 往返） | 8/8 + 1/1 |
| `test-m3.sh`（runc 与 gVisor） | 8/8、8/8。双 harness 用例扩展为 Codex、Claude、OpenCode 三个：<br>• OpenCode 覆盖补充指令排入下一轮、模型切换、计划模式、审批、文件交付、`/compact`、工作区恢复后 `session/load`、取消；<br>• Claude 覆盖子 agent 条目嵌套在调用下 |
| `test-m4.sh` | 1/1 |
| `flutter analyze`、`flutter test` | 无问题，118 项通过（另有 2 项按条件跳过） |
| dev 浏览器 | 新账号的 Work 模型菜单出现 OpenCode。<br>用真实上游（Responses）新建 OpenCode 对话，回复“你好，很高兴见到你！”，回复下标注 OpenCode，输入框下提示“补充指令排入下一轮”。控制台无错误 |

## 部署

- **迁移**：0026 只新增列与索引，并放宽 `model_credentials` 的 provider 约束，不改写数据。
- **镜像**：M3 镜像带 OpenCode（`var/m3/image.txt`）。已有 Work 工作区沿用创建时的镜像；新工作区使用新镜像。
- **执行节点**：host 与 guest 都有改动（ACP 桥、输出流、停止重试、代理路由），需要热替换。
- **客户端**：Web 需用 `scripts/build-web.sh`（proto、core、bridge 都有改动）。
- **配置**：
  - 启用 OpenCode 需配置 `[[harness.acp]]`。
  - 策略 `harnesses` 若有白名单，需加上它的名字。
  - 给它用的模型在 `llm.models` 的 `harnesses` 里列出这个名字。
  - dev 的上游只有 Responses，所以 dev 的 profile 用 OpenCode 的 OpenAI provider。

### dev 部署记录（2026-10-11）

1. 确认 `ryzhub_m3` 没有进行中的 run，备份到 `var/backups/0026-migration-20261011-221522/ryzhub_m3.dump`。
2. `var/m4/config.toml`（原文件留在 `config.toml.before-phase5`）：
   - `sandbox.image` 改为新镜像 `ryzhub-m3-base@sha256:aee52a11…`；
   - 加上 `[[harness.acp]] opencode`（OpenAI provider，Responses）；
   - 策略 `harnesses` 与 `gpt-6.1-sol` 的路由加上 `opencode`。
3. 构建后端，用 `scripts/dev-m4.sh` 重启，迁移到 26。`/healthz` 正常，`/capabilities` 的 harnesses 为 `codex`、`opencode`，日志无 WARN。
4. 用 `scripts/build-web.sh` 构建 Web 到 `var/m4/web`。
5. 执行节点已由测试脚本与 `start-execution-node.sh` 热替换为当前代码。
