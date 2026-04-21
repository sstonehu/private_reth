# MEV Phase 5 详细设计：`mev_debug_traceCall` 支持 `withAccessList`

> 版本：v1  
> 依赖：Phase 4（过期请求快速拒绝已上线）  
> 目标：在 `mev_debug_traceCall` 单次执行中零开销获取 EIP-2930 access list，供 `tryArbiBatchDirect` 阶段附加到上链 tx

---

## 1. 背景与动机

### 1.1 业务需求

MEV bot 的 `tryArbiBatchDirect` 阶段在模拟 `direct` / `dynamic` 路径时，已经调用 `mev_debug_traceCall` 进行最终验证。这次 EVM 执行与实际上链的 tx 完全等价（EOA sender、ProxyAddress / SimulateAddress target、最终 calldata 含 approve bytes）。

若能在同一次执行中顺带获取 EIP-2930 access list，可直接附加到上链 tx，降低冷存储槽的 gas 消耗，无需额外 RPC 调用，也无需再运行一次 EVM。

### 1.2 实现原理：res.state 已是 warm set 的载体

revm 的 `transact()` 返回 `ResultAndState`，其中 `state: EvmState`（即 `HashMap<Address, Account>`）包含本次执行**所有被触达**的账户及存储槽——这是 EIP-2929 gas 计量必然维护的结构。

在 `exec_debug_trace` 执行完毕后遍历一次 `res.state`，即可提取 access list：

```
有 withAccessList：  EVM transact() → 遍历 res.state（5~50µs）→ 返回 trace + accessList
无 withAccessList：  EVM transact() → 丢弃 res.state            → 返回 trace
```

遍历 10~15 个地址、20~120 个 storage entry 的开销在 **5~50 微秒（< 0.1ms）**，远低于 EVM 执行本身（数十 ms），可视为零额外开销。

> 详细原理和方案对比见主架构文档附录"附录：accessList 支持（`tryArbiBatchDirect` 阶段）"。

### 1.3 收集阶段选择：为何在 `tryArbiBatchDirect`，而非 `mid1`

| 阶段 | sender | target | calldata | 适合收集 access list？ |
|------|--------|--------|----------|----------------------|
| `mid1` | `util.TESTER`（simulator）| `cd.Target` | `cd.EncodeData`（含 logAmounts、双路径 approve 检测等 simulator 特有分支） | ❌ calldata 含 simulator 分支逻辑，不等价于上链 tx；`ProxyAddress` 从未被调用，warm set 不完整 |
| `tryArbiBatchDirect` | `util.EOA`（真实 sender）| `ProxyAddress` / `SimulateAddress` | `r.DirectCallData` / `r.DynamicCallData`（含真实 approve bytes） | ✅ 与实际上链 tx 完全一致，warm set 语义正确 |

---

## 2. 设计概览

### 2.1 变更范围

所有改动限定在 `crates/mev/` 内，**零侵入 Reth 已有 crate**。

共修改 **5 个文件**，新增 **0 个文件**：

| 文件 | 改动类型 | 说明 |
|------|----------|------|
| `crates/mev/Cargo.toml` | 新增依赖 | `serde_json` |
| `crates/mev/src/api/types.rs` | 修改 | 新增 `MevDebugTracingCallOptions`；`CallKind::DebugTrace` 加 `with_access_list` 字段 |
| `crates/mev/src/api/mod.rs` | 修改 | RPC trait：`opts` 类型和返回类型更新 |
| `crates/mev/src/worker/mod.rs` | 修改 | `WorkerOutput::DebugTrace` 追加 `Option<AccessList>` |
| `crates/mev/src/worker/worker.rs` | 修改 | `exec_debug_trace` 加 `with_access_list` 参数；执行后遍历 `res.state` |
| `crates/mev/src/api/server.rs` | 修改 | 提取 `with_access_list`；更新 `dispatch` 和结果匹配；序列化注入 `accessList` 字段 |

### 2.2 RPC 协议变化（向后兼容）

**请求**（opts map 新增可选字段，旧调用不传即可）：

```jsonc
// 现有调用（不变）
{
  "tracer": "callTracer",
  "tracerConfig": { "onlyTopCall": true }
}

// Phase 5 新增用法（withAccessList=true）
{
  "tracer": "callTracer",
  "tracerConfig": { "onlyTopCall": true },
  "withAccessList": true
}
```

**响应**（不传 `withAccessList` 时响应结构完全不变）：

```jsonc
// withAccessList=false（默认）：与现有完全一致
{
  "type": "CALL",
  "gasUsed": "0x...",
  "output": "0x..."
}

// withAccessList=true：追加 accessList 字段
{
  "type": "CALL",
  "gasUsed": "0x...",
  "output": "0x...",
  "accessList": [
    {
      "address": "0xAbCd...",
      "storageKeys": ["0x0000...0001", "0x0000...0002"]
    }
  ]
}
```

---

## 3. 组件改动（逐文件）

### 3.1 `crates/mev/Cargo.toml`

在 `[dependencies]` 的 `serde` 行之后添加：

```toml
serde_json.workspace = true
```

---

### 3.2 `crates/mev/src/api/types.rs`

**改动一：新增 `MevDebugTracingCallOptions` 包装类型**

这是解决 alloy `GethDebugTracingCallOptions` 没有扩展字段的关键设计。通过 `#[serde(flatten)]` 吸收所有原有字段，再追加 `with_access_list`。

在文件顶部 `use` 语句中添加导入：

```rust
use alloy_rpc_types_trace::geth::GethDebugTracingCallOptions;
use serde::Deserialize;
```

