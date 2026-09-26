# M3 第二条路径验收

2026-09-26。实现架构 §13 的 Claude Code（官方 Agent SDK）、Responses Chat、统一命令和私有项目。真实模型上游沿用本地协议 fixture；本轮没有收费模型调用。

后续 review 的五项修复及独立回归见 [M3 review 修复记录](m3-review-fixes.md)。

## 另一对话修复的独立复核

复核了 [M0–M2 修复记录](m0-m2-review-fixes.md) 对应的实现，并实际运行回归。恢复失败防护、固定恢复目标和幂等应用、跨轮事件路由、并行审批、Host 生命周期锁、长运行密钥轮换、原生提问、历史分页均保留。

长时间回归还发现 Stop 后保留 containerd task 会累积 shim 并耗尽进程配额；现于确认停止后回收 task，保留容器和工作区。恢复回归增加了旧 task 已释放的断言。

补齐 `test-integration.sh` 的公开测试签名 key；修复执行节点绑定 Cargo 临时输出目录的问题，改为安装到 `var/execution/runtime`，避免 `cargo clean` 后运行节点持有失效目录。M3 另增加旧运行配置回填：已有 M1/M2 运行沿用记录的模型，升级后不会采用新的实例默认值。原 0001–0008 迁移未改写。

## 实现

- Claude Agent SDK **0.3.283**，随包 CLI **2.1.283**，TypeScript **5.9.3**。Guest 持有 SDK 进程和原生会话，通过本地代理接 LLM Gateway；真实上游 key 不进入沙箱。支持 streaming input、工具审批、AskUserQuestion、interrupt、模型与权限/思考设置、命令发现、原生 fork/resume。
- Claude 的 steer 为 emulated：当前原生轮结束后启动下一轮，沿用同一个平台 run。Codex 使用原生 turn/steer。界面区分 accepted、waiting、handed；不把转交当成模型采纳。修复“第一轮尚未登记就收到 steer”的重复 turn/start 边界，以及主动关闭旧 SDK query 时的误报。
- Responses Chat 保存 provider 原生 input/output，包括 reasoning/encrypted content 和 response ID。工具生命周期、审批、故障恢复复用现有持久流程；compact 结果替换旧上下文。续跑和 fork 均使用持久原生记录，不由界面文字重建。
- UpdateRunConfig、Compact、Fork 沿用 CID 幂等、Owner fencing、事务提交后 ACK。模型、推理、审批和计划模式下一轮生效；运行快照不变。harness/protocol 在已有 run 后不可变。组织可禁用 full_auto。Codex 模型/推理目录取原生发现交集；Claude SDK 可接受管理员显式配置的模型 ID，其原生便利别名与显式路由分开处理。
- 私有项目支持指令、文本知识文件、默认模式/harness/model、不可变 default/project_only 标记、移入移出和配额。每个 run 快照项目内容；Chat 提供 project_file_search，Chat/Work exec 只读挂载 `/project`，Work 导入 `.platform/project.md`。组织策略控制新建与修改，访问始终受 creator 和租户边界限制。
- 删除项目同时隐藏项目内对话，30 天内可恢复。到期 collector 与恢复/命令争用同一项目及频道锁，清除项目文件、对话内容、原生日志、沙箱和本地暂存，撤销 key 并交由检查点 GC 回收对象。保留身份/用量/审计墓碑；外部分支 restore pin 保护仍被使用的检查点。Host 保存工作区删除墓碑，防止迟到 Create 复活；文件回收不占用全局心跳锁。
- 前端增加执行路径、项目管理、能力驱动的配置控件及命令菜单、消息分叉、导出和移动。保留检查点/归档恢复及历史分页。消息标签不随当前模型选择改写历史模型归属。本地补齐 SDK 响应符号需要的 Noto Emoji 字体分片。
- 浏览器截图复核修复原生提问直接显示 JSON 的问题：展示问题文字与可点击选项，保留自由输入；多问题分别收集答案并按原生 ID 提交，避免用户手写 JSON。输入法组字与重复提交保护保持有效。

