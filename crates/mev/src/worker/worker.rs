use super::{cache::WorkerL1Cache, EthTxEnv, WorkerError, WorkerOutput, WorkerTask};
use crate::{
    api::types::CallKind,
    cache::GlobalSharedCache,
    epoch::EpochContext,
    provider::{CachedStateProvider, ProviderStats},
};
use alloy_eips::eip2930::{AccessList, AccessListItem};
use alloy_primitives::B256;
use alloy_primitives::map::HashSet;
use crossbeam_channel::Receiver;
use reth_evm::{env::BlockEnvironment, ConfigureEvm, Evm, TransactionEnvMut};
use reth_evm_ethereum::EthEvmConfig;
use reth_revm::{database::StateProviderDatabase, db::State};
use revm::Database as _;
use revm::context_interface::result::ExecutionResult;
use revm_inspectors::tracing::{DebugInspector, TracingInspector, TracingInspectorConfig};
use std::{sync::Arc, time::Instant};

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
            let kind = task.kind.label();
            metrics::histogram!(
                "mev_worker_queue_wait_seconds",
                "method" => task.method,
                "kind" => kind,
            )
            .record(task.enqueued_at.elapsed().as_secs_f64());

            let _active = WorkerActiveGuard::new(task.method, kind);
            let handle_start = Instant::now();
            let result = self.handle_task(&task);
            let send_start = Instant::now();
            let _ = task.result_tx.send(result);
            metrics::histogram!(
                "mev_worker_result_send_seconds",
                "method" => task.method,
                "kind" => kind,
            )
            .record(send_start.elapsed().as_secs_f64());
            metrics::histogram!(
                "mev_worker_handle_seconds",
                "method" => task.method,
                "kind" => kind,
            )
            .record(handle_start.elapsed().as_secs_f64());

            metrics::counter!("mev_worker_tasks_total", "worker_id" => self.id.to_string())
                .increment(1);
        }
    }

    fn handle_task(&mut self, task: &WorkerTask) -> Result<WorkerOutput, WorkerError> {
        if task.epoch.epoch_id != self.current_epoch_id {
            let switch_start = Instant::now();
            self.switch_epoch(&task.epoch)?;
            metrics::histogram!(
                "mev_worker_switch_epoch_seconds",
                "method" => task.method,
                "kind" => task.kind.label(),
            )
            .record(switch_start.elapsed().as_secs_f64());
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
        let execute_start = Instant::now();
        let state_provider = self
            .state_provider
            .as_ref()
            .ok_or_else(|| WorkerError::Internal("state provider missing".to_string()))?;

        let build_db_start = Instant::now();
        let worker_provider = CachedStateProvider {
            l1: &mut self.l1,
            global: self.global_cache.clone(),
            db: StateProviderDatabase::new(state_provider),
            stats: ProviderStats::default(),
        };

        let mut db = State::builder().with_database(worker_provider).with_bundle_update().build();
        metrics::histogram!(
            "mev_worker_build_db_seconds",
            "method" => task.method,
            "kind" => task.kind.label(),
        )
        .record(build_db_start.elapsed().as_secs_f64());

        let mut evm_env = task.evm_env.clone();

        let overrides_start = Instant::now();
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
        metrics::histogram!(
            "mev_worker_apply_overrides_seconds",
            "method" => task.method,
            "kind" => task.kind.label(),
        )
        .record(overrides_start.elapsed().as_secs_f64());

        // Align with native `prepare_call_env`: nonce is taken from state after request nonce is
        // cleared in API preprocessing.
        let mut tx_env = task.tx_env.clone();
        let nonce_start = Instant::now();
        let state_nonce = db
            .basic(tx_env.caller)
            .map_err(|err| WorkerError::Internal(err.to_string()))?
            .map(|acc| acc.nonce)
            .unwrap_or_default();
        tx_env.set_nonce(state_nonce);
        metrics::histogram!(
            "mev_worker_nonce_basic_seconds",
            "method" => task.method,
            "kind" => task.kind.label(),
        )
        .record(nonce_start.elapsed().as_secs_f64());

        let evm_config = self.evm_config.clone();

        let result = match &task.kind {
            CallKind::Basic => Self::exec_basic(
                &evm_config,
                &mut db,
                evm_env,
                tx_env,
                task.method,
                task.kind.label(),
            ),
            CallKind::DebugTrace { opts, with_access_list } => {
                Self::exec_debug_trace(
                    &evm_config,
                    &mut db,
                    evm_env,
                    tx_env,
                    opts,
                    *with_access_list,
                    task.method,
                    task.kind.label(),
                )
            }
            CallKind::ParityTrace { trace_types } => Self::exec_parity_trace(
                &evm_config,
                &mut db,
                evm_env,
                tx_env,
                trace_types,
                task.method,
                task.kind.label(),
            ),
        };

        // Flush task-local cache-access counters to Prometheus in a single batch.
        // This replaces per-access atomic increments + DashMap lookups with at most 9 ops total.
        let flush_start = Instant::now();
        db.database.stats.flush();
        metrics::histogram!(
            "mev_worker_stats_flush_seconds",
            "method" => task.method,
            "kind" => task.kind.label(),
        )
        .record(flush_start.elapsed().as_secs_f64());
        metrics::histogram!(
            "mev_worker_execute_seconds",
            "method" => task.method,
            "kind" => task.kind.label(),
        )
        .record(execute_start.elapsed().as_secs_f64());

        result
    }

    fn exec_basic(
        evm_config: &EthEvmConfig,
        db: &mut WorkerStateDb<'_>,
        evm_env: super::EthEvmEnv,
        tx_env: EthTxEnv,
        method: &'static str,
        kind: &'static str,
    ) -> Result<WorkerOutput, WorkerError> {
        let transact_start = Instant::now();
        let res = evm_config
            .evm_with_env(&mut *db, evm_env)
            .transact(tx_env)
            .map_err(|err| WorkerError::Evm(format!("{err:?}")))?;
        metrics::histogram!(
            "mev_worker_transact_seconds",
            "method" => method,
            "kind" => kind,
        )
        .record(transact_start.elapsed().as_secs_f64());

        match res.result {
            ExecutionResult::Success { output, .. } => Ok(WorkerOutput::Basic(output.into_data())),
            ExecutionResult::Revert { output, .. } => Err(WorkerError::Revert(output)),
            ExecutionResult::Halt { reason, gas, .. } => {
                Err(WorkerError::Halt {
                    reason: format!("{reason:?}"),
                    gas_used: gas.tx_gas_used(),
                })
            }
        }
    }

    fn exec_debug_trace(
        evm_config: &EthEvmConfig,
        db: &mut WorkerStateDb<'_>,
        evm_env: super::EthEvmEnv,
        tx_env: EthTxEnv,
        opts: &alloy_rpc_types_trace::geth::GethDebugTracingCallOptions,
        with_access_list: bool,
        method: &'static str,
        kind: &'static str,
    ) -> Result<WorkerOutput, WorkerError> {
        let mut inspector = DebugInspector::new(opts.tracing_options.clone())
            .map_err(|err| WorkerError::Inspect(format!("{err:?}")))?;

        let transact_start = Instant::now();
        let res = evm_config
            .evm_with_env_and_inspector(&mut *db, evm_env.clone(), &mut inspector)
            .transact(tx_env.clone())
            .map_err(|err| WorkerError::Evm(format!("{err:?}")))?;
        metrics::histogram!(
            "mev_worker_transact_seconds",
            "method" => method,
            "kind" => kind,
        )
        .record(transact_start.elapsed().as_secs_f64());

        let trace_build_start = Instant::now();
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
        metrics::histogram!(
            "mev_worker_trace_build_seconds",
            "method" => method,
            "kind" => kind,
        )
        .record(trace_build_start.elapsed().as_secs_f64());

        Ok(WorkerOutput::DebugTrace(trace, access_list))
    }

    fn exec_parity_trace(
        evm_config: &EthEvmConfig,
        db: &mut WorkerStateDb<'_>,
        evm_env: super::EthEvmEnv,
        tx_env: EthTxEnv,
        trace_types: &HashSet<alloy_rpc_types_trace::parity::TraceType>,
        method: &'static str,
        kind: &'static str,
    ) -> Result<WorkerOutput, WorkerError> {
        let config = TracingInspectorConfig::from_parity_config(trace_types);
        let mut inspector = TracingInspector::new(config);

        let transact_start = Instant::now();
        let res = evm_config
            .evm_with_env_and_inspector(&mut *db, evm_env, &mut inspector)
            .transact(tx_env)
            .map_err(|err| WorkerError::Evm(format!("{err:?}")))?;
        metrics::histogram!(
            "mev_worker_transact_seconds",
            "method" => method,
            "kind" => kind,
        )
        .record(transact_start.elapsed().as_secs_f64());

        let trace_build_start = Instant::now();
        let trace_results = inspector
            .into_parity_builder()
            .into_trace_results_with_state(&res, trace_types, db)
            .map_err(|err| WorkerError::Tracing(format!("{err:?}")))?;
        metrics::histogram!(
            "mev_worker_trace_build_seconds",
            "method" => method,
            "kind" => kind,
        )
        .record(trace_build_start.elapsed().as_secs_f64());

        Ok(WorkerOutput::ParityTrace(trace_results))
    }
}

struct WorkerActiveGuard {
    method: &'static str,
    kind: &'static str,
}

impl WorkerActiveGuard {
    fn new(method: &'static str, kind: &'static str) -> Self {
        metrics::gauge!("mev_worker_active", "method" => method, "kind" => kind).increment(1.0);
        Self { method, kind }
    }
}

impl Drop for WorkerActiveGuard {
    fn drop(&mut self) {
        metrics::gauge!("mev_worker_active", "method" => self.method, "kind" => self.kind)
            .decrement(1.0);
    }
}
