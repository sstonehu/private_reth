# Reth `v1.11.3.local` → `v2.2.0.local` 升级设计文档

> 版本：v1
> 状态：待开发
> 目标读者：执行升级的工程师 / 大模型 Agent（如 Claude Sonnet 4.6）
> 目标：在 reth `v2.2.0` 主线基础上重建 `v2.2.0.local` 分支，完整迁移现有 MEV 功能（Phase 1~5），行为与 `v1.11.3.local` 完全等价
> 工时预估：1~3 天

---

## 0. 文档使用说明

本文档为**执行级别**指南，可直接由 AI Agent 端到端执行。每个步骤遵循以下结构：

- **前置条件**：操作前必须满足的状态
- **操作**：完整可粘贴的命令 / 代码 diff
- **验收**：明确的判定标准（exit code、日志匹配、grep 输出等）
- **失败应对**：常见错误及修复方法

约定：
- 所有路径以 `/home/ecs-user/dt_workspace/private_reth/` 为根（下文简称 `$REPO`）
- v2.2.0 上游参考：`/home/ecs-user/dt_workspace/reth/`（只读，**禁止修改**）
- 当前 v1.11.3.local：`private_reth` 仓库 `v1.11.3.local` 分支
- 目标分支名：`v2.2.0.local`
- 涉及的环境变量、systemd 配置参考 `doc/reth_config_option.md`

### 配套文档

| 文档 | 角色 | 何时写 |
|---|---|---|
| 本文档（`Reth_upgrade_1.11.3_2.2.0.md`） | **设计 / 蓝图**：定义要做什么、API 矩阵、风险清单、执行步骤 | 升级前一次性写就，过程中不改动 |
| [`Reth_upgrade_1.11.3_2.2.0_impl.md`](./Reth_upgrade_1.11.3_2.2.0_impl.md) | **实施记录**：每一轮的 sonnet prompt、进度回写、代码 review、下一轮迭代 | 升级过程中持续更新 |

执行流程：
1. **每一轮迭代开始前**：把本文档（设计）作为唯一真理来源，再把 `_impl.md` 中对应轮次的 §x.1 prompt 交给 Sonnet
2. **Sonnet 执行过程中**：实时把进度写到 `_impl.md` 的 §x.2
3. **每一轮迭代结束后**：人类（或审计 Agent）填写 `_impl.md` 的 §x.3 review
4. **若 review 不通过**：在 `_impl.md` 中追加下一轮章节（§(x+1)），回到步骤 1

---

## 1. 背景

### 1.1 现状

`private_reth` 在 reth 上游 `v1.11.3` 基础上叠加了 MEV 路径模拟加速功能，涵盖 Phase 1~5：

| Phase | 内容 | 引入的核心组件 |
|---|---|---|
| 1 | EVM Worker Pool + `mev_*` 单笔接口 | `EpochManager`、`MevWorkerPool`、`MevWorker`、`mev_eth_call/mev_debug_traceCall/mev_trace_call` |
| 2 | `GlobalSharedCache` 全局读缓存 | `GlobalSharedCache`（moka）、`CachedStateProvider`、`WorkerL1Cache`、MissCoordinator（实际由 moka `try_get_with` 实现 singleflight） |
| 3 | 精确 Diff 缓存失效 | `on_epoch_change_diff` + `pre_fill_diff`，移除 `epoch_id` 命名空间，`MEV_DIFF_CACHE` 环境变量切换 |
| 4 | `mev_*` 接口过期请求快速拒绝（`-39001`） | `epoch_mismatch_error`、`block_gap`、`MEV_REJECT_STALE_CALL` 环境变量切换 |
| 5 | `mev_subscribe("newBlockRawIds")` + Block Impact 计算 | `BlockImpactRegistry`、5 个 `ImpactLogHandler`（BalancerV2/V3、UniswapV4、FluidDexLite、CoreSwap）、`MevNewBlock` 推送 |

完整设计见 `doc/mev-path-simulation-architecture-v3.md`，各阶段实施细节见 `doc/Reth_simulate_optimize_phase{1..5}.md`。

### 1.2 MEV 改动隔离度评估

**自上游 `v1.11.3` commit `c3f8e62e5` 起，本仓库共 28 个 commit 涉及以下文件**（已通过 `git diff --name-only c3f8e62e5 HEAD` 全量盘点）：

```
bin/reth/Cargo.toml                 ← +1 行
bin/reth/src/main.rs                ← +2 行
Cargo.toml                          ← +2 行
Cargo.lock                          ← 自动
crates/mev/Cargo.toml               ← 新增
crates/mev/src/api/{mod,server,types}.rs  ← 新增
crates/mev/src/cache/mod.rs                ← 新增
crates/mev/src/epoch.rs                    ← 新增
crates/mev/src/impact.rs                   ← 新增
crates/mev/src/lib.rs                      ← 新增
crates/mev/src/metrics.rs                  ← 新增
crates/mev/src/provider.rs                 ← 新增
crates/mev/src/worker/{mod,worker,cache}.rs ← 新增
doc/*.md / doc/*.json                       ← 新增（文档）
```

**关键结论：`crates/` 主线代码零侵入**，所有 MEV 逻辑封装在 `crates/mev/`（12 文件、2718 LOC）；外部仅 5 行 glue 代码（2 行 main.rs + 1 行 bin/Cargo.toml + 2 行 workspace Cargo.toml）。这是符合 `doc/mev-path-simulation-architecture-v3.md` §3.2 「最小侵入 Reth」原则的设计。

### 1.3 升级目标

1. 把 reth 主线版本从 `v1.11.3` 提升至 `v2.2.0`（4 major 跨度的 `revm`、major 跨度的 `alloy-*`、新版 `alloy-evm` / `revm-inspectors`）。
2. 完整迁移 MEV 功能，行为、接口、环境变量、Prometheus 指标、订阅协议**完全保持兼容**。
3. 输出新分支 `v2.2.0.local`，作为生产部署的新基线。

---

## 2. 依赖版本矩阵（FYI，无需手动改 `Cargo.toml` 的版本号）

| 依赖 | v1.11.3 | v2.2.0 | 性质 | 由谁提供 |
|---|---|---|---|---|
| `[workspace.package].version` | `1.11.3` | `2.2.0` | major | workspace 根 Cargo.toml |
| `rust-version` | `1.88` | `1.93` | bump | 同上 |
| `edition` | `2024` | `2024` | 同 | 同上 |
| `revm` | `34.0.0` | `38.0.0` | **4 major** | workspace deps |
| `revm-inspectors` | `0.34.2` | `0.39.0` | breaking (pre-1.0) | workspace deps |
| `alloy-evm` | `0.27.2` | `0.34.0` | breaking (pre-1.0) | workspace deps |
| `alloy-consensus` / `alloy-eips` / `alloy-network` / `alloy-rpc-types-eth` / `alloy-rpc-types-trace` | `1.6.3` | `2.0.4` | **major** | workspace deps |
| `alloy-primitives` | `1.5.6` | `1.5.6` | 同 | workspace deps |
| `jsonrpsee` | `0.26.0` | `0.26.0` | 同 | workspace deps |
| `moka` | `0.12` | `0.12` | 同 | `crates/mev/Cargo.toml` 直接引用 |
| `crossbeam-channel` | `0.5.13` | `0.5.13` | 同 | workspace deps |
| `tokio` | `1.44.2` | `1.51.1` | minor | workspace deps |

所有这些版本都由 v2.2.0 上游 workspace 已经配置好，**只要直接采用 v2.2.0 的 `Cargo.toml` 作为基底，无需手动管理依赖版本**。

---

## 3. API 兼容性矩阵（关键参考）

下表是 MEV crate 引用的所有外部 API 在 v2.2.0 中的存活情况，**已通过源码逐项对比验证**。开发时遇到编译错误**先查此表**，避免误以为某个 API 已被移除。

