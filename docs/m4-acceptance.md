# M4 搜索与记忆

2026-10-03。实现架构 §9、§10 和 §13 的搜索/记忆路径。数据库使用 PG18、PGroonga 4.0.4、pgvector 0.8.2；本地向量验收调用协议 fixture，不调用收费模型。

## 升级与配置

先结束运行中的任务，更新数据库镜像并应用 0013，再启动新版 Server 和客户端：

```sh
docker compose build postgres
docker compose up -d --wait
# 使用现有迁移账号环境
ryzhub-server migrate
```

Compose 复用原有 PostgreSQL volume，不删除数据库。既有库必须先安装 PGroonga 和 pgvector >=0.8.2，迁移不会偷偷退回 LIKE 或生成伪向量。更新 Guest 和 Claude adapter；已有 Work 沙箱应保存检查点后停止/恢复，让新版 guest 生效。

搜索默认启用。只配置全文检索也可运行；`vector`/`hybrid` 模式需真实的 OpenAI-compatible embedding 服务，固定 1024 维：

```toml
[search]
enabled = true
embedding_url = "https://api.openai.com"
embedding_model = "text-embedding-3-small"
# 留空时按原 OpenAI BYOK/组织/平台凭据策略选择。
embedding_key_env = ""
memory_budget_chars = 12000
max_attempts = 6
```

离线部署可将 `embedding_url` 指向支持上述协议和维度的本地 embedding 服务，并显式设置该服务自己的 `embedding_key_env`；个人 OpenAI 密钥仅自动发送到 `https://api.openai.com`。向量携带模型名和维度，检索只使用当前模型。更换模型/新增向量配置后，以迁移账号运行 `ryzhub-server reindex`，通过同一队列低优先级回填；回填期间全文检索可用，当前模型的向量结果随回填增加。embedding 与摘要调用沿用 Gateway 的凭据/策略模块和 usage 账本（后台记录的 `run_id` 可为空），不是沙箱凭据。

## 实现与边界

- PGroonga 中文全文、`halfvec(1024)` HNSW 和 SQL RRF。REST 支持全文/向量/混合模式、项目/对话/时间过滤，返回高亮片段、消息 seq 和来源链接。Flutter 搜索可定位消息，引用链接可打开原对话。
- 源更新和索引任务同事务提交。worker 在事务外分块/调用 embedding，短事务锁源、验证版本和领取 token，再原子发布文档与向量。旧版本、租约过期和重新领取的任务不能回写。embedding 故障时先发布当前全文且不保留旧向量，再退避重试向量；达到上限进入 `dead`，全文继续可用，`reindex` 可重新投递。hybrid 查询的 embedding 故障时使用全文结果。
- 每一路检索在 SQL 中关联实时源数据，核对组织、创建者、源版本、删除状态及项目边界。索引里的归属不能授予访问权限。删除/转移所有者/移动项目立即影响结果；删除后的索引墓碑和已完成任务按 7 天保留期回收。
- 对话标题、用户/助手消息和项目文本文件进入索引；工具只索引名称，不索引原始输出或完整参数；思考内容、side query、临时对话不索引。项目文件工具复用索引；编辑后的旧 run 或索引积压时，以 PGroonga 查询该 run 的冻结文本，同时关联实时项目/文件/创建者权限；已删除文件不会通过冻结文本回显。
- 记忆按 `(org,user)` 私有归属。`default` 使用用户级记忆；`project_only` 只使用本项目记忆与历史，默认 agent 不检索其他 project_only 项目的历史。项目记忆不会随对话移动。
- 显式写入直接生效，更新采用新条目并失效旧条目；变更日志支持撤销，撤销核对当前版本，拒绝覆盖后续修改。删除/失效条目与撤销事件保留 30 天后清除正文；组织保留期到期和项目回收一并清理事件副本。saved 快照、引用时间线和变更提示只保留 ID/元数据；引用展开时按当前权限解析，已删除的来源也从 recent 快照中移除。工具重试幂等，重复请求键不能用于不同参数。保存前脱敏常见 key/token/password/Bearer 形态。
- 记忆快照在对话首次使用时生成并冻结；新保存的记忆通过当前工具结果呈现，下一个对话才进入快照。每次渲染仍检查开关与条目的当前有效性，删除条目不继续注入。注入块声明“资料，不是指令”，预算按优先级截断；本轮引用的记忆/历史在时间线上可展开查看。
- 空闲 30 秒后保存对话标题与用户要点摘要。有 LLM 时使用无工具的非流式摘要调用；模拟/无模型模式保留脱敏后的用户摘录。准备期间不持有数据库事务；提交时重新核对成员关系、开关、对话版本、消息水位和空闲状态。自动摘要排除敏感类别；不做合成画像。
- “启用记忆”是总开关，同时控制保存记忆与历史引用；“引用历史对话”单独控制历史，两个偏好分别存储。关闭历史后不再注入或供 agent 检索，摘要在 30 天后清理；组织关闭记忆时立即冻结使用，30 天后清理保存记忆和摘要。
- 临时对话不读写记忆、不进入索引或摘要、不显示在历史列表；fork 保留临时属性，30 天后回收。删除对话先即时撤销访问，Owner 后台清理文本/事件/沙箱/检查点；“同时删除相关记忆”可选，默认保留显式记忆。
- Work 生成 `.platform/memory.md`，权限 0444，由 AGENTS.md/CLAUDE.md 导入；Claude SDK 同时追加该文件到系统上下文。沙箱内 root 可以修改文件权限，文件只是数据注入，权限边界由服务端 MCP 的实时认证保证。平台内置记忆工具不需要审批；远程 MCP 工具继续强制审批。