在 `CallKind` enum 定义**之前**插入：

```rust
/// `mev_debug_traceCall` 的请求选项，在标准 [`GethDebugTracingCallOptions`] 基础上
/// 新增 MEV 扩展字段。
///
/// 使用 `#[serde(flatten)]` 吸收所有原有 alloy 字段，追加 `withAccessList` 扩展字段。
/// 序列化/反序列化与原有 `GethDebugTracingCallOptions` 完全兼容；
/// 旧调用方不传 `withAccessList` 时该字段默认为 `false`，响应中不携带 `accessList`。
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MevDebugTracingCallOptions {
    /// 标准 alloy trace 选项（tracer、tracerConfig、stateOverrides、blockOverrides、timeout 等）。
    #[serde(flatten)]
    pub inner: GethDebugTracingCallOptions,

    /// 若为 `true`，执行完毕后遍历 `res.state` 提取 EIP-2930 access list 并附加到响应。
    /// 开销 < 0.1ms（5~50µs），对 EVM 执行路径零影响。
    /// 默认 `false`；旧调用方无需改动。
    #[serde(default)]
    pub with_access_list: bool,
}
```

**改动二：`CallKind::DebugTrace` 新增 `with_access_list` 字段**

将现有：

```rust
/// `mev_debug_traceCall`
DebugTrace { opts: Box<GethDebugTracingCallOptions> },
```

替换为：

```rust
/// `mev_debug_traceCall`
DebugTrace {
    opts: Box<GethDebugTracingCallOptions>,
    /// 若为 true，worker 在执行后从 res.state 提取 access list 随 trace 一起返回。
    with_access_list: bool,
},
```

**改动后完整文件**（供 Codex 参考，保持其余内容不变）：

```rust
use alloy_primitives::map::HashSet;
use alloy_rpc_types_trace::{geth::GethDebugTracingCallOptions, parity::TraceType};
use serde::{Deserialize, Serialize};

/// `mev_debug_traceCall` 的请求选项，在标准 [`GethDebugTracingCallOptions`] 基础上
/// 新增 MEV 扩展字段。
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MevDebugTracingCallOptions {
    #[serde(flatten)]
    pub inner: GethDebugTracingCallOptions,
    #[serde(default)]
    pub with_access_list: bool,
}

/// Worker 执行的调用类型。
#[derive(Debug)]
pub enum CallKind {
    /// `mev_eth_call`
    Basic,
    /// `mev_debug_traceCall`
    DebugTrace {
        opts: Box<GethDebugTracingCallOptions>,
        /// 若为 true，worker 在执行后从 res.state 提取 access list 随 trace 一起返回。
        with_access_list: bool,
    },
    /// `mev_trace_call`
    ParityTrace { trace_types: HashSet<TraceType> },
}

// MevNewBlock 保持不变（略）
```

---

### 3.3 `crates/mev/src/api/mod.rs`

**改动一：导入 `MevDebugTracingCallOptions`**

在文件顶部修改 `use` 块：

```rust
// 原有
use alloy_rpc_types_trace::{
    geth::{GethDebugTracingCallOptions, GethTrace},
    parity::{TraceResults, TraceType},
};

// Phase 5 替换：移除 GethDebugTracingCallOptions（改为使用包装类型）
use alloy_rpc_types_trace::{
    parity::{TraceResults, TraceType},
};
use crate::api::types::MevDebugTracingCallOptions;
```

**改动二：更新 `mev_debug_trace_call` 签名**

将：

```rust
#[method(name = "debug_traceCall")]
async fn mev_debug_trace_call(
    &self,
    request: TransactionRequest,
    block_id: Option<BlockId>,
    opts: Option<GethDebugTracingCallOptions>,
) -> jsonrpsee::core::RpcResult<GethTrace>;
```

替换为：

```rust
/// 等价于 `debug_traceCall`，执行路径走 worker pool。
///
/// Phase 5 扩展：opts 中可传入 `"withAccessList": true`，响应中将追加 `accessList` 字段
/// （EIP-2930 格式）。开销 < 0.1ms；不传或传 `false` 时响应与原有完全一致。
#[method(name = "debug_traceCall")]
async fn mev_debug_trace_call(
    &self,
    request: TransactionRequest,
    block_id: Option<BlockId>,
    opts: Option<MevDebugTracingCallOptions>,
) -> jsonrpsee::core::RpcResult<serde_json::Value>;
```

> **返回类型从 `GethTrace` 改为 `serde_json::Value` 的原因**：`GethTrace` 是枚举，不同 tracer 序列化结构各异，无法通过 `#[serde(flatten)]` 向其注入额外字段。改为 `serde_json::Value` 后，先完成 `GethTrace` 的正常序列化，再在 map 层插入 `accessList` key，保持原有结构完全不变，仅追加字段——对所有现有调用方完全透明（Go `json.Unmarshal` 对额外字段静默忽略）。

---

### 3.4 `crates/mev/src/worker/mod.rs`

**改动：`WorkerOutput::DebugTrace` 追加 `Option<AccessList>`**

在文件顶部导入：

```rust
use alloy_eips::eip2930::AccessList;
```

将：

```rust
#[derive(Debug)]
pub enum WorkerOutput {
    Basic(Bytes),
    DebugTrace(GethTrace),
    ParityTrace(TraceResults),
}
```

替换为：

