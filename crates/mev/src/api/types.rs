use alloy_primitives::map::HashSet;
use alloy_rpc_types_trace::{geth::GethDebugTracingCallOptions, parity::TraceType};
use serde::{Deserialize, Serialize};

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

/// `mev_subscribe("newBlock")` 推送给订阅者的每块数据。
///
/// `changed_raw_ids` 是当前 block 中价格可能变动的 pool 标识集合（原始格式），
/// 来源于：
///   - S1：bundle state diff 中所有有变更的合约地址（`0x` + 小写hex）
///   - S2/S3：共享 Vault/PoolManager log 中解析出的 poolAddress / poolId / dexId
///
/// Go 侧只需查 `BlockImpactIndex.changed_raw_ids ∩ pool.rawIds` 决定是否重计算。
#[derive(Debug, Clone, Serialize)]
pub struct MevNewBlock {
    /// 块高度。
    pub block_number: u64,
    /// 块哈希（`0x` + 64 hex chars）。
    pub block_hash: String,
    /// Unix 秒级时间戳。
    pub timestamp: u64,
    /// EIP-1559 当前块 base fee（单位 wei），pre-London 块为 null。
    pub base_fee_per_gas: Option<u64>,
    /// 下一块预测 base fee（单位 wei）。
    /// 由 reth 按 EIP-1559 公式 calc_next_block_base_fee(gas_used, gas_limit, base_fee) 计算，
    /// 精度与链上一致，Go 侧无需再估算。
    pub next_base_fee_per_gas: Option<u64>,
    /// 本 block 中可能有价格变化的 raw pool 标识列表。
    pub changed_raw_ids: Vec<String>,
}
