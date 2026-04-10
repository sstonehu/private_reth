use crate::{
    api::types::CallKind,
    epoch::{EpochContext, EpochManager},
    metrics::{self, method, MevCounters},
    worker::{MevWorkerPool, WorkerError, WorkerOutput, WorkerTask},
};
use alloy_network::TransactionBuilder;
use alloy_primitives::{map::HashSet, Bytes};
use alloy_rpc_types_eth::{state::StateOverride, BlockId, BlockOverrides, TransactionRequest};
use alloy_rpc_types_trace::{
    geth::{GethDebugTracingCallOptions, GethTrace},
    parity::{TraceResults, TraceType},
};
use jsonrpsee::core::RpcResult;
use reth_rpc_convert::{RpcConvert, RpcTypes};
use reth_rpc_eth_api::{
    helpers::{EthCall, EthTransactions, TraceExt},
    EthApiTypes, RpcNodeCore,
};
use reth_rpc_eth_types::{error::api::FromRevert, EthApiError};
use reth_rpc_server_types::result::internal_rpc_err;
use std::{sync::Arc, time::Instant};

/// 节点级配置，由 `install_mev_rpc` 读取构造。
#[derive(Debug, Clone, Copy)]
pub struct MevCallConfig {
    pub call_gas_cap: u64,
    pub evm_memory_limit: u64,
}

pub struct MevApiServer<EthApi: RpcNodeCore<Evm = reth_evm_ethereum::EthEvmConfig>> {
    pub epoch_manager: Arc<EpochManager>,
    pub worker_pool: Arc<MevWorkerPool>,
    pub call_config: MevCallConfig,
    pub counters: Arc<MevCounters>,
    pub eth_api: EthApi,
    pub debug_api: reth_rpc::DebugApi<EthApi>,
    pub trace_api: reth_rpc::TraceApi<EthApi>,
    /// Phase 4: if true, mev_eth_call with stale block_id returns -39001 immediately
    /// instead of degrading to the native eth_call path.
    /// Controlled by MEV_REJECT_STALE_CALL env var (default: true).
    pub reject_stale_call: bool,
}

impl<EthApi: RpcNodeCore<Evm = reth_evm_ethereum::EthEvmConfig>> std::fmt::Debug
    for MevApiServer<EthApi>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MevApiServer")
            .field("epoch_manager", &self.epoch_manager)
            .field("worker_pool", &self.worker_pool)
            .field("call_config", &self.call_config)
            .field("counters", &self.counters)
            .field("reject_stale_call", &self.reject_stale_call)
            .finish_non_exhaustive()
    }
}

impl<EthApi> MevApiServer<EthApi>
where
    EthApi: RpcNodeCore<Evm = reth_evm_ethereum::EthEvmConfig>,
{
    fn prepare_evm_env(
        &self,
        epoch: &EpochContext,
        mut request: TransactionRequest,
    ) -> (reth_evm::EvmEnvFor<reth_evm_ethereum::EthEvmConfig>, TransactionRequest) {
        let mut evm_env = epoch.block_env.clone();

        evm_env.cfg_env.disable_block_gas_limit = true;
        evm_env.cfg_env.disable_eip3607 = true;
        evm_env.cfg_env.disable_base_fee = true;
        evm_env.cfg_env.tx_gas_limit_cap = Some(u64::MAX);
        evm_env.cfg_env.disable_fee_charge = true;
        evm_env.cfg_env.memory_limit = self.call_config.evm_memory_limit;

        let cap = self.call_config.call_gas_cap;
        match request.gas {
            Some(gas) if cap != 0 && cap < gas => request.set_gas_limit(cap),
            None => request.set_gas_limit(cap),
            _ => {}
        }

        request.take_nonce();
        (evm_env, request)
    }
}

