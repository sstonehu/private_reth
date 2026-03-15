use crate::{
    api::types::CallKind,
    epoch::{EpochContext, EpochManager},
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
use reth_rpc_api::{DebugApiServer, TraceApiServer};
use reth_rpc_convert::{RpcConvert, RpcTypes};
use reth_rpc_eth_api::{
    helpers::{EthCall, EthTransactions, TraceExt},
    EthApiTypes, RpcNodeCore,
};
use reth_rpc_eth_types::{error::api::FromRevert, EthApiError};
use reth_rpc_server_types::result::internal_rpc_err;
use std::sync::Arc;

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
    pub eth_api: EthApi,
    pub debug_api: reth_rpc::DebugApi<EthApi>,
    pub trace_api: reth_rpc::TraceApi<EthApi>,
}

impl<EthApi: RpcNodeCore<Evm = reth_evm_ethereum::EthEvmConfig>> std::fmt::Debug
    for MevApiServer<EthApi>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MevApiServer")
            .field("epoch_manager", &self.epoch_manager)
            .field("worker_pool", &self.worker_pool)
            .field("call_config", &self.call_config)
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
        if !self.epoch_manager.matches_active(block_id) {
            let overrides =
                alloy_rpc_types_eth::state::EvmOverrides::new(state_overrides, block_overrides);
            return self.eth_api.call(request, block_id, overrides).await.map_err(Into::into);
        }

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

        match result_rx.await {
            Ok(Ok(WorkerOutput::Basic(bytes))) => Ok(bytes),
            Ok(Err(WorkerError::Revert(data))) => Err(EthApiError::from_revert(data).into()),
            Ok(Err(err)) => Err(internal_rpc_err(err.to_string())),
            Err(_) => Err(internal_rpc_err("worker dropped")),
            _ => Err(internal_rpc_err("unexpected worker output")),
        }
    }

    async fn mev_debug_trace_call(
        &self,
        request: TransactionRequest,
        block_id: Option<BlockId>,
        opts: Option<GethDebugTracingCallOptions>,
    ) -> RpcResult<GethTrace> {
        if !self.epoch_manager.matches_active(block_id) {
            return DebugApiServer::debug_trace_call(
                &self.debug_api,
                request,
                block_id,
                Some(opts.unwrap_or_default()),
            )
            .await;
        }

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

        match result_rx.await {
            Ok(Ok(WorkerOutput::DebugTrace(trace))) => Ok(trace),
            Ok(Err(err)) => Err(internal_rpc_err(err.to_string())),
            Err(_) => Err(internal_rpc_err("worker dropped")),
            _ => Err(internal_rpc_err("unexpected worker output")),
        }
    }

    async fn mev_trace_call(
        &self,
        request: TransactionRequest,
        trace_types: Vec<TraceType>,
        block_id: Option<BlockId>,
    ) -> RpcResult<TraceResults> {
        let trace_types: HashSet<_> = trace_types.into_iter().collect();

        if !self.epoch_manager.matches_active(block_id) {
            return TraceApiServer::trace_call(
                &self.trace_api,
                request,
                trace_types,
                block_id,
                None,
                None,
            )
            .await;
        }

        let epoch = self.epoch_manager.current();
        let (evm_env, prepared_request) = self.prepare_evm_env(&epoch, request);
        let tx_env: reth_evm::TxEnvFor<reth_evm_ethereum::EthEvmConfig> =
            self.eth_api.converter().tx_env(prepared_request, &evm_env).map_err(Into::into)?;

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let task = WorkerTask {
            epoch,
            evm_env,
            tx_env,
            block_overrides: None,
            state_overrides: None,
            kind: CallKind::ParityTrace { trace_types },
            result_tx,
        };

        self.worker_pool.dispatch(task).map_err(|err| internal_rpc_err(err.to_string()))?;

        match result_rx.await {
            Ok(Ok(WorkerOutput::ParityTrace(trace))) => Ok(trace),
            Ok(Err(err)) => Err(internal_rpc_err(err.to_string())),
            Err(_) => Err(internal_rpc_err("worker dropped")),
            _ => Err(internal_rpc_err("unexpected worker output")),
        }
    }
}
