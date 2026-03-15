# Phase 1 详细设计：EVM Worker Pool + mev_* 单笔接口

> 依据：[架构总文档](./mev-path-simulation-architecture-v3.md) Phase 1 章节  
> 目标读者：实现 AI（Codex）及工程师  
> 范围：仅 Phase 1，不涉及 GlobalSharedCache / ExEx / 批量 IPC 接口

---

## 1. 总体目标与边界

### Phase 1 解决的问题

原生 `eth_call` 对每条请求独立创建 `revm::State` 和 `StateProvider`，重建开销在 1 万条/批场景下累积显著。Phase 1 通过**常驻 EVM Worker Pool** 复用初始化开销，每个 worker 在同一 epoch 内的多次请求之间共享 Worker-L1 本地读缓存（clean reads 的 HashMap），减少对底层 `StateProvider` 的重复调用。

### Phase 1 明确不做的事

- **无 GlobalSharedCache**（跨 worker 的共享缓存，Phase 2 引入）
- **无 MissCoordinator / singleflight**（Phase 2 引入）
- **无 SafeUnchangedSet / 跨 epoch 继承**（Phase 2 引入）
- **无自定义 IPC 批量接口**（Phase 3 引入）

### 验收标准

1. `mev_eth_call` / `mev_debug_traceCall` / `mev_trace_call` 三个接口功能正确，结果与原生接口完全一致。
2. 同一 block 内连续批次，`T_batch_core` 相比原生接口有可观测下降（以 Worker-L1 命中率指标验证）。
3. 旧块请求降级走对应原生接口，结果语义正确。
4. 新 epoch 切换后，旧 epoch Worker-L1 数据被清空，不存在状态污染。

---

## 2. 代码目录结构与集成点全览

### 2.1 新建文件（全部在 `crates/mev/`）

```
crates/mev/                          ← 新建 crate（Phase 1 全部新代码在此）
├── Cargo.toml
└── src/
    ├── lib.rs                       # pub 导出 + install_mev_rpc 注册入口
    ├── epoch.rs                     # EpochContext, EpochManager
    ├── worker/
    │   ├── mod.rs                   # MevWorkerPool, WorkerTask, WorkerResult, PoolError
    │   ├── worker.rs                # MevWorker（单个 worker OS 线程主循环）
    │   └── cache.rs                 # WorkerL1Cache（Worker-L1 本地缓存）
    ├── provider.rs                  # WorkerStateProvider（revm::Database 适配）
    └── api/
        ├── mod.rs                   # MevApiServer trait（jsonrpsee #[rpc] 宏生成）
        ├── server.rs                # MevApiServer 实现体 + 路由降级逻辑
        └── types.rs                 # CallKind 等内部辅助类型
```

> Phase 2 新增：`src/cache.rs`（GlobalSharedCache）、`src/coordinator.rs`（MissCoordinator）；Phase 1 不创建这两个文件。

### 2.1.1 `crates/mev/Cargo.toml`（完整依赖清单）

```toml
[package]
name    = "reth-mev"
version = "0.1.0"
edition = "2021"

[dependencies]
# ── Reth 核心 ──────────────────────────────────────────────────────────
reth-evm              = { workspace = true }
reth-ethereum-evm     = { workspace = true }   # EthEvmConfig
reth-revm             = { workspace = true }   # StateProviderDatabase, State
reth-errors           = { workspace = true }
reth-storage-api      = { workspace = true }   # StateProviderFactory, StateProviderBox
reth-chain-state      = { workspace = true }   # CanonStateNotification, CanonStateSubscriptions
reth-rpc-eth-api      = { workspace = true }   # EthApiTypes, helpers::Call trait
reth-rpc-eth-types    = { workspace = true }   # GethTrace, GethDebugTracingCallOptions, EthApiError
reth-node-builder     = { workspace = true }   # RpcContext, FullNodeComponents
reth-node-api         = { workspace = true }   # FullNodeComponents

# ── alloy ──────────────────────────────────────────────────────────────
alloy-primitives       = { workspace = true }
alloy-eips             = { workspace = true }
alloy-rpc-types-eth    = { workspace = true }   # TransactionRequest, BlockId, StateOverride, BlockOverrides
alloy-rpc-types-trace  = { workspace = true }   # parity::TraceResults, parity::TraceType, geth::GethDebugTracingCallOptions
alloy-evm              = { workspace = true }   # overrides::apply_block_overrides, apply_state_overrides

# ── revm ───────────────────────────────────────────────────────────────
revm                   = { workspace = true }   # Database, DatabaseCommit, ExecutionResult, Inspector
revm-inspectors        = { workspace = true }   # TracingInspector, DebugInspector

# ── jsonrpsee ──────────────────────────────────────────────────────────
jsonrpsee = { workspace = true, features = ["server", "macros"] }

# ── 并发工具 ───────────────────────────────────────────────────────────
crossbeam-channel = { workspace = true }
tokio             = { workspace = true, features = ["sync"] }

# ── 工具库 ─────────────────────────────────────────────────────────────
async-trait  = { workspace = true }
thiserror    = { workspace = true }
tracing      = { workspace = true }
metrics      = { workspace = true }
```

> **注**：所有依赖均通过 `workspace = true` 继承版本，无需在此单独指定版本号。若 workspace 中某个 crate 尚未声明为 workspace dependency，需在根 `Cargo.toml` 的 `[workspace.dependencies]` 中补充。

---

### 2.2 需要修改的现有文件

Phase 1 对 Reth 原有代码的修改极少，分三类：

#### ① 工作区注册（必须）

**文件：`Cargo.toml`（workspace 根）**

在 `[workspace] members` 数组中追加一行，并在 `[workspace.dependencies]` 中声明路径依赖：

```toml
# [workspace] members 数组中追加：
"crates/mev/",

# [workspace.dependencies] 中追加：
reth-mev = { path = "crates/mev" }
```

---

#### ② 节点二进制入口（必须）

**文件：`bin/reth/Cargo.toml`**

在 `[dependencies]` 中追加：

```toml
reth-mev.workspace = true
```

**文件：`bin/reth/src/main.rs`**

当前代码：
```rust
let handle = builder
    .node(EthereumNode::default())
    .launch_with_debug_capabilities()
    .await?;
```

修改为（追加 `.extend_rpc_modules` 调用，不改其他逻辑）：
```rust
use reth_mev::install_mev_rpc;

let handle = builder
    .node(EthereumNode::default())
    .extend_rpc_modules(install_mev_rpc)   // ← 追加此行
    .launch_with_debug_capabilities()
    .await?;
```

`install_mev_rpc` 是 `crates/mev/src/lib.rs` 导出的闭包工厂函数（见 §9）。

---

#### ③ Reth 原有 crate 的可见性修改（按需，可能为零）

以下仅在通过已有 public 接口无法访问时才需要修改，**只加 `pub`，不改任何逻辑**：

| 文件 | 字段/方法 | 修改内容 | 必要性 |
|---|---|---|---|
| `crates/ethereum/evm/src/lib.rs` | `EthEvmConfig` 内部字段 | 加 `pub` | 仅当需直接访问 `spec_id` 推导逻辑时；通常通过 `ConfigureEvm::spec_id_at_head` trait 方法访问即可，**大概率不需要修改** |
| `crates/rpc/rpc/src/eth/core.rs` | `EthApiInner` 某方法 | 加 `pub` | 如果降级 fallback 需要绕过 trait，**大概率不需要修改**（通过 `EthApiServer` trait 调用即可） |

> **原则**：先通过已有 public trait 实现，只有编译器报 `private` 错误时才考虑加 `pub`。

---

### 2.3 集成关系图

```
[现有文件，不改逻辑]                    [新建文件]
─────────────────────────────────────   ──────────────────────────────────────
bin/reth/src/main.rs                    crates/mev/src/lib.rs
  └─ 追加 .extend_rpc_modules()  ──────►  install_mev_rpc(ctx)
                                              ├─► EpochManager::spawn(provider)
                                              │     └─ 订阅 CanonStateNotifications
Cargo.toml (workspace)                        ├─► MevWorkerPool::new(evm_config)
  └─ 追加 members + deps ────────────────►    │     └─ 256 × MevWorker OS 线程
                                              └─► MevApiServer { epoch_manager,
                                                    worker_pool, eth_api }
                                                    ├─ mev_eth_call
                                                    │    ├─ [新块] → WorkerPool
                                                    │    └─ [旧块] → eth_api.call() ①
                                                    ├─ mev_debug_traceCall
                                                    └─ mev_trace_call
```

