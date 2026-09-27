# M2 Work 验收与配置

实施日期：2026-09-26。下列自动验收与浏览器回归均通过，对应架构 §13 的 M2。真实模型上游按本轮约定使用本地协议 fixture；未调用收费模型。

本轮 review 的 8 项修复与新增回归见 [修复记录](m0-m2-review-fixes.md)。部署前需执行新增的 `0008_restore_progress.sql` 迁移。

## 实现范围

- OpenAI Responses 与 compact 原生代理，保留原始请求/SSE，分离 Anthropic/OpenAI 凭据，记录原生 usage。失败或 incomplete 响应不会记成完成。
- 固定 Codex app-server 0.154.0 在 guest 中独立运行，支持 thread/start/resume、turn/start/steer/interrupt、原生命令/文件审批及提问映射。Owner 接管通过 native operation ID、gseq 和 ItemFold 继续投影。检查点重放按 native turn ID 路由到所属 run；尚未登记的轮次不提前 ACK，恢复后的旧用量通知不阻塞后续事件。
- Work 的整个可写 upper 持久化，包括 `/workspace`、系统改动和 Codex 原生状态。归档是 tar.zst，转换 OCI whiteout/opaque，保留 uid/gid、权限、mtime、扩展属性和未跟踪文件。`target`、依赖目录不按 Git ignore 排除；只排除显式缓存、临时挂载与凭据规则。
- 每轮完成、手动请求及运行中默认每 300 秒检查点：暂停 → sync → 本地复制及 gseq 水位 → 恢复运行 → 压缩上传/校验 → 原子切换 head。失败保留前一个 head。
- primary/secondary BlobStore 均支持目录或 S3 SigV4。对象按组织隔离，以 SHA256 寻址；主对象缺失或损坏时用 secondary 校验后修复。
- restore pin 无 TTL。恢复先确认旧 writer 停止或租约过期，再恢复新 upper；后水位 Item 标记 `invalidated`，保留时间线并追加恢复说明，后续模型输入也收到恢复说明。新的恢复请求停止当前 writer；同一恢复作业的重试固定源沙箱、检查点与目标 ID，已应用的恢复不会重复失效时间线或追加通知。失败恢复保留 recovery_required，阻止创建空工作区。
- 执行节点租约由 LLM Gateway 心跳维护，独立于 Owner。Host 按沙箱串行化操作，检查点复制不持有全局状态锁；仅活动检查点中的暂停任务续租，遗留暂停任务仍可失效恢复。运行及等待审批期间每分钟检查虚拟密钥，在到期前轮换且不打断已接收的流。租约到期自动从最近提交检查点重建，旧密钥撤销；滚动重启 Owner 不重发原生模型请求。
- MCP Gateway 独立检查强制审批，绑定 run、sandbox epoch、工具参数 fingerprint、请求 ID 和有效期。批准只消费一次，结果可重读；不明确的执行结果不会自动重做副作用。
- Flutter 显示 Work/Codex、检查点/归档/恢复按钮、水位和失效状态，第二台设备实时读取新对话的正确 mode。PTY、fork、Work side query 明确 unsupported。