```rust
#[derive(Debug)]
pub enum WorkerOutput {
    Basic(Bytes),
    /// debug trace 结果。
    /// 若 `with_access_list=true` 则携带从 `res.state` 提取的 EIP-2930 access list，否则为 `None`。
    DebugTrace(GethTrace, Option<AccessList>),
    ParityTrace(TraceResults),
}
```

---

### 3.5 `crates/mev/src/worker/worker.rs`

**改动一：更新 `exec_debug_trace` 调用点**

在 `execute_task` 的 match 分支中，将：

```rust
CallKind::DebugTrace { opts } => {
    Self::exec_debug_trace(&evm_config, &mut db, evm_env, tx_env, opts)
}
```

替换为：

```rust
CallKind::DebugTrace { opts, with_access_list } => {
    Self::exec_debug_trace(&evm_config, &mut db, evm_env, tx_env, opts, *with_access_list)
}
```

**改动二：更新 `exec_debug_trace` 函数签名与实现**

在文件顶部导入（与现有 imports 合并）：

```rust
use alloy_eips::eip2930::{AccessList, AccessListItem};
use alloy_primitives::B256;
```

将现有函数：

```rust
fn exec_debug_trace(
    evm_config: &EthEvmConfig,
    db: &mut WorkerStateDb<'_>,
    evm_env: super::EthEvmEnv,
    tx_env: EthTxEnv,
    opts: &alloy_rpc_types_trace::geth::GethDebugTracingCallOptions,
) -> Result<WorkerOutput, WorkerError> {
    let mut inspector = DebugInspector::new(opts.tracing_options.clone())
        .map_err(|err| WorkerError::Inspect(format!("{err:?}")))?;

    let res = evm_config
        .evm_with_env_and_inspector(&mut *db, evm_env.clone(), &mut inspector)
        .transact(tx_env.clone())
        .map_err(|err| WorkerError::Evm(format!("{err:?}")))?;

    let trace = inspector
        .get_result(None, &tx_env, &evm_env.block_env, &res, db)
        .map_err(|err| WorkerError::Inspect(format!("{err:?}")))?;

    Ok(WorkerOutput::DebugTrace(trace))
}
```

替换为：

```rust
fn exec_debug_trace(
    evm_config: &EthEvmConfig,
    db: &mut WorkerStateDb<'_>,
    evm_env: super::EthEvmEnv,
    tx_env: EthTxEnv,
    opts: &alloy_rpc_types_trace::geth::GethDebugTracingCallOptions,
    with_access_list: bool,
) -> Result<WorkerOutput, WorkerError> {
    let mut inspector = DebugInspector::new(opts.tracing_options.clone())
        .map_err(|err| WorkerError::Inspect(format!("{err:?}")))?;

    let res = evm_config
        .evm_with_env_and_inspector(&mut *db, evm_env.clone(), &mut inspector)
        .transact(tx_env.clone())
        .map_err(|err| WorkerError::Evm(format!("{err:?}")))?;

    let trace = inspector
        .get_result(None, &tx_env, &evm_env.block_env, &res, db)
        .map_err(|err| WorkerError::Inspect(format!("{err:?}")))?;

    // res.state 包含本次执行所有被触达的账户与存储槽（EIP-2929 warm set 的载体）。
    // 遍历一次即可得到 EIP-2930 access list，无需额外 EVM 执行或 per-opcode hook。
    // 预编译合约地址不会出现在 res.state 中（它们走 warm_preloaded_addresses 早路径，
    // 不经过 load_account_with_code，因此不写入 JournaledState.state），自然被过滤。
    let access_list = if with_access_list {
        let items: Vec<AccessListItem> = res
            .state
            .iter()
            .map(|(addr, acc)| AccessListItem {
                address: *addr,
                storage_keys: acc
                    .storage
                    .keys()
                    .map(|slot| B256::from(*slot))
                    .collect(),
            })
            .collect();
        Some(AccessList(items))
    } else {
        None
    };

    Ok(WorkerOutput::DebugTrace(trace, access_list))
}
```

---

### 3.6 `crates/mev/src/api/server.rs`

涉及三处改动。

**改动一：更新导入**

在顶部 `use` 语句中：

```rust
// 移除（后面不再直接用 GethDebugTracingCallOptions）
use alloy_rpc_types_trace::{
    geth::{GethDebugTracingCallOptions, GethTrace},
    ...
};

// 改为
use alloy_rpc_types_trace::{
    geth::GethTrace,           // 仍需用于降级路径
    parity::{TraceResults, TraceType},
    tracerequest::TraceCallRequest,
};
use crate::api::types::MevDebugTracingCallOptions;
```

同时添加：

```rust
use serde_json;
```

**改动二：更新 `mev_debug_trace_call` 方法签名和实现**

> **完整替换**，找到下面的方法并整体替换：

将 `async fn mev_debug_trace_call(...)` 整体替换如下（行范围参考：Phase 4 后为约 179~255 行）：