### 2.4 各层对现有 Reth 代码的调用关系

新代码分两层调用现有 Reth 代码：API 层处理纯值逻辑，Worker 处理需 DB 的操作和执行。

**API 层（`MevApiServer`）调用现有代码**

| 任务 | 调用位置 | 方式 |
|---|---|---|
| 路由判断（新块 / 旧块）| `EpochManager::matches_active` | 自实现 |
| `cfg_env` 标志位设置 | `prepare_call_env` 中约 10 行逻辑 | **API 层等价实现**（`MevApiServer::prepare_evm_env`）；`prepare_call_env` 依赖 `&self`（EthApi），无法直接调用 |
| gas 封顶 + nonce 清空 | 同上 | **API 层等价实现** |
| 旧块降级执行 | 原生 `eth_api.call()` / `debug_traceCall` / `trace_call` | **完整委托**（原样透传，含 `block_id`）|

**Worker 层调用现有代码**

| 任务 | 调用位置 | 方式 |
|---|---|---|
| `block_overrides` 应用 | `alloy_evm::overrides::apply_block_overrides` | **直接调用**（独立函数，不依赖 EthApi）|
| `state_overrides` 应用 | `alloy_evm::overrides::apply_state_overrides` | **直接调用**（独立函数）|
| `TxEnv` 构造 | `ConfigureEvm::tx_env`（`EthEvmConfig` 实现）| **直接调用**（Worker 持有 `evm_config`）|
| EVM 执行 | `revm::Evm::transact` / `inspect` | **Worker 独立执行**（核心价值所在）|
| Geth tracer 创建 | `revm_inspectors::tracing` 类型 | **直接使用** |
| Parity tracer 创建 | `revm_inspectors::tracing::TracingInspector` | **直接使用** |
| 结果格式化 | `reth_rpc_eth_types` 中独立格式化函数 | **直接调用** |

---

## 3. 核心设计原则：Worker 是薄 EVM 执行器

### 职责边界

**Worker 只做两件事：管理 revm 生命周期，提供 EVM 执行能力。**  
它不理解"这是一个 `mev_eth_call` 请求"，不关心 RPC 语义，也不负责 API 层的参数预处理。

```
MevApiServer（API 层，tokio 异步上下文）
  职责：路由判断 / EvmEnv 准备（cfg_env 标志 + gas 封顶 + nonce 清空）/ 打包 WorkerTask
  ↓ WorkerTask
MevWorker（EVM 执行层，OS 线程）
  职责：仅做"需要访问 Worker DB 的事" + EVM 执行
  ├─ apply block_overrides → worker DB（block hash overrides 需 DB）
  ├─ apply state_overrides → worker DB（覆盖账户/存储状态）
  ├─ evm_config.tx_env()  → 构造 TxEnv（需 DB 计算 gas allowance）
  └─ evm.transact() / evm.inspect()  → EVM 执行，结果回传
```

### 职责对照表

| 任务 | 负责方 | 原因 |
|---|---|---|
| 路由（新块 vs 旧块降级）| API 层 | 路由逻辑是 RPC 语义，与 EVM 无关 |
| `cfg_env` 标志位设置（disable_base_fee 等）| API 层 | 纯值操作，不需要 DB |
| gas 封顶（call_gas_cap）| API 层 | 节点配置层面的策略，与 DB 无关 |
| nonce 清空 | API 层 | 纯值操作 |
| `block_overrides` 应用 | Worker | `apply_block_overrides` 需要 `&mut DB`（block hash 覆盖）|
| `state_overrides` 应用 | Worker | 需要向 Worker 自己的 DB 写入覆盖值 |
| `TxEnv` 构造 | Worker | `evm_config.tx_env()` 需要 DB 计算 gas allowance |
| EVM 执行（transact / inspect）| Worker | 核心执行，Worker Pool 的存在价值 |
| 结果格式化（output 提取 / trace 转换）| Worker | 紧随执行结果，在 Worker 线程内完成效率最高 |

> **为什么 `block_overrides` 不在 API 层处理**：`apply_block_overrides` 的签名需要 `&mut DB`（用于 block hash 覆盖）。尽管多数场景不用 block hash 覆盖，但为保持正确性不做假设，统一交 Worker 处理。

---

## 4. 依赖（`crates/mev/Cargo.toml`）

```toml
[package]
name = "reth-mev"
version.workspace = true
edition.workspace = true

[dependencies]
# Reth 内部
reth-chain-state    = { workspace = true }
reth-evm            = { workspace = true }
reth-ethereum-evm   = { workspace = true }    # EthEvmConfig
reth-node-api       = { workspace = true }
reth-provider       = { workspace = true }
reth-rpc-eth-api    = { workspace = true }
reth-rpc-eth-types  = { workspace = true }
reth-rpc-types      = { workspace = true }
reth-storage-api    = { workspace = true }
reth-revm           = { workspace = true }

# revm
revm                = { workspace = true }

# jsonrpsee
jsonrpsee           = { workspace = true, features = ["server"] }

# 异步 + 并发
tokio               = { workspace = true, features = ["full"] }
crossbeam-channel   = "0.5"

# 工具
alloy-primitives    = { workspace = true }
alloy-rpc-types-eth = { workspace = true }
alloy-eips          = { workspace = true }
tracing             = { workspace = true }
metrics             = { workspace = true }
eyre                = { workspace = true }

[dev-dependencies]
reth-testing-utils  = { workspace = true }
tokio               = { workspace = true, features = ["test-util"] }
```

---

## 4. 核心数据结构

### 4.1 `EpochContext`（`epoch.rs`）

```rust
use alloy_primitives::B256;
use alloy_eips::eip1559::BaseFeeParams;
use reth_evm::EvmEnv;
use reth_storage_api::StateProviderFactory;
use std::sync::Arc;

/// 唯一标识一个区块版本的 epoch 上下文，不可变，创建后只读。
#[derive(Debug, Clone)]
pub struct EpochContext {
    /// 单调递增，每个新 committed block 对应一个新 epoch_id
    pub epoch_id: u64,
    pub block_number: u64,
    pub block_hash: B256,
    /// 完整的 EVM 区块环境（coinbase, basefee, timestamp, prevrandao, gas_limit 等）
    pub block_env: EvmEnv,
    /// EVM 规格（Cancun / Prague 等），决定 opcode 集合
    pub spec_id: revm::primitives::SpecId,
    /// 用于在该区块高度打开 StateProvider；Arc 使多 worker 共享同一 factory
    pub state_provider_factory: Arc<dyn StateProviderFactory + Send + Sync>,
}

impl EpochContext {
    /// EpochManager 初始化时使用的占位 epoch（epoch_id = 0）。
    /// 该 epoch 仅作启动占位，不应有真实请求绑定到它；
    /// 第一个 CanonStateNotification 到达后会被替换为真实 epoch。
    pub fn placeholder() -> Self {
        Self {
            epoch_id: 0,
            block_number: 0,
            block_hash: B256::ZERO,
            block_env: EvmEnv::default(),
            spec_id: revm::primitives::SpecId::LATEST,
            // placeholder 不会被 Worker 的 switch_epoch 调用（epoch_id 匹配后才调用）
            // 若意外调用 state_by_block_hash(B256::ZERO) 会报错，方便排查
            state_provider_factory: Arc::new(PlaceholderProviderFactory),
        }
    }
}

/// EpochContext::placeholder() 所用的空实现，触发时 panic 以暴露逻辑错误。
struct PlaceholderProviderFactory;
impl StateProviderFactory for PlaceholderProviderFactory {
    // 所有方法 panic，确保不被意外使用
    fn latest(&self) -> reth_errors::ProviderResult<reth_storage_api::StateProviderBox> {
        panic!("PlaceholderProviderFactory should not be used")
    }
    // … 其余方法同理（编译器会提示缺少哪些方法）
}
```

**说明**：
- `EvmEnv` 来自 `reth_evm`，已包含 `BlockEnv`（revm 的 `CfgEnv` + `BlockEnv`）。
- `StateProviderFactory` 在 `reth_storage_api` 中定义；`BlockchainProvider` 实现了该 trait。
- `epoch_id` 自增计数器即可，不需要与链上任何字段对齐。

---

### 4.2 `EpochManager`（`epoch.rs`）

