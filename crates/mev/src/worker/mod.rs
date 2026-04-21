pub mod cache;
mod worker;

use crate::{api::types::CallKind, epoch::EpochContext};
use alloy_eips::eip2930::AccessList;
use alloy_primitives::Bytes;
use alloy_rpc_types_eth::{state::StateOverride, BlockOverrides};
use alloy_rpc_types_trace::{geth::GethTrace, parity::TraceResults};
use crossbeam_channel::{bounded, Sender};
use reth_evm_ethereum::EthEvmConfig;
use std::sync::Arc;
use tokio::sync::oneshot;
pub use worker::MevWorker;

/// 默认 worker 数量，针对 64 线程服务器（EPYC 9554P）：
/// 保留 16 给 tokio + 4 给 MDBX = 20 非 EVM 线程，剩余 40 用于 EVM 执行。
/// 可通过环境变量 MEV_WORKER_COUNT 覆盖。
pub const DEFAULT_POOL_SIZE: usize = 40;

/// 工作队列容量（有界 channel 背压上限）。
/// 目标场景：新 block 后 1s 内 ~50,000 请求集中到达。
/// 65536 × ~512B/task ≈ 32MB，容纳峰值积压而不触发 QueueFull。
pub const TASK_QUEUE_CAPACITY: usize = 65536;

pub type EthEvmEnv = reth_evm::EvmEnvFor<EthEvmConfig>;
pub type EthTxEnv = reth_evm::TxEnvFor<EthEvmConfig>;

pub struct WorkerTask {
    pub epoch: Arc<EpochContext>,
    pub evm_env: EthEvmEnv,
    pub tx_env: EthTxEnv,
    pub block_overrides: Option<Box<BlockOverrides>>,
    pub state_overrides: Option<StateOverride>,
    pub kind: CallKind,
    pub result_tx: oneshot::Sender<WorkerResult>,
}

impl std::fmt::Debug for WorkerTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerTask")
            .field("epoch_id", &self.epoch.epoch_id)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

pub type WorkerResult = Result<WorkerOutput, WorkerError>;

#[derive(Debug)]
pub enum WorkerOutput {
    Basic(Bytes),
    /// debug trace 结果。第二个字段：`with_access_list=true` 时为从 `res.state` 提取的
    /// EIP-2930 access list，否则为 `None`。
    DebugTrace(GethTrace, Option<AccessList>),
    ParityTrace(TraceResults),
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("evm execution reverted")]
    Revert(Bytes),
    #[error("evm halted: {reason}, gas_used={gas_used}")]
    Halt { reason: String, gas_used: u64 },
    #[error("evm error: {0}")]
    Evm(String),
    #[error("debug inspector error: {0}")]
    Inspect(String),
    #[error("parity tracing error: {0}")]
    Tracing(String),
    #[error("state provider error: {0}")]
    Provider(#[from] reth_errors::ProviderError),
    #[error("internal: {0}")]
    Internal(String),
}

pub struct MevWorkerPool {
    task_tx: Sender<WorkerTask>,
    pub num_workers: usize,
}

impl std::fmt::Debug for MevWorkerPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MevWorkerPool")
            .field("num_workers", &self.num_workers)
            .field("queue_len", &self.task_tx.len())
            .field("queue_capacity", &TASK_QUEUE_CAPACITY)
            .finish()
    }
}

impl MevWorkerPool {
    pub fn new(
        num_workers: usize,
        evm_config: EthEvmConfig,
        global_cache: Arc<crate::cache::GlobalSharedCache>,
    ) -> Arc<Self> {
        let (task_tx, task_rx) = bounded(TASK_QUEUE_CAPACITY);

        for id in 0..num_workers {
            MevWorker::spawn(id, task_rx.clone(), evm_config.clone(), global_cache.clone());
        }

        Arc::new(Self { task_tx, num_workers })
    }

    pub fn dispatch(&self, task: WorkerTask) -> Result<(), PoolError> {
        metrics::gauge!("mev_pool_queue_depth").set(self.task_tx.len() as f64);

        self.task_tx.try_send(task).map_err(|_| {
            metrics::counter!("mev_pool_queue_full_total").increment(1);
            PoolError::QueueFull
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("worker pool queue full")]
    QueueFull,
}