```rust
async fn mev_debug_trace_call(
    &self,
    request: TransactionRequest,
    block_id: Option<BlockId>,
    opts: Option<MevDebugTracingCallOptions>,
) -> RpcResult<serde_json::Value> {
    let t0 = Instant::now();
    let c = &self.counters.debug_trace_call;
    metrics::record_request(method::DEBUG_TRACE, c);

    if !self.epoch_manager.matches_active(block_id) {
        let gap = self.epoch_manager.block_gap(block_id);
        if !self.reject_stale_call {
            metrics::record_degraded_path(method::DEBUG_TRACE, c);
            metrics::record_degraded_gap(method::DEBUG_TRACE, gap);
            // 降级路径：将包装类型内部的 GethDebugTracingCallOptions 传给原生接口
            let inner_opts = opts.map(|o| o.inner).unwrap_or_default();
            let result = self
                .debug_api
                .debug_trace_call(request, block_id, inner_opts)
                .await
                .map_err(Into::into);
            metrics::record_e2e_latency(method::DEBUG_TRACE, t0.elapsed());
            // 降级结果序列化为 Value（无 accessList）
            return result.and_then(|trace| {
                serde_json::to_value(&trace).map_err(|e| internal_rpc_err(e.to_string()))
            });
        }
        if gap != Some(1) {
            // gap >= 2 or non-number block_id: fast rejection.
            metrics::record_epoch_mismatch(method::DEBUG_TRACE, gap);
            metrics::record_e2e_latency(method::DEBUG_TRACE, t0.elapsed());
            return Err(epoch_mismatch_error(
                block_id,
                self.epoch_manager.active_block_number(),
                gap,
            ));
        }
        // gap == Some(1): drain — fall through to worker path.
        metrics::record_epoch_mismatch(method::DEBUG_TRACE, gap);
    }

    metrics::record_worker_path(method::DEBUG_TRACE, c);

    // 拆解包装类型，提取 with_access_list 和内部 opts
    let mev_opts = opts.unwrap_or_default();
    let with_access_list = mev_opts.with_access_list;
    let inner_opts = mev_opts.inner;

    let state_overrides = inner_opts.state_overrides.clone();
    let block_overrides = inner_opts.block_overrides.clone().map(Box::new);

    let epoch = self.epoch_manager.current();
    let (evm_env, prepared_request) = self.prepare_evm_env(&epoch, request);
    let tx_env: reth_evm::TxEnvFor<reth_evm_ethereum::EthEvmConfig> =
        self.eth_api.converter().tx_env(prepared_request, &evm_env).map_err(Into::into)?;

    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let task = WorkerTask {
        epoch,
        evm_env,
        tx_env,
        block_overrides,
        state_overrides,
        kind: CallKind::DebugTrace { opts: Box::new(inner_opts), with_access_list },
        result_tx,
    };

    self.worker_pool.dispatch(task).map_err(|err| internal_rpc_err(err.to_string()))?;

    let result = match result_rx.await {
        Ok(Ok(WorkerOutput::DebugTrace(trace, opt_al))) => {
            // 先将 GethTrace 序列化为 JSON Value，保留原有结构
            let mut json = serde_json::to_value(&trace)
                .map_err(|e| internal_rpc_err(e.to_string()))?;
            // 若 withAccessList=true 且 worker 成功提取，注入 accessList 字段
            if let (Some(al), serde_json::Value::Object(ref mut map)) = (opt_al, &mut json) {
                let al_value = serde_json::to_value(&al)
                    .map_err(|e| internal_rpc_err(e.to_string()))?;
                map.insert("accessList".to_string(), al_value);
            }
            Ok(json)
        }
        Ok(Err(err)) => {
            metrics::record_error(method::DEBUG_TRACE, "worker_error", c);
            Err(internal_rpc_err(err.to_string()))
        }
        Err(_) => {
            metrics::record_error(method::DEBUG_TRACE, "worker_dropped", c);
            Err(internal_rpc_err("worker dropped"))
        }
        _ => Err(internal_rpc_err("unexpected worker output")),
    };
    metrics::record_e2e_latency(method::DEBUG_TRACE, t0.elapsed());
    result
}
```

---

## 4. Go 侧改动（`go_service`）

### 4.1 新增 `AccessTuple` 类型

在合适的 types 文件（如 `core/simulator/types.go` 或 `util/rpc_types.go`）新增：

```go
// AccessTuple 对应 EIP-2930 access list 中的单条记录。
type AccessTuple struct {
    Address     string   `json:"address"`
    StorageKeys []string `json:"storageKeys"`
}
```

### 4.2 更新 `DebugTraceCallResult`

找到现有的 `DebugTraceCallResult` struct（在 `core/simulator/` 下），追加 `AccessList` 字段：

```go
type DebugTraceCallResult struct {
    GasUsed    string        `json:"gasUsed"`
    Output     string        `json:"output"`
    Error      string        `json:"error,omitempty"`
    AccessList []AccessTuple `json:"accessList,omitempty"`  // Phase 5 新增
}
```

`omitempty` 确保：
- 旧调用（不传 `withAccessList`）响应中无该字段时，反序列化后为 nil slice，不影响现有逻辑
- 新调用（传 `withAccessList: true`）可直接读取 `result.AccessList`

### 4.3 `tryArbiBatchDirect.go` 调用侧

在 `tryArbiBatchDirect` 的两次 `mev_debug_traceCall` 调用中，opts 内新增 `"withAccessList": true`：

```go
// 现有调用
opts := map[string]any{
    "tracer": "callTracer",
    "tracerConfig": map[string]any{
        "onlyTopCall": true,
    },
    "stateOverrides": stateOverrides,
    "blockOverrides": blockOverrides,
}

// Phase 5：追加 withAccessList
opts := map[string]any{
    "tracer": "callTracer",
    "tracerConfig": map[string]any{
        "onlyTopCall": true,
    },
    "stateOverrides": stateOverrides,
    "blockOverrides": blockOverrides,
    "withAccessList": true,  // 新增
}
```

解析响应后，读取 `result.AccessList` 字段，按路径（direct/dynamic）分别附加到最终上链 tx 的 `accessList` 字段。

> `mid1` 阶段的调用**不传 `withAccessList`**，响应中无该字段，Go 侧 `omitempty` 直接忽略，**零影响**。