```rust
use std::sync::{Arc, RwLock};
use reth_chain_state::{CanonStateNotification, CanonStateSubscriptions};
use tokio::sync::watch;

pub struct EpochManager {
    /// 当前活跃 epoch，所有 mev_* 请求绑定到此
    active: watch::Sender<Arc<EpochContext>>,
    /// 外部只读订阅句柄
    pub active_rx: watch::Receiver<Arc<EpochContext>>,
}

impl EpochManager {
    /// 启动后台任务：监听 CanonStateNotification，维护 active_epoch
    pub fn spawn<P>(provider: P) -> Arc<Self>
    where
        P: CanonStateSubscriptions + Send + 'static,
    {
        // 实现见 §6.1
    }

    /// 获取当前 active_epoch 的快照（Arc，零拷贝）
    pub fn current(&self) -> Arc<EpochContext> {
        self.active_rx.borrow().clone()
    }
}
```

**实现要点**（§6.1 展开）：
- 调用 `provider.subscribe_to_canonical_state()` 获取 `CanonStateNotifications`（`tokio::sync::broadcast::Receiver<CanonStateNotification>`）。
- 在 `tokio::spawn` 任务中 loop：收到 `CanonStateNotification::Commit { new }` 时，从 `new.tip()` 提取 block header，用 `EthEvmConfig::evm_env(&header)` 构造 `EvmEnv`，生成新 `EpochContext`，通过 `watch::Sender::send` 原子更新。
- `Reorg` 事件同样触发更新（取 reorg 后的新链尖）。

---

### 4.3 `WorkerTask`、`WorkerResult`（`worker/mod.rs`）

`WorkerTask` 携带的是 **API 层已完成预处理的内容**，Worker 收到后仅需处理"依赖 DB 的步骤"和 EVM 执行。

```rust
use tokio::sync::oneshot;
use alloy_rpc_types_eth::{state::StateOverride, BlockOverrides, transaction::TransactionRequest};
use reth_evm::EvmEnv;

/// Worker 执行的调用类型（执行层关注：输出格式不同）
#[derive(Debug)]
pub enum CallKind {
    /// mev_eth_call：返回执行输出字节
    Basic,
    /// mev_debug_traceCall：附带 GethDebugTracingCallOptions
    DebugTrace { opts: Box<reth_rpc_eth_types::GethDebugTracingCallOptions> },
    /// mev_trace_call：附带 trace_types（parity trace），HashSet 与原生 trace_call 一致
    ParityTrace {
        trace_types: std::collections::HashSet<alloy_rpc_types_trace::parity::TraceType>,
    },
}

#[derive(Debug)]
pub struct WorkerTask {
    /// Worker 用于 epoch 切换判断（Layer 2 管理）
    pub epoch: Arc<EpochContext>,

    // ── API 层已完成预处理 ─────────────────────────────────────────────
    /// EvmEnv：API 层已设置 cfg_env 标志位（disable_base_fee / disable_eip3607 等）
    /// block_env 来自 epoch.block_env（尚未应用 block_overrides，Worker 负责应用）
    pub evm_env: EvmEnv,
    /// TransactionRequest：API 层已完成 gas 封顶 + nonce 清空
    pub request: TransactionRequest,

    // ── 需 Worker DB 介入，在 Worker 内处理 ───────────────────────────
    /// 应用到 Worker DB 的区块环境覆盖（block hash 覆盖需要 DB，统一交 Worker）
    pub block_overrides: Option<Box<BlockOverrides>>,
    /// 应用到 Worker DB 的账户/存储状态覆盖
    pub state_overrides: Option<StateOverride>,

    pub kind: CallKind,
    pub result_tx: oneshot::Sender<WorkerResult>,
}

pub type WorkerResult = Result<WorkerOutput, WorkerError>;

#[derive(Debug)]
pub enum WorkerOutput {
    Basic(alloy_primitives::Bytes),
    DebugTrace(reth_rpc_eth_types::GethTrace),
    /// `trace_call` 返回与原生 `trace_call` 完全一致的 TraceResults 结构
    ParityTrace(alloy_rpc_types_trace::parity::TraceResults),
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// EVM 执行 revert，含 revert data
    #[error("evm execution reverted")]
    Revert(alloy_primitives::Bytes),
    /// EVM 执行 halt（OOG / invalid opcode 等），保留 gas_used 供上层按需使用
    #[error("evm halted: {reason}, gas_used={gas_used}")]
    Halt { reason: String, gas_used: u64 },
    /// transact/evm_with_env 返回的底层 EVM 错误（非 revert/halt）
    #[error("evm error: {0}")]
    Evm(String),
    /// DebugInspector::new / get_result 错误
    #[error("debug inspector error: {0}")]
    Inspect(String),
    /// into_trace_results_with_state 错误
    #[error("parity tracing error: {0}")]
    Tracing(String),
    #[error("state provider error: {0}")]
    Provider(#[from] reth_errors::ProviderError),
    #[error("internal: {0}")]
    Internal(String),
}
```

---

### 4.4 `WorkerL1Cache`（`worker/cache.rs`）

```rust
use alloy_primitives::{Address, B256, U256};
use revm::primitives::{AccountInfo, Bytecode};
use std::collections::HashMap;

/// Phase 1 的 Worker-L1 本地缓存：仅存储"干净读"（从 StateProvider 读到的值）。
/// 切块时整个 bucket 丢弃，新 epoch 从空缓存开始。
#[derive(Debug, Default)]
pub struct WorkerL1Cache {
    pub epoch_id: u64,
    /// address -> AccountInfo（nonce, balance, code_hash）
    pub accounts: HashMap<Address, Option<AccountInfo>>,
    /// code_hash -> Bytecode（不可变，可跨 epoch 保留，但 Phase 1 简化处理同样清空）
    pub bytecodes: HashMap<B256, Bytecode>,
    /// (address, slot) -> value
    pub storage: HashMap<(Address, U256), U256>,
}

impl WorkerL1Cache {
    pub fn new(epoch_id: u64) -> Self {
        Self { epoch_id, ..Default::default() }
    }

    /// 判断是否属于当前 epoch
    pub fn is_valid_for(&self, epoch_id: u64) -> bool {
        self.epoch_id == epoch_id
    }

    /// 切 epoch 时调用，清空所有数据，重置 epoch_id
    pub fn reset(&mut self, new_epoch_id: u64) {
        self.accounts.clear();
        self.bytecodes.clear();
        self.storage.clear();
        self.epoch_id = new_epoch_id;
        // bytecodes 不可变，Phase 2 可优化为保留，Phase 1 保持简单
    }
}
```

---

### 4.5 `WorkerStateProvider`（`provider.rs`）

实现 `revm::Database`，读路径：Worker-L1 命中 → miss → StateProvider(DB)，DB 结果自动回填 Worker-L1。

```rust
use revm::{Database, primitives::{AccountInfo, Bytecode, B256, U256, Address}};
use reth_storage_api::StateProviderBox;
use reth_revm::database::StateProviderDatabase;
use crate::worker::cache::WorkerL1Cache;

/// revm::Database 适配层（Phase 1）：Worker-L1 → StateProvider
pub struct WorkerStateProvider<'a> {
    pub l1: &'a mut WorkerL1Cache,
    pub db: StateProviderDatabase<&'a StateProviderBox>,
}

impl<'a> Database for WorkerStateProvider<'a> {
    type Error = reth_errors::ProviderError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        // 1. Worker-L1 命中（含负缓存 None）
        if let Some(cached) = self.l1.accounts.get(&address) {
            return Ok(cached.clone());
        }
        // 2. DB 读取，回填 L1
        let info = self.db.basic(address)?;
        self.l1.accounts.insert(address, info.clone());
        Ok(info)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        if let Some(code) = self.l1.bytecodes.get(&code_hash) {
            return Ok(code.clone());
        }
        let code = self.db.code_by_hash(code_hash)?;
        self.l1.bytecodes.insert(code_hash, code.clone());
        Ok(code)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if let Some(&val) = self.l1.storage.get(&(address, index)) {
            return Ok(val);
        }
        let val = self.db.storage(address, index)?;
        self.l1.storage.insert((address, index), val);
        Ok(val)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        // 不缓存 block_hash，直接透传（调用频率极低）
        self.db.block_hash(number)
    }
}
```

