# v1 剩余工作 · 阶段 7：身份、生命周期、推送、搜索与记忆

2026-10-12。实现 v1 剩余工作计划的阶段 7。用法见 README 的“用其他账号登录”“自动归档与保留期”“Android 通知”“项目文档”“附件与文件库”（分片上传）和“M4 搜索与记忆”（本地 embedding），接口变化见[线协议](wire-protocol-v1.md)的“阶段 7 的 REST 补充”。

## 变更

| 项 | 内容 |
|---|---|
| 1 OIDC + GitHub 登录 | 配置：`[[auth.oidc]]`（任意多个）、`[auth.github]`、`auth.link_verified_email`、`auth.return_urls`，顶层 `public_url` 用于回调地址；`/capabilities` 的 `auth_providers` 列出提供方。<br>流程：`/auth/oauth/{provider}/start` 带 PKCE、state、nonce；回调时：<br>• OIDC：按 JWKS 校验 ID token 的签名、issuer、audience、nonce、有效期（jsonwebtoken 11，rust_crypto）；<br>• GitHub：从 API 取已验证的主邮箱。<br>之后带一次性 `login_code` 回到客户端，`POST /auth/oauth/exchange` 换会话，code 只存哈希、只能用一次。<br>账号匹配：<br>• 已绑定的身份直接登录；<br>• 提供方确认的邮箱已有账号且允许时绑定；<br>• 其余新建用户与个人组织（复用注册逻辑）。<br>绑定已登录账号用 WS ticket 证明身份；`GET /me/identities`、`DELETE /me/identities/{provider}/{subject}`，最后一种登录方式不能解绑。迁移 0028：`user_identities.email/created_at`、`login_flows`、`login_codes`。<br>客户端：<br>• 登录页的提供方按钮；<br>• 原生端在系统浏览器登录并回到本机回环地址，网页端整页跳转后从地址取结果；<br>• 设置 › 账户 › 登录方式。 |
| 2 自动归档与存储状态 | 策略 `auto_archive_after_secs`、`archive_retention_secs` 生效，Owner 角色每 5 分钟扫描（`lifecycle.rs`）。<br>归档：<br>• 闲置对话（无运行、无排队命令、无工作区作业；置顶与临时对话除外）自动归档；<br>• 运行中或挂起的 Work 工作区排入归档作业，先提交检查点再停止沙箱；<br>• 归档与取消归档记录 `archived_at` 并发布到用户频道。<br>保留期过后（从归档或之后最后一次运行结束算起）：<br>• 先清除执行节点上留着的 Chat 工作区；<br>• 摘掉 head，检查点全部退役，按检查点回收规则删除对象：共用或被恢复钉住的对象等它空闲后再删；<br>• 所有检查点删完后标为 `deleted`。<br>全程在对话频道锁下进行，与命令接收、取消归档、工作区作业串行。<br>`workspace_heads.storage_state`：hot / archiving / cold / restoring / purging / deleted。归档、恢复作业进行中分别是 archiving、restoring，失败时回到原状态；新检查点提交时回到 hot。<br>被删除的 Work 工作区拒绝恢复与运行（`WORKSPACE_PURGED`），Chat 对话取消归档后从空的工作区开始。<br>审计：`conversation.auto_archive`、`workspace.purge`。 |
| 3 Android 推送 | 迁移 0029 新增 `push_devices`（绑定登录 `auth_refresh`，退出即失效）；接口 `POST /devices`、`DELETE /devices/{device}`。<br>触发：审批与提问（`runs::request_action`）、运行结束（`runs::finish`，completed / failed）时在同一事务写入 `push` 作业（jobs 即 outbox，按动作 / run 去重）。<br>投递（Owner 的作业队列，`push.rs`）：<br>• FCM HTTP v1：服务账号签 RS256 JWT 换访问令牌，令牌缓存到过期前 5 分钟；<br>• 高优先级可见通知，`data` 带 `org_id`、`conversation_id`、`kind`，Android 通知以对话 id 为 tag，重复投递只显示一条；<br>• 跳过有活跃 WS 的设备（WS 连接期间每次心跳刷新 `online_until`，Welcome 之前先标记在线）；<br>• 跳过投递前已被处理的审批；<br>• FCM 说令牌无效（404、`UNREGISTERED`、`INVALID_ARGUMENT`）时删除该设备，其他失败整体重试；<br>• 标题与正文按设备语言生成。<br>客户端：`lib/push.dart` 加 Android 工程里的 `PushService`（FirebaseMessagingService）与 `ryzhub/push` MethodChannel：请求通知权限（Android 13+）、取令牌并登记、令牌更新时重新登记、点击通知打开对话、前台时自己显示通知。退出登录前取消登记。 |
| 4 原生端会话与平台事件 | 原生端 `keepsSession = true`：`RefreshCookieClient` 替原生 HTTP 客户端扮演浏览器保存 refresh cookie：<br>• 按服务器存进 flutter_secure_storage（Android Keystore、Windows 凭据管理器）；<br>• 只发给 `/auth/` 接口；<br>• 服务端轮换时更新，退出时清除；<br>• 存储打不开时当作没有，只是下次需要登录。<br>启动时用它恢复会话，访问令牌过期时续期。<br>Windows：runner 处理 `WM_POWERBROADCAST`（`PBT_APMRESUMEAUTOMATIC/SUSPEND`），经 `ryzhub/power` 通知 Dart 立即重连。 |
| 5 记忆策略字段 | `Policy.memory_forbidden_categories`（各层取并集，组织只能增加）、`allow_memory_auto_extract`。<br>`memory_save` 写入前检查，命中返回 `MEMORY_CATEGORY_FORBIDDEN`；自动摘要在关闭时跳过，开启时按类别过滤，摘要提示列出额外的禁记类别。 |
| 6 本地 embedding | cargo feature `local-embedding`：fastembed 7.1 的 `Qwen3TextEmbedding`（candle，CPU，ort 以 load-dynamic 构建、不链接 ONNX Runtime）。<br>`search.embedding_provider = "local"`、`search.local_embedding_model`（默认 `Qwen/Qwen3-Embedding-0.6B`，从 `HF_HOME` 缓存加载）。<br>模型首次使用时加载，推理在阻塞线程上逐批进行，向量截到 1024 维并重新归一化。<br>所有嵌入调用改走 `embedding::embed`，按配置选择 provider；`SearchConfig::embeddings()` 决定是否生成向量。 |
| 7 项目文件文本抽取 | `POST /orgs/{org}/projects/{project}/files/upload?name=`（最大 32 MiB）：<br>• 按内容判断 PDF、DOCX、UTF-8 文本；<br>• PDF（pdf-extract）与 DOCX（zip + quick-xml，段落、制表、换行）在 `ryzhub-server extract-text` 子进程里抽取，限制内存 1 GiB、CPU 30 秒、超时 45 秒、并发 2；<br>• 文本整理后作为知识文件内容，超过 1 MiB 截断并标记。<br>原文件存进组织文件存储（与文件库共用内容地址）：<br>• `GET …/files/{file}/source` 以下载方式返回；<br>• 删除、替换知识文件或删除项目时释放；<br>• 文件库回收也会核对知识文件的引用。<br>迁移 0029：`project_files.source_sha256/source_type/source_size/truncated`。<br>客户端：项目对话框“上传文档”，列表标出来源，可下载原文件。 |
| 8 附件分片上传 | 迁移 0029：`uploads`、`upload_parts`。<br>接口：`POST /orgs/{org}/uploads`、`GET/DELETE …/{id}`、`PUT …/parts/{n}`、`POST …/parts/{n}/sent`、`POST …/complete`。分片 8 MiB（`/capabilities` 的 `attachments.part_bytes`）。<br>经服务端：每片边收边算 sha256，按摘要存进主存储的上传命名空间；重传替换旧片。<br>主存储是 S3 时直传：<br>• `BlobStore::start_direct/presign_part/finish_direct/abort_direct`，基于 object_store 0.14 的 `signed_url_opts` 与 `MultipartStore`；<br>• 客户端 PUT 到预签名地址后报告 ETag。<br>完成：按顺序读回整份文件，计算整份 sha256，之后与普通上传相同（类型判断、配额、渲染）；重复完成返回同一文件；尺寸或配额不符时结束上传。<br>回收：过期（24 小时）或放弃的上传由文件库回收任务清理。<br>客户端：大于分片大小的文件自动分片上传，每片重试 3 次；同一文件再次上传时从已收到的分片继续。 |