| MEV 引用的 API | v2.2.0 状态 | 验证位置 |
|---|---|---|
| `reth_evm::ConfigureEvm` trait + `evm_env(header)` / `evm_with_env(db, env)` / `evm_with_env_and_inspector(db, env, ins)` | ✅ 保留，签名一致 | `crates/evm/evm/src/lib.rs:178,268,294` |
| `reth_evm::EvmEnvFor<E>` / `TxEnvFor<E>` 类型别名 | ✅ 保留 | `crates/evm/evm/src/aliases.rs:33,44` |
| `reth_evm::env::BlockEnvironment`（via `pub use alloy_evm::*` 透传） | ✅ 保留 | `crates/evm/evm/src/lib.rs:52` |
| `evm_env.block_env.inner_mut()` 用法 | ✅ 保留 | v2.2.0 主线 `crates/rpc/rpc/src/{debug.rs:484,eth/sim_bundle.rs:309,eth/bundle.rs:100}` |
| `reth_evm_ethereum::EthEvmConfig` | ✅ 保留 | `crates/ethereum/evm/src/lib.rs:81` |
| `reth_node_builder::rpc::RpcContext` | ✅ 保留 | `crates/node/builder/src/rpc.rs:271` |
| `NodeBuilder::extend_rpc_modules(hook)` | ✅ 保留 | `crates/node/builder/src/builder/{mod.rs:631,states.rs:307}` |
| `ctx.registry.provider() / eth_api()` | ✅ 保留 | `crates/node/builder/src/rpc.rs:316,406` |
| `ctx.registry.debug_api() / trace_api()`（在 RpcRegistryInner 上） | ✅ 保留 | `crates/rpc/rpc-builder/src/lib.rs:795,821` |
| `reth_rpc::{DebugApi, TraceApi}::new(...)` 构造器 | ✅ 保留，签名一致（4-arg / 3-arg） | 同上 |
| `reth_rpc_eth_api::{RpcNodeCore, EthApiTypes}` | ✅ 保留 | `crates/rpc/rpc-eth-api/src/{node.rs:24,types.rs:18}` |
| `reth_rpc_eth_api::helpers::{Call, EthCall, EthTransactions, TraceExt}` | ✅ 保留 | `crates/rpc/rpc-eth-api/src/helpers/{mod.rs:48,call.rs:54,541,transaction.rs:62}` |
| `Call::call_gas_limit() / evm_memory_limit()` 默认方法 | ✅ 保留 | `crates/rpc/rpc-eth-api/src/helpers/call.rs:552,558` |
| `reth_rpc_convert::{RpcConvert, RpcTypes}` | ✅ 保留 | `crates/rpc/rpc-convert/src/{rpc.rs:10,transaction.rs:113}` |
| `eth_api.converter().tx_env(req, env)` | ✅ 保留 | RpcConvert trait 未改 |
| `reth_rpc_eth_types::error::api::FromRevert` + `EthApiError::from_revert(data)` | ✅ 保留 | `crates/rpc/rpc-eth-types/src/error/api.rs:157,164` |
| `reth_rpc_server_types::result::internal_rpc_err` | ✅ 保留 | 路径不变 |
| `reth_chain_state::{CanonStateNotification, CanonStateSubscriptions}` | ✅ 保留 | `crates/chain-state/src/notifications.rs:29,86` |
| `provider.subscribe_to_canonical_state()` + `notifications.recv().await` | ✅ 保留 | 同上 |
| `CanonStateNotification::Commit { new } / Reorg { new, .. }` 二元枚举 | ✅ 保留 | 同上 |
| `chain.execution_outcome() / execution_outcome_mut()` | ✅ 保留 | `crates/evm/execution-types/src/chain.rs:119,124` |
| `outcome.bundle_accounts_iter()` 返回 `(Address, &BundleAccount)` | ✅ 保留 | `crates/evm/execution-types/src/execution_outcome.rs:184` |
| `outcome.receipts_iter()` 返回 `impl Iterator<Item=&[T]>` | ✅ 保留 | 同上 `:259` |
| `reth_storage_api::{StateProviderBox, StateProviderFactory, BlockHashReader, BlockNumReader, BlockIdReader, HeaderProvider, AccountReader, ChangeSetReader, FullRpcProvider}` | ✅ 保留 | `crates/storage/storage-api/src/full.rs` 完全相同 |
| `reth_node_api::{FullNodeComponents, NodeTypes, BlockTy, HeaderTy, ReceiptTy, TxTy, NodePrimitives}` | ✅ 保留 | `crates/node/api/src/node.rs:66,83`；`crates/node/types/src/lib.rs:27` |
| `reth_network_api::{NetworkInfo, Peers}` | ✅ 保留 | 未改名 |
| `reth_ethereum_primitives::EthPrimitives` | ✅ 保留 | 未改 |
| `reth_chainspec::{EthereumHardforks, ChainInfo}` | ✅ 保留 | 未改 |
| `reth_revm::database::StateProviderDatabase` | ✅ 保留 | reth-revm `lib.rs` 完全一致 |
| `reth_revm::db::{State, states::StorageSlot, AccountStatus, BundleAccount}`（透过 `pub use revm::database as db`） | ✅ 保留 | revm v75 `crates/database/src/states/`；用法见 `crates/trie/common/src/hashed_state.rs:909` |
| `BundleAccount::new(original_info, present_info, storage, status)` 4 参数构造器 | ✅ 保留 | revm v75 `bundle_account.rs:36` |
| `State::builder().with_database(db).with_bundle_update().build()` | ✅ 保留 | `crates/storage/provider/src/writer/mod.rs:104` |
| `revm::primitives::hardfork::SpecId` | ✅ 保留 | `crates/evm/evm/src/lib.rs:31` |
| `revm::context_interface::result::ExecutionResult` | ✅ 保留 | `crates/rpc/rpc/src/otterscan.rs:24` |
| `ExecutionResult::Success { output, .. }` + `output.into_data()` | ✅ 保留 | `crates/rpc/rpc-eth-types/src/error/api.rs:126` |
| `ExecutionResult::Revert { output, .. }` | ✅ 保留 | 同上 :127 |
| **`ExecutionResult::Halt { reason, gas_used }`** | ⚠️ **字段改名** | **见 §5.1** |
| `revm::state::AccountInfo` 路径 | ✅ 保留 | `crates/trie/db/src/state.rs:366` |
| `revm::bytecode::Bytecode` + `Bytecode::new_raw(Bytes)` | ✅ 保留 | `crates/storage/rpc-provider/src/lib.rs:1051,1108` |
| `revm::{Database, DatabaseRef}` traits | ✅ 保留 | 路径不变 |
| `revm_inspectors::tracing::{DebugInspector, TracingInspector, TracingInspectorConfig}` | ✅ 保留 | `crates/rpc/rpc/src/{debug.rs:40,trace.rs:100}`；`crates/rpc/rpc-eth-api/src/helpers/trace.rs:22` |
| `TracingInspectorConfig::from_parity_config(trace_types)` | ✅ 保留 | 用法不变 |
| `DebugInspector::new(opts) -> Result<Self, _>` | ✅ 保留 | `crates/rpc/rpc/src/debug.rs:121,247` 仍 `.map_err()` |
| `inspector.get_result(None, &tx_env, &evm_env.block_env, &res, db)` 5 参数 | ✅ 保留 | `crates/rpc/rpc/src/debug.rs:312,374,472` |
| `TracingInspector::into_parity_builder().into_trace_results_with_state(&res, trace_types, db)` | ✅ 保留 | reth 内部 trace.rs 用法不变 |
| `alloy_evm::overrides::{apply_block_overrides, apply_state_overrides}` 函数签名 | ✅ 保留 | `crates/rpc/rpc-eth-api/src/helpers/call.rs:164,478,905,908` |
| `alloy_consensus::BlockHeader::{number, timestamp, base_fee_per_gas, next_block_base_fee}` | ✅ 保留 | alloy_consensus 2.0.4 `header.rs:252,726` |
| `alloy_eips::{BlockId, BlockNumHash, BlockNumberOrTag, eip1559::BaseFeeParams}` | ✅ 保留 | 未改 |
| `alloy_network::TransactionBuilder`（`request.set_gas_limit / take_nonce`） | ✅ 保留 | 路径不变 |
| `alloy_primitives::{Address, B256, U256, Bytes, address!, map::HashSet, map::HashMap, keccak256, Log}` | ✅ 保留 | alloy-primitives 1.5.6 同版本 |
| `alloy_rpc_types_eth::{TransactionRequest, BlockId, BlockOverrides, state::{StateOverride, EvmOverrides}}` | ✅ 保留 | 仍存在 |
| `alloy_rpc_types_trace::{geth::{GethDebugTracingCallOptions, GethTrace}, parity::{TraceResults, TraceType}, tracerequest::TraceCallRequest}` | ✅ 保留 | `crates/rpc/rpc/src/trace.rs:16` |
| `jsonrpsee::{Extensions, SubscriptionMessage, proc_macros::rpc, types::ErrorObject, core::RpcResult}` | ✅ 保留 | jsonrpsee 0.26 同版本 |
| `register_subscription("name", "notif", "unsub", handler)` | ✅ 保留 | `crates/rpc/ipc/src/server/mod.rs:992` 仍这么用 |
| `moka::sync::Cache::try_get_with(key, load) -> Result<V, Arc<E>>` | ✅ 保留 | moka 0.12 同版本 |

**结论：所有非 `Halt` 字段相关的 API 均无变化**。`Halt` 字段重命名是唯一**必然**的破坏点（§5.1）；其余如有失败均为编译期可发现的次要问题，可按 §7 应对清单逐项修复。

---

## 4. 升级范围（白名单 + 黑名单）

### 4.1 白名单：必须迁移的文件

**完整目录**（直接从 `v1.11.3.local` 复制）：