**关键点**：
- `l1` 是 `&mut WorkerL1Cache`，worker 单线程执行，无需锁。
- `StateProviderDatabase` 是 reth 对 `StateProviderBox` 的 revm 适配，已位于 `reth_revm::database`。
- Worker-L1 存储的是 clean reads（链上状态），不存储 transaction 执行过程中的 dirty state（那部分由 revm 的 `State` 层管理，执行完即丢弃）。

---

## 5. Worker 主循环（`worker/worker.rs`）

```rust
use crossbeam_channel::Receiver;
use reth_evm::ConfigureEvm;
use reth_revm::database::StateProviderDatabase;
use revm::{State, context::TxEnv};
use std::sync::Arc;
use crate::{
    epoch::EpochContext,
    worker::{cache::WorkerL1Cache, mod::{WorkerTask, WorkerOutput, WorkerError}},
    provider::WorkerStateProvider,
};

pub struct MevWorker {
    id: usize,
    task_rx: Receiver<WorkerTask>,
    // Layer 0: EVM 配置（进程级常驻，EthEvmConfig 是 Clone + Send）
    evm_config: reth_ethereum_evm::EthEvmConfig,
    // Layer 1: Worker-L1 Cache（epoch 分桶，按需 reset）
    l1: WorkerL1Cache,
    // Layer 2: 当前 epoch 的 StateProvider（切 epoch 时替换）
    current_epoch_id: u64,
    state_provider: Option<reth_storage_api::StateProviderBox>,
}

impl MevWorker {
    pub fn spawn(
        id: usize,
        task_rx: Receiver<WorkerTask>,
        evm_config: reth_ethereum_evm::EthEvmConfig,
    ) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name(format!("mev-worker-{id}"))
            .spawn(move || {
                let mut worker = MevWorker {
                    id,
                    task_rx,
                    evm_config,
                    l1: WorkerL1Cache::new(0),
                    current_epoch_id: 0,
                    state_provider: None,
                };
                worker.run();
            })
            .expect("spawn mev worker thread")
    }

    fn run(&mut self) {
        while let Ok(task) = self.task_rx.recv() {
            let epoch = &task.epoch;

            // Layer 2 切换：epoch 变更时替换 StateProvider，清空 L1
            if epoch.epoch_id != self.current_epoch_id {
                self.switch_epoch(epoch);
            }

            // Layer 3：执行单次请求，完成后 Layer 3 数据自动析构
            let result = self.execute_task(&task);

            // 结果回传（忽略 receiver 已关闭的情况）
            let _ = task.result_tx.send(result);

            // 指标
            metrics::counter!("mev_worker_tasks_total", "worker_id" => self.id.to_string())
                .increment(1);
        }
    }

    fn switch_epoch(&mut self, epoch: &Arc<EpochContext>) {
        let sp = epoch
            .state_provider_factory
            .state_by_block_hash(epoch.block_hash)
            .expect("state provider for epoch block");

        self.state_provider = Some(sp);
        self.l1.reset(epoch.epoch_id);
        self.current_epoch_id = epoch.epoch_id;

        tracing::debug!(
            target: "reth::mev::worker",
            worker_id = self.id,
            epoch_id = epoch.epoch_id,
            block_number = epoch.block_number,
            "worker switched epoch"
        );

        metrics::counter!("mev_worker_epoch_switches_total").increment(1);
    }

    /// Worker 执行一次任务（Layer 3 生命周期）。
    /// 入参 WorkerTask 由 API 层完成预处理（cfg_env / gas cap / nonce 已就绪）。
    /// Worker 仅负责：需 DB 的覆盖应用 + TxEnv 构造 + EVM 执行。
    fn execute_task(&mut self, task: &WorkerTask) -> Result<WorkerOutput, WorkerError> {
        let sp = self.state_provider.as_ref()
            .expect("state_provider must be set after switch_epoch");

        // Layer 3：构造 WorkerStateProvider（借用 L1 + StateProvider）
        let mut wsp = WorkerStateProvider {
            l1: &mut self.l1,
            db: StateProviderDatabase::new(sp),
        };

        // revm::State：管理本次执行的 dirty state（执行完毕后随函数栈析构）
        let mut db = State::builder()
            .with_database(&mut wsp)
            .with_bundle_update()
            .build();

        // evm_env 由 API 层预处理完毕（cfg_env 标志已设置），Worker 在此基础上继续
        let mut evm_env = task.evm_env.clone();

        // ── 步骤 1：应用 block_overrides（需 Worker DB，独立函数）────────────
        if let Some(ref bo) = task.block_overrides {
            alloy_evm::overrides::apply_block_overrides(
                *bo.clone(), &mut db, evm_env.block_env.inner_mut(),
            );
        }

        // ── 步骤 2：应用 state_overrides（需 Worker DB，独立函数）───────────
        if let Some(ref so) = task.state_overrides {
            alloy_evm::overrides::apply_state_overrides(so.clone(), &mut db)
                .map_err(|e| WorkerError::Internal(e.to_string()))?;
        }

        // ── 步骤 3：构造 TxEnv（需 Worker DB 计算 gas allowance）───────────
        let tx_env = self.evm_config
            .tx_env(&evm_env, task.request.clone(), &mut db)
            .map_err(|e| WorkerError::Internal(format!("tx_env: {e:?}")))?;

        // ── 步骤 4：EVM 执行 ─────────────────────────────────────────────
        let result = match &task.kind {
            CallKind::Basic => self.exec_basic(&mut db, evm_env, tx_env)?,
            CallKind::DebugTrace { opts } => {
                self.exec_debug_trace(&mut db, evm_env, tx_env, opts)?
            }
            CallKind::ParityTrace { trace_types } => {
                self.exec_parity_trace(&mut db, evm_env, tx_env, trace_types)?
            }
        };

        // db / wsp 析构：Worker-L1 保留本次新读的 clean values；dirty state 随 db 释放
        Ok(result)
    }

    /// 基础 eth_call：执行并返回输出字节。
    ///
    /// `evm_with_env` 接收 `DB: Database` by value；这里传 `&mut *db`（reborrow），
    /// EVM 持有该 reborrow，transact 完毕后 EVM drop，reborrow 释放，db 恢复可用。
    fn exec_basic<DB>(
        &self,
        db: &mut DB,
        evm_env: EvmEnv,
        tx_env: TxEnv,
    ) -> Result<WorkerOutput, WorkerError>
    where
        DB: revm::Database + revm::DatabaseCommit,
        DB::Error: std::fmt::Debug,
    {
        let res = self.evm_config
            .evm_with_env(&mut *db, evm_env)
            .transact(tx_env)
            .map_err(|e| WorkerError::Evm(format!("{e:?}")))?;

        match res.result {
            revm::primitives::ExecutionResult::Success { output, .. } => {
                Ok(WorkerOutput::Basic(output.into_data()))
            }
            revm::primitives::ExecutionResult::Revert { output, .. } => {
                Err(WorkerError::Revert(output))
            }
            revm::primitives::ExecutionResult::Halt { reason, gas_used } => {
                Err(WorkerError::Halt { reason: format!("{reason:?}"), gas_used })
            }
        }
    }

    /// Geth-style debug trace（debug_traceCall 对应逻辑）。
    ///
    /// `DebugInspector::new` 解析 `GethDebugTracingOptions` 并配置跟踪级别；
    /// `get_result` 在 EVM drop（reborrow 释放）后调用，可访问最终 db 状态。
    fn exec_debug_trace<DB>(
        &self,
        db: &mut DB,
        evm_env: EvmEnv,
        tx_env: TxEnv,
        opts: alloy_rpc_types_trace::geth::GethDebugTracingCallOptions,
    ) -> Result<WorkerOutput, WorkerError>
    where
        DB: revm::Database + revm::DatabaseCommit,
        DB::Error: std::fmt::Debug,
        revm_inspectors::tracing::DebugInspector:
            revm::Inspector<revm_inspectors::tracing::revm::EvmContext<&mut DB>>,
    {
        use revm_inspectors::tracing::DebugInspector;

        let tracing_options = opts.tracing_options;
        let mut inspector = DebugInspector::new(tracing_options)
            .map_err(|e| WorkerError::Inspect(format!("{e:?}")))?;

        let res = self.evm_config
            .evm_with_env_and_inspector(&mut *db, evm_env.clone(), &mut inspector)
            .transact(tx_env.clone())
            .map_err(|e| WorkerError::Evm(format!("{e:?}")))?;
        // EVM dropped here, &mut reborrow on db released

        let trace = inspector
            .get_result(None, &tx_env, &evm_env.block_env, &res, db)
            .map_err(|e| WorkerError::Inspect(format!("{e:?}")))?;

        Ok(WorkerOutput::DebugTrace(trace))
    }

    /// Parity-style trace（trace_call 对应逻辑）。
    ///
    /// `TracingInspectorConfig::from_parity_config` 根据请求的 TraceType 集合配置采集粒度；
    /// `into_trace_results_with_state` 需要 db 的只读访问来填充 stateDiff，
    /// 在 EVM drop 后调用（reborrow 已释放）。
    fn exec_parity_trace<DB>(
        &self,
        db: &mut DB,
        evm_env: EvmEnv,
        tx_env: TxEnv,
        trace_types: std::collections::HashSet<alloy_rpc_types_trace::parity::TraceType>,
    ) -> Result<WorkerOutput, WorkerError>
    where
        DB: revm::Database + revm::DatabaseCommit,
        DB::Error: std::fmt::Debug,
        revm_inspectors::tracing::TracingInspector:
            revm::Inspector<revm_inspectors::tracing::revm::EvmContext<&mut DB>>,
    {
        use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

        let config = TracingInspectorConfig::from_parity_config(&trace_types);
        let mut inspector = TracingInspector::new(config);

        let res = self.evm_config
            .evm_with_env_and_inspector(&mut *db, evm_env, &mut inspector)
            .transact(tx_env)
            .map_err(|e| WorkerError::Evm(format!("{e:?}")))?;
        // EVM dropped here, &mut reborrow on db released

        let trace_res = inspector
            .into_parity_builder()
            .into_trace_results_with_state(&res, &trace_types, db)
            .map_err(|e| WorkerError::Tracing(format!("{e:?}")))?;

        Ok(WorkerOutput::ParityTrace(trace_res))
    }
}
```