## 与计划不同之处

- **生命周期扫描不经 jobs 表**：与其他回收任务一样，是 Owner 角色的定时循环，在每个组织自己的事务里修改；结果一样，少一层排队。
- **没有 `workspaces` 表**：存储状态记在已有的 `workspace_heads`。Chat 工作区在归档时记为 cold，取消归档时回到 hot。
- **推送不用 firebase_messaging 插件**：它的 Windows 版会在构建时下载 Firebase C++ SDK。改为 Android 工程内的原生实现，Web 与 Windows 不受影响。
- **推送只通知对话创建者**：对话都是私有的；组织切换没有做（已定决策），点击通知时在当前组织里打开对话。
- **整份文件的 sha256 在完成时计算**：分片可以乱序、重传，所以收到每片时只算该片的摘要，完成时按顺序读回一遍得到整份摘要。S3 直传同样读回校验。
- **纯文本的项目文件不另存原文件**：内容就是原文。
- **本地 embedding 不写用量记录**：没有外部调用与费用，也不受策略模型列表限制。

## 未能在本机验证

- **Android**：本机没有 Android SDK。推送的 Kotlin 代码、Gradle 配置和安全存储只经过 Dart 静态分析与单元测试，需要在装有 SDK 的机器上带 `google-services.json` 构建，并在真机上验证（随阶段 8 的真机 PoC）。
- **Windows**：唤醒重连的 runner 代码与 flutter_secure_storage 的 Windows 插件未在 Windows 上构建。
- **FCM 实际投递**：服务端对 FCM 的调用用本地替身验证（JWT 签名、令牌交换、消息格式、无效令牌）；接真实 Firebase 项目需要用户提供服务账号。