## 可复现验证

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
./scripts/test-integration.sh
./scripts/test-m1.sh
./scripts/test-m2.sh
cd client
flutter analyze
flutter test
```

`test-m2.sh` 实际使用 PostgreSQL、NATS、containerd、Codex 0.154.0，先跑 runc 场景，再用 gVisor 重跑 upper 往返。Responses 和 MCP 远端是本地协议 fixture，S3 fixture 使用 AWS botocore 独立校验 SigV4，并拒绝错误凭据。它们不是商业模型或真实云 S3 服务的验收。

| 测试 | 验证内容 |
|---|---|
| Rust 单元/组件 | 29 项通过：seq、Delta/Fold、journal、upper 权限/xattr/mtime、协议解析等 |
| M0 一致性与修复回归 | 9 项，幂等命令、双设备决议、Owner fencing、重连与 outbox |
| M1 集成 | 8 项，含撤员中断运行中流、最新已提交旁问前缀与完整工具结果配对 |
| M2 集成 | 7 项，覆盖下述故障和安全边界 |
| gVisor 往返 | 实际 lower 目录重命名、chmod、未跟踪文件和恢复后 native thread |
| Flutter | analyze 与 5 项组件测试 |
| 浏览器 | M0 13 个、历史分页 4 个、M1 9 个、M2 9 个场景，均无页面错误 |

M2 集成包括：实际 Codex turn 与 usage；provider/BYOK 隔离；压缩包内无测试密钥；primary 损坏后的 secondary 修复；恢复后继续同一 thread；重复恢复停止当前 sandbox；直接绕过 harness 调用 MCP 必须审批，参数修改被拒绝；原生命令未审批不写文件、审批后只写一次；节点执行丢失后按 RPO 恢复并标记失效；Owner kill/restart 后模型请求仍只有一次；恢复持有行锁时并发 GC 等待，pin 提交后 GC 不删除；保留期、宽限期、head、共享 blob 与跨租户隔离；生产 Owner 角色启动回收任务；S3 签名与错误凭据。

浏览器重放（先启动 `dev-m2.sh`、构建 Web 并运行 `serve-web.py`）：

```sh
uv run --with playwright python scripts/browser-m2.py
RYZHUB_API_URL=http://127.0.0.1:8180 uv run --with playwright python scripts/browser-m1.py
```

重复浏览器验收发现常驻测试沙箱耗尽执行节点 PID 配额，Codex 在 initialize 前退出。已停止这些测试实例并保留文件，限制 guest/Codex 的 Tokio worker 数，浏览器成功后自动归档释放进程；原生进程退出时，挂起 RPC 立即进入持久化的 `outcome_unknown`，不会等满超时或重放外部副作用。新增单元回归覆盖退出时的原生请求与独立 exec 隔离。

浏览器报告与截图：[M1 报告](browser-m1.json)、[M2 报告](browser-m2.json)、[桌面](browser-m2-desktop.png)、[390px](browser-m2-mobile.png)。[验证汇总](m2-verification.json)。本地完整日志位于被忽略的 `var/m2-review/`。GitHub Actions 已加入 M2 和固定 Codex/gVisor 安装流程，远程 CI 尚未执行。

## 存储与保留期

```toml
[work]
enabled = true
model = "gpt-5.4" # 本地 fixture 使用此名称；真实部署填可用模型
storage_root = "/srv/ryzhub/primary"
secondary_root = "s3config:/etc/ryzhub/secondary.toml"
checkpoint_interval_secs = 300
checkpoint_retention_days = 30 # 默认 30 天；0 表示不按时间淘汰
checkpoint_keep_last = 20      # 每个对话最多保留的检查点数；0 表示不限
checkpoint_gc_grace_secs = 86400
idle_suspend_secs = 1800       # 空闲沙箱先提交检查点再停止；0 表示不挂起
```

（迁移 0012 起的默认值；当前恢复点与 restore pin 始终保留。）

`secondary.toml` 只引用凭据环境变量：

```toml
endpoint = "https://s3.example.test"
bucket = "ryzhub-archives"
region = "us-east-1"
access_env = "RYZHUB_S3_ACCESS_KEY"
secret_env = "RYZHUB_S3_SECRET_KEY"
```

Owner 每 60 秒收集到期对象，先 tombstone，再过宽限期，最后检查引用和 pin 后删除。最新 head 始终保留，即使工作区归档时间很长；M2 没有默认到期删除最后一个恢复点的策略。相同组织内共享 blob 只在最后一个引用删除时清理；上传提交和 GC 用相同的 advisory lock 串行化。失败恢复保留 pin，运维需先核对失败作业并完成/终止恢复，再明确清理对应 pin，不能按年龄猜测失效。

manifest 固定 base image digest、归档编码、Codex 版本与执行水位。执行节点保留 containerd 的 digest 镜像引用，M2 不自动清理镜像，因此有恢复点的镜像不会由平台回收。分层依赖去重与镜像清理器属于后续里程碑。

## MCP catalog

```toml
[[work.mcp_tools]]
name = "write_note"
description = "写入笔记"
url = "http://127.0.0.1:9200/mcp"
remote_name = "write_note"
input_schema = { type = "object", properties = { text = { type = "string" } }, required = ["text"] }
```

Gateway 对远端执行 MCP initialize / initialized / tools/call，支持 JSON 或 SSE 响应及 session header。M2 catalog 中的工具全部需要审批；OAuth 与完整插件目录留待 M5。

## 验证边界

- gVisor 固定 release-20260921.0，校验官方 tarball SHA512；`overlay2=none`，宿主 overlay `redirect_dir=off,metacopy=off`。`network=host` 指沙箱独立的 OCI network namespace，里面只有 loopback，无外网接口。
- 当前 runsc sidecar 保留 create 的 stdout 管道会阻塞 containerd shim。`scripts/runsc-containerd.sh` 仅将 create 输出写到节点日志，失败时回传错误；未改变 run/exec 协议。
- 当前配置连接单个开发执行节点。故障测试验证执行丢失、租约失效与独立 upper 重建，没有验证物理多节点调度、真实云故障或生产容量/RPO 延迟指标。
- 归档当前整包在内存中读写，限制 8 GiB；尚无大工作区容量压测。开发节点保留已停沙箱的诊断文件；自动热盘清理不在本次验收内。
- Windows/Android 真机交互、商业上游、真实 S3/NAS 和多版本 Codex 兼容性未验证。

协议依据：[App Server](https://learn.chatgpt.com/docs/app-server)、[Codex 配置](https://learn.chatgpt.com/docs/config-file/config-reference)、[Responses](https://developers.openai.com/api/reference/resources/responses/methods/create)。