**关键实现说明**：

- Worker 是 OS 线程（`std::thread::spawn`），不是 tokio task，因为 EVM 执行是 CPU 密集型，不应占用 tokio 调度线程。
- `execute_task` 中创建的 `State` / `WorkerStateProvider` 是栈上临时对象，函数返回后自动析构（Layer 3 语义）。
- `state_provider` 在 `switch_epoch` 时替换为新 epoch 对应的 `StateProviderBox`，旧 `StateProviderBox` 自动 drop。
- `WorkerL1Cache` 的 `bytecodes` 字段存储字节码，Phase 1 切块时一并清空，Phase 2 可优化为保留（`code_hash` 不可变）。

**exec_* 函数 Reborrow 模式说明**：

三个 `exec_*` 函数**全部**通过 `&mut *db`（reborrow）将 db 传给 `evm_with_env[_and_inspector]`，模式完全一致：
- `evm_with_env / evm_with_env_and_inspector` 接受 `DB: Database` by value；传 `&mut *db` 即以 `DB = &mut CacheDB<WorkerStateProvider>` 形式传入一个 reborrow（不移走原始 `&mut db`）。
- EVM 持有该 reborrow，`transact` 完毕后 EVM 随作用域 drop，reborrow 自动释放，原始 `db` 恢复可访问。

三者的**唯一区别**在于 EVM drop 后是否还需要访问 db：

| 函数 | EVM drop 后访问 db？ | 原因 |
|---|---|---|
| `exec_basic` | **不需要** | 返回值（output bytes）已在 `res.result` 中，自包含 |
| `exec_debug_trace` | **需要** | `inspector.get_result(…, db)` 读取最终 db 状态构造 GethTrace |
| `exec_parity_trace` | **需要** | `into_trace_results_with_state(…, db)` 读取最终状态填充 stateDiff |

- Reth 核心代码相同模式见 `crates/rpc/rpc/src/debug.rs:313` 和 `crates/rpc/rpc-eth-api/src/helpers/trace.rs:41`。

**关键 Reth API 对照**（供 Codex 直接查阅）：

| 我们的调用 | Reth 源文件 | 行号 |
|---|---|---|
| `evm_config.evm_with_env(db, env)` | `crates/evm/evm/src/lib.rs` | 274 |
| `evm_config.evm_with_env_and_inspector(db, env, insp)` | `crates/evm/evm/src/lib.rs` | 300 |
| `DebugInspector::new(opts)` / `inspector.get_result(...)` | `crates/rpc/rpc/src/debug.rs` | 311–321 |
| `TracingInspectorConfig::from_parity_config(&types)` | `crates/rpc/rpc/src/trace.rs` | 97 |
| `inspector.into_parity_builder().into_trace_results_with_state(...)` | `crates/rpc/rpc/src/trace.rs` | 105–108 |
| `ExecutionResult::Success { output, .. }` / `output.into_data()` | `crates/rpc/rpc-eth-types/src/error/api.rs` | 124–132 |
| `ctx.registry.provider()` / `ctx.registry.evm_config()` | `crates/rpc/rpc-builder/src/lib.rs` | 586–594 |
| `eth_api.call_gas_limit()` / `eth_api.evm_memory_limit()` | `crates/rpc/rpc/src/eth/helpers/call.rs` | 26–37 |

---

## 6. EpochManager 实现（`epoch.rs`）

### 6.1 完整实现

```rust
use std::sync::Arc;
use tokio::sync::watch;
use reth_chain_state::{CanonStateNotification, CanonStateSubscriptions};
use reth_ethereum_evm::EthEvmConfig;
use reth_storage_api::StateProviderFactory;
use reth_evm::ConfigureEvm;

pub struct EpochManager {
    active_tx: watch::Sender<Arc<EpochContext>>,
    pub active_rx: watch::Receiver<Arc<EpochContext>>,
}

impl EpochManager {
    pub fn spawn<P>(provider: P, evm_config: EthEvmConfig) -> Arc<Self>
    where
        P: CanonStateSubscriptions + StateProviderFactory + Clone + Send + Sync + 'static,
    {
        // 用一个占位的初始 epoch（未收到 ChainCommitted 前不应有请求进来）
        let initial = Arc::new(EpochContext::placeholder());
        let (tx, rx) = watch::channel(initial);

        let mgr = Arc::new(Self { active_tx: tx, active_rx: rx });
        let mgr_clone = mgr.clone();

        tokio::spawn(async move {
            let mut notifications = provider.subscribe_to_canonical_state();
            let mut epoch_counter: u64 = 0;

            loop {
                match notifications.recv().await {
                    Ok(CanonStateNotification::Commit { new }) => {
                        epoch_counter += 1;
                        let tip = new.tip();
                        let header = tip.header();

                        let block_env = evm_config
                            .evm_env(header)
                            .expect("build evm_env from header");

                        let spec_id = evm_config
                            .spec_id_at_head(header)
                            .unwrap_or(revm::primitives::SpecId::CANCUN);

                        let epoch = Arc::new(EpochContext {
                            epoch_id: epoch_counter,
                            block_number: header.number(),
                            block_hash: tip.hash(),
                            block_env,
                            spec_id,
                            state_provider_factory: Arc::new(provider.clone()),
                        });

                        let _ = mgr_clone.active_tx.send(epoch);

                        tracing::info!(
                            target: "reth::mev::epoch",
                            epoch_id = epoch_counter,
                            block_number = header.number(),
                            "new epoch activated"
                        );
                    }
                    Ok(CanonStateNotification::Reorg { new, .. }) => {
                        // Reorg：同样切到新链尖，与 Commit 处理一致
                        epoch_counter += 1;
                        let tip = new.tip();
                        // ... 同上，略
                        tracing::warn!(target: "reth::mev::epoch", "reorg detected, epoch reset");
                    }
                    Err(e) => {
                        tracing::error!(target: "reth::mev::epoch", ?e, "canon state recv error");
                        break;
                    }
                }
            }
        });

        mgr
    }

    pub fn current(&self) -> Arc<EpochContext> {
        self.active_rx.borrow().clone()
    }

    /// 判断给定 block_id 是否与 active_epoch 一致（或未指定）
    pub fn matches_active(&self, block_id: Option<alloy_eips::BlockId>) -> bool {
        match block_id {
            None => true,
            Some(alloy_eips::BlockId::Number(n)) => {
                use alloy_eips::BlockNumberOrTag;
                match n {
                    BlockNumberOrTag::Latest | BlockNumberOrTag::Pending => true,
                    BlockNumberOrTag::Number(n) => {
                        n == self.active_rx.borrow().block_number
                    }
                    _ => false,
                }
            }
            Some(alloy_eips::BlockId::Hash(h)) => {
                h.block_hash == self.active_rx.borrow().block_hash
            }
        }
    }
}
```

