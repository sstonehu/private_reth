use crate::{cache::GlobalSharedCache, worker::cache::WorkerL1Cache};
use alloy_primitives::{Address, B256, U256};
use reth_revm::database::StateProviderDatabase;
use reth_storage_api::StateProviderBox;
use revm::{bytecode::Bytecode, state::AccountInfo, Database, DatabaseRef};
use std::{sync::Arc, time::Instant};

/// Per-task cache-access counters, accumulated in plain u64 (no atomics, no shared state).
///
/// All 9 EVM hot-path counters are accumulated here during task execution and flushed to
/// Prometheus in a single batch at task completion, replacing O(EVM accesses) atomic
/// operations + DashMap lookups with a fixed 9-operation flush per task.
#[derive(Debug, Default)]
pub struct ProviderStats {
    pub l1_hits_account: u64,
    pub l1_hits_storage: u64,
    pub l1_hits_bytecode: u64,
    pub l1_misses_account: u64,
    pub l1_misses_storage: u64,
    pub l1_misses_bytecode: u64,
    pub db_reads_account: u64,
    pub db_reads_storage: u64,
    pub db_reads_bytecode: u64,
}

impl ProviderStats {
    /// Flush accumulated counts to Prometheus. Called once per task — at most 9 registry
    /// lookups total instead of one per EVM state access. Skips zero-value counters to
    /// avoid unnecessary DashMap lookups on tasks that don't touch all data kinds.
    pub fn flush(&self) {
        macro_rules! inc {
            ($metric:expr, $kind:literal, $val:expr) => {
                if $val > 0 {
                    metrics::counter!($metric, "kind" => $kind).increment($val);
                }
            };
        }
        inc!("mev_worker_l1_hits_total", "account", self.l1_hits_account);
        inc!("mev_worker_l1_hits_total", "storage", self.l1_hits_storage);
        inc!("mev_worker_l1_hits_total", "bytecode", self.l1_hits_bytecode);
        inc!("mev_worker_l1_misses_total", "account", self.l1_misses_account);
        inc!("mev_worker_l1_misses_total", "storage", self.l1_misses_storage);
        inc!("mev_worker_l1_misses_total", "bytecode", self.l1_misses_bytecode);
        inc!("mev_global_cache_db_reads_total", "account", self.db_reads_account);
        inc!("mev_global_cache_db_reads_total", "storage", self.db_reads_storage);
        inc!("mev_global_cache_db_reads_total", "bytecode", self.db_reads_bytecode);
    }
}

/// revm::Database 适配层（Phase 2）：Worker-L1 -> GlobalSharedCache -> StateProvider。
#[derive(Debug)]
pub struct CachedStateProvider<'a> {
    pub l1: &'a mut WorkerL1Cache,
    pub global: Arc<GlobalSharedCache>,
    pub db: StateProviderDatabase<&'a StateProviderBox>,
    /// Task-local stats accumulated during EVM execution; flushed once at task completion.
    pub stats: ProviderStats,
}

impl Database for CachedStateProvider<'_> {
    type Error = reth_errors::ProviderError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let op_start = Instant::now();
        if let Some(cached) = self.l1.accounts.get(&address) {
            self.stats.l1_hits_account += 1;
            record_provider_op("account", "l1", op_start);
            return Ok(cached.clone());
        }

        self.stats.l1_misses_account += 1;
        let db = &self.db;
        let mut db_read = false;
        let mut db_read_secs = 0.0;
        let info = self.global.get_or_load_account(address, || {
            let db_start = Instant::now();
            db_read = true;
            let result = db.basic_ref(address);
            db_read_secs = db_start.elapsed().as_secs_f64();
            result
        })?;
        record_provider_op("account", "global", op_start);
        if db_read {
            self.stats.db_reads_account += 1;
            metrics::histogram!("mev_provider_op_seconds", "op" => "account", "source" => "db")
                .record(db_read_secs);
        }
        self.l1.accounts.insert(address, info.clone());
        Ok(info)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        let op_start = Instant::now();
        if let Some(code) = self.l1.bytecodes.get(&code_hash) {
            self.stats.l1_hits_bytecode += 1;
            record_provider_op("bytecode", "l1", op_start);
            return Ok(code.clone());
        }

        self.stats.l1_misses_bytecode += 1;
        let db = &self.db;
        let mut db_read = false;
        let mut db_read_secs = 0.0;
        let code = self.global.get_or_load_bytecode(code_hash, || {
            let db_start = Instant::now();
            db_read = true;
            let result = db.code_by_hash_ref(code_hash);
            db_read_secs = db_start.elapsed().as_secs_f64();
            result
        })?;
        record_provider_op("bytecode", "global", op_start);
        if db_read {
            self.stats.db_reads_bytecode += 1;
            metrics::histogram!("mev_provider_op_seconds", "op" => "bytecode", "source" => "db")
                .record(db_read_secs);
        }
        self.l1.bytecodes.insert(code_hash, code.clone());
        Ok(code)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        let op_start = Instant::now();
        if let Some(&value) = self.l1.storage.get(&(address, index)) {
            self.stats.l1_hits_storage += 1;
            record_provider_op("storage", "l1", op_start);
            return Ok(value);
        }

        self.stats.l1_misses_storage += 1;
        let db = &self.db;
        let mut db_read = false;
        let mut db_read_secs = 0.0;
        let value = self.global.get_or_load_storage(address, index, || {
            let db_start = Instant::now();
            db_read = true;
            let result = db.storage_ref(address, index);
            db_read_secs = db_start.elapsed().as_secs_f64();
            result
        })?;
        record_provider_op("storage", "global", op_start);
        if db_read {
            self.stats.db_reads_storage += 1;
            metrics::histogram!("mev_provider_op_seconds", "op" => "storage", "source" => "db")
                .record(db_read_secs);
        }
        self.l1.storage.insert((address, index), value);
        Ok(value)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.db.block_hash(number)
    }
}

fn record_provider_op(op: &'static str, source: &'static str, start: Instant) {
    metrics::histogram!("mev_provider_op_seconds", "op" => op, "source" => source)
        .record(start.elapsed().as_secs_f64());
}

impl DatabaseRef for CachedStateProvider<'_> {
    type Error = reth_errors::ProviderError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.db.basic_ref(address)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.db.code_by_hash_ref(code_hash)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.db.storage_ref(address, index)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.db.block_hash_ref(number)
    }
}