```
crates/mev/
├── Cargo.toml
└── src/
    ├── api/
    │   ├── mod.rs
    │   ├── server.rs
    │   └── types.rs
    ├── cache/mod.rs
    ├── epoch.rs
    ├── impact.rs
    ├── lib.rs
    ├── metrics.rs
    ├── provider.rs
    └── worker/
        ├── cache.rs
        ├── mod.rs
        └── worker.rs

doc/
├── mev-path-simulation-architecture-v3.md
├── mev-optimization-changelog.md
├── myReth_grafana.json
├── pricer_no_change_pool_shadow_validate.md
├── reth_config_option.md
├── Reth_simulate_optimize_phase1.md
├── Reth_simulate_optimize_phase2.md
├── Reth_simulate_optimize_phase3.md
├── Reth_simulate_optimize_phase4.md
├── Reth_simulate_optimize_phase5.md
└── Reth_upgrade_1.11.3_2.2.0.md  ← 本文档
```

**手工编辑 4 个文件**：

```
Cargo.toml                  ← 加 members 条目 + 加 reth-mev workspace dep（§6.1）
bin/reth/Cargo.toml         ← 加 reth-mev.workspace = true（§6.2）
bin/reth/src/main.rs        ← 加 use + extend_rpc_modules 调用（§6.3）
bin/reth/src/lib.rs         ← 加 use reth_mev as _; 抑制 unused_crate_dependencies lint（§6.4）
```

> **修订说明（2026-05-13）**：第一轮 review 发现 `bin/reth/src/lib.rs` 是 v2.2.0 上游 `unused_crate_dependencies` lint 配置下的**必要 glue 文件**（上游已有 5 处同 pattern：`use alloy_primitives as _;` / `use aquamarine as _;` 等）。原文档遗漏，本轮 fix。详见 `Reth_upgrade_1.11.3_2.2.0_impl.md` §1.3.3 I-001。

**必须修改的 MEV 源文件**（共 1 处）：

```
crates/mev/src/worker/worker.rs:174-180  ← ExecutionResult::Halt 字段适配（§5.1）
```

### 4.2 黑名单：禁止做的事

| 禁止行为 | 原因 |
|---|---|
| 用 `git rebase` 把 28 个 MEV commit 重新应用到 v2.2.0 | 上游在期间动过 `Cargo.toml` / members / 大量 crate 重命名，rebase 会产生大量无意义冲突。直接整目录拷贝更干净。 |
| 修改 `crates/mev/` 以外的 reth 主线 crate（除 §4.1 列出的 4 个 glue 文件外） | 违反 MEV 设计的「最小侵入」原则；后续 reth 升级会非常痛苦 |
| 把 MEV 改造为复用 `reth-execution-cache` / `PayloadExecutionCache` | `doc/mev-path-simulation-architecture-v3.md` §13 已论证不可行（写覆盖、is_available() 排他性、fixed_cache 不支持遍历） |
| 一边升级一边做新功能（Phase 6+） | 必须先保证 v2.2.0.local 行为完全等价于 v1.11.3.local，再叠新功能 |
| 修改 `crates/mev/Cargo.toml` 中的依赖版本号 | 所有依赖通过 `.workspace = true` 引用，版本由 v2.2.0 workspace 统一管理 |
| 升级时顺手改 `MEV_*` 环境变量默认值或语义 | 与 Go 侧 Bot 行为绑定，必须保持 1:1 兼容 |
| 在 `crates/mev/Cargo.toml` 中新增对 v2.2.0 新 crate（如 `reth-execution-cache`）的依赖 | 当前 MEV 不需要；引入会扩大升级表面积 |

---

## 5. 必须修改的代码（明确的 Breaking Change）

### 5.1 `ExecutionResult::Halt` 字段重命名（revm 34 → 38）

**根因**：`revm` v34 中 `ExecutionResult::Halt` 的字段叫 `gas_used: u64`，v38 中改为 `gas: Gas`（带 `tx_gas_used()` 等访问器）。

**v2.2.0 主线已采用同样模式**（参考实现）：

```126:132:/home/ecs-user/dt_workspace/reth/crates/rpc/rpc-eth-types/src/error/api.rs
ExecutionResult::Success { output, .. } => Ok(output.into_data()),
ExecutionResult::Revert { output, .. } => Err(Self::from_revert(output)),
ExecutionResult::Halt { reason, gas, .. } => {
    Err(Self::from_evm_halt(reason, gas.tx_gas_used()))
}
```

**MEV 当前代码**：

```174:181:/home/ecs-user/dt_workspace/private_reth/crates/mev/src/worker/worker.rs
match res.result {
    ExecutionResult::Success { output, .. } => Ok(WorkerOutput::Basic(output.into_data())),
    ExecutionResult::Revert { output, .. } => Err(WorkerError::Revert(output)),
    ExecutionResult::Halt { reason, gas_used } => {
        Err(WorkerError::Halt { reason: format!("{reason:?}"), gas_used })
    }
}
```

**升级后代码**（替换 `crates/mev/src/worker/worker.rs:174-180` 这 7 行）：

```rust
match res.result {
    ExecutionResult::Success { output, .. } => Ok(WorkerOutput::Basic(output.into_data())),
    ExecutionResult::Revert { output, .. } => Err(WorkerError::Revert(output)),
    ExecutionResult::Halt { reason, gas, .. } => {
        Err(WorkerError::Halt {
            reason: format!("{reason:?}"),
            gas_used: gas.tx_gas_used(),
        })
    }
}
```

**注意**：
- 改的是**解构模式 + 字段访问**，**不要**改 `WorkerError::Halt` 自身的字段名（仍叫 `gas_used`，对外保持兼容）。
- `..` 是必需的，因为 revm 38 的 `Halt` 还有其他字段（避免精确解构带来的未来 breaking）。

---

## 6. 必须新增的 glue 代码

### 6.1 `Cargo.toml`（workspace 根）

**操作**：v2.2.0 上游 `Cargo.toml` 已有完整的 members 列表和 workspace deps。基于 v2.2.0 上游内容做两处增量：

**位置 1**：在 `[workspace]` → `members = [ ... ]` 数组中**按字母顺序**插入 `"crates/mev/",`。  
建议插在 `"crates/metrics/",` 和 `"crates/net/banlist/",` 之间。

参考 v2.2.0 上游片段（节选）：

```toml
"crates/metrics/",
"crates/mev/",                   # ← 新增此行
"crates/net/banlist/",
```

**位置 2**：在 `[workspace.dependencies]` 中**按字母顺序**插入 `reth-mev = { path = "crates/mev" }`。  
建议插在 `reth-metrics` 之后、`reth-net-*` 之前。

参考 v2.2.0 上游片段（节选）：

```toml
reth-metrics = { path = "crates/metrics", default-features = false }
reth-mev = { path = "crates/mev" }       # ← 新增此行
reth-net-banlist = { path = "crates/net/banlist" }
```

> ⚠️ 若 v2.2.0 上游 `Cargo.toml` 中已存在同名 key（极不可能），以 v2.2.0 上游为准。本仓库的 `reth-mev` 是**新增的**，不会冲突。

### 6.2 `bin/reth/Cargo.toml`

**操作**：在 `[dependencies]` 段插入一行：

```toml
reth-mev.workspace = true
```

参考位置：紧跟 `reth-ethereum-cli.workspace = true` 之后即可（与 v2.2.0 上游 `[dependencies]` 不严格字母序的现状一致）。

> **修订说明（2026-05-13）**：原文同时建议"紧跟 reth-ethereum-cli 之后" + "保持字母顺序"，但二者在 v2.2.0 上游中冲突（`reth-chainspec` < `reth-mev` 但出现在 `reth-ethereum-cli` 之后）。本轮删除字母序建议，只保留位置建议。详见 `_impl.md` §1.3.3 I-004。

### 6.3 `bin/reth/src/main.rs`

**操作**：在 v2.2.0 上游版本基础上加 2 处：

**位置 1**：`use` 语句区域插入：

```rust
use reth_mev::install_mev_rpc;
```

建议插在 `use reth_ethereum_cli::chainspec::EthereumChainSpecParser;` 之后、`use reth_node_ethereum::EthereumNode;` 之前。

**位置 2**：`builder.node(EthereumNode::default())` 链式调用后插入 `.extend_rpc_modules(install_mev_rpc)`：

```rust
let handle = builder
    .node(EthereumNode::default())
    .extend_rpc_modules(install_mev_rpc)        // ← 新增此行
    .launch_with_debug_capabilities()
    .await?;
```

**完整 main.rs 期望内容**（v2.2.0.local 最终态）：

```rust
#![allow(missing_docs)]

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

#[cfg(all(feature = "jemalloc-prof", unix))]
#[unsafe(export_name = "_rjem_malloc_conf")]
static MALLOC_CONF: &[u8] = b"prof:true,prof_active:true,lg_prof_sample:19\0";

use clap::Parser;
use reth::cli::Cli;
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_mev::install_mev_rpc;
use reth_node_ethereum::EthereumNode;
use tracing::info;

fn main() {
    reth_cli_util::sigsegv_handler::install();

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    if let Err(err) = Cli::<EthereumChainSpecParser>::parse().run(async move |builder, _| {
        info!(target: "reth::cli", "Launching node");
        let handle = builder
            .node(EthereumNode::default())
            .extend_rpc_modules(install_mev_rpc)
            .launch_with_debug_capabilities()
            .await?;

        handle.wait_for_node_exit().await
    }) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
```

