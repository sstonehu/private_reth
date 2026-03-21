use super::{cache::WorkerL1Cache, EthTxEnv, WorkerError, WorkerOutput, WorkerTask};
use crate::{
    api::types::CallKind,
    cache::GlobalSharedCache,
    epoch::EpochContext,
    provider::CachedStateProvider,
};
use alloy_primitives::map::HashSet;
use crossbeam_channel::Receiver;
use reth_evm::{env::BlockEnvironment, ConfigureEvm, Evm, TransactionEnv};
use reth_evm_ethereum::EthEvmConfig;
use reth_revm::{database::StateProviderDatabase, db::State};
use revm::Database as _;
use revm::context_interface::result::ExecutionResult;
use revm_inspectors::tracing::{DebugInspector, TracingInspector, TracingInspectorConfig};
use std::sync::Arc;

pub struct MevWorker {
    id: usize,
    task_rx: Receiver<WorkerTask>,
    evm_config: EthEvmConfig,
    l1: WorkerL1Cache,
    current_epoch_id: u64,
    state_provider: Option<reth_storage_api::StateProviderBox>,
    global_cache: Arc<GlobalSharedCache>,
}

impl std::fmt::Debug for MevWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MevWorker")
            .field("id", &self.id)
            .field("current_epoch_id", &self.current_epoch_id)
            .field("has_state_provider", &self.state_provider.is_some())
            .finish_non_exhaustive()
    }
}

type WorkerStateDb<'a> = State<CachedStateProvider<'a>>;

impl MevWorker {
    pub fn spawn(
        id: usize,
        task_rx: Receiver<WorkerTask>,
        evm_config: EthEvmConfig,
        global_cache: Arc<GlobalSharedCache>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name(format!("mev-worker-{id}"))
            .spawn(move || {
                let mut worker = Self {
                    id,
                    task_rx,
                    evm_config,
                    l1: WorkerL1Cache::new(0),
                    current_epoch_id: 0,
                    state_provider: None,
                    global_cache,
                };
                worker.run();
            })
            .expect("spawn mev worker thread")
    }

    fn run(&mut self) {
        while let Ok(task) = self.task_rx.recv() {
            let result = self.handle_task(&task);
            let _ = task.result_tx.send(result);

            metrics::counter!("mev_worker_tasks_total", "worker_id" => self.id.to_string())
                .increment(1);
        }
    }

    fn handle_task(&mut self, task: &WorkerTask) -> Result<WorkerOutput, WorkerError> {
        if task.epoch.epoch_id != self.current_epoch_id {
            self.switch_epoch(&task.epoch)?;
        }

        self.execute_task(task)
    }

    fn switch_epoch(&mut self, epoch: &Arc<EpochContext>) -> Result<(), WorkerError> {
        let provider = epoch.state_provider_factory.state_by_block_hash(epoch.block_hash)?;
        self.state_provider = Some(provider);
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

        Ok(())
    }

    fn execute_task(&mut self, task: &WorkerTask) -> Result<WorkerOutput, WorkerError> {
        let state_provider = self
            .state_provider
            .as_ref()
            .ok_or_else(|| WorkerError::Internal("state provider missing".to_string()))?;

        let worker_provider = CachedStateProvider {
            l1: &mut self.l1,
            global: self.global_cache.clone(),
            db: StateProviderDatabase::new(state_provider),
        };

        let mut db = State::builder().with_database(worker_provider).with_bundle_update().build();

        let mut evm_env = task.evm_env.clone();

        if let Some(block_overrides) = task.block_overrides.clone() {
            alloy_evm::overrides::apply_block_overrides(
                *block_overrides,
                &mut db,
                evm_env.block_env.inner_mut(),
            );
        }

        if let Some(state_overrides) = task.state_overrides.clone() {
            alloy_evm::overrides::apply_state_overrides(state_overrides, &mut db)
                .map_err(|err| WorkerError::Internal(err.to_string()))?;
        }

        // Align with native `prepare_call_env`: nonce is taken from state after request nonce is
        // cleared in API preprocessing.
        let mut tx_env = task.tx_env.clone();
        let state_nonce = db
            .basic(tx_env.caller)
            .map_err(|err| WorkerError::Internal(err.to_string()))?
            .map(|acc| acc.nonce)
            .unwrap_or_default();
        tx_env.set_nonce(state_nonce);

        let evm_config = self.evm_config.clone();

        match &task.kind {
            CallKind::Basic => Self::exec_basic(&evm_config, &mut db, evm_env, tx_env),
            CallKind::DebugTrace { opts } => {
                Self::exec_debug_trace(&evm_config, &mut db, evm_env, tx_env, opts)
            }
            CallKind::ParityTrace { trace_types } => Self::exec_parity_trace(
                &evm_config,
                &mut db,
                evm_env,
                tx_env,
                trace_types,
            ),
        }
    }

    fn exec_basic(
        evm_config: &EthEvmConfig,
        db: &mut WorkerStateDb<'_>,
        evm_env: super::EthEvmEnv,
        tx_env: EthTxEnv,
    ) -> Result<WorkerOutput, WorkerError> {
        let res = evm_config
            .evm_with_env(&mut *db, evm_env)
            .transact(tx_env)
            .map_err(|err| WorkerError::Evm(format!("{err:?}")))?;

        match res.result {
            ExecutionResult::Success { output, .. } => Ok(WorkerOutput::Basic(output.into_data())),
            ExecutionResult::Revert { output, .. } => Err(WorkerError::Revert(output)),
            ExecutionResult::Halt { reason, gas_used } => {
                Err(WorkerError::Halt { reason: format!("{reason:?}"), gas_used })
            }
        }
    }

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

    fn exec_parity_trace(
        evm_config: &EthEvmConfig,
        db: &mut WorkerStateDb<'_>,
        evm_env: super::EthEvmEnv,
        tx_env: EthTxEnv,
        trace_types: &HashSet<alloy_rpc_types_trace::parity::TraceType>,
    ) -> Result<WorkerOutput, WorkerError> {
        let config = TracingInspectorConfig::from_parity_config(trace_types);
        let mut inspector = TracingInspector::new(config);

        let res = evm_config
            .evm_with_env_and_inspector(&mut *db, evm_env, &mut inspector)
            .transact(tx_env)
            .map_err(|err| WorkerError::Evm(format!("{err:?}")))?;

        let trace_results = inspector
            .into_parity_builder()
            .into_trace_results_with_state(&res, trace_types, db)
            .map_err(|err| WorkerError::Tracing(format!("{err:?}")))?;

        Ok(WorkerOutput::ParityTrace(trace_results))
    }
}
