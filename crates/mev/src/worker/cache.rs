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
        self.storage.clear();
        self.epoch_id = new_epoch_id;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, Bytes};
    use revm::bytecode::Bytecode;

    #[test]
    fn test_bytecodes_retained_after_reset() {
        let mut l1 = WorkerL1Cache::new(1);
        let hash = B256::from([0xabu8; 32]);
        l1.bytecodes.insert(hash, Bytecode::new_raw(Bytes::from(vec![0x60])));

        l1.reset(2);

        assert!(l1.bytecodes.contains_key(&hash));
        assert!(l1.accounts.is_empty());
        assert!(l1.storage.is_empty());
        assert_eq!(l1.epoch_id, 2);
    }
}