> ⚠️ 如果 v2.2.0 上游的 `main.rs` 有其他改动（如新的 trace flag 注入），保留上游版本，仅在合适位置加上述 2 行。

### 6.4 `bin/reth/src/lib.rs`

**根因**：v2.2.0 上游 `bin/reth` workspace 的 lint 配置启用了 `unused_crate_dependencies`，要求**每个直接依赖**至少在 `lib.rs` 或 `main.rs` 的编译单元中被引用。当 `reth-mev` 只在 `main.rs` 中使用（`install_mev_rpc`），library crate（`lib.rs`）不引用 `reth_mev`，会触发该 lint，编译时被 `-Dwarnings` 阻断。

v2.2.0 上游 `bin/reth/src/lib.rs` 已为同类场景准备了"占位引用"惯用法（grep `/home/ecs-user/dt_workspace/reth/bin/reth/src/lib.rs`：line 54 / 217 / 220 / 221 / 222 共 5 处）：

```rust
// Used in feature flags only (`asm-keccak`, `keccak-cache-global`)
use alloy_primitives as _;

// ... 后续区块
use aquamarine as _;
// ...
use clap as _;
use reth_cli_util as _;
use tracing as _;
```

**操作**：在 `bin/reth/src/lib.rs` 中 `use alloy_primitives as _;` 这一行之后插入 2 行：

```rust
// Used in feature flags only (`asm-keccak`, `keccak-cache-global`)
use alloy_primitives as _;
// Used in main.rs via install_mev_rpc
use reth_mev as _;
```

**注意**：
- **不是** `use reth_mev::install_mev_rpc as _;`（这会 import unused 名字），**必须**用 `use reth_mev as _;`（仅"声明该 crate 被本编译单元用到"）
- 位置不必紧跟 `alloy_primitives as _;`，放在 5 处既有 `as _;` 块中的任意位置都可以；建议放在 `alloy_primitives as _;` 之后保持类型相近（"为了别的编译单元的 feature/binary 使用"）

---

## 7. 编译失败应对清单（已知风险点）

除 §5.1 的必然破坏点外，其余 MEV 代码理论上应能直接编译通过。下表列出**抽样验证未发现破坏**但因依赖 major bump 可能在首次编译时暴露的位置。**遇到编译错误先查此表**。

| 编号 | 位置 | 风险 | 应对策略 |
|---|---|---|---|
| R1 | `crates/mev/src/worker/worker.rs:138` `tx_env.set_nonce(state_nonce)` | `TransactionEnv::set_nonce` 来自 `alloy_evm` trait | 若编译失败，到 `/home/ecs-user/dt_workspace/reth/crates/` 搜 `tx_env.set_nonce` 找上游写法；常见替代：`tx_env.nonce = state_nonce` 或 `tx_env.as_mut().set_nonce(state_nonce)` |
| R2 | `crates/mev/src/cache/mod.rs:14-35` 三个 `*_weigher` 函数 | 静默风险：`Bytecode` 内部布局可能改变，导致权重估算偏差 | 不会编译失败；上线后观察 `mev_global_cache_entry_count` 指标，若偏差大可微调三个 weigher 常数 |
| R3 | `crates/mev/src/worker/worker.rs:189-200` `DebugInspector::new(opts).map_err(...)` + `inspector.get_result(...).map_err(...)` | v2.2.0 上游已验证签名一致 | 直接对照 `/home/ecs-user/dt_workspace/reth/crates/rpc/rpc/src/debug.rs:121,312,374` 的 4 处用法 |
| R4 | `crates/mev/src/worker/worker.rs:212-224` `TracingInspector::into_parity_builder().into_trace_results_with_state(&res, trace_types, db)` | revm-inspectors 0.34→0.39 可能改链式 API | 对照 `/home/ecs-user/dt_workspace/reth/crates/rpc/rpc/src/trace.rs` 的同模式写法 |
| R5 | `crates/mev/src/epoch.rs:303` `provider.subscribe_to_canonical_state()` | `CanonStateSubscriptions` trait 签名一致 | 应无变化；若失败检查 `CanonStateSubscriptions::Primitives` 关联类型约束 |
| R6 | `crates/mev/src/cache/mod.rs:179-292` 单元测试中的 `BundleAccount::new(...)` / `StorageSlot { present_value, .. }` / `AccountStatus::default()` | revm v75 `bundle_account.rs:36` 已确认 4 参数构造器和字段名不变 | 测试若失败，去 `revm v75` 仓库 `crates/database/src/states/` 对应类型 |
| R7 | `crates/mev/src/lib.rs:50-85` 巨型 trait bound（`Node::Provider: FullRpcProvider<Header=..., Block=..., Receipt=..., Transaction=...> + AccountReader + ChangeSetReader + CanonStateSubscriptions<Primitives=EthPrimitives> + StateProviderFactory + ...`） | 约束写法在 v2.2.0 中保留，但 `FullRpcProvider` supertrait 链上 associated types 可能微调 | 按编译错误信息调整。常见情况：去掉冗余的 `Header=`、`Block=` 约束（已由 supertrait 自动保证） |
| R8 | `crates/mev/src/api/server.rs:8-22` 的 import 块 | 类型路径理论上都保留 | 全部已在 §3 矩阵中验证 |
| R9 | `crates/mev/src/impact.rs:18-21` `use alloy_primitives::{address, Address, B256}` + `use reth_chain_state::CanonStateNotification` | 路径不变 | 无需调整 |
| R10 | `crates/mev/src/api/server.rs:153,157` `block_overrides`、`state_overrides` 透传到 `WorkerTask` | `BlockOverrides` / `StateOverride` 类型未改 | 无需调整 |

### 7.1 通用故障恢复方法

每当 `cargo build -p reth-mev` 报错时，按以下流程处理：

1. **先看错误是否在 §3 矩阵或 §7 风险表中已经覆盖**。如果是，按指引修改。
2. **如果错误指向某个 API**：
   ```bash
   # 在 v2.2.0 上游中搜索该 API 的实际用法
   cd /home/ecs-user/dt_workspace/reth
   grep -rn "<api_name>" crates/ | head -20
   ```
   照着上游的最新用法改 MEV 代码。**禁止凭空编造 API**。
3. **如果错误是 trait bound 不满足**：
   ```bash
   # 找上游的 EthApiBuilder / 类似处的 trait bound 写法
   grep -rn "FullRpcProvider\|FullNodeComponents" /home/ecs-user/dt_workspace/reth/crates/rpc/rpc-builder/src/ | head -10
   ```
4. **如果错误来自 revm**：到 `https://github.com/bluealloy/revm/tree/v75/crates` 查 v75 源码（v75 对应 revm 38.x）。
5. **如果错误来自 alloy**：到 `https://github.com/alloy-rs/alloy/tree/v2.0.4/crates` 查 alloy 2.0.4 源码。

### 7.2 严禁的"修复"模式

- ❌ **删除编译报错的代码块**（包括"暂时注释"），这会导致功能丢失
- ❌ **修改 MEV API 对外签名**（如改 `mev_eth_call` 参数顺序、改 `MevNewBlock` JSON 字段名），这会破坏 Go 侧 Bot 兼容性
- ❌ **修改 Prometheus metric 名称**，会破坏 Grafana 看板
- ❌ **改 `MEV_*` 环境变量默认值**，会破坏生产配置
- ❌ **改 `crates/mev/Cargo.toml` 的依赖版本号**，必须保持 `.workspace = true`

---

## 8. 执行步骤（详细）

### Step 0：环境检查与基线打标（10 分钟）

#### 8.0.1 前置条件

- `$REPO = /home/ecs-user/dt_workspace/private_reth` 是 git working tree、无 uncommitted 改动
- `/home/ecs-user/dt_workspace/reth` 存在且 checkout 在 `v2.2.0` tag
- 已安装 Rust toolchain ≥ 1.93

#### 8.0.2 操作

```bash
cd /home/ecs-user/dt_workspace/private_reth

# 确认在 v1.11.3.local 分支且无脏改
git status
git rev-parse --abbrev-ref HEAD       # 应输出 v1.11.3.local

# 标记当前 HEAD 作为升级前基线（便于回退）
git tag pre-upgrade-v1.11.3.local

# 确认 v2.2.0 参考目录可读
ls /home/ecs-user/dt_workspace/reth/Cargo.toml

# 确认 rust 工具链
rustc --version                       # 应输出 ≥ 1.93
```

#### 8.0.3 验收

- `git status` 输出 `nothing to commit, working tree clean`
- `git tag` 列表中包含 `pre-upgrade-v1.11.3.local`
- `rustc --version` ≥ 1.93。**若不足**：执行 `rustup update stable && rustup default stable`，再确认 ≥ 1.93。

#### 8.0.4 失败应对

- 若有脏改动且不能丢弃：`git stash` 暂存，升级完后再决定恢复
- 若 rust 工具链 < 1.93 且 `rustup update` 后仍不达标：检查 `rust-toolchain.toml`，必要时改为 `channel = "1.93"` 或 `channel = "stable"`

