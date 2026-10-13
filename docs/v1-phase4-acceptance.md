# v1 剩余工作 · 阶段 4：客户端内核、协议与输入交互

2026-10-11。实现 v1 剩余工作计划的阶段 4。线协议的变化见 [线协议](wire-protocol-v1.md) 的“v1.6 新增”和“传输”，用法见 README。

## 变更

| 项 | 内容 |
|---|---|
| 4.1 内核渲染 | Markdown 由内核（pulldown-cmark）解析成块，Flutter 只按块渲染（`ui/markdown.dart`），移除 flutter_markdown_plus、markdown 与 re_highlight。<br>预处理照旧支持：四种 TeX 写法（`$` 按 Pandoc 规则）、跨空行的 `$$` 公式、引用来源折叠成芯片（流式时隐藏未写完或未被引用的一组）、`sandbox:` 与 `/attachments/` 文件链接、裸网址自动链接、GFM 表格、任务列表、删除线、脚注。<br>流式回复：未闭合的 `` ` ``、`**`、`*`、`~~` 在末尾补全，写到一半的链接只显示文字。<br>代码高亮用 syntect 加 bat 的语法集（two-face，约 200 种语言），按代码块增量着色，颜色仍是 Atom One。<br>diff 用 similar 生成或解析统一 diff；ANSI 用 vte 解析成带样式的片段。工具输出含转义序列时按终端样式显示，`diff`/`patch` 代码块按增删着色 |
| 4.2 时间线与合批 | 内核为打开的对话维护时间线：行（消息，以及折叠在一起的步骤）按位置下发 insert/set/remove/reset。<br>条目只在 revision 变化时整条下发，流式时只下发追加的文本和从某块起的新块。Flutter 未变的块沿用原 widget，流式时只重建最后一块。<br>合批移进内核：帧到达即交给内核，协议状态与要发的帧立即处理，视图变化攒 32ms 一批；空闲后的第一批立即交出。搜索定位的历史片段用同一套投影（`timeline_snapshot`） |
| 4.3 CreateRun | 新对话由 `CreateRun` 命令创建，离线也能新建：对话立即出现在侧栏，命令进入本地持久队列。<br>Gateway 在一个事务里建对话并排队命令，Owner 按 SendMessage 执行。同一 cid 重发返回第一次的结果，别人的 ID 返回 `CONVERSATION_ID_TAKEN`。<br>内核在 CreateRun 得到结果前暂扣该对话的订阅与命令，被拒绝时客户端撤下侧栏条目、提供“恢复编辑”。旧服务端（无 `create_run` 能力）仍走 REST |
| 4.4 撤销记忆 | `UndoMemoryUpdate` 发在用户频道，由 Gateway 执行并按 cid 保存结果，只撤销一次（迁移 0025 使 `commands.conversation_id` 可为空）。时间线里的记忆通知和设置页都改走命令队列 |
| 4.5 已读与未读 | 主线 run 结束时记录对话的 `activity_seq`（迁移 0025）并发到用户频道，大于已读位置即未读。侧栏用圆点和加粗标出。打开对话并停在最新处 0.8 秒后发送 MarkRead，各设备同步 |
| 4.6 草稿同步 | `PUT …/draft`（迁移 0025 `drafts` 表，后写入者胜），被接受的写入以 `DraftChanged` 发到用户频道，用户频道快照带着全部草稿。<br>客户端按对话保存输入框内容，停止输入 1.5 秒后同步。其他设备的草稿只在输入框没有焦点时采用 |
| 4.7 压缩 | `ryzhub.v1.protobuf+zstd`：不小于 256 字节的服务端帧各自是一个 zstd frame，客户端内核用 ruzstd 按魔数识别并解压 |
| 4.8 调试与降级 | JSON 子协议 `ryzhub.v1.json`（开发配置 `json_protocol`）。<br>SSE 降级：`GET /stream` + `POST /stream/{connection}`，客户端 WebSocket 连续 3 次失败后切换，事件流失败后再试 WebSocket。<br>连接逻辑与传输无关：WebSocket 与事件流共用同一个服务循环。<br>开启 axum 的 `http2`，`/ws` 支持 RFC 8441 |
| 4.9 `/` 输入联想 | 输入 `/` 按组列出命令：通用命令（compact、stop）→ 对话操作（new、rename、archive、export）→ 项目与插件命令（阶段 6 前为空）→ Agent 命令（harness 自带，选中后填入 `/name ` 再作为文本发送）。上下键选择，回车或 Tab 选定，Esc 关闭；输入法组字期间不弹出 |

## 与计划不同之处

- **zstd 不共享上下文**：计划是每连接一个流式压缩上下文。实现时发现 ruzstd 在一个 zstd frame 结束前，保留最后一个窗口（最多 128 KiB）的解码数据不交出，整条连接一个流就无法逐条解码。
  - 改为每条大消息各自压缩，小消息原样发送，客户端按 zstd 魔数区分。protobuf Frame 不会以这个魔数开头：开头字节会是 Frame 没有的 5 号字段的键。
  - 快照、历史和长回复这些大帧仍然压缩；流式 Delta 帧通常百余字节，跨消息上下文对它们的收益在每秒几 KB 的量级。
  - 已同步修改架构文档 §12.2。
- **语法集与体积**：syntect 自带的语法集没有 TypeScript、Kotlin、Swift、Dart、TOML、Dockerfile，改用 bat 的语法集。编译好的语法集无法裁剪出子集（上下文已按索引链接），只能整体使用。
  - 原生端随库内置；Web 端单独下载 `syntaxes.bin`（959 KB，自身已压缩），第一次出现代码时才加载，之前代码先显示为纯文本。
  - 首屏传输（gzip）：main.dart.js 1.56 MB → 1.43 MB，bridge wasm 250 KB → 1.02 MB，两者合计增加约 640 KB。增量主要是 fancy-regex（含 Unicode 表）与 syntect。
- **图片**：回复中的 Markdown 图片显示为链接（🖼 加说明文字），点开才访问。原来会直接加载远程图片，模型输出的图片地址可以把对话内容带到第三方。
- **选择**：每条消息一个 `SelectionArea`，可以在同一条消息内跨段落选择（原来每段各自可选）。代码块仍单独可选，并保留复制按钮。
- **表格**：列平分宽度、不再横向滚动。原因是公式无法提供按内容定列宽所需的尺寸，flutter_markdown 原来也是这样排版的。
- **草稿接口**：草稿用 REST `PUT` 写入，推送走用户频道。写入不需要离线排队：本机输入框就是离线副本。
- **SSE 连接按进程**：事件流连接只存在于打开它的 Gateway 进程，多个 Gateway 时 `POST /stream/{connection}` 需要会话粘滞。已写入线协议文档。

## 实现中发现并修复的问题

- **新对话订阅过早**（浏览器验证时发现）：客户端先订阅新对话、后排队 CreateRun，订阅没有被暂扣。服务端还不认识这个频道，撤销了订阅，对话建好了，时间线却是空的。已改为先排队 CreateRun，并补了 bridge 测试。
- **上下键被菜单锚点截获**：输入框外层的 `MenuAnchor` 用方向键做菜单导航，联想列表收不到上下键。快捷键改为直接绑在输入框上。
- **公式在表格里**：按内容定列宽要计算公式的 dry baseline，flutter_math 不支持，于是改为平分列宽。
- **裸网址被下划线截断**：pulldown-cmark 在 `_` 处拆分文本事件。改为整段收集完再识别网址。
- **HTML 块里出现占位符**：公式预处理没有跳过 HTML 块，占位符漏到了输出里。HTML 块现在与代码一同跳过；图片说明里的占位符还原为原文。
- **离线查询元数据过期**：`commands.conversation_id` 改为可空后，`accept` 里 `query!` 的离线元数据仍把它当作非空，遇到用户频道命令的 cid 会解码失败。改为运行时查询。

## 验证

| 检查 | 结果 |
|---|---|
| fmt、clippy `-D warnings`、`cargo test --workspace --locked` | 通过，113 项单元测试。新增的有：<br>• Markdown：块与样式、四种 TeX、价格与转义、文件链接与自动链接、引用芯片、流式补全；<br>• 性质测试：400 篇随机文档按随机分块流式输入，每一步与整条解析一致（另以 4000 篇跑过一次）；<br>• 高亮：含增量着色与整段一致；<br>• diff、ANSI；<br>• 时间线：步骤折叠、行 diff、流式增量；<br>• 内核：CreateRun 暂扣与释放、草稿、zstd 帧解包；<br>• bridge：合批与新对话订阅 |
| `test-integration.sh` | 19/19。新增两项：<br>• CreateRun 重发与占用、未读位置、草稿推送与快照、撤销只生效一次；<br>• zstd 协商与 ruzstd 解压、JSON、事件流（含跨用户投递被拒）、HTTP/2 extended CONNECT |
| `test-m1.sh` | 22/22 |
| `test-m2.sh`（含 gVisor 往返） | 8/8 + 1/1 |
| `test-m3.sh`（runc 与 gVisor） | 8/8、8/8 |
| `test-m4.sh` | 1/1 |
| `flutter analyze`、`flutter test`（`scripts/test-client.sh`，用本机构建的 Rust 内核渲染） | 无问题，117 项通过（另有 2 项按条件跳过）。新增的有：<br>• 块渲染与块复用；<br>• 内核高亮、diff 与 ANSI 显示；<br>• CreateRun 的显示与撤回；<br>• 未读与草稿；<br>• 事件流传输；<br>• `/` 联想 |
| dev 浏览器 | 登录后新建对话不调用 REST 创建、回复正常，WebSocket 协商为 zstd 子协议。<br>代码块按需加载语法集后着色，表格、任务列表、TeX、引用芯片、折叠步骤显示正常。<br>`/` 列出命令；离开对话后回复到达，侧栏显示未读圆点；草稿出现在第二个浏览器上下文的输入框中。控制台无错误 |

## 部署

- **迁移**：0025 只新增列与表，并放宽一个非空约束，不改写数据。
- **客户端**：Web 需用 `scripts/build-web.sh`，它会生成 `syntaxes.bin`。
- **兼容**：旧客户端不受影响，它们不提供 zstd 子协议，不认识的字段会被忽略。
- **测试环境**：`test-m1.sh`、`test-m2.sh` 需要 `CC_x86_64_unknown_linux_musl=gcc`。本机 codex 已升级到 0.160.1，`test-m2.sh` 需用 `RYZHUB_CODEX_BINARY` 指向 0.154.0（`var/execution/bin/codex`）。

### dev 部署记录（2026-10-11）

1. 确认 `ryzhub_m3` 没有进行中的 run，备份到 `var/backups/0025-migration-20261011-193818/ryzhub_m3.dump`。
2. 构建后端，用 `scripts/dev-m4.sh` 重启，迁移到 25。`/healthz` 正常，`/capabilities` 宣告 `create_run`、`unread`、`drafts`、`event_stream`、`memory_undo_command`，日志无 WARN。
3. 用 `scripts/build-web.sh` 构建 Web 到 `var/m4/web`（含 `syntaxes.bin`）。修复新对话订阅问题后，又重建了一次 Dart 部分。
4. 执行节点未改动；`test-m1.sh` 已用当前代码热替换过一次。
