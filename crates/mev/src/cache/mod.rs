use alloy_primitives::{Address, B256, U256};
use moka::sync::Cache;
use reth_errors::ProviderError;
use revm::{bytecode::Bytecode, state::AccountInfo};
use std::sync::Arc;
use std::time::Duration;

// Weigher functions must include the FULL per-entry memory cost:
//   key bytes + value bytes + moka internal overhead (hash table slot, deque nodes,
//   frequency sketch amortized, Arc headers) ≈ 100 bytes per entry.
// Underestimating the weight causes moka to allow far more entries than intended,
// growing actual heap well beyond the configured budget.

fn account_weigher(_k: &(u64, Address), _v: &Option<AccountInfo>) -> u32 {
    // key: (u64=8, Address=20) = 28 bytes
    // value: Option<AccountInfo> (nonce:u64, balance:U256, code_hash:B256, code:None) ≈ 80 bytes
    // moka overhead ≈ 100 bytes
    // total ≈ 208 → round to 200
    200
}

fn storage_weigher(_k: &(u64, Address, U256), _v: &U256) -> u32 {
    // key: (u64=8, Address=20, U256=32) = 60 bytes
    // value: U256 = 32 bytes
    // moka overhead ≈ 100 bytes
    // total ≈ 192 → round to 200
    200
}

fn bytecode_weigher(_k: &B256, v: &Bytecode) -> u32 {
    // key: B256 = 32 bytes
    // value: Bytecode (raw bytes + optional jump table) = v.len() bytes
    // moka overhead ≈ 100 bytes
    (32 + v.len() + 100).min(u32::MAX as usize) as u32
}

/// Cross-worker shared read cache.
#[derive(Debug)]
pub struct GlobalSharedCache {
    /// (epoch_id, address) -> Option<AccountInfo> (negative-cache aware)
    accounts: Cache<(u64, Address), Option<AccountInfo>>,
    /// (epoch_id, address, slot) -> value
    storage: Cache<(u64, Address, U256), U256>,
    /// code_hash -> bytecode
    bytecodes: Cache<B256, Bytecode>,
}

impl GlobalSharedCache {
    pub fn new(max_mb: u64) -> Arc<Self> {
        let total_bytes = max_mb.saturating_mul(1024).saturating_mul(1024);
        let storage_budget = total_bytes.saturating_mul(70) / 100;
        let account_budget = total_bytes.saturating_mul(25) / 100;
        let bytecode_budget = total_bytes.saturating_sub(storage_budget + account_budget);

        // Ethereum produces one block every ~12 seconds.  Entries keyed by epoch_id
        // become unreachable as soon as the epoch advances, so we evict them after
        // EPOCH_TTI seconds of idleness.  This bounds memory even under sustained
        // high-insertion bursts where moka's background eviction thread cannot keep
        // pace with max_capacity alone (observed: 125 M entries vs 60 M limit).
        const EPOCH_TTI: Duration = Duration::from_secs(30);

        Arc::new(Self {
            accounts: Cache::builder()
                .max_capacity(account_budget)
                .weigher(account_weigher)
                .time_to_idle(EPOCH_TTI)
                .build(),
            storage: Cache::builder()
                .max_capacity(storage_budget)
                .weigher(storage_weigher)
                .time_to_idle(EPOCH_TTI)
                .build(),
            bytecodes: Cache::builder()
                .max_capacity(bytecode_budget)
                .weigher(bytecode_weigher)
                .build(),
        })
    }

    pub fn get_or_load_account<F>(
        &self,
        epoch_id: u64,
        address: Address,
        load: F,
    ) -> Result<Option<AccountInfo>, ProviderError>
    where
        F: FnOnce() -> Result<Option<AccountInfo>, ProviderError>,
    {
        self.accounts
            .try_get_with((epoch_id, address), load)
            .map_err(|arc_err| (*arc_err).clone())
    }

