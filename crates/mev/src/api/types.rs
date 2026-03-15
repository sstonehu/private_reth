use alloy_primitives::map::HashSet;
use alloy_rpc_types_trace::{geth::GethDebugTracingCallOptions, parity::TraceType};

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