---

### Step 1：创建 `v2.2.0.local` 分支并切换到 v2.2.0 主线（10 分钟）

#### 8.1.1 前置条件

- Step 0 通过
- 工作目录干净

#### 8.1.2 操作

**方案 A**（推荐，若 `private_reth` 已有 upstream remote）：

```bash
cd /home/ecs-user/dt_workspace/private_reth

# 检查是否有 paradigm 上游
git remote -v

# 若没有 upstream，添加
git remote add upstream https://github.com/paradigmxyz/reth.git

# 获取 v2.2.0 tag
git fetch upstream tag v2.2.0

# 基于 v2.2.0 创建新分支
git checkout -b v2.2.0.local v2.2.0
```

**方案 B**（备用，无网络情况下）：

```bash
cd /home/ecs-user/dt_workspace/private_reth

# 把本地 /home/ecs-user/dt_workspace/reth 作为本地远程
git remote add local-reth /home/ecs-user/dt_workspace/reth

git fetch local-reth

# /home/ecs-user/dt_workspace/reth 当前在 v2.2.0 tag（detached HEAD），其 HEAD commit 即 v2.2.0
RETH_V220_COMMIT=$(git -C /home/ecs-user/dt_workspace/reth rev-parse HEAD)
echo "v2.2.0 commit: $RETH_V220_COMMIT"

# 基于该 commit 建新分支
git checkout -b v2.2.0.local $RETH_V220_COMMIT
```

#### 8.1.3 验收

```bash
# 当前分支应为 v2.2.0.local
git rev-parse --abbrev-ref HEAD       # → v2.2.0.local

# Cargo.toml workspace 版本应为 2.2.0
head -5 Cargo.toml | grep '^version'  # → version = "2.2.0"

# 应无 crates/mev/ 目录
test ! -d crates/mev && echo "OK: no crates/mev"

# 工作树干净
git status                            # → nothing to commit
```

#### 8.1.4 失败应对

- 若 `git fetch` 失败：检查网络或回退到方案 B
- 若 v2.2.0 tag 不存在：在 `reth` 仓库执行 `git -C /home/ecs-user/dt_workspace/reth describe --tags` 看其 tag

---

### Step 2：迁移 MEV 代码与文档（20 分钟）

#### 8.2.1 前置条件

- Step 1 完成，当前在 `v2.2.0.local` 分支
- 旧分支 `v1.11.3.local` 或 tag `pre-upgrade-v1.11.3.local` 可访问

#### 8.2.2 操作

```bash
cd /home/ecs-user/dt_workspace/private_reth

# 把 crates/mev 整个目录从 pre-upgrade-v1.11.3.local 复制过来
git checkout pre-upgrade-v1.11.3.local -- crates/mev/

# 把 doc/ 下 MEV 相关文档复制过来
git checkout pre-upgrade-v1.11.3.local -- \
    doc/mev-path-simulation-architecture-v3.md \
    doc/mev-optimization-changelog.md \
    doc/myReth_grafana.json \
    doc/pricer_no_change_pool_shadow_validate.md \
    doc/reth_config_option.md \
    doc/Reth_simulate_optimize_phase1.md \
    doc/Reth_simulate_optimize_phase2.md \
    doc/Reth_simulate_optimize_phase3.md \
    doc/Reth_simulate_optimize_phase4.md \
    doc/Reth_simulate_optimize_phase5.md \
    doc/Reth_upgrade_1.11.3_2.2.0.md
```

#### 8.2.3 验收

```bash
# crates/mev/src 下应有 12 个 rs 文件
find crates/mev/src -name '*.rs' | wc -l    # → 12

# 行数应为 2718（容忍 ±5）
find crates/mev/src -name '*.rs' -exec wc -l {} + | tail -1    # → ~2718 total

# Cargo.toml 此时仍是 v2.2.0 原版（未加 reth-mev）
! grep -q "^reth-mev " Cargo.toml && echo "OK: Cargo.toml not yet patched"

# git 状态应显示新增的 crates/mev/ 和 doc/
git status --short | head -20
```

#### 8.2.4 失败应对

- 若 `git checkout` 报 `pathspec did not match`：确认 tag `pre-upgrade-v1.11.3.local` 存在，或改用 `v1.11.3.local` 分支名

---

### Step 3：补 glue 代码（Cargo.toml × 2 + main.rs × 1）（15 分钟）

#### 8.3.1 前置条件

- Step 2 完成

#### 8.3.2 操作

按 §6.1、§6.2、§6.3、§6.4 的说明手工编辑 4 个文件。下面给出具体的搜索锚点：

**文件 1：`Cargo.toml`（workspace 根）**

在 `members =` 列表中找到 `"crates/metrics/",` 这一行，在其后插入 `"crates/mev/",`：

```diff
     "crates/metrics/",
+    "crates/mev/",
     "crates/net/banlist/",
```

在 `[workspace.dependencies]` 中找到 `reth-metrics = ...` 这一行，在其后插入 `reth-mev` 引用：

```diff
 reth-metrics = { path = "crates/metrics", default-features = false }
+reth-mev = { path = "crates/mev" }
 reth-net-banlist = { path = "crates/net/banlist" }
```

> 若 v2.2.0 上游 `Cargo.toml` 的字母序与上述锚点不完全一致（如 `crates/metrics/` 名字微调），按字母序插入到 `crates/mev/` 应在的位置即可。

**文件 2：`bin/reth/Cargo.toml`**

在 `[dependencies]` 段找到 `reth-ethereum-cli.workspace = true` 这一行，紧跟其后插入：

```diff
 reth-ethereum-cli.workspace = true
+reth-mev.workspace = true
```

或保险起见放在段末（位置不影响功能，仅影响代码风格）。

**文件 3：`bin/reth/src/main.rs`**

按 §6.3 的"完整期望内容"覆盖 main.rs。或仅做 2 处增量：

```diff
 use reth::cli::Cli;
 use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
+use reth_mev::install_mev_rpc;
 use reth_node_ethereum::EthereumNode;
```

```diff
         let handle = builder
             .node(EthereumNode::default())
+            .extend_rpc_modules(install_mev_rpc)
             .launch_with_debug_capabilities()
             .await?;
```

**文件 4：`bin/reth/src/lib.rs`**

按 §6.4 的说明，在 `use alloy_primitives as _;` 这一行之后插入 2 行：

```diff
 // Used in feature flags only (`asm-keccak`, `keccak-cache-global`)
 use alloy_primitives as _;
+// Used in main.rs via install_mev_rpc
+use reth_mev as _;
```

#### 8.3.3 验收

```bash
# 6 处改动 grep 验证
grep -c '"crates/mev/"' Cargo.toml                          # → ≥ 1
grep -c '^reth-mev = ' Cargo.toml                            # → 1
grep -c '^reth-mev.workspace = true' bin/reth/Cargo.toml     # → 1
grep -c 'use reth_mev::install_mev_rpc' bin/reth/src/main.rs # → 1
grep -c '.extend_rpc_modules(install_mev_rpc)' bin/reth/src/main.rs # → 1
grep -c '^use reth_mev as _;' bin/reth/src/lib.rs            # → 1
```

全部 6 行 grep 都应输出 ≥ 1。**任何一项为 0 都视为失败，必须修复**。

#### 8.3.4 失败应对

- 若 Cargo.toml 文件结构与预期不一致：阅读 v2.2.0 上游 Cargo.toml，找到 members 列表起止位置和 dependencies 起止位置，再次插入

---

### Step 4：应用必然破坏点（`ExecutionResult::Halt`）（5 分钟）

#### 8.4.1 前置条件

- Step 3 完成

#### 8.4.2 操作

按 §5.1 的说明，用以下精确替换：

**搜索**（`crates/mev/src/worker/worker.rs` 中唯一一处）：

```rust
        ExecutionResult::Halt { reason, gas_used } => {
            Err(WorkerError::Halt { reason: format!("{reason:?}"), gas_used })
        }
```

**替换为**：

```rust
        ExecutionResult::Halt { reason, gas, .. } => {
            Err(WorkerError::Halt {
                reason: format!("{reason:?}"),
                gas_used: gas.tx_gas_used(),
            })
        }
```

#### 8.4.3 验收

```bash
# 旧的 gas_used 解构应消失
! grep -n "ExecutionResult::Halt { reason, gas_used }" crates/mev/src/worker/worker.rs && echo "OK: old pattern removed"

# 新的 gas .. 解构应存在
grep -n "ExecutionResult::Halt { reason, gas, \.\. }" crates/mev/src/worker/worker.rs
# → 应输出 1 行匹配
```

#### 8.4.4 失败应对

- 若 grep 没找到旧模式：用 `grep -n "ExecutionResult::Halt" crates/mev/src/worker/worker.rs` 看实际写法，按其格式调整搜索

---

### Step 5：首次编译 `reth-mev`（30 分钟~2 小时）

#### 8.5.1 前置条件

- Step 4 完成

#### 8.5.2 操作