    pub fn get_or_load_storage<F>(
        &self,
        epoch_id: u64,
        address: Address,
        slot: U256,
        load: F,
    ) -> Result<U256, ProviderError>
    where
        F: FnOnce() -> Result<U256, ProviderError>,
    {
        self.storage
            .try_get_with((epoch_id, address, slot), load)
            .map_err(|arc_err| (*arc_err).clone())
    }

    pub fn get_or_load_bytecode<F>(&self, code_hash: B256, load: F) -> Result<Bytecode, ProviderError>
    where
        F: FnOnce() -> Result<Bytecode, ProviderError>,
    {
        self.bytecodes.try_get_with(code_hash, load).map_err(|arc_err| (*arc_err).clone())
    }

    pub fn eager_prefetch(&self, _new_epoch_id: u64, _safe_set: &SafeUnchangedSet) {
        // no-op in Phase 2
    }

    /// Immediately schedule all epoch-keyed entries for eviction.
    ///
    /// Called when the canonical head advances to a new block. Entries keyed by
    /// the old epoch_id will never be accessed again, so there is no reason to
    /// keep them in memory.  `bytecodes` is intentionally excluded: its key is
    /// just a B256 code_hash with no epoch component, so bytecodes remain valid
    /// across epoch boundaries.
    ///
    /// `invalidate_all` is O(1) – it schedules eviction; the background
    /// housekeeper performs the actual removal asynchronously.
    pub fn on_epoch_change(&self) {
        self.accounts.invalidate_all();
        self.storage.invalidate_all();
    }

    pub fn account_entry_count(&self) -> u64 {
        self.accounts.entry_count()
    }

    pub fn storage_entry_count(&self) -> u64 {
        self.storage.entry_count()
    }

    pub fn bytecode_entry_count(&self) -> u64 {
        self.bytecodes.entry_count()
    }
}

#[derive(Debug, Default)]
pub struct SafeUnchangedSet;

impl SafeUnchangedSet {
    pub fn empty() -> Self {
        Self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::cache::WorkerL1Cache;
    use alloy_primitives::{Address, B256, Bytes, U256};
    use revm::{bytecode::Bytecode, state::AccountInfo};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::Duration;

    #[test]
    fn test_singleflight_concurrent_miss() {
        let cache = GlobalSharedCache::new(256);
        let addr = Address::from([0x01u8; 20]);
        let call_count = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let cache = cache.clone();
                let cc = call_count.clone();
                std::thread::spawn(move || {
                    cache.get_or_load_account(1_u64, addr, move || {
                        cc.fetch_add(1, Ordering::Relaxed);
                        std::thread::sleep(Duration::from_millis(10));
                        Ok(Some(AccountInfo { nonce: 42, ..Default::default() }))
                    })
                })
            })
            .collect();

        for h in handles {
            let v = h.join().unwrap().unwrap();
            assert_eq!(v.unwrap().nonce, 42);
        }

