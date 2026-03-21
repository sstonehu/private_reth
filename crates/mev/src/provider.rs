use crate::{cache::GlobalSharedCache, worker::cache::WorkerL1Cache};
use alloy_primitives::{Address, B256, U256};
use reth_revm::database::StateProviderDatabase;
use reth_storage_api::StateProviderBox;
use revm::{bytecode::Bytecode, state::AccountInfo, Database, DatabaseRef};
use std::sync::Arc;

/// revm::Database 适配层（Phase 2）：Worker-L1 -> GlobalSharedCache -> StateProvider。
#[derive(Debug)]
pub struct CachedStateProvider<'a> {
    pub l1: &'a mut WorkerL1Cache,
    pub global: Arc<GlobalSharedCache>,
    pub db: StateProviderDatabase<&'a StateProviderBox>,
}

impl Database for CachedStateProvider<'_> {
    type Error = reth_errors::ProviderError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if let Some(cached) = self.l1.accounts.get(&address) {
            metrics::counter!("mev_worker_l1_hits_total", "kind" => "account").increment(1);
            return Ok(cached.clone());
        }

        metrics::counter!("mev_worker_l1_misses_total", "kind" => "account").increment(1);
        let db = &self.db;
        let info = self.global.get_or_load_account(address, || {
            metrics::counter!("mev_global_cache_db_reads_total", "kind" => "account").increment(1);
            db.basic_ref(address)
        })?;
        self.l1.accounts.insert(address, info.clone());
        Ok(info)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        if let Some(code) = self.l1.bytecodes.get(&code_hash) {
            metrics::counter!("mev_worker_l1_hits_total", "kind" => "bytecode").increment(1);
            return Ok(code.clone());
        }

        metrics::counter!("mev_worker_l1_misses_total", "kind" => "bytecode").increment(1);
        let db = &self.db;
        let code = self.global.get_or_load_bytecode(code_hash, || {
            metrics::counter!("mev_global_cache_db_reads_total", "kind" => "bytecode").increment(1);
            db.code_by_hash_ref(code_hash)
        })?;
        self.l1.bytecodes.insert(code_hash, code.clone());
        Ok(code)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if let Some(&value) = self.l1.storage.get(&(address, index)) {
            metrics::counter!("mev_worker_l1_hits_total", "kind" => "storage").increment(1);
            return Ok(value);
        }

        metrics::counter!("mev_worker_l1_misses_total", "kind" => "storage").increment(1);
        let db = &self.db;
        let value = self.global.get_or_load_storage(address, index, || {
            metrics::counter!("mev_global_cache_db_reads_total", "kind" => "storage").increment(1);
            db.storage_ref(address, index)
        })?;
        self.l1.storage.insert((address, index), value);
        Ok(value)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.db.block_hash(number)
    }
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
