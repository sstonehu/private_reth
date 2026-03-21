use alloy_primitives::{Address, B256, U256};
use moka::sync::Cache;
use reth_chain_state::CanonStateNotification;
use reth_errors::ProviderError;
use revm::{bytecode::Bytecode, state::AccountInfo};
use std::sync::Arc;

// Weigher functions must include the FULL per-entry memory cost:
//   key bytes + value bytes + moka internal overhead (hash table slot, deque nodes,
//   frequency sketch amortized, Arc headers) ≈ 100 bytes per entry.
// Underestimating the weight causes moka to allow far more entries than intended,
// growing actual heap well beyond the configured budget.

fn account_weigher(_k: &Address, _v: &Option<AccountInfo>) -> u32 {
    // key: Address = 20 bytes
    // value: Option<AccountInfo> (nonce:u64, balance:U256, code_hash:B256, code:None) ≈ 80 bytes
    // moka overhead ≈ 100 bytes
    // total ≈ 200
    200
}

fn storage_weigher(_k: &(Address, U256), _v: &U256) -> u32 {
    // key: (Address=20, U256=32) = 52 bytes
    // value: U256 = 32 bytes
    // moka overhead ≈ 100 bytes
    // total ≈ 184 → round to 192
    192
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
    /// address -> Option<AccountInfo> (negative-cache aware)
    accounts: Cache<Address, Option<AccountInfo>>,
    /// (address, slot) -> value
    storage: Cache<(Address, U256), U256>,
    /// code_hash -> bytecode
    bytecodes: Cache<B256, Bytecode>,
}

impl GlobalSharedCache {
    pub fn new(max_mb: u64) -> Arc<Self> {
        let total_bytes = max_mb.saturating_mul(1024).saturating_mul(1024);
        let storage_budget = total_bytes.saturating_mul(70) / 100;
        let account_budget = total_bytes.saturating_mul(25) / 100;
        let bytecode_budget = total_bytes.saturating_sub(storage_budget + account_budget);

        Arc::new(Self {
            accounts: Cache::builder().max_capacity(account_budget).weigher(account_weigher).build(),
            storage: Cache::builder().max_capacity(storage_budget).weigher(storage_weigher).build(),
            bytecodes: Cache::builder()
                .max_capacity(bytecode_budget)
                .weigher(bytecode_weigher)
                .build(),
        })
    }

    pub fn get_or_load_account<F>(
        &self,
        address: Address,
        load: F,
    ) -> Result<Option<AccountInfo>, ProviderError>
    where
        F: FnOnce() -> Result<Option<AccountInfo>, ProviderError>,
    {
        self.accounts.try_get_with(address, load).map_err(|arc_err| (*arc_err).clone())
    }

    pub fn get_or_load_storage<F>(
        &self,
        address: Address,
        slot: U256,
        load: F,
    ) -> Result<U256, ProviderError>
    where
        F: FnOnce() -> Result<U256, ProviderError>,
    {
        self.storage.try_get_with((address, slot), load).map_err(|arc_err| (*arc_err).clone())
    }

    pub fn get_or_load_bytecode<F>(&self, code_hash: B256, load: F) -> Result<Bytecode, ProviderError>
    where
        F: FnOnce() -> Result<Bytecode, ProviderError>,
    {
        self.bytecodes.try_get_with(code_hash, load).map_err(|arc_err| (*arc_err).clone())
    }

    /// 精确失效：仅驱逐 diff 中变更的账户与存储槽。
    /// Commit 走精确失效；Reorg 走全量失效。
    pub fn on_epoch_change_diff<N>(&self, notification: &CanonStateNotification<N>)
    where
        N: reth_node_api::NodePrimitives,
    {
        match notification {
            CanonStateNotification::Reorg { .. } => {
                self.accounts.invalidate_all();
                self.storage.invalidate_all();
            }
            CanonStateNotification::Commit { new } => {
                for (address, account) in new.execution_outcome().bundle_accounts_iter() {
                    self.accounts.invalidate(&address);
                    for (slot, _) in account.storage.iter() {
                        self.storage.invalidate(&(address, *slot));
                    }
                }
            }
        }
    }

