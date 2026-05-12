pub mod server;
pub mod types;

use alloy_primitives::Bytes;
use alloy_rpc_types_eth::{state::StateOverride, BlockId, BlockOverrides, TransactionRequest};
use alloy_rpc_types_trace::{
    geth::{GethDebugTracingCallOptions, GethTrace},
    parity::{TraceResults, TraceType},
};
use jsonrpsee::proc_macros::rpc;

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
    // mev_subscribe 订阅通过 lib.rs 中的 register_subscription 手动注册，
    // 以实现订阅方法名（mev_subscribe）与通知方法名（mev_subscription）分离，
    // 兼容 go-ethereum rpc.Client.Subscribe 的 {ns}_subscription 通知约定。
}