## API 与验证

全部请求使用已有用户 Bearer 和实时组织成员关系检查：

| 请求 | 内容 |
|---|---|
| `GET /orgs/{org}/search?q=&mode=&project=&conversation=&from=&to=` | `mode=fulltext/vector/hybrid`；时间为 RFC3339 |
| `GET /orgs/{org}/memories?project=&q=&sort=` | 当前作用域记忆、个人开关与组织限制；按 `priority` 或 `time` 排序 |
| `POST /orgs/{org}/memories` | `content`、可选 `replace/priority/project_id/source_conversation_id/request_key` |
| `DELETE /orgs/{org}/memories/{id}` | 删除并返回撤销事件 ID |
| `POST /orgs/{org}/memory-events/{id}/undo` | 版本校验后撤销 |
| `PATCH /orgs/{org}/memories/preferences` | `enabled` 与 `reference_history` |
| `POST /orgs/{org}/conversations` | 新增 `temporary: true` |
| `DELETE /orgs/{org}/conversations/{conv}?delete_memories=true` | 可选删除相关记忆 |

Chat 和 Work MCP 提供 `conversation_search`、`memory_search`、`memory_save`、`memory_forget`。配置了 embedding 时搜索工具默认 hybrid，未配置时使用全文；`memory_save` 的描述要求只记住用户明确要求保存的事实。四个内置工具名保留，不允许远程工具重名。

```sh
cargo fmt --all --check
SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings
SQLX_OFFLINE=true cargo test --workspace --locked
./scripts/test-integration.sh
./scripts/test-m4.sh
cd client
flutter analyze
flutter test
```

M4 集成测试使用独立临时数据库和实际 PGroonga/pgvector，并验证真实 worker 账号及后台循环；embedding 上游是可控协议 fixture。覆盖中文高亮、RRF、来源定位、跨租户与实时所有者核对、删除时索引滞后、旧版本与重新领取任务 fencing、临时对话、project_only、冻结快照、密钥脱敏、幂等、编辑/删除撤销、开关、关联记忆删除及内容回收。Flutter 增加异步搜索乱序、独立开关和写入提示撤销测试。收费上游及嵌入质量不属于本地 fixture 的验证结果。

## 实际验证记录

2026-10-03，在独立 PG18/PGroonga/pgvector 数据库和执行节点验证，未升级或重启原开发数据库与原执行节点。

| 检查 | 结果 |
|---|---|
| Rust workspace 单元/文档测试 | 通过（ignored 集成测试另行运行） |
| 一致性集成测试 | 13/13 通过 |
| M3 Codex/Claude SDK、项目和命令回归 | 7/7 通过 |
| M4 实际数据库、worker 与 MCP 验收 | 通过 |
| Flutter analyze / test | 无问题；25 项测试通过 |
| Claude adapter TypeScript 构建 | 通过 |
| PostgreSQL 搜索镜像构建 | 通过 |
| fmt、Clippy、diff 检查 | 通过 |

[Claude Code 审查与修复记录](m4-code-review.md) 保存实际初审报告、各问题处理与复查结果。

## 本地部署迁移记录

2026-10-04，原预览服务使用的 `ryzhub_m3` 已从 `0012` 升级到 `0013`。PostgreSQL 复用 `ryzhub-m0_postgres` 数据卷，镜像更新为 `ryzhub-postgres:18-search`，扩展版本为 PGroonga 4.0.4、pgvector 0.8.2。后端沿用原配置重启，`/healthz` 与 `/capabilities` 均报告 M4；原 NATS 和执行节点持续运行。

迁移前后用户、组织、成员、对话、消息、任务、项目、项目文件、沙箱、工作区和检查点的记录数完全一致，其中保留 64 个对话、456 条消息、125 个任务、52 个沙箱、82 个检查点。534 个历史索引任务全部完成，没有 `dead` 任务。使用临时登录会话验证中文全文搜索返回原有来源、高亮和定位链接，记忆列表返回 HTTP 200，未登录搜索返回 HTTP 401；验证会话随后删除。验证时后端日志没有 WARN 或 ERROR。

备份位于 `var/backups/m4-migration-20261004-111032/`，包含全库 SQL 压缩备份、`ryzhub_m3` 自定义格式备份、旧服务二进制和配置，以及 `migration.json`、`verification.json` 验证记录；备份目录和文件已限制访问权限。当前配置未启用 embedding 服务，全文索引可用，向量索引尚未生成。