    /// 预填充：将 diff 新值直接写入缓存，减少切块后首批 DB 读取。
    ///
    /// 调用顺序要求：先 `on_epoch_change_diff()`，再 `pre_fill_diff()`。
    pub fn pre_fill_diff<N>(&self, notification: &CanonStateNotification<N>)
    where
        N: reth_node_api::NodePrimitives,
    {
        let CanonStateNotification::Commit { new } = notification else {
            return;
        };

        for (address, account) in new.execution_outcome().bundle_accounts_iter() {
            if account.status.was_destroyed() {
                // 销毁账户：保留失效结果，不做预填充，后续读会落到 DB 并形成负缓存。
                continue;
            }

            if let Some(info) = account.info.as_ref() {
                self.accounts.insert(address, Some(info.clone()));
            }

            for (slot, storage_slot) in account.storage.iter() {
                self.storage.insert((address, *slot), storage_slot.present_value);
            }
        }
    }

    /// Phase 2 全量失效路径，Phase 3 作为回退保留。
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
    use alloy_primitives::{map::HashMap, Address, B256, Bytes, U256};
    use reth_chain_state::CanonStateNotification;
    use reth_revm::db::{states::StorageSlot, AccountStatus, BundleAccount};
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
                    cache.get_or_load_account(addr, move || {
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
    fn test_diff_invalidation() {
        let cache = GlobalSharedCache::new(256);
        let changed_addr = Address::from([0x02u8; 20]);
        let unchanged_addr = Address::from([0x03u8; 20]);
        let changed_slot = U256::from(1_u64);
        let unchanged_slot = U256::from(2_u64);

        cache.accounts.insert(changed_addr, Some(AccountInfo { nonce: 10, ..Default::default() }));
        cache.accounts.insert(
            unchanged_addr,
            Some(AccountInfo { nonce: 99, ..Default::default() }),
        );
        cache.storage.insert((changed_addr, changed_slot), U256::from(111_u64));
        cache.storage.insert((unchanged_addr, unchanged_slot), U256::from(999_u64));

        let mut notification =
            CanonStateNotification::<reth_ethereum_primitives::EthPrimitives>::Commit {
                new: Arc::new(Default::default()),
            };

        if let CanonStateNotification::Commit { ref mut new } = notification {
            let chain = Arc::make_mut(new);
            let mut changed_storage = HashMap::default();
            changed_storage.insert(
                changed_slot,
                StorageSlot { present_value: U256::from(222_u64), ..Default::default() },
            );
            chain.execution_outcome_mut().bundle.state.insert(
                changed_addr,
                BundleAccount::new(
                    Some(AccountInfo { nonce: 10, ..Default::default() }),
                    Some(AccountInfo { nonce: 77, ..Default::default() }),
                    changed_storage,
                    AccountStatus::default(),
                ),
            );
        }

        cache.on_epoch_change_diff(&notification);
        cache.pre_fill_diff(&notification);

        let changed_info = cache
            .accounts
            .get(&changed_addr)
            .expect("changed account should be present after prefill");
        assert_eq!(changed_info.expect("changed account info").nonce, 77);
        assert_eq!(
            cache.storage.get(&(changed_addr, changed_slot)).expect("changed slot should be present"),
            U256::from(222_u64),
        );

        let unchanged_info = cache
            .accounts
            .get(&unchanged_addr)
            .expect("unchanged account should remain cached");
        assert_eq!(unchanged_info.expect("unchanged account info").nonce, 99);
        assert_eq!(
            cache
                .storage
                .get(&(unchanged_addr, unchanged_slot))
                .expect("unchanged slot should remain cached"),
            U256::from(999_u64),
        );
    }

    #[test]
    fn test_negative_cache() {
        let cache = GlobalSharedCache::new(256);
        let addr = Address::from([0x03u8; 20]);

        let v1 = cache.get_or_load_account(addr, || Ok(None)).unwrap();
        assert!(v1.is_none());

        let db2_called = Arc::new(AtomicUsize::new(0));
        let c = db2_called.clone();
        let v2 = cache
            .get_or_load_account(addr, move || {
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
            .get_or_load_account(addr, || {
                Ok(Some(AccountInfo { nonce: 77, ..Default::default() }))
            })
            .unwrap();

        let mut l1_b = WorkerL1Cache::new(1);
        let db2_called = Arc::new(AtomicUsize::new(0));
        let c = db2_called.clone();
        let info = cache
            .get_or_load_account(addr, move || {
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
                .get_or_load_account(addr, move || {
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
                    cache.get_or_load_storage(addr, slot, move || {
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