---

## 5. 关键约束

1. **不新增 RPC 接口**：无 `mev_createAccessList`，无 `mev_debug_traceCallWithAccessList`
2. **不修改 Reth 已有代码**：所有改动限定在 `crates/mev/` 的 5 个文件内
3. **不使用 cache 统计近似 access list**：Worker-L1 / GlobalSharedCache 命中记录与 EVM warm set 语义不等价，禁止
4. **向后兼容**：`withAccessList` 默认 `false`，所有现有调用方（包括 `mid1` 阶段）无需变更
5. **降级路径正确性**：gap=1 drain 走 worker 路径时，`with_access_list` 正常传递；gap≥2 降级走原生 `debug_traceCall` 时，access list 不收集（降级结果的 access list 语义已不准确，不应使用）

---

## 6. 验收标准

### Reth 侧

- `cargo check -p reth-mev`：零错误零警告
- `cargo nextest run -p reth-mev`：全部通过
- `withAccessList=false`（或不传）：响应与 Phase 4 完全一致，`accessList` 字段不出现
- `withAccessList=true`：响应中出现 `accessList` 字段，格式为 EIP-2930 `[{address, storageKeys}]`
- 降级路径（`reject_stale_call=false` 或 gap=1 drain）：`with_access_list=false` 时行为不变

### Go 侧

- `mid1` 阶段调用：不传 `withAccessList`，响应反序列化后 `AccessList == nil`，现有逻辑不受影响
- `tryArbiBatchDirect` 调用：传 `withAccessList: true`，`result.AccessList` 非空，可附加到上链 tx

---

## 7. 实施顺序

```
Step 1（Reth 侧，本次 Codex 实施目标）
  ├─ Cargo.toml：添加 serde_json.workspace = true
  ├─ api/types.rs：新增 MevDebugTracingCallOptions；CallKind::DebugTrace 加 with_access_list
  ├─ api/mod.rs：更新 opts 类型为 MevDebugTracingCallOptions；返回类型为 serde_json::Value
  ├─ worker/mod.rs：WorkerOutput::DebugTrace 追加 Option<AccessList>；添加 alloy_eips import
  ├─ worker/worker.rs：exec_debug_trace 加 with_access_list 参数；遍历 res.state
  └─ api/server.rs：更新 mev_debug_trace_call 实现
  
Step 2（Go 侧，Reth 侧上线后）
  ├─ 新增 AccessTuple 类型
  ├─ DebugTraceCallResult 追加 AccessList 字段
  └─ tryArbiBatchDirect 调用时传 withAccessList: true，读取并附加到上链 tx
```

---

## 8. 类型速查

| 类型 / 函数 | 来源 |
|-------------|------|
| `AccessList` | `alloy_eips::eip2930` |
| `AccessListItem` | `alloy_eips::eip2930` |
| `B256` | `alloy_primitives` |
| `Account.storage` | `revm_state::Account`，类型为 `EvmStorage = HashMap<StorageKey, EvmStorageSlot>` |
| `StorageKey` | `primitives::StorageKey`（即 `U256`）|
| `B256::from(U256)` | `alloy_primitives`，32 字节 big-endian 转换 |
| `serde_json::to_value` | `serde_json` |
| `GethDebugTracingCallOptions` | `alloy_rpc_types_trace::geth` |
| `MevDebugTracingCallOptions` | `crate::api::types`（Phase 5 新增）|

---

## 9. 实施说明（2026-04-21）

### 9.1 已实施结果（Reth 侧）

Phase 5 已在 `crates/mev/` 完成落地，核心行为如下：

- `mev_debug_traceCall` 请求 `opts` 扩展为 `MevDebugTracingCallOptions`，支持 `withAccessList`（默认 `false`）。
- worker 侧在单次 `transact()` 后遍历 `res.state`，提取 `AccessList`，不增加额外 EVM 执行。
- RPC 返回类型改为 `serde_json::Value`，在 `withAccessList=true` 时向 trace JSON 追加 `accessList` 字段；默认无该字段。
- stale 降级路径（`MEV_REJECT_STALE_CALL=0`）保持兼容：仍走原生 `debug_trace_call`，并统一序列化为 `Value` 返回。

本次实际修改文件（Reth 侧）：

1. `crates/mev/Cargo.toml`
2. `crates/mev/src/api/types.rs`
3. `crates/mev/src/api/mod.rs`
4. `crates/mev/src/worker/mod.rs`
5. `crates/mev/src/worker/worker.rs`
6. `crates/mev/src/api/server.rs`

### 9.2 验证结果

- `cargo check -p reth-mev`：通过
- `cargo nextest run -p reth-mev`：通过（`8 passed, 0 skipped`）

### 9.3 与设计不一致/需澄清之处

1. **文件数量表述不一致**  
   本文 `10. Codex 实现 Prompt` 中写“共 5 个文件”，但同一段落的细分清单实际覆盖了 **6 个文件**（包含 `api/server.rs`）。工程落地按 6 个文件执行。

2. **文档内“4 个文件”表述已过期**  
   总架构文档附录中“Reth 侧实施细节”写“需修改 4 个文件”，与 Phase 5 详设不一致。实际 Reth 侧实现需要 `Cargo.toml` + `api/mod.rs` + 其余核心文件，总计 6 个。

3. **`serde_json::Value::Object` 的模式匹配写法差异（Rust 2024）**  
   设计示例中使用：
   `serde_json::Value::Object(ref mut map)`  
   实际编译环境（Rust 2024 绑定模式）需写为：
   `serde_json::Value::Object(map)`  
   二者语义等价，均为在对象上插入 `accessList` 字段。