#[async_trait::async_trait]
impl<EthApi> crate::api::MevApiServer for MevApiServer<EthApi>
where
    EthApi: RpcNodeCore<Evm = reth_evm_ethereum::EthEvmConfig>
        + EthApiTypes<
            NetworkTypes: RpcTypes<TransactionRequest = TransactionRequest>,
            RpcConvert: RpcConvert<
                Evm = reth_evm_ethereum::EthEvmConfig,
                Network = EthApi::NetworkTypes,
            >,
        > + EthCall
        + EthTransactions
        + TraceExt
        + Clone
        + Send
        + Sync
        + 'static,
{
    async fn mev_eth_call(
        &self,
        request: TransactionRequest,
        block_id: Option<BlockId>,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> RpcResult<Bytes> {
        let t0 = Instant::now();
        let c = &self.counters.eth_call;
        metrics::record_request(method::ETH_CALL, c);

        if !self.epoch_manager.matches_active(block_id) {
            let gap = self.epoch_manager.block_gap(block_id);
            if self.reject_stale_call {
                // Phase 4: fast rejection — no DB read, no EVM execution.
                metrics::record_epoch_mismatch(method::ETH_CALL, gap);
                metrics::record_e2e_latency(method::ETH_CALL, t0.elapsed());
                return Err(epoch_mismatch_error(
                    block_id,
                    self.epoch_manager.active_block_number(),
                    gap,
                ));
            }
            // Phase 3 fallback (MEV_REJECT_STALE_CALL=0): degrade to native eth_call.
            metrics::record_degraded_path(method::ETH_CALL, c);
            metrics::record_degraded_gap(method::ETH_CALL, gap);
            let overrides =
                alloy_rpc_types_eth::state::EvmOverrides::new(state_overrides, block_overrides);
            let result =
                self.eth_api.call(request, block_id, overrides).await.map_err(Into::into);
            metrics::record_e2e_latency(method::ETH_CALL, t0.elapsed());
            return result;
        }

        metrics::record_worker_path(method::ETH_CALL, c);
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
            kind: CallKind::Basic,
            result_tx,
        };

        self.worker_pool.dispatch(task).map_err(|err| internal_rpc_err(err.to_string()))?;

        let result = match result_rx.await {
            Ok(Ok(WorkerOutput::Basic(bytes))) => Ok(bytes),
            Ok(Err(WorkerError::Revert(data))) => Err(EthApiError::from_revert(data).into()),
            Ok(Err(err)) => {
                metrics::record_error(method::ETH_CALL, "worker_error", c);
                Err(internal_rpc_err(err.to_string()))
            }
            Err(_) => {
                metrics::record_error(method::ETH_CALL, "worker_dropped", c);
                Err(internal_rpc_err("worker dropped"))
            }
            _ => Err(internal_rpc_err("unexpected worker output")),
        };
        metrics::record_e2e_latency(method::ETH_CALL, t0.elapsed());
        result
    }

    async fn mev_debug_trace_call(
        &self,
        request: TransactionRequest,
        block_id: Option<BlockId>,
        opts: Option<GethDebugTracingCallOptions>,
    ) -> RpcResult<GethTrace> {
        let t0 = Instant::now();
        let c = &self.counters.debug_trace_call;
        metrics::record_request(method::DEBUG_TRACE, c);

        if !self.epoch_manager.matches_active(block_id) {
            let gap = self.epoch_manager.block_gap(block_id);
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
            // gap == Some(1): drain — record and fall through to worker path.
            // Execute on current epoch N; path simulation on latest state remains useful.
            // Record as "drain" so the gap-count dashboard captures all non-zero gap events.
            metrics::record_epoch_mismatch(method::DEBUG_TRACE, gap);
        }

        metrics::record_worker_path(method::DEBUG_TRACE, c);
        let opts = opts.unwrap_or_default();
        let state_overrides = opts.state_overrides.clone();
        let block_overrides = opts.block_overrides.clone().map(Box::new);

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
            kind: CallKind::DebugTrace { opts: Box::new(opts) },
            result_tx,
        };

        self.worker_pool.dispatch(task).map_err(|err| internal_rpc_err(err.to_string()))?;

        let result = match result_rx.await {
            Ok(Ok(WorkerOutput::DebugTrace(trace))) => Ok(trace),
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

    async fn mev_trace_call(
        &self,
        request: TransactionRequest,
        trace_types: Vec<TraceType>,
        block_id: Option<BlockId>,
        state_overrides: Option<StateOverride>,
        block_overrides: Option<Box<BlockOverrides>>,
    ) -> RpcResult<TraceResults> {
        let t0 = Instant::now();
        let c = &self.counters.trace_call;
        metrics::record_request(method::TRACE_CALL, c);

        let trace_types: HashSet<_> = trace_types.into_iter().collect();

        if !self.epoch_manager.matches_active(block_id) {
            let gap = self.epoch_manager.block_gap(block_id);
            if gap != Some(1) {
                // gap >= 2 or non-number block_id: fast rejection.
                metrics::record_epoch_mismatch(method::TRACE_CALL, gap);
                metrics::record_e2e_latency(method::TRACE_CALL, t0.elapsed());
                return Err(epoch_mismatch_error(
                    block_id,
                    self.epoch_manager.active_block_number(),
                    gap,
                ));
            }
            // gap == Some(1): drain — record and fall through to worker path.
            // Execute on current epoch N; path simulation on latest state remains useful.
            // Record as "drain" so the gap-count dashboard captures all non-zero gap events.
            metrics::record_epoch_mismatch(method::TRACE_CALL, gap);
        }

        metrics::record_worker_path(method::TRACE_CALL, c);
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
            kind: CallKind::ParityTrace { trace_types },
            result_tx,
        };

        self.worker_pool.dispatch(task).map_err(|err| internal_rpc_err(err.to_string()))?;

        let result = match result_rx.await {
            Ok(Ok(WorkerOutput::ParityTrace(trace))) => Ok(trace),
            Ok(Err(err)) => {
                metrics::record_error(method::TRACE_CALL, "worker_error", c);
                Err(internal_rpc_err(err.to_string()))
            }
            Err(_) => {
                metrics::record_error(method::TRACE_CALL, "worker_dropped", c);
                Err(internal_rpc_err("worker dropped"))
            }
            _ => Err(internal_rpc_err("unexpected worker output")),
        };
        metrics::record_e2e_latency(method::TRACE_CALL, t0.elapsed());
        result
    }

}


/// 构造 -39001 EpochMismatch JSON-RPC 错误，供 Phase 4 快速拒绝使用。
fn epoch_mismatch_error(
    requested: Option<BlockId>,
    current_epoch: u64,
    gap: Option<u64>,
) -> jsonrpsee::types::error::ErrorObject<'static> {
    let requested_block = match requested {
        Some(BlockId::Number(alloy_rpc_types_eth::BlockNumberOrTag::Number(n))) => Some(n),
        _ => None,
    };

    let mut data = std::collections::BTreeMap::new();
    data.insert("requestedBlock", requested_block);
    data.insert("currentEpoch", Some(current_epoch));
    data.insert("gap", gap);

    jsonrpsee::types::error::ErrorObject::owned(
        -39001,
        "epoch mismatch: stale block_id",
        Some(data),
    )
}
