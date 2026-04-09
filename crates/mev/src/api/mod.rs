pub mod server;
pub mod types;

use alloy_primitives::Bytes;
use alloy_rpc_types_eth::{state::StateOverride, BlockId, BlockOverrides, TransactionRequest};
use alloy_rpc_types_trace::{
    geth::{GethDebugTracingCallOptions, GethTrace},
    parity::{TraceResults, TraceType},
};
use jsonrpsee::proc_macros::rpc;

#[allow(unused_imports)]
use crate::api::types::MevNewBlock;

#[rpc(server, namespace = "mev")]
pub trait MevApi {
    /// 等价于 `eth_call`，执行路径走 worker pool。
    #[method(name = "eth_call")]
    async fn mev_eth_call(
        &self,
        request: TransactionRequest,
        block_id: Option<BlockId>,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> jsonrpsee::core::RpcResult<Bytes>;

    /// 等价于 `debug_traceCall`，执行路径走 worker pool。
    #[method(name = "debug_traceCall")]
    async fn mev_debug_trace_call(
        &self,
        request: TransactionRequest,
        block_id: Option<BlockId>,
        opts: Option<GethDebugTracingCallOptions>,
    ) -> jsonrpsee::core::RpcResult<GethTrace>;

    /// 等价于 `trace_call`，执行路径走 worker pool。
    #[method(name = "trace_call")]
    async fn mev_trace_call(
        &self,
        request: TransactionRequest,
        trace_types: Vec<TraceType>,
        block_id: Option<BlockId>,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> jsonrpsee::core::RpcResult<TraceResults>;

    /// 订阅每个新块的 block impact 数据（`changed_raw_ids`）。
    ///
    /// - 订阅方法：`mev_subscribeNewBlock`
    /// - 退订方法：`mev_unsubscribeNewBlock`
    /// - 推送类型：[`MevNewBlock`]
    ///
    /// Go 侧替换 `eth_subscribe("newHeads")` + `eth_getLogs`，直接消费此订阅。
    #[subscription(
        name = "subscribeNewBlock",
        unsubscribe = "unsubscribeNewBlock",
        item = MevNewBlock
    )]
    async fn subscribe_new_block(&self) -> jsonrpsee::core::SubscriptionResult;
}