### 9.4 正式代码评审结果（2026-04-21）

逐文件与设计文档对照评审，结论：**代码与设计完全一致，所有验收标准通过**。

#### 逐文件核对

| 文件 | 设计要求 | 实际代码 | 符合 |
|------|---------|----------|------|
| `Cargo.toml` | `serde_json.workspace = true` | 已添加 | ✅ |
| `api/types.rs` | `MevDebugTracingCallOptions`，`#[serde(flatten)]` + `with_access_list: bool` | 第 7-17 行，完全一致 | ✅ |
| `api/types.rs` | `CallKind::DebugTrace` 加 `with_access_list: bool` 字段 | 第 25-29 行 | ✅ |
| `api/mod.rs` | 移除 `GethDebugTracingCallOptions` import，引入包装类型 | 第 6、8 行 | ✅ |
| `api/mod.rs` | 返回类型 `RpcResult<serde_json::Value>` | 第 31 行 | ✅ |
| `worker/mod.rs` | `use alloy_eips::eip2930::AccessList` | 第 5 行 | ✅ |
| `worker/mod.rs` | `WorkerOutput::DebugTrace(GethTrace, Option<AccessList>)` | 第 54 行 | ✅ |
| `worker/worker.rs` | `AccessList`、`AccessListItem`、`B256` 导入 | 第 8-9 行 | ✅ |
| `worker/worker.rs` | `exec_debug_trace` 签名加 `with_access_list: bool` | 第 198 行 | ✅ |
| `worker/worker.rs` | `execute_task` 调用点 `*with_access_list` 解引用 | 第 153 行 | ✅ |
| `worker/worker.rs` | `res.state.iter()` 遍历生成 `AccessList`，`B256::from(*slot)` | 第 216-228 行 | ✅ |
| `api/server.rs` | 导入替换，方法签名更新 | 第 2、10-13、182-183 行 | ✅ |
| `api/server.rs` | 降级路径提取 `inner_opts`，结果序列化为 `Value` | 第 193、200-202 行 | ✅ |
| `api/server.rs` | `with_access_list` 拆解 + dispatch + `accessList` 注入 | 第 218-251 行 | ✅ |

#### 验收标准核查

| 验收项 | 结果 |
|--------|------|
| `cargo check -p reth-mev`：零错误零警告 | ✅ Finished in 6.85s |
| `cargo nextest run -p reth-mev`：全部通过 | ✅ 8/8 passed, 0 skipped |
| `withAccessList=false`（默认）：响应不含 `accessList` | ✅ `None` 分支不插入字段 |
| `withAccessList=true`：响应追加 `accessList` 字段 | ✅ 遍历 `res.state` 后插入 |
| 降级路径行为正确 | ✅ `inner_opts` 传原生接口，无 `accessList` |
| 不修改 Reth 已有 crate | ✅ 所有改动限定在 `crates/mev/` |
| 向后兼容（旧调用方不传 `withAccessList`） | ✅ `#[serde(default)]` 默认 `false` |

#### 与设计文档差异归纳

| 差异 | 说明 | 影响 |
|------|------|------|
| `Value::Object(map)` vs `Value::Object(ref mut map)` | Rust 2024 自动推断 `ref mut`，语义等价 | 无 |
| 文件数 "5 个" → 实际 6 个 | `Cargo.toml` 在正文"设计概览"中漏列，Codex Prompt 中已正确包含 | 无（文档小瑕疵，不影响实现） |
| 主架构文档附录遗留 `additional_fields` 草稿写法 | 评审时已同步修正为 `MevDebugTracingCallOptions` 方案 | 无 |

**Phase 5 Reth 侧实施完成。待 Go 侧（Step 2）完成后关闭本阶段。**

---

## 10. Codex 实现 Prompt

> 将以下 prompt 完整粘贴给 Codex，配合本文档和当前代码库使用。

---

````
你是一名 Rust 专家，正在为 Reth（高性能以太坊执行客户端）的 MEV 模块实现 Phase 5：
`mev_debug_traceCall` 支持 `withAccessList` 可选参数。

详细设计见：`doc/Reth_simulate_optimize_phase5.md`
总体架构见：`doc/mev-path-simulation-architecture-v3.md`

## 任务目标

在 `mev_debug_traceCall` 的单次 EVM 执行中，若请求 opts 中包含 `"withAccessList": true`，
则在执行完毕后遍历 `res.state`（已有的 EIP-2929 warm set 载体），提取 EIP-2930 access list
并注入响应 JSON。不传或传 `false` 时，响应与现有完全一致。

## 需要修改的文件（共 5 个）

所有改动限定在 `crates/mev/` 内，零侵入 Reth 已有 crate。

---

### 1. `crates/mev/Cargo.toml`

在 `[dependencies]` 的 `serde` 行之后添加：

```toml
serde_json.workspace = true
```

---

### 2. `crates/mev/src/api/types.rs`

#### 2.1 更新导入

在文件顶部添加（与已有 use 合并）：

```rust
use serde::Deserialize;
```

（`Serialize` 已通过 `use serde::Serialize` 存在，保持原样。）

#### 2.2 在 `CallKind` enum 定义**之前**插入新类型

