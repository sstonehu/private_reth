use alloy_primitives::{Address, B256, U256};
use revm::{bytecode::Bytecode, state::AccountInfo};
use std::collections::HashMap;

/// Phase 1 的 Worker-L1 本地缓存：仅存储 clean reads。
#[derive(Debug, Default)]
pub struct WorkerL1Cache {
    pub epoch_id: u64,
    /// address -> AccountInfo（含负缓存）
    pub accounts: HashMap<Address, Option<AccountInfo>>,
    /// code_hash -> Bytecode
    pub bytecodes: HashMap<B256, Bytecode>,
    /// (address, slot) -> value
    pub storage: HashMap<(Address, U256), U256>,
}

impl WorkerL1Cache {
    pub fn new(epoch_id: u64) -> Self {
        Self { epoch_id, ..Default::default() }
    }

    pub fn is_valid_for(&self, epoch_id: u64) -> bool {
        self.epoch_id == epoch_id
    }

    pub fn reset(&mut self, new_epoch_id: u64) {
        self.accounts.clear();
        self.bytecodes.clear();
        self.storage.clear();
        self.epoch_id = new_epoch_id;
    }
}
