use alloy_primitives::map::HashSet;
use alloy_rpc_types_trace::{geth::GethDebugTracingCallOptions, parity::TraceType};
use serde::Serialize;

/// Worker 执行的调用类型。
#[derive(Debug)]
pub enum CallKind {
    /// `mev_eth_call`
    Basic,
    /// `mev_debug_traceCall`
    DebugTrace { opts: Box<GethDebugTracingCallOptions> },
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
    /// EIP-1559 base fee（单位 wei），pre-London 块为 null。
    pub base_fee_per_gas: Option<u64>,
    /// 本 block 中可能有价格变化的 raw pool 标识列表。
    pub changed_raw_ids: Vec<String>,
}