## 实现中发现并修复的问题

- Work 归档作业原先不设置 `archived_at`，也不发布到用户频道：其他设备要等刷新才看到归档。现在归档、取消归档（包括恢复作业）都会设置并发布。
- WS 连接在发出 Welcome 之后才标记在线，客户端收到 Welcome 后立即发生的事件仍可能推送到这台设备；改为先标记再发 Welcome。

## 验证

| 检查 | 结果 |
|---|---|
| fmt、clippy `-D warnings`（默认与 `local-embedding`）、`cargo test --workspace --locked` | 通过，150 项单元测试。新增的有：<br>• 文档抽取：DOCX 段落、制表、换行与实体，PDF 文字，格式判断（zip 不当文本、截断的 UTF-8 字符），文本截断与整理；<br>• 分片边界；<br>• 推送文案与无效令牌判断；<br>• 向量截断后归一化；<br>• 记忆禁记类别（OIDC 的 JWKS 校验在 m1 中测）。 |
| 本地 embedding（release，`--ignored`） | Qwen3-Embedding-0.6B 实际下载并推理：1024 维；“How do I reset my password?”与“忘记密码怎么重置”的相似度高于与“Volcanoes erupt lava.”的相似度。 |
| `test-integration.sh` | 19/19 |
| `test-m1.sh` | 33/33。新增 6 项：<br>• OIDC 与 GitHub 登录：签名、nonce、audience、密钥不符时拒绝，PKCE 与 state，邮箱已验证时绑定、未验证时新建，绑定与解绑，最后一种登录方式不能解绑；<br>• 自动归档与保留期（Chat）：无策略时不动，置顶与新对话不归档，审计；取消归档重新计时；恢复钉住的对象保留到释放后才删除；状态接口；取消归档后从空工作区开始，时间线仍在；<br>• 项目文档：PDF、DOCX、文本，原文件原样下载、他人不能下载，不支持与读不出的文档被拒绝，与文件库共用的对象在最后一个引用删除后才删除，替换时释放旧对象；<br>• 分片上传：乱序、尺寸不符被拒、重传替换、未传完不能完成、他人看不到、续传、整份摘要、重复完成返回同一文件、分片删除、完成后关闭、过期回收、超限拒绝；<br>• 推送：FCM 替身验证服务账号 JWT；运行结束通知离线设备，无效令牌被删除，有活跃连接的设备不通知，连接关闭后不再在线，退出登录后不再通知。 |
| `test-m2.sh`（含 gVisor 往返） | 9/9 + 1/1。新增：<br>• Work 闲置后经归档作业提交检查点并停止，保留期后检查点全部删除，恢复返回 `WORKSPACE_PURGED`；<br>• S3 直传：预签名分片上传与合并（S3 替身用 botocore 校验查询签名与有效期），为其他分片签发的地址被拒绝，放弃的直传可以中止。 |
| `test-m3.sh`（runc 与 gVisor） | 8/8、8/8 |
| `test-m4.sh` | 1/1 |
| 客户端 `flutter analyze` / `flutter test` | analyze 无问题；124 通过（2 跳过）。新增：refresh cookie 的保存、轮换、按服务器隔离、只发 `/auth/`、退出清除；分片切块。 |

## 部署到 dev

- 备份：`var/backups/0029-migration-20261012-014750/ryzhub_m3.dump`，部署时没有进行中的 run。
- 后端重新构建并重启，迁移到 29，`/healthz` 正常；`/capabilities` 有 `attachments.part_bytes = 8388608`。dev 配置没有改：未配置外部登录提供方与 FCM 服务账号（`auth_providers` 为空、`push` 为 false），策略没有设置自动归档与保留期，已有对话不受影响。
- 网页端重新构建到 `var/m4/web`，登录页正常加载，没有页面错误。（core、bridge、proto 没有变化，不需要重建 wasm。）执行节点在跑 m1、m2 时已经热替换为当前构建。
- 冒烟（新注册的 `v1-phase7-smoke-*@example.test`）：项目上传 PDF 得到文字“Phase seven document”；9 MiB 文件按 2 片倒序上传后完成，sha256 与原文件一致。