```bash
cd /home/ecs-user/dt_workspace/private_reth

# 单独编译 mev crate，便于聚焦错误
cargo build -p reth-mev 2>&1 | tee /tmp/mev_build_step5.log
```

#### 8.5.3 验收

- **理想情况**：`cargo build -p reth-mev` exit code = 0，无 error。
- **可接受的 warning**：unused_variables、deprecated（pre-1.0 库的弃用提示）、unused_imports —— 这些不阻塞升级。
- **必须为 0 的指标**：error 数 = 0。

```bash
# 验证
grep -c "^error" /tmp/mev_build_step5.log    # → 0
grep -c "^warning" /tmp/mev_build_step5.log  # 允许 > 0
```

#### 8.5.4 失败应对

按 §7 风险表逐条排查。具体策略：

**A. 错误聚类**：
```bash
# 按错误类型分组统计
grep "^error\[" /tmp/mev_build_step5.log | sort -u
```

**B. 对每类错误，按 §7.1 通用故障恢复方法处理**：
1. 在 §3 API 矩阵中查找该 API → 按矩阵指引修
2. 在 §7 风险表中查找位置 → 按表中应对策略修
3. 在 v2.2.0 上游 `/home/ecs-user/dt_workspace/reth/crates/` 搜该 API 的实际用法 → 对齐写法

**C. 修改后重新编译并迭代**：
```bash
cargo build -p reth-mev 2>&1 | tee /tmp/mev_build_step5_iter2.log
diff <(grep "^error" /tmp/mev_build_step5.log) <(grep "^error" /tmp/mev_build_step5_iter2.log)
```

**D. 严禁行为**：见 §7.2。任何"删功能省事"的修法都不能采用。

**E. 紧急升级情况**：若某个修法实在卡死，**先记录到 `/tmp/mev_known_blockers.md`**（包含错误信息、当前修法尝试、卡点描述），跳过该错误继续看下一类（用 `cargo build -p reth-mev --message-format=short 2>&1 | head -50`），最后回头攻坚。

---

### Step 6：编译全工程并跑单元测试（30 分钟~1 小时）

#### 8.6.1 前置条件

- Step 5 通过（`cargo build -p reth-mev` exit 0）

#### 8.6.2 操作

```bash
cd /home/ecs-user/dt_workspace/private_reth

# 编译整个 workspace（含 bin/reth）
cargo build --release 2>&1 | tee /tmp/full_build_step6.log

# 跑 mev 单元测试
cargo test -p reth-mev 2>&1 | tee /tmp/mev_test_step6.log

# 二进制检查
ls -lh target/release/reth
./target/release/reth --version
```

#### 8.6.3 验收

- `cargo build --release` exit 0，`target/release/reth` 存在且可执行
- `cargo test -p reth-mev` exit 0，所有测试通过

**MEV 单元测试期望全部通过**（共 8 个）：

| 测试位置 | 测试名 | 验证内容 |
|---|---|---|
| `cache/mod.rs::tests` | `test_singleflight_concurrent_miss` | 8 并发 miss → DB call ≤ 2 |
| `cache/mod.rs::tests` | `test_diff_invalidation` | bundle diff 触发精确失效与预填充 |
| `cache/mod.rs::tests` | `test_negative_cache` | None 值被缓存，第二次不打 DB |
| `cache/mod.rs::tests` | `test_l2_hit_backfills_l1` | GlobalSharedCache 命中后写回 L1 |
| `cache/mod.rs::tests` | `test_three_layer_l1_priority` | L1 命中时 L2 不被调用 |
| `cache/mod.rs::tests` | `test_storage_singleflight` | 同上对 storage |
| `cache/mod.rs::tests` | `test_bytecode_l2_dedup` | bytecode 同 hash 不重复读 |
| `worker/cache.rs::tests` | `test_bytecodes_retained_after_reset` | epoch 切换后 bytecode 缓存保留 |

```bash
# 验证
grep "^test result: ok" /tmp/mev_test_step6.log | grep -c "passed; 0 failed"  # → 至少 1
grep "test result: FAILED" /tmp/mev_test_step6.log  # → 应为空
```

#### 8.6.4 失败应对

- **`cargo build --release` 失败**：
  - 若错误在 `crates/mev/`：回 Step 5 流程修复
  - 若错误在 `bin/reth/`：通常是 §6 glue 代码没补对，回 Step 3 检查
  - 若错误在其他 crate：v2.2.0 上游应自身能编译，**不应该**出现这种情况；优先怀疑 toolchain 问题
- **`cargo test -p reth-mev` 失败**：
  - 测试代码使用了 `BundleAccount::new`、`StorageSlot { present_value, .. }`、`AccountStatus::default()`
  - 若失败，按 §7 R6 应对：参考 revm v75 源码 `crates/database/src/states/{bundle_account.rs,storage_slot.rs}`
  - **不要降低测试覆盖度**（如把失败的 assert 改弱），必须真正修好

---

### Step 7：本地启动节点冒烟测试（30 分钟）

#### 8.7.1 前置条件

- Step 6 通过
- 有可用的 reth datadir（小型，几个 GB 即可）

#### 8.7.2 操作

```bash
cd /home/ecs-user/dt_workspace/private_reth

# 启动节点（请按实际 datadir / config 调整）
export MEV_WORKER_COUNT=8
export MEV_GLOBAL_CACHE_MAX_MB=1024
export MEV_STATS_INTERVAL_SECS=10
export MEV_DIFF_CACHE=1
export MEV_REJECT_STALE_CALL=1
export RUST_LOG="info,reth::mev=debug"

./target/release/reth node \
    --datadir /path/to/your/local/datadir \
    --http \
    --http.api eth,debug,trace,net,web3,mev \
    --authrpc.port 8551 \
    --http.port 8545 \
    2>&1 | tee /tmp/reth_smoketest.log &

RETH_PID=$!
sleep 30
```

**期望日志**（10 秒内出现）：

```text
INFO reth::cli: Launching node
INFO reth::mev: mev RPC module installed (Phase 4 stale-call handling configured) num_workers=8 cache_max_mb=1024 call_gas_cap=... stats_interval_secs=10 reject_stale_call=true
INFO reth::mev::epoch: EpochManager starting diff_cache_enabled=true
INFO reth::mev::impact: registered impact handler addr=... handler="BalancerV2Handler"
INFO reth::mev::impact: registered impact handler addr=... handler="BalancerV3Handler"
INFO reth::mev::impact: registered impact handler addr=... handler="UniswapV4Handler"
INFO reth::mev::impact: registered impact handler addr=... handler="FluidDexLiteHandler"
INFO reth::mev::impact: registered impact handler addr=... handler="CoreSwapHandler(coreSwap)"
INFO reth::mev::impact: registered impact handler addr=... handler="CoreSwapHandler(coreSwapV3)"
```

**简单 RPC 测试**：

```bash
# 测试 mev_eth_call（应工作，对 latest 块）
curl -s http://localhost:8545 \
    -H 'Content-Type: application/json' \
    -d '{
        "jsonrpc": "2.0",
        "id": 1,
        "method": "mev_eth_call",
        "params": [
            {"to": "0x0000000000000000000000000000000000000000", "data": "0x"},
            "latest"
        ]
    }' | jq .

# 测试 mev_subscribe 需要 ws 连接（本步可跳过，进 Step 8 联调时再测）
```

```bash
# 停止节点
kill $RETH_PID
wait $RETH_PID 2>/dev/null
```

#### 8.7.3 验收

- 节点启动后**至少存活 30 秒**不 panic、不 OOM
- 日志中出现上面列出的 7 条 `INFO reth::mev*` 启动行（impact handler 6 个）
- `mev_eth_call` 返回 `{"jsonrpc":"2.0","id":1,"result":"0x"}` 或类似（具体 result 取决于目标地址，但**必须不报 `Method not found`**）

```bash
# 验证日志
grep -q "mev RPC module installed" /tmp/reth_smoketest.log && echo "OK: mev module installed"
grep -q "EpochManager starting" /tmp/reth_smoketest.log && echo "OK: EpochManager started"
grep -c "registered impact handler" /tmp/reth_smoketest.log    # → 6
grep -q "Method not found" /tmp/reth_smoketest.log && echo "FAIL: mev_eth_call not registered" || echo "OK: no Method not found error"
```

#### 8.7.4 失败应对

- **`Method not found`**：`install_mev_rpc` 未被调用 → 回 Step 3 检查 `main.rs`
- **`mev RPC module installed` 日志缺失**：trait bound 在运行时无法满足（编译期通过但实例化失败）→ 检查 `EthereumNode::default()` 是否实现了 `install_mev_rpc` 要求的全部 trait
- **Panic on `compute_changed_raw_ids`**：可能是 `bundle_accounts_iter` 或 `receipts_iter` 行为微变 → 按 §7.1 流程，去 v2.2.0 上游对照
- **OOM**：把 `MEV_GLOBAL_CACHE_MAX_MB` 调小（如 256），重启再试

---

### Step 8：链上行为对照（半天~1 天，可与 Go 侧并行）

#### 8.8.1 前置条件

- Step 7 通过