```rust
/// `mev_debug_traceCall` 的请求选项，在标准 [`GethDebugTracingCallOptions`] 基础上
/// 新增 MEV 扩展字段。使用 `#[serde(flatten)]` 完全向后兼容原有调用方。
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MevDebugTracingCallOptions {
    /// 标准 alloy trace 选项（tracer、tracerConfig、stateOverrides、blockOverrides 等）。
    #[serde(flatten)]
    pub inner: GethDebugTracingCallOptions,
    /// 若为 `true`，执行后遍历 `res.state` 提取 EIP-2930 access list 附加到响应。
    /// 开销 < 0.1ms；默认 `false`，旧调用方无需改动。
    #[serde(default)]
    pub with_access_list: bool,
}
```

#### 2.3 修改 `CallKind::DebugTrace` variant

将：
```rust
DebugTrace { opts: Box<GethDebugTracingCallOptions> },
```
替换为：
```rust
DebugTrace {
    opts: Box<GethDebugTracingCallOptions>,
    /// 若为 true，worker 在执行后从 res.state 提取 access list 随 trace 一起返回。
    with_access_list: bool,
},
```

---

### 3. `crates/mev/src/api/mod.rs`

#### 3.1 更新导入

将：
```rust
use alloy_rpc_types_trace::{
    geth::{GethDebugTracingCallOptions, GethTrace},
    parity::{TraceResults, TraceType},
};
```
替换为：
```rust
use alloy_rpc_types_trace::{
    parity::{TraceResults, TraceType},
};
use crate::api::types::MevDebugTracingCallOptions;
```

#### 3.2 修改 `mev_debug_trace_call` 方法签名

将：
```rust
#[method(name = "debug_traceCall")]
async fn mev_debug_trace_call(
    &self,
    request: TransactionRequest,
    block_id: Option<BlockId>,
    opts: Option<GethDebugTracingCallOptions>,
) -> jsonrpsee::core::RpcResult<GethTrace>;
```
替换为：
```rust
/// 等价于 `debug_traceCall`，执行路径走 worker pool。
/// Phase 5：opts 中可传入 `"withAccessList": true`，响应追加 `accessList` 字段（EIP-2930）。
/// 不传或传 `false` 时响应与原有完全一致。
#[method(name = "debug_traceCall")]
async fn mev_debug_trace_call(
    &self,
    request: TransactionRequest,
    block_id: Option<BlockId>,
    opts: Option<MevDebugTracingCallOptions>,
) -> jsonrpsee::core::RpcResult<serde_json::Value>;
```

---

### 4. `crates/mev/src/worker/mod.rs`

#### 4.1 新增导入

在文件顶部（与已有 use 合并）添加：
```rust
use alloy_eips::eip2930::AccessList;
```

#### 4.2 修改 `WorkerOutput` enum

将：
```rust
#[derive(Debug)]
pub enum WorkerOutput {
    Basic(Bytes),
    DebugTrace(GethTrace),
    ParityTrace(TraceResults),
}
```
替换为：
```rust
#[derive(Debug)]
pub enum WorkerOutput {
    Basic(Bytes),
    /// debug trace 结果。第二个字段：`with_access_list=true` 时为从 `res.state` 提取的
    /// EIP-2930 access list，否则为 `None`。
    DebugTrace(GethTrace, Option<AccessList>),
    ParityTrace(TraceResults),
}
```

---

### 5. `crates/mev/src/worker/worker.rs`

#### 5.1 新增导入

在文件顶部（与已有 use 合并）添加：
```rust
use alloy_eips::eip2930::{AccessList, AccessListItem};
use alloy_primitives::B256;
```

#### 5.2 更新 `execute_task` 中的调用点

将：
```rust
CallKind::DebugTrace { opts } => {
    Self::exec_debug_trace(&evm_config, &mut db, evm_env, tx_env, opts)
}
```
替换为：
```rust
CallKind::DebugTrace { opts, with_access_list } => {
    Self::exec_debug_trace(&evm_config, &mut db, evm_env, tx_env, opts, *with_access_list)
}
```

#### 5.3 替换 `exec_debug_trace` 函数

将**整个** `exec_debug_trace` 函数替换为：

```rust
fn exec_debug_trace(
    evm_config: &EthEvmConfig,
    db: &mut WorkerStateDb<'_>,
    evm_env: super::EthEvmEnv,
    tx_env: EthTxEnv,
    opts: &alloy_rpc_types_trace::geth::GethDebugTracingCallOptions,
    with_access_list: bool,
) -> Result<WorkerOutput, WorkerError> {
    let mut inspector = DebugInspector::new(opts.tracing_options.clone())
        .map_err(|err| WorkerError::Inspect(format!("{err:?}")))?;

    let res = evm_config
        .evm_with_env_and_inspector(&mut *db, evm_env.clone(), &mut inspector)
        .transact(tx_env.clone())
        .map_err(|err| WorkerError::Evm(format!("{err:?}")))?;

    let trace = inspector
        .get_result(None, &tx_env, &evm_env.block_env, &res, db)
        .map_err(|err| WorkerError::Inspect(format!("{err:?}")))?;

    // res.state 包含本次执行所有被触达的账户与存储槽（EIP-2929 warm set 的载体）。
    // 遍历一次即得 EIP-2930 access list，无需额外 EVM 执行或 per-opcode hook（< 0.1ms）。
    // 预编译合约地址不会出现在 res.state 中（走 warm_preloaded_addresses 早路径，
    // 不经 load_account_with_code），自然被过滤，行为与 eth_createAccessList 一致。
    let access_list = if with_access_list {
        let items: Vec<AccessListItem> = res
            .state
            .iter()
            .map(|(addr, acc)| AccessListItem {
                address: *addr,
                storage_keys: acc.storage.keys().map(|slot| B256::from(*slot)).collect(),
            })
            .collect();
        Some(AccessList(items))
    } else {
        None
    };

    Ok(WorkerOutput::DebugTrace(trace, access_list))
}
```

---

### 6. `crates/mev/src/api/server.rs`

#### 6.1 更新导入

将：
```rust
use alloy_rpc_types_trace::{
    geth::{GethDebugTracingCallOptions, GethTrace},
    parity::{TraceResults, TraceType},
    tracerequest::TraceCallRequest,
};
```
替换为：
```rust
use alloy_rpc_types_trace::{
    geth::GethTrace,
    parity::{TraceResults, TraceType},
    tracerequest::TraceCallRequest,
};
use crate::api::types::MevDebugTracingCallOptions;
```

#### 6.2 整体替换 `mev_debug_trace_call` 方法

找到 `async fn mev_debug_trace_call(` 开头的整个方法体，完整替换为：

```rust
async fn mev_debug_trace_call(
    &self,
    request: TransactionRequest,
    block_id: Option<BlockId>,
    opts: Option<MevDebugTracingCallOptions>,
) -> RpcResult<serde_json::Value> {
    let t0 = Instant::now();
    let c = &self.counters.debug_trace_call;
    metrics::record_request(method::DEBUG_TRACE, c);

    if !self.epoch_manager.matches_active(block_id) {
        let gap = self.epoch_manager.block_gap(block_id);
        if !self.reject_stale_call {
            metrics::record_degraded_path(method::DEBUG_TRACE, c);
            metrics::record_degraded_gap(method::DEBUG_TRACE, gap);
            let inner_opts = opts.map(|o| o.inner).unwrap_or_default();
            let result = self
                .debug_api
                .debug_trace_call(request, block_id, inner_opts)
                .await
                .map_err(Into::into);
            metrics::record_e2e_latency(method::DEBUG_TRACE, t0.elapsed());
            return result.and_then(|trace| {
                serde_json::to_value(&trace).map_err(|e| internal_rpc_err(e.to_string()))
            });
        }
        if gap != Some(1) {
            metrics::record_epoch_mismatch(method::DEBUG_TRACE, gap);
            metrics::record_e2e_latency(method::DEBUG_TRACE, t0.elapsed());
            return Err(epoch_mismatch_error(
                block_id,
                self.epoch_manager.active_block_number(),
                gap,
            ));
        }
        metrics::record_epoch_mismatch(method::DEBUG_TRACE, gap);
    }

    metrics::record_worker_path(method::DEBUG_TRACE, c);

    let mev_opts = opts.unwrap_or_default();
    let with_access_list = mev_opts.with_access_list;
    let inner_opts = mev_opts.inner;

    let state_overrides = inner_opts.state_overrides.clone();
    let block_overrides = inner_opts.block_overrides.clone().map(Box::new);

    let epoch = self.epoch_manager.current();
    let (evm_env, prepared_request) = self.prepare_evm_env(&epoch, request);
    let tx_env: reth_evm::TxEnvFor<reth_evm_ethereum::EthEvmConfig> =
        self.eth_api.converter().tx_env(prepared_request, &evm_env).map_err(Into::into)?;

    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let task = WorkerTask {
        epoch,
        evm_env,
        tx_env,
        block_overrides,
        state_overrides,
        kind: CallKind::DebugTrace { opts: Box::new(inner_opts), with_access_list },
        result_tx,
    };

    self.worker_pool.dispatch(task).map_err(|err| internal_rpc_err(err.to_string()))?;

    let result = match result_rx.await {
        Ok(Ok(WorkerOutput::DebugTrace(trace, opt_al))) => {
            let mut json = serde_json::to_value(&trace)
                .map_err(|e| internal_rpc_err(e.to_string()))?;
            if let (Some(al), serde_json::Value::Object(ref mut map)) = (opt_al, &mut json) {
                let al_value = serde_json::to_value(&al)
                    .map_err(|e| internal_rpc_err(e.to_string()))?;
                map.insert("accessList".to_string(), al_value);
            }
            Ok(json)
        }
        Ok(Err(err)) => {
            metrics::record_error(method::DEBUG_TRACE, "worker_error", c);
            Err(internal_rpc_err(err.to_string()))
        }
        Err(_) => {
            metrics::record_error(method::DEBUG_TRACE, "worker_dropped", c);
            Err(internal_rpc_err("worker dropped"))
        }
        _ => Err(internal_rpc_err("unexpected worker output")),
    };
    metrics::record_e2e_latency(method::DEBUG_TRACE, t0.elapsed());
    result
}
```

---

## 关键约束

1. **不修改 Reth 已有代码**：所有改动限定在 `crates/mev/` 的 5 个文件内
2. **不引入新 RPC 接口**：仅扩展 `mev_debug_traceCall` 的 opts 参数
3. **向后兼容**：`withAccessList` 默认 `false`，所有现有调用方（mid1 等）无需改动
4. **降级路径正确**：`reject_stale_call=false` 时降级走原生接口，返回类型统一为 `serde_json::Value`，无 `accessList` 字段

## 验收标准

- `cargo check -p reth-mev`：零错误零警告
- `cargo nextest run -p reth-mev`：全部通过
- `withAccessList=false`（默认）：响应 JSON 与 Phase 4 完全一致，不含 `accessList` 字段
- `withAccessList=true`：响应 JSON 中追加 `accessList: [{address, storageKeys}]` 字段
- 编译时：`CallKind::DebugTrace { opts, with_access_list }` 模式匹配无遗漏
````