---

## 7. Worker Pool（`worker/mod.rs`）

### 7.1 结构与初始化

```rust
use crossbeam_channel::{bounded, Sender};
use std::sync::Arc;

pub const DEFAULT_POOL_SIZE: usize = 256;
pub const TASK_QUEUE_CAPACITY: usize = 4096; // 背压上限

pub struct MevWorkerPool {
    /// 共享工作队列：所有 worker 从同一 channel 拉取任务（自然负载均衡）
    task_tx: Sender<WorkerTask>,
    pub num_workers: usize,
}

impl MevWorkerPool {
    pub fn new(
        num_workers: usize,
        evm_config: reth_ethereum_evm::EthEvmConfig,
    ) -> Arc<Self> {
        let (task_tx, task_rx) = bounded(TASK_QUEUE_CAPACITY);

        for id in 0..num_workers {
            MevWorker::spawn(id, task_rx.clone(), evm_config.clone());
        }

        Arc::new(Self { task_tx, num_workers })
    }

    /// 分发单个任务，返回 oneshot receiver 供等待结果
    pub fn dispatch(
        &self,
        task: WorkerTask,
    ) -> Result<tokio::sync::oneshot::Receiver<WorkerResult>, PoolError> {
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let task = WorkerTask { result_tx, ..task };

        self.task_tx
            .try_send(task)
            .map_err(|_| PoolError::QueueFull)?;

        Ok(result_rx)
    }

    /// 并发分发一批任务，按提交顺序收集结果（保序）
    pub async fn dispatch_batch(
        &self,
        tasks: Vec<WorkerTask>,
    ) -> Vec<WorkerResult> {
        let receivers: Vec<_> = tasks
            .into_iter()
            .map(|t| self.dispatch(t))
            .collect::<Result<Vec<_>, _>>()
            .expect("dispatch batch");

        // 等待所有结果（保序）
        let mut results = Vec::with_capacity(receivers.len());
        for rx in receivers {
            results.push(rx.await.unwrap_or(Err(WorkerError::Internal("dropped".into()))));
        }
        results
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("worker pool queue full")]
    QueueFull,
}
```

**背压策略**：
- `TASK_QUEUE_CAPACITY = 4096` 是硬限制。`dispatch` 使用 `try_send`（非阻塞），满队列时立即返回 `PoolError::QueueFull`，由 RPC 层转换为 JSON-RPC `-32603` 错误。
- 可通过配置调整队列大小和 worker 数量。

---

## 8. RPC API 层（`api/`）

### 8.1 Trait 定义（`api/mod.rs`）

```rust
use jsonrpsee::proc_macros::rpc;
use alloy_primitives::Bytes;
use alloy_rpc_types_eth::{
    state::StateOverride, BlockOverrides, BlockId,
    transaction::TransactionRequest,
};

#[rpc(server, namespace = "mev")]
pub trait MevApi {
    /// 等价于 eth_call，执行路径走 worker pool
    #[method(name = "eth_call")]
    async fn mev_eth_call(
        &self,
        request: TransactionRequest,
        block_id: Option<BlockId>,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> jsonrpsee::core::RpcResult<Bytes>;

    /// 等价于 debug_traceCall，执行路径走 worker pool
    #[method(name = "debug_traceCall")]
    async fn mev_debug_trace_call(
        &self,
        request: TransactionRequest,
        block_id: Option<BlockId>,
        opts: Option<reth_rpc_eth_types::GethDebugTracingCallOptions>,
    ) -> jsonrpsee::core::RpcResult<reth_rpc_eth_types::GethTrace>;

    /// 等价于 trace_call，执行路径走 worker pool；返回类型与原生 trace_call 一致
    #[method(name = "trace_call")]
    async fn mev_trace_call(
        &self,
        request: TransactionRequest,
        trace_types: Vec<alloy_rpc_types_trace::parity::TraceType>,
        block_id: Option<BlockId>,
    ) -> jsonrpsee::core::RpcResult<alloy_rpc_types_trace::parity::TraceResults>;
}
```

> **注意**：jsonrpsee 的 `#[rpc(namespace = "mev")]` + `#[method(name = "eth_call")]` 会生成方法名 `mev_eth_call`，与命名约定一致。

### 8.2 服务端实现（`api/server.rs`）

```rust
use std::sync::Arc;
use jsonrpsee::core::RpcResult;
use reth_rpc_eth_api::EthApiServer;  // 原生 eth 接口，用于降级
use crate::{epoch::EpochManager, worker::MevWorkerPool};

/// 节点级配置，由 install_mev_rpc 构造时从 node config 读取
#[derive(Clone, Copy)]
pub struct MevCallConfig {
    /// 对应原生 eth_call 的 call_gas_cap（来自 EthConfig::rpc_gas_cap）
    pub call_gas_cap: u64,
    /// 对应原生 eth_call 的 memory_limit（来自 EthConfig::rpc_memory_limit）
    pub evm_memory_limit: u64,
}

pub struct MevApiServer<EthApi> {
    pub epoch_manager: Arc<EpochManager>,
    pub worker_pool: Arc<MevWorkerPool>,
    pub call_config: MevCallConfig,
    /// 原生 EthApi 引用，用于旧块请求的降级路由
    pub eth_api: EthApi,
}

impl<EthApi> MevApiServer<EthApi> {
    /// API 层预处理：不需要 DB，纯值操作。
    /// 等价于 prepare_call_env 中 cfg_env 标志 + gas cap + nonce 的部分。
    fn prepare_evm_env(&self, epoch: &EpochContext, mut request: TransactionRequest)
        -> (reth_evm::EvmEnv, TransactionRequest)
    {
        let mut evm_env = epoch.block_env.clone();

        // cfg_env 标志（与原生 eth_call 行为一致）
        evm_env.cfg_env.disable_block_gas_limit = true;
        evm_env.cfg_env.disable_eip3607         = true;
        evm_env.cfg_env.disable_base_fee        = true;
        evm_env.cfg_env.tx_gas_limit_cap        = Some(u64::MAX);
        evm_env.cfg_env.disable_fee_charge      = true;
        evm_env.cfg_env.memory_limit            = self.call_config.evm_memory_limit;

        // gas 封顶
        let cap = self.call_config.call_gas_cap;
        match request.gas_limit() {
            Some(g) if cap != 0 && cap < g => request.set_gas_limit(cap),
            None                            => request.set_gas_limit(cap),
            _                               => {}
        }

        // nonce 由 EVM 自动确定
        request.take_nonce();

        (evm_env, request)
    }
}

#[async_trait::async_trait]
impl<EthApi> MevApiServer for MevApiServer<EthApi>
where
    EthApi: EthApiServer + Clone + Send + Sync + 'static,
{
    async fn mev_eth_call(
        &self,
        request: TransactionRequest,
        block_id: Option<BlockId>,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> RpcResult<Bytes> {
        // ── 路由判断 ──────────────────────────────────────────────────
        if !self.epoch_manager.matches_active(block_id) {
            // 旧块：原样委托原生 eth_call，由原生接口按 block_id 执行
            return self.eth_api
                .call(request, block_id, state_overrides, block_overrides)
                .await;
        }

        // ── API 层预处理（不需要 DB，纯值操作）───────────────────────
        let epoch = self.epoch_manager.current();
        let (evm_env, prepared_request) = self.prepare_evm_env(&epoch, request);

        // ── 打包 WorkerTask，分发到 Worker Pool ───────────────────────
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let task = WorkerTask {
            epoch,
            evm_env,                        // cfg_env 已设置，block_env 来自 epoch（待 Worker 应用 block_overrides）
            request: prepared_request,      // gas 已封顶，nonce 已清空
            block_overrides,                // Worker 负责应用（需 DB）
            state_overrides,                // Worker 负责应用（需 DB）
            kind: CallKind::Basic,
            result_tx,
        };

        self.worker_pool
            .dispatch(task)
            .map_err(|e| jsonrpsee::core::Error::Custom(e.to_string()))?;

        match result_rx.await {
            Ok(Ok(WorkerOutput::Basic(bytes))) => Ok(bytes),
            Ok(Err(WorkerError::Revert(data))) => {
                Err(reth_rpc_eth_types::EthApiError::Revert(data).into())
            }
            Ok(Err(e)) => Err(jsonrpsee::core::Error::Custom(e.to_string())),
            Err(_)     => Err(jsonrpsee::core::Error::Custom("worker dropped".into())),
        }
    }

    async fn mev_debug_trace_call(
        &self,
        request: TransactionRequest,
        block_id: Option<BlockId>,
        opts: Option<reth_rpc_eth_types::GethDebugTracingCallOptions>,
    ) -> RpcResult<reth_rpc_eth_types::GethTrace> {
        if !self.epoch_manager.matches_active(block_id) {
            return self.eth_api.debug_trace_call(request, block_id, opts.unwrap_or_default()).await;
        }
        let epoch = self.epoch_manager.current();
        let (evm_env, prepared_request) = self.prepare_evm_env(&epoch, request);
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let task = WorkerTask {
            epoch, evm_env, request: prepared_request,
            block_overrides: None, state_overrides: None,  // debug_traceCall 通常不带 overrides
            kind: CallKind::DebugTrace { opts: Box::new(opts.unwrap_or_default()) },
            result_tx,
        };
        self.worker_pool.dispatch(task).map_err(|e| jsonrpsee::core::Error::Custom(e.to_string()))?;
        match result_rx.await {
            Ok(Ok(WorkerOutput::DebugTrace(t))) => Ok(t),
            Ok(Err(e)) => Err(jsonrpsee::core::Error::Custom(e.to_string())),
            Err(_)     => Err(jsonrpsee::core::Error::Custom("worker dropped".into())),
        }
    }

    async fn mev_trace_call(
        &self,
        request: TransactionRequest,
        trace_types: Vec<alloy_rpc_types_trace::parity::TraceType>,
        block_id: Option<BlockId>,
    ) -> RpcResult<alloy_rpc_types_trace::parity::TraceResults> {
        // trace_types 从 Vec 统一转为 HashSet（与 CallKind 及原生 trace_call 一致）
        let trace_types_set: std::collections::HashSet<_> = trace_types.into_iter().collect();

        if !self.epoch_manager.matches_active(block_id) {
            return self.eth_api.trace_call(request, trace_types_set, block_id, None, None).await;
        }
        let epoch = self.epoch_manager.current();
        let (evm_env, prepared_request) = self.prepare_evm_env(&epoch, request);
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let task = WorkerTask {
            epoch, evm_env, request: prepared_request,
            block_overrides: None, state_overrides: None,
            kind: CallKind::ParityTrace { trace_types: trace_types_set },
            result_tx,
        };
        self.worker_pool.dispatch(task).map_err(|e| jsonrpsee::core::Error::Custom(e.to_string()))?;
        match result_rx.await {
            Ok(Ok(WorkerOutput::ParityTrace(t))) => Ok(t),
            Ok(Err(e)) => Err(jsonrpsee::core::Error::Custom(e.to_string())),
            Err(_)     => Err(jsonrpsee::core::Error::Custom("worker dropped".into())),
        }
    }
}
```