#### 8.8.2 操作

1. **启动 v2.2.0.local 节点和 v1.11.3.local 节点指向同一个 mainnet 历史区块**（或同步到链头后取同一 latest 块）。
2. **用 Go 侧 Bot（dural_trade 或独立测试脚本）对两个节点打打同样的请求**：
   - 100~1000 笔 `mev_eth_call` 各种 path
   - 50~100 笔 `mev_debug_traceCall` 含 `withAccessList: true`（若已实现 Phase 5 子项）
   - 20~50 笔 `mev_trace_call`
   - 1 个 `mev_subscribe("newBlockRawIds")` 长连接，记录至少 5 个区块的推送

3. **对照项**：

| 项 | 期望 |
|---|---|
| `mev_eth_call` 同输入的 `result` bytes | 100% 一致 |
| `mev_debug_traceCall` 的 `gasUsed` + `output` | 100% 一致；`accessList` 字段顺序可能不同但内容一致 |
| `mev_trace_call` 的 `output` + `gasUsed` | 100% 一致 |
| `mev_subscribe` 推送的 `block_number` / `block_hash` / `timestamp` | 100% 一致 |
| `mev_subscribe` 推送的 `changed_raw_ids` 集合 | **完全一致**（集合相等，元素顺序可不同） |
| `-39001 EpochMismatch` 错误的 `data.gap` 计算 | 100% 一致 |

#### 8.8.3 验收

差异比例 = 0%。若有任何项不一致，立即停止部署、进入 Step 8.8.4。

#### 8.8.4 失败应对

- **`mev_eth_call` 结果不同**：revm 38 的 EVM 执行结果应与 34 一致（mainnet hardfork 都已包含），若不同高度怀疑是 `prepare_evm_env` 中的 cfg flag 设置失效或 `prepare_call_env` 等效写法变化 → 对照 v2.2.0 上游 `crates/rpc/rpc-eth-api/src/helpers/call.rs:895-910`
- **`changed_raw_ids` 不一致**：检查 `receipts_iter()` / `bundle_accounts_iter()` 在 reorg 时的行为，特别是 `CanonStateNotification::Reorg`
- **`-39001.data.gap` 计算不同**：检查 `block_gap()` 是否被新增的 `BlockNumberOrTag` 变体影响

---

### Step 9：性能基线对齐（1 天）

#### 8.9.1 前置条件

- Step 8 通过

#### 8.9.2 操作

按 `doc/mev-path-simulation-architecture-v3.md` §8.4 跑基线测试：

- 固定硬件、固定区块、固定路径集（1 万条）
- 连续 10 个区块，每区块 5 个批次
- 同时跑 v1.11.3.local 和 v2.2.0.local

**记录指标**（来自 Prometheus）：

```text
mev_e2e_duration_seconds{method="eth_call"}    P50 / P95 / P99
mev_e2e_duration_seconds{method="debug_traceCall"}    同上
mev_e2e_duration_seconds{method="trace_call"}    同上
mev_worker_l1_hits_total / mev_worker_l1_misses_total    （命中率推算）
mev_global_cache_db_reads_total                          （miss 计数）
mev_epoch_warmup_duration_seconds                        （切块预热耗时）
mev_epoch_block_delay_seconds                            （网络+引擎+预热总延迟）
```

#### 8.9.3 验收

- **不允许的退化**：稳态 P99 退化 > 10%、首批 P99 退化 > 20%、`mev_global_cache_db_reads_total` 增长 > 30%
- **可接受的微变**：±5% 以内的波动（revm 38 自身性能优化可能带来正向收益）

#### 8.9.4 失败应对

- **稳态 P99 大幅恶化**：先检查 `mev_worker_l1_hits_total` 命中率有没有掉，若掉了说明缓存层级有 bug → 重点排查 `CachedStateProvider::basic/storage/code_by_hash`
- **首批 P99 大幅恶化**：检查 `mev_epoch_warmup_duration_seconds` 和 `mev_epoch_diff_*_total`，若 diff 量异常（如远高于 v1.11.3 平均值）则 `bundle_accounts_iter` 语义可能变化
- **`mev_global_cache_db_reads_total` 飙升**：moka 缓存可能因 weigher 估算偏差被过早驱逐 → 调整 §7 R2 的 weigher 常数

---

### Step 10：提交、推送、合并（1 小时）

#### 8.10.1 前置条件

- Step 0~9 全部通过

#### 8.10.2 操作

```bash
cd /home/ecs-user/dt_workspace/private_reth

# 查看待提交的全部变更
git status
git diff --stat

# 应仅包含：
#   - crates/mev/**                    （新增 12 文件）
#   - doc/**                            （新增 11+ 文件）
#   - bin/reth/Cargo.toml               （+1 行）
#   - bin/reth/src/main.rs              （+5 行：1 行 use + 4 行链式 builder）
#   - bin/reth/src/lib.rs               （+2 行：use reth_mev as _; 加注释）
#   - Cargo.toml                        （+2 行）
#   - Cargo.lock                        （自动）

# 分成 3~5 个 atomic commit
git add crates/mev/Cargo.toml crates/mev/src/
git commit -m "feat(mev): port reth-mev crate from v1.11.3.local"

git add bin/reth/Cargo.toml bin/reth/src/main.rs bin/reth/src/lib.rs Cargo.toml Cargo.lock
git commit -m "feat(mev): register reth-mev RPC module in NodeBuilder"

git add crates/mev/src/worker/worker.rs
git commit -m "fix(mev): adapt ExecutionResult::Halt to revm 38 field rename"

git add doc/
git commit -m "docs(mev): port MEV design docs and upgrade plan"

# 推送
git push -u origin v2.2.0.local
```

> **注意**：上面把 `worker.rs` 改动和 crate port 分开 commit 是为了便于将来 review。如果嫌麻烦也可一个 commit 完成，但**禁止**把 §5.1 的修改隐藏在 `feat(mev): port reth-mev crate ...` 这种笼统的 commit 信息里。
>
> **subject 弹性规则（2026-05-13 增补）**：上面给的 4 条 commit subject 是**推荐措辞**，不是**字面必须**。实际可在保留 type（`feat` / `fix` / `docs`）与语义的前提下微调，但要满足两个条件：
> 1. **body 必须列全实际包含的修复点**（参考第一轮 C3 实际处理方式：subject 改为 `fix(mev): adapt worker.rs to revm 38 and alloy-evm 0.34 API changes`，body 同时列出 §5.1 Halt fix 与 §7 R1 TransactionEnvMut fix，与设计文档锚点交叉引用）
> 2. **不允许**把多个 type 混合到同一 subject（如 `feat-fix-docs(mev): ...`）
>
> 详见 `_impl.md` §1.3.3 I-002。

#### 8.10.3 验收

- `git log v2.2.0..v2.2.0.local --oneline` 输出 4 个 atomic commit
- `git push` 成功，远端有 `v2.2.0.local` 分支

---

## 9. 验收清单（最终交付前必检）

| 项 | 验证方法 | 通过标准 |
|---|---|---|
| 9.1 分支已建立 | `git rev-parse --abbrev-ref HEAD` | `v2.2.0.local` |
| 9.2 workspace 版本 | `head -5 Cargo.toml \| grep version` | `version = "2.2.0"` |
| 9.3 MEV crate 已就位 | `find crates/mev/src -name '*.rs' \| wc -l` | `12` |
| 9.4 glue 代码完整 | 6 个 grep（§8.3.3） | 全部 ≥ 1 |
| 9.5 Halt 修复完成 | `grep -n "Halt { reason, gas, .. }" crates/mev/src/worker/worker.rs` | 1 行匹配 |
| 9.6 整体编译通过 | `cargo build --release` | exit 0 |
| 9.7 MEV 测试通过 | `cargo test -p reth-mev` | 8 测试全 pass |
| 9.8 节点能启动 | Step 7 冒烟 | 30 秒不 panic + 6 个 impact handler 注册日志 |
| 9.9 链上行为等价 | Step 8 对照 | 0 不一致 |
| 9.10 性能无退化 | Step 9 基线 | 稳态 P99 退化 ≤ 10% / 首批 P99 退化 ≤ 20% |
| 9.11 环境变量兼容 | 启动日志中所有 `MEV_*` 变量取值与 v1.11.3.local 默认一致 | `reject_stale_call=true`、`diff_cache_enabled=true`、`num_workers=...`、`cache_max_mb=...`、`stats_interval_secs=...` |
| 9.12 RPC 接口兼容 | `curl mev_eth_call / mev_debug_traceCall / mev_trace_call` 返回结构与 v1.11.3.local 完全一致 | 字段集合相等、字段类型一致、错误码 `-39001` 一致 |
| 9.13 订阅接口兼容 | `wscat -c ws://.../ -x mev_subscribe newBlockRawIds`，收到的 JSON 与 v1.11.3.local 一致 | `block_number / block_hash / timestamp / base_fee_per_gas / next_base_fee_per_gas / changed_raw_ids` 字段全部存在且语义相同 |
| 9.14 Prometheus metric 兼容 | `curl http://localhost:9001/` 抓取 metric 列表 | 包含全部 `mev_*` metric 名（参考 `doc/myReth_grafana.json` 中的查询） |

