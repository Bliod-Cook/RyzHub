# v1 剩余工作 · 阶段 2：基础设施

2026-10-11。实现 v1 剩余工作计划的阶段 2：secrets 信封加密、通用 jobs 表，以及 outbox。插件、OAuth、推送等后续阶段都依赖这些基础设施。

## 变更

### secrets 信封加密（`crates/server/src/vault.rs`，迁移 0019）

- **加密方式**：每个 secret 有自己的随机数据密钥（DEK），用 AES-256-GCM 加密内容；DEK 再由 vault 主密钥（KEK，即现有的 `llm.vault_key_env`）包裹，并记录 `kek_id`。
- **绑定关系**：密文绑定组织、secret ID 和调用方给出的用途字符串，被移到其他行或其他组织后无法解密。BYOK 凭据的用途是"凭据 ID + 所有者"，和原先的绑定相同。
- **新凭据**：写入 `secrets`，`model_credentials` 只存 `secret_id`。
- **旧凭据**：转换前仍按原格式解密，照常可用。
- **撤销**：撤销凭据会删除对应的 secret，密钥随之不可恢复。
- **`ryzhub-server migrate-secrets`**：逐个组织在该组织的 RLS 作用域内执行，迁移账号有没有 BYPASSRLS 都能用。它做两件事：
  - 把旧格式凭据转入 secrets，并清除已撤销凭据残留的旧密文；
  - 把所有不由当前 KEK 包裹的 DEK 重新包裹。
- **轮换**：轮换 vault 主密钥后运行一次 `migrate-secrets`，旧密钥即可移除。
- **权限**：worker 角色对 `secrets` 没有任何权限。

### jobs（`crates/server/src/jobs.rs`，迁移 0019）

- **入队**：`enqueue(tx, org, NewJob)` 在业务事务内写入，事务回滚则任务不存在。
- **去重**：同一 kind 和 `dedupe_key` 至多一个排队中的任务。重复入队会把执行时间提前、优先级取高，载荷保留第一次的。正在运行的任务不吸收新的入队，避免漏掉之后的触发。
- **领取与 fencing**：worker 用 `FOR UPDATE SKIP LOCKED` 领取，每个组织每轮最多 32 个，不让单个组织饿死其他组织。完成和失败都要校验 claim token，租约被接管的旧 worker 改不了任务状态。
- **失败与超时**：失败按 2^n 秒退避（上限 1 小时），用完 `max_attempts` 后进入 `dead`。处理器超过租约会被中止并按失败处理。
- **崩溃保护**：最后一次尝试的租约过期时，由 sweep 标记为 `dead`，避免拖垮进程的任务无限重试。
- **清理**：已完成的任务保留 7 天，死信保留 30 天。
- **未知 kind**：本版本没有处理器的 kind 保持排队，留给有处理器的版本。
- **至少一次投递**：处理器必须幂等。需要原子性的处理器可以在自己的事务里调用 `finish`。
- **启动**：处理器在 `jobs::handlers()` 中登记；有处理器时 Owner 角色才启动 worker。本阶段没有登记任何 kind，第一批处理器在推送、OAuth 刷新、插件拉取等阶段加入。

### outbox

没有单独建表。计划对 outbox 的要求是"与业务写入同一事务、提交后由 worker 投递"，jobs 的事务内入队已经满足；外部副作用（推送、webhook）作为一种 jobs kind，免得维护两套语义相同的队列。

## 验证

| 检查 | 结果 |
|---|---|
| `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -D warnings` | 通过 |
| `cargo test --workspace --locked` | 78 项通过，含 vault 的 AAD 绑定与篡改检测 |
| `./scripts/test-integration.sh` | 17/17，新增 `jobs_deduplicate_retry_fence_stale_claims_and_die_after_their_attempts`：去重与提前、回滚不入队、跨组织不可见、退避与死信、过期租约被接管后旧 token 无效、崩溃任务由 sweep 标记死信、未知 kind 保持排队 |
| `./scripts/test-m1.sh` | 15/15。新增 `legacy_credentials_move_to_secrets_and_key_rotation_rewraps_them`：旧格式凭据转换前可用、转换后可用、重复转换无操作，轮换后旧密钥移除仍可解密。原有测试改为检查 secrets 密文不含明文；撤销测试检查 secret 已删除 |
| m2 `actual_codex_native_turn_and_checkpoint_restore`（OpenAI BYOK） | 通过 |
| 迁移 0019 + `migrate-secrets` 在开发库副本上执行 | 唯一的（已撤销）凭据被清除旧密文；`secrets` 只授权给 `ryzhub_app`；副本已删除 |

## 部署

- 迁移 0019 只新增表、可空列和约束，不改写已有数据。
- 部署新版后，在带 vault 密钥的环境中运行一次 `ryzhub-server migrate-secrets`。不运行也不影响使用，旧凭据会继续按原格式解密。

## 本地部署记录

2026-10-11，dev server（M4 栈，数据库 `ryzhub_m3`）：

- 部署前确认没有进行中的 run，完整备份到 `var/backups/0019-migration-20261011-144140/`。
- 重启后端并应用迁移 0019；`/healthz` 正常，日志无 WARN 或 ERROR。
- 运行 `migrate-secrets`：没有需要转换的有效凭据，唯一一条已撤销凭据残留的旧密文已清除。
