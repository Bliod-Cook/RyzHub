# v1 剩余工作 · 阶段 8：客户端完善与真机验证

2026-10-12。实现 v1 剩余工作计划的阶段 8。这一阶段只改客户端，服务端与协议不变。

## 变更

| 项 | 内容 |
|---|---|
| 1 审批模式选择器 | Work 对话的输入框工具区新增审批模式胶囊（`ApprovalMenu`）：每次询问、自动允许编辑、全部自动，每项附一句说明。<br>选项取自命令目录的 `approval_modes`，策略禁止全自动时不出现。选择后经 `UpdateRunConfig` 下一轮生效，全部自动用警示色图标。Chat 没有审批，不显示。 |
| 2 emulated 能力提示 | 命令目录里能力为 `emulated` 时，在控件旁加说明图标（`EmulatedNote`，悬停或点按显示）：<br>• 补充指令：Work 底部的“补充指令排入下一轮”改为按 `steer` 能力判断，不再写死“非 Codex”，并附说明；<br>• 计划模式开关；<br>• 分叉按钮的提示文字；<br>• `/compact` 的说明。<br>文案集中在 `l10n.dart` 的 `emulatedNote`，`HubController.emulated()` 判断。 |
| 3 检查点显示 | 工作区栏显示最近一次检查点的保存时间（“刚刚”“N 分钟前”“N 小时前”，更早显示日期时间；悬停显示完整时间），不再显示水位 seq。<br>`stale` 时显示警示：超过 20 分钟没有新检查点，节点故障时之后的改动可能丢失。<br>按阶段 7 的 `storage_state` 显示归档中、已归档、恢复中、已按保留期删除。 |
| 4 design tokens | `client/tokens/tokens.json`：强调色、浅色与深色调色板、间距刻度、圆角、字号。<br>`scripts/gen-tokens.py` 生成 `client/lib/tokens.g.dart`（`accentTokens`、`LightTokens`、`DarkTokens`、`Space`、`Radii`、`FontSizes`），`theme.dart` 导出它。<br>`theme.dart` 的颜色、圆角与字号改用 token；19 个文件里 185 处 `EdgeInsets` 的数值改成 `Space.*`，数值不变。<br>`test-client.sh` 用 `gen-tokens.py --check` 检查生成文件是否最新。 |
| 5 golden 测试 | `client/test/golden_test.dart` 加载 RyzHub Sans、0xProto 与 Material 图标字体，用内核把固定的条目（`test/goldens/timeline.json`）投影成时间线后渲染：<br>• 时间线（浅色、深色）：中英混排的 Markdown、行内代码、列表、带行号与高亮的代码块（含中文注释）、表格、展开的文件 diff、运行中命令的 ANSI 终端输出；<br>• 卡片：审批卡片与插件声明式组件（卡片、表格、按钮）。<br>结果与 `test/goldens/*.png` 逐像素比对。 |
| 6 真机 PoC | 本机（Linux，无 Android SDK、无 Windows）无法完成，整理为 [真机验证清单](v1-device-poc.md)：M0 遗留项（Windows 输入法、Android 切网与锁屏、中英混排字形、帧率），加上阶段 6、7 的原生代码（MCP Apps 的 WebView、FCM 通知、安全存储的会话保持、Windows 唤醒重连、外部账号登录）。 |

## 实现中发现并修复的问题

- **文件 diff 窄而居中**：golden 测试发现，展开的文件变更 diff 只有内容那么宽，并且居中显示。原因是折叠动画（`Unfold`）放松了约束，而 diff 的容器没有指定宽度。现在 diff 容器占满宽度。
- **用户消息气泡总是撑到最宽**：阶段 4 把 Markdown 移进内核后，渲染块的那一列用 stretch 排列，用户气泡因此总是撑到最大宽度（行宽的 78%），短消息也是一个长条。现在气泡里的块（包括列表和脚注）按内容宽度排列，助手回复仍占满宽度。新增回归测试：短消息的气泡窄于 300 像素，长消息换行后正好是 78%。
- **气泡里文字偏上**：同样来自阶段 4，每个渲染块后面都留 8 像素间距，最后一块也留，气泡因此上边距 10、下边距 18。改为只在块与块之间留间距，与原来 flutter_markdown 的排法一致；助手回复、思考与计划条目末尾也因此少了这 8 像素。回归测试检查气泡里文字上下的空白相等。
- **去掉回答进行中输入框上方的提示**“补充指令将在下一次模型请求前采用”（按要求删除提示和相应测试；每条补充指令自己的状态文字保留）。

## 与计划不同之处

- **审批模式回到输入框**：之前的界面调整把它从输入框拿掉，打算以后做成命令。按本计划放在 Work 的工具区；旁问仍不在输入框上。
- **token 只覆盖计划列出的范围**：颜色、圆角、字号先在 `theme.dart` 使用 token；各组件里内联的字号与圆角没有逐一替换，间距（`EdgeInsets`）全部替换。
- **真机 PoC 未执行**：见第 6 项。清单里的项目都还没有在设备上看过。

## 验证

| 检查 | 结果 |
|---|---|
| `test-client.sh`（token 检查、`flutter analyze`、`flutter test`） | analyze 无问题；131 通过（2 跳过）。新增：<br>• 审批模式菜单（说明、策略去掉全自动、选择）；<br>• emulated 说明图标；<br>• 检查点时间与存储状态文案；<br>• 用户气泡宽度；<br>• 3 张 golden。 |
| 服务端 | 本阶段没有改动，沿用阶段 7 的结果（fmt、clippy、150 项单元测试、integration 19/19、m1 33/33、m2 9/9 + 1/1、m3 8/8 × 2、m4 1/1）。 |

## 部署到 dev

- 网页端重新构建到 `var/m4/web`，后端不需要重启。
- 浏览器冒烟：用阶段 7 的冒烟账号新建一个 Work 对话，输入框显示审批模式“每次询问”与计划模式，工作区栏正常显示，页面没有错误。