        let count = call_count.load(Ordering::Relaxed);
        assert!(count <= 2, "singleflight: DB called {count} times, expected <= 2");
    }

    #[test]
    fn test_epoch_namespace_isolation() {
        let cache = GlobalSharedCache::new(256);
        let addr = Address::from([0x02u8; 20]);

        let v1 = cache
            .get_or_load_account(1_u64, addr, || {
                Ok(Some(AccountInfo { nonce: 10, ..Default::default() }))
            })
            .unwrap();
        assert_eq!(v1.unwrap().nonce, 10);

        let epoch2_called = Arc::new(AtomicUsize::new(0));
        let c = epoch2_called.clone();
        let v2 = cache
            .get_or_load_account(2_u64, addr, move || {
                c.fetch_add(1, Ordering::Relaxed);
                Ok(None)
            })
            .unwrap();

        assert_eq!(epoch2_called.load(Ordering::Relaxed), 1);
        assert!(v2.is_none());
    }

    #[test]
    fn test_negative_cache() {
        let cache = GlobalSharedCache::new(256);
        let addr = Address::from([0x03u8; 20]);

        let v1 = cache.get_or_load_account(1_u64, addr, || Ok(None)).unwrap();
        assert!(v1.is_none());

        let db2_called = Arc::new(AtomicUsize::new(0));
        let c = db2_called.clone();
        let v2 = cache
            .get_or_load_account(1_u64, addr, move || {
                c.fetch_add(1, Ordering::Relaxed);
                Ok(Some(AccountInfo { nonce: 99, ..Default::default() }))
            })
            .unwrap();

        assert_eq!(db2_called.load(Ordering::Relaxed), 0);
        assert!(v2.is_none());
    }

    #[test]
    fn test_l2_hit_backfills_l1() {
        let cache = GlobalSharedCache::new(256);
        let addr = Address::from([0x04u8; 20]);

        let _ = cache
            .get_or_load_account(1_u64, addr, || {
                Ok(Some(AccountInfo { nonce: 77, ..Default::default() }))
            })
            .unwrap();

        let mut l1_b = WorkerL1Cache::new(1);
        let db2_called = Arc::new(AtomicUsize::new(0));
        let c = db2_called.clone();
        let info = cache
            .get_or_load_account(1_u64, addr, move || {
                c.fetch_add(1, Ordering::Relaxed);
                Ok(Some(AccountInfo { nonce: 0, ..Default::default() }))
            })
            .unwrap();
        assert_eq!(db2_called.load(Ordering::Relaxed), 0);

        l1_b.accounts.insert(addr, info.clone());
        assert_eq!(l1_b.accounts.get(&addr).unwrap().as_ref().unwrap().nonce, 77);
    }

    #[test]
    fn test_three_layer_l1_priority() {
        let cache = GlobalSharedCache::new(256);
        let mut l1 = WorkerL1Cache::new(1);
        let addr = Address::from([0x05u8; 20]);
        l1.accounts.insert(addr, Some(AccountInfo { nonce: 99, ..Default::default() }));

        let l2_called = Arc::new(AtomicUsize::new(0));
        let hit = if let Some(v) = l1.accounts.get(&addr) {
            v.clone()
        } else {
            let c = l2_called.clone();
            cache
                .get_or_load_account(1_u64, addr, move || {
                    c.fetch_add(1, Ordering::Relaxed);
                    Ok(None)
                })
                .unwrap()
        };

        assert_eq!(hit.unwrap().nonce, 99);
        assert_eq!(l2_called.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_storage_singleflight() {
        let cache = GlobalSharedCache::new(256);
        let addr = Address::from([0x06u8; 20]);
        let slot = U256::from(0_u64);
        let call_count = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let cache = cache.clone();
                let cc = call_count.clone();
                std::thread::spawn(move || {
                    cache.get_or_load_storage(1_u64, addr, slot, move || {
                        cc.fetch_add(1, Ordering::Relaxed);
                        std::thread::sleep(Duration::from_millis(10));
                        Ok(U256::from(7_u64))
                    })
                })
            })
            .collect();

        for h in handles {
            let v = h.join().unwrap().unwrap();
            assert_eq!(v, U256::from(7_u64));
        }

        let count = call_count.load(Ordering::Relaxed);
        assert!(count <= 2, "singleflight: DB called {count} times, expected <= 2");
    }

    #[test]
    fn test_bytecode_l2_dedup() {
        let cache = GlobalSharedCache::new(256);
        let code_hash = B256::from([0xabu8; 32]);
        let code = Bytecode::new_raw(Bytes::from(vec![0x60, 0x01]));

        let c1 = cache
            .get_or_load_bytecode(code_hash, || Ok(code.clone()))
            .unwrap();
        assert_eq!(c1.len(), 2);

        let db2_called = Arc::new(AtomicUsize::new(0));
        let c = db2_called.clone();
        let c2 = cache
            .get_or_load_bytecode(code_hash, move || {
                c.fetch_add(1, Ordering::Relaxed);
                Ok(Bytecode::new_raw(Bytes::from(vec![0xff])))
            })
            .unwrap();

        assert_eq!(db2_called.load(Ordering::Relaxed), 0);
        assert_eq!(c2.len(), 2);
    }
}