## 验证

最终统计与每项命令结果见 [机器可读报告](m3-verification.json)。日志在 `var/m3-review/`；测试使用临时数据库，浏览器使用测试账号。

最终通过：Rust 单元/组件 31 项、一致性与升级 10 项、M1 8 项、M2 7 项及 gVisor 恢复 1 项、M3 runc/gVisor 各 2 项，共 61 项后端测试；Flutter 7 项组件测试；浏览器 50 个场景（M3 为 15 个）。fmt、严格 Clippy、Flutter analyze、TypeScript 与 release Web 构建均通过。最终 M3 浏览器无页面错误和外部请求，测试工作区已归档。

- Rust 单元/组件；fmt、Clippy `-D warnings`；TypeScript 编译。
- M0 一致性与旧数据升级；M1 模型协议、审批、执行、撤权；M2 原生运行、MCP、检查点、二级存储、RPO、Owner 重启及 gVisor。
- M3 两条大场景分别覆盖 Responses 原生续跑/compact/fork、项目边界与文件快照、默认设置、删除恢复与到期回收，以及 Codex/Claude 的 steer、运行中配置变更、模型切换、计划、审批、提问、分支、恢复和停止；使用实际容器并分别验证 runc 与 gVisor。
- Flutter analyze、7 项组件测试、release Web；Chromium 的 M0 多设备、M1、M2、225 条历史分页和 M3 项目/命令流程，并检查页面错误、外部请求及桌面/390px 截图。

浏览器报告：[基础](browser-regression.json) · [M1](browser-m1.json) · [M2](browser-m2.json) · [分页](browser-history.json) · [M3](browser-m3.json)。截图：[桌面](browser-m3-desktop.png) · [手机](browser-m3-mobile.png) · [原生提问](browser-m3-question.png) · [手机提问](browser-m3-question-mobile.png)。

## 运行与升级

见 [README](../README.md#m3-本地协议联调)。依次准备执行节点和固定 Codex、构建 M3 SDK 镜像、运行 `dev-m3.sh`。API/WS 8280，Gateway 8281，fixtures 9181/9182；数据库独立为 `ryzhub_m3`。

应用迁移 **0009–0011**，随后更新 Server、Host/Guest、镜像和 Web。镜像 digest 记录在 `var/m3/image.txt`。旧工作区复用创建时的 image/runtime，恢复使用检查点固定镜像；新镜像只用于新工作区。密钥文件 `var/m1/secrets.env` 必须保留，不能重生成 vault 主密钥替代已有部署密钥。

## 边界

模型验收使用 Codex `gpt-5.4 → gpt-5.2`、Claude `claude-sonnet-4-6 → claude-opus-4-6`。当前 Codex 二进制的部分目录别名会发送不同的 wire model（本地观察到 `gpt-5.6-sol` 发送 `gpt-5.4`）；Gateway 保持精确模型授权并拒绝这种不匹配，平台暂不重写此类别名，管理员需配置与实际请求一致的模型。

真实商业上游、计费/限流语义、物理多节点故障、大型工作区和 Windows/Android 真机尚未验收。本地测试验证的是官方 harness 与协议/执行链路。

项目文件当前为文本，单文件 1 MiB、默认总量 2 MiB；检索为文本匹配。全文/向量索引、记忆读写和 project_only 检索隔离属于 M4；当前仅保存边界并注入说明。插件集合属于 M5；env_profile 尚为单一 base 环境。

Work 旁问使用主线摘要启动独立只读 Chat，不访问文件、不提供工具，标记为 emulated。Claude 分叉使用已完成的原生 assistant 消息边界；Codex 仅允许已完成轮次末尾的回复，避免模型上下文包含所选消息之后的工具操作。文件采用分叉时的工作区快照，不提供历史文件回退。未运行过的复制历史不再显示不可执行的分叉按钮。