---

## 10. 回退策略

如果在任何 Step 失败且无法在 1 天内修复，按以下策略回退：

### 10.1 软回退（保留 `v2.2.0.local` 分支继续调试）

```bash
# 生产仍跑 v1.11.3.local
systemctl restart reth  # 在 systemd 配置中指向 v1.11.3.local 二进制
```

### 10.2 硬回退（删除 `v2.2.0.local` 分支）

```bash
cd /home/ecs-user/dt_workspace/private_reth

git checkout v1.11.3.local
git branch -D v2.2.0.local       # 删除本地分支
git push origin --delete v2.2.0.local   # 删除远端分支（若已推送）
git tag -d pre-upgrade-v1.11.3.local    # 删除基线 tag（可选）
```

### 10.3 灰度回退（运行时切换）

即使在 v2.2.0.local 节点上，可通过环境变量退化到 Phase 3 / Phase 2 行为以排查问题：

```ini
# /etc/systemd/system/reth.service.d/override.conf
[Service]
Environment=MEV_REJECT_STALE_CALL=0     # 回退到 Phase 3 降级行为
Environment=MEV_DIFF_CACHE=0             # 回退到 Phase 2 全量失效
```

`systemctl daemon-reload && systemctl restart reth`。

---

## 11. 附录

### 11.1 MEV crate 文件清单（参考）

| 文件 | 行数 | 角色 |
|---|---|---|
| `crates/mev/Cargo.toml` | ~60 | crate 元信息与依赖（全部 `.workspace = true` + `moka = "0.12"`） |
| `crates/mev/src/lib.rs` | 244 | `install_mev_rpc` 注册入口、`mev_subscribe` 订阅闭包 |
| `crates/mev/src/api/mod.rs` | 46 | `#[rpc(server, namespace = "mev")] trait MevApi`（3 个方法） |
| `crates/mev/src/api/server.rs` | 365 | `MevApiServer` 实现：epoch 路由、worker 派发、Phase 4 stale 处理、`epoch_mismatch_error` |
| `crates/mev/src/api/types.rs` | 40 | `CallKind` / `MevNewBlock`（订阅 payload） |
| `crates/mev/src/cache/mod.rs` | 410 | `GlobalSharedCache`（moka 3 sub-cache）+ Phase 3 `on_epoch_change_diff` + `pre_fill_diff` + 8 个单元测试 |
| `crates/mev/src/epoch.rs` | 512 | `EpochContext` / `EpochManager`：canon 订阅、`MEV_DEBUG_FIXED_EPOCH`、`block_gap` / `active_block_number`、impact 广播 |
| `crates/mev/src/impact.rs` | 272 | 5 个 `ImpactLogHandler` + `BlockImpactRegistry::mainnet_defaults` + `compute_changed_raw_ids` |
| `crates/mev/src/metrics.rs` | 285 | `MethodCounters` / `MevCounters` / 13 个 metric 名常量 / `spawn_periodic_reporter` |
| `crates/mev/src/provider.rs` | 146 | `CachedStateProvider` 实现 `revm::Database`：L1 → GlobalSharedCache → DB；`ProviderStats` 任务级聚合 |
| `crates/mev/src/worker/mod.rs` | 117 | `MevWorkerPool` / `WorkerTask` / `WorkerOutput` / `WorkerError` |
| `crates/mev/src/worker/worker.rs` | 227 | `MevWorker` 主循环 + 3 种执行模式（Basic/DebugTrace/ParityTrace） |
| `crates/mev/src/worker/cache.rs` | 54 | `WorkerL1Cache`（bytecodes 跨 epoch 保留） |

合计 **2718 行**（按 Step 2 验收）。

### 11.2 MEV 环境变量速查

| 变量 | 默认 | 作用 |
|---|---|---|
| `MEV_WORKER_COUNT` | `40` | EVM Worker Pool 线程数 |
| `MEV_GLOBAL_CACHE_MAX_MB` | `16384` | GlobalSharedCache 总内存上限（MB） |
| `MEV_STATS_INTERVAL_SECS` | `30` | 周期 stat 日志输出间隔（秒） |
| `MEV_DEBUG_FIXED_EPOCH` | unset | 调试用：冻结 EpochManager 在指定块高度 |
| `MEV_DIFF_CACHE` | `1` | Phase 3 精确 Diff 缓存失效开关 |
| `MEV_REJECT_STALE_CALL` | `1` | Phase 4 stale 请求快速拒绝开关 |

完整说明见 `doc/mev-path-simulation-architecture-v3.md` §14。

### 11.3 MEV Prometheus metric 速查

**Counter**：

```text
mev_requests_total{method}
mev_worker_path_total{method}
mev_degraded_path_total{method}                      # Phase 4 后基本为 0
mev_errors_total{method, kind}
mev_degraded_gap_total{method, reason}               # 已废弃，由下条替代
mev_epoch_mismatch_total{method, reason}             # Phase 4 新指标
mev_worker_tasks_total{worker_id}
mev_worker_epoch_switches_total
mev_pool_queue_full_total
mev_worker_l1_hits_total{kind}                       # account / storage / bytecode
mev_worker_l1_misses_total{kind}
mev_global_cache_db_reads_total{kind}
```

**Gauge**：

```text
mev_degraded_pct{method}
mev_global_cache_entry_count{sub}                    # account / storage / bytecode
mev_pool_queue_depth
mev_epoch_warmup_duration_latest_seconds
mev_epoch_diff_accounts_total
mev_epoch_diff_storage_slots_total
mev_epoch_current_block_number
mev_net_engine_delay_latest_seconds
mev_epoch_manager_delay_latest_seconds
mev_epoch_block_delay_latest_seconds
```

**Histogram**：

```text
mev_e2e_duration_seconds{method}
mev_epoch_warmup_duration_seconds
mev_net_engine_delay_seconds
mev_epoch_manager_delay_seconds
mev_epoch_block_delay_seconds
```

### 11.4 关键依赖参考链接

- revm v38 / tag `v75`：<https://github.com/bluealloy/revm/tree/v75>
- alloy v2.0.4：<https://github.com/alloy-rs/alloy/tree/v2.0.4>
- alloy-evm 0.34：<https://github.com/alloy-rs/evm/tree/v0.34.0>
- revm-inspectors 0.39：<https://github.com/paradigmxyz/revm-inspectors/tree/v0.39.0>
- reth v2.2.0：<https://github.com/paradigmxyz/reth/tree/v2.2.0>

### 11.5 升级前后关键 diff 摘要（Cheatsheet）

| 维度 | v1.11.3.local | v2.2.0.local |
|---|---|---|
| reth 主线 | 1.11.3 | 2.2.0 |
| revm | 34 | 38 |
| alloy（非 primitives） | 1.6.3 | 2.0.4 |
| alloy-evm | 0.27.2 | 0.34.0 |
| revm-inspectors | 0.34.2 | 0.39.0 |
| rust toolchain | 1.88 | 1.93 |
| MEV crate 文件数 | 12 | 12（无新增/删除） |
| MEV crate 行数 | 2718 | 2718 + 3（worker.rs：§5.1 Halt + §7 R1 TransactionEnvMut） |
| 外部 glue 文件数 | 3（Cargo.toml × 2 + main.rs） | **4**（Cargo.toml × 2 + main.rs + lib.rs，本轮 §6.4 补充） |
| 外部 glue 代码行数 | 5 行 | ~9 行（main.rs +5 / lib.rs +2 / 2 个 Cargo.toml +2） |
| MEV 接口签名 | `mev_eth_call / mev_debug_traceCall / mev_trace_call / mev_subscribe` | **完全相同** |
| MEV 错误码 | `-39001 EpochMismatch` | **完全相同** |
| MEV 环境变量 | 6 个 | **完全相同** |
| Prometheus metric 名 | 见 §11.3 | **完全相同** |

---

## 12. 后续动作（升级完成后做）

### 12.1 短期（升级完成 1 周内）

- 更新 `dt_eks_scripts/.vscode/erigon/reth.service` 中的 reth 版本字符串（若有硬编码）
- 更新内部 Wiki / 部署文档中提及 `v1.11.3.local` 的位置
- 通知 Go 侧（dural_trade / go-service）一线开发者升级已完成，可继续在 v2.2.0.local 上做后续 MEV 改动

### 12.2 中期（升级完成 1 月内）

- 验证生产环境跑 7 天，确认无 OOM / panic / 性能退化
- 把 `Reth_simulate_optimize_phase{1..5}.md` 中对 reth 内部代码路径的引用刷新到 v2.2.0 行号
- 评估是否合适开 Phase 6（如 `mev_callBatch` / `mev_callBundleBatch`，见架构文档 §7.3 待规划项）

### 12.3 长期

- 跟随 reth 上游持续合并安全补丁（小版本）
- 当 alloy → 3.0 / revm → 40+ 等下一波 major 升级到来时，本文档可作为参考模板