---

## 9. 注册入口实现（`crates/mev/src/lib.rs`）

`install_mev_rpc` 是提供给 `bin/reth/src/main.rs` 调用的闭包，符合 `NodeBuilder::extend_rpc_modules` 所需的签名（参考 `examples/node-custom-rpc`）：

```rust
// crates/mev/src/lib.rs

pub mod epoch;
pub mod worker;
pub mod provider;
pub mod api;

use reth_node_builder::rpc::RpcContext;
use reth_node_api::FullNodeComponents;

pub use worker::MevWorkerPool;
pub use worker::DEFAULT_POOL_SIZE;
pub use epoch::EpochManager;
pub use api::server::MevApiServer;

/// 传入 `NodeBuilder::extend_rpc_modules` 的注册闭包。
///
/// 用法（bin/reth/src/main.rs）：
/// ```rust
/// builder
///     .node(EthereumNode::default())
///     .extend_rpc_modules(reth_mev::install_mev_rpc)
///     .launch_with_debug_capabilities()
///     .await?;
/// ```
pub fn install_mev_rpc<Node, EthApi>(
    ctx: RpcContext<'_, Node, EthApi>,
) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    EthApi: reth_rpc_eth_api::EthApiTypes
        // call_gas_limit() / evm_memory_limit() 由 Call trait 提供
        + reth_rpc_eth_api::helpers::Call
        + Clone + Send + Sync + 'static,
    // 注：EthApi 还需实现具体 RpcServer trait 用于降级路由；
    //     编译时按错误提示追加 bound（通常是 EthApiServer<...>）。
{
    // RpcRegistry<Node, EthApi> 实现 Deref<Target = RpcRegistryInner<...>>，
    // 因此可以直接通过 ctx.registry 访问 provider() / evm_config()，
    // 无需访问 pub(crate) 的 ctx.node 字段。
    let provider   = ctx.registry.provider().clone();
    let evm_config = ctx.registry.evm_config().clone();

    // 创建 EpochManager（订阅 CanonStateNotifications，内部 tokio::spawn）
    let epoch_manager = EpochManager::spawn(provider.clone(), evm_config.clone());

    // 取原生 EthApi 实例（用于旧块降级路由，同时提取 gas_cap / memory_limit）
    // ctx.registry.eth_api() 通过 RpcRegistryInner::eth_api() 返回 &EthApi
    let eth_api = ctx.registry.eth_api().clone();

    // 从 eth_api 读取 gas_cap 和 memory_limit（与原生 eth_call 保持一致）
    // call_gas_limit() / evm_memory_limit() 是 reth_rpc_eth_api::helpers::Call trait 的默认方法：
    //   fn call_gas_limit(&self) -> u64 { self.inner.gas_cap() }
    //   fn evm_memory_limit(&self) -> u64 { self.inner.evm_memory_limit }
    let call_config = crate::api::server::MevCallConfig {
        call_gas_cap:       eth_api.call_gas_limit(),
        evm_memory_limit:   eth_api.evm_memory_limit(),
    };

    // 创建 Worker Pool（OS 线程，非 tokio task）
    let num_workers = std::env::var("MEV_WORKER_COUNT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_POOL_SIZE);
    let worker_pool = MevWorkerPool::new(num_workers, evm_config);

    // 构造 mev 模块并注册到所有传输层（http + ws + ipc）
    let mev_module = MevApiServer { epoch_manager, worker_pool, call_config, eth_api }.into_rpc();
    ctx.modules.merge_configured(mev_module)?;

    tracing::info!(
        target: "reth::mev",
        num_workers,
        call_gas_cap = %call_config.call_gas_cap,
        "mev RPC module installed (mev_eth_call / mev_debug_traceCall / mev_trace_call)"
    );

    Ok(())
}
```

**说明**：
- `ctx.modules.merge_configured()` 同时注册到 http / ws / ipc，不影响已有的 `eth_*` 方法。
- `install_mev_rpc` 函数签名与 `FnOnce(RpcContext<'_, Node, EthApi>) -> eyre::Result<()>` 兼容，可直接传给 `.extend_rpc_modules()`（参考官方 `node-custom-rpc` 示例的 `ctx.modules.merge_configured(ext.into_rpc())?`）。

---

## 10. 指标埋点

| 指标名 | 类型 | 说明 |
|---|---|---|
| `mev_worker_tasks_total` | Counter（label: `worker_id`）| 每个 worker 完成的任务总数 |
| `mev_worker_epoch_switches_total` | Counter | 全部 worker epoch 切换次数之和 |
| `mev_worker_l1_hits_total` | Counter | Worker-L1 缓存命中次数（account + storage 分开统计）|
| `mev_worker_l1_misses_total` | Counter | Worker-L1 缓存 miss 次数 |
| `mev_pool_queue_depth` | Gauge | 当前任务队列深度 |
| `mev_pool_queue_full_total` | Counter | 任务队列满导致拒绝的次数 |
| `mev_request_duration_ms` | Histogram（label: `method`, `path`）| 请求端到端耗时（ms）|
| `mev_epoch_switch_lag_ms` | Histogram | ChainCommitted 到 EpochManager 切换完成的延迟 |

在 `WorkerStateProvider` 的 `basic()` 和 `storage()` 中分别增加 hit/miss counter。

---

## 11. 错误处理策略

| 情形 | 处理方式 |
|---|---|
| `block_id` 为旧块 | 委托原生接口，原生接口报错则透传给客户端 |
| Worker 池队列满 | 返回 JSON-RPC `-32603 Internal error: worker pool queue full` |
| EVM revert | 返回标准 `execution reverted` 错误（带 revert data） |
| StateProvider 读取失败 | 返回 `-32603 Internal error: state provider error` |
| Worker 线程 panic | crossbeam channel 接收端感知（recv 返回 Err），向客户端返回 `-32603`；应添加 panic hook 打印堆栈 |
| EpochManager 任务中断 | 日志 error 级别；此时 active_epoch 不再更新，所有请求会降级到旧块路径或报错 |

---

## 12. Phase 1 不需要修改的 Reth 原有文件

Phase 1 对 Reth 原有代码**零逻辑修改**，仅可能需要以下最小变更：

| 文件 | 可能的变更 | 原因 |
|---|---|---|
| `crates/ethereum/evm/src/lib.rs` | 将 `EthEvmConfig` 某内部字段或方法加 `pub` | 如果 Phase 1 需要访问当前不可见的 spec_id 获取逻辑 |
| `crates/rpc/rpc/src/eth/core.rs` | 将 `EthApiInner` 某方法加 `pub` | 如果 fallback 路由需要直接调用 EthApi 内部逻辑（通常不需要，通过 trait 接口调用即可）|
| 节点入口文件（`bin/` 下）| 调用 `install_mev_rpc()` | 注册 mev 模块 |

原则：若通过已有 public trait 接口无法实现，**只加 `pub` 修饰，不改逻辑**。

---

## 13. 实现顺序（建议）

按以下顺序实现，每步可独立编译验证：

1. **`epoch.rs`**：实现 `EpochContext`（含 `placeholder()`）和 `EpochManager::spawn`  
   - 验收：写单测订阅 `TestCanonStateSubscriptions`，推入 `Commit` 通知，断言 `current()` 返回新 epoch

2. **`worker/cache.rs`**：实现 `WorkerL1Cache`  
   - 验收：单测 `reset()` 行为，确认清空后 epoch_id 更新

3. **`provider.rs`**：实现 `WorkerStateProvider`  
   - 验收：构造 mock StateProvider，首次读走 DB（hit counter +1），二次读走 L1（miss counter 不变），切 epoch 后重新走 DB

4. **`worker/worker.rs`**：实现 `MevWorker::run` 主循环（先实现 `exec_basic`）  
   - 验收：向 worker 发送任务，断言结果与直接调用 `revm` 执行一致

5. **`worker/mod.rs`**：实现 `MevWorkerPool`  
   - 验收：压测 `dispatch_batch` 256 条任务，断言结果顺序与提交顺序一致

6. **`api/mod.rs` + `api/server.rs`**：实现 RPC trait 和服务端  
   - 验收：e2e 测试：启动 reth 节点（reth_node_builder 测试框架），发送 `mev_eth_call` JSON-RPC batch，结果与 `eth_call` 一致

7. **`lib.rs`**：实现注册入口 `install_mev_rpc`  
   - 验收：节点启动后通过 `curl` 调用 `mev_eth_call`，HTTP 200

8. **指标**：在每个关键路径补充 `metrics::counter!` / `metrics::histogram!`  
   - 验收：Prometheus scrape 可见所有指标

---

## 14. 关键测试用例

```rust
// tests/integration.rs（或 crates/mev/tests/）

/// mev_eth_call 结果与 eth_call 一致
#[tokio::test]
async fn test_mev_eth_call_matches_native() { ... }

/// 旧块请求降级，结果正确
#[tokio::test]
async fn test_old_block_fallback() { ... }

/// 同一 epoch 第二次相同请求，Worker-L1 命中率 > 0
#[tokio::test]
async fn test_worker_l1_cache_warmup() { ... }

/// 切块后 Worker-L1 清空，不复用旧 epoch 数据
#[tokio::test]
async fn test_epoch_switch_clears_l1() { ... }

/// Worker pool 队列满时，返回明确错误而不是 hang
#[tokio::test]
async fn test_pool_queue_full_returns_error() { ... }

/// 并发 256 条请求，结果与顺序提交一致
#[tokio::test]
async fn test_concurrent_dispatch_ordering() { ... }
```

---

## 15. 与 Phase 2 的接口边界

Phase 1 为 Phase 2 预留以下扩展点：

| 位置 | Phase 1 行为 | Phase 2 扩展 |
|---|---|---|
| `WorkerStateProvider::basic()` | Worker-L1 miss → DB | Worker-L1 miss → GlobalSharedCache → DB |
| `MevWorker::switch_epoch()` | 清空 L1 | 清空 L1；通知 GlobalReadCache 做 Eager Prefetch |
| `WorkerL1Cache::bytecodes` | 切块清空 | Phase 2 可跨 epoch 保留（code_hash 不可变）|
| `MevWorkerPool::dispatch_batch` | 顺序等待 | Phase 3 可替换为 IPC 批量协议 |

Phase 2 实现时只需：
1. 新增 `GlobalSharedCache`，修改 `WorkerStateProvider` 的 miss 路径
2. 修改 `MevWorker::switch_epoch` 触发 Eager Prefetch  
3. 不需要改动 RPC trait、EpochManager、Worker 主循环结构

---

## 附录：Codex 实现 Prompt（直接使用）

> 将以下内容完整粘贴给 Codex 5.3，让其读取本文档后执行实现。

```
# Task: Implement Reth MEV Path Simulation — Phase 1

## Context

You are implementing a new Rust crate `crates/mev/` inside the Reth Ethereum execution client.
This crate adds a high-performance EVM worker pool and three new JSON-RPC methods
(`mev_eth_call`, `mev_debug_traceCall`, `mev_trace_call`) as drop-in replacements for their
native equivalents, with significantly lower per-call overhead via a persistent worker pool
and a per-worker L1 read cache.

## Required Reading (read these files first, in order)

1. `doc/Reth_simulate_optimize_phase1.md`  — **primary spec**, read every section
2. `doc/mev-path-simulation-architecture-v3.md` — background architecture, skim for context

## What to Implement

Create all files listed in §2.1 of the spec:

  crates/mev/Cargo.toml              (§2.1.1)
  crates/mev/src/lib.rs              (§9)
  crates/mev/src/epoch.rs            (§4.1, §4.2, §6)
  crates/mev/src/worker/mod.rs       (§4.3, §7)
  crates/mev/src/worker/cache.rs     (§4.4)
  crates/mev/src/worker/worker.rs    (§5)
  crates/mev/src/provider.rs         (§4.5)
  crates/mev/src/api/mod.rs          (§8.1)
  crates/mev/src/api/server.rs       (§8.2)
  crates/mev/src/api/types.rs        (CallKind enum)

And modify these existing files as described in §2.2:

  Cargo.toml               (add crates/mev to [workspace.members])
  bin/reth/Cargo.toml      (add reth-mev dependency)
  bin/reth/src/main.rs     (add .extend_rpc_modules(reth_mev::install_mev_rpc))

## Implementation Rules

- Do not modify any file outside the list above. The Minimal Intrusion principle is strict.
- Follow each section of the spec precisely. The spec contains complete, compilable Rust
  code for every struct, trait, and function — transcribe it faithfully.
- For `PlaceholderProviderFactory` in `epoch.rs`: implement all methods required by the
  `StateProviderFactory` trait; each method body should be `unimplemented!("placeholder")`.
- Run `cargo +nightly fmt --all` after generating all files.
- The crate must compile with `cargo check -p reth-mev --all-features`.

## Key Design Constraints (do not deviate)

- Workers are OS threads (`std::thread::spawn`), NOT tokio tasks.
- The `WorkerStateProvider` read path is:
    Worker-L1 hit → miss → StateProvider(DB) → backfill L1.
- API layer does all pure-value preprocessing (`cfg_env` flags, gas cap, nonce clear)
  BEFORE sending the task to the worker. Workers never touch those fields.
- Old-block requests (`block_id` != active epoch) are routed to the native `eth_api`
  fallback WITHOUT entering the worker pool.
- `exec_basic`, `exec_debug_trace`, `exec_parity_trace` all use the `&mut *db` reborrow
  pattern (see §5 "Reborrow 模式说明").

## Success Criteria

1. `cargo check -p reth-mev` passes with no errors.
2. `mev_eth_call` returns bytes identical to `eth_call` for the same input.
3. `mev_debug_traceCall` returns a `GethTrace` identical to `debug_traceCall`.
4. `mev_trace_call` returns `TraceResults` identical to `trace_call`.
5. Old-block requests degrade gracefully to the native API.
6. No changes to any file outside the list in §2.2.
```
