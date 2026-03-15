use crate::worker::cache::WorkerL1Cache;
use alloy_primitives::{Address, B256, U256};
use reth_revm::database::StateProviderDatabase;
use reth_storage_api::StateProviderBox;
use revm::{bytecode::Bytecode, state::AccountInfo, Database, DatabaseRef};

/// revm::Database 适配层（Phase 1）：Worker-L1 -> StateProvider。
#[derive(Debug)]
pub struct WorkerStateProvider<'a> {
    pub l1: &'a mut WorkerL1Cache,
    pub db: StateProviderDatabase<&'a StateProviderBox>,
}

impl Database for WorkerStateProvider<'_> {
    type Error = reth_errors::ProviderError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if let Some(cached) = self.l1.accounts.get(&address) {
            metrics::counter!("mev_worker_l1_hits_total", "kind" => "account").increment(1);
            return Ok(cached.clone());
        }

        metrics::counter!("mev_worker_l1_misses_total", "kind" => "account").increment(1);
        let info = self.db.basic(address)?;
        self.l1.accounts.insert(address, info.clone());
        Ok(info)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        if let Some(code) = self.l1.bytecodes.get(&code_hash) {
            return Ok(code.clone());
        }

        let code = self.db.code_by_hash(code_hash)?;
        self.l1.bytecodes.insert(code_hash, code.clone());
        Ok(code)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if let Some(&value) = self.l1.storage.get(&(address, index)) {
            metrics::counter!("mev_worker_l1_hits_total", "kind" => "storage").increment(1);
            return Ok(value);
        }

        metrics::counter!("mev_worker_l1_misses_total", "kind" => "storage").increment(1);
        let value = self.db.storage(address, index)?;
        self.l1.storage.insert((address, index), value);
        Ok(value)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.db.block_hash(number)
    }
}

impl DatabaseRef for WorkerStateProvider<'_> {
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
