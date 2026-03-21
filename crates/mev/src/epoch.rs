use alloy_consensus::BlockHeader;
use alloy_eips::{BlockId, BlockNumHash, BlockNumberOrTag};
use alloy_primitives::{BlockHash, BlockNumber, B256};
use reth_chain_state::CanonStateSubscriptions;
use reth_chainspec::ChainInfo;
use reth_evm::ConfigureEvm;
use reth_storage_api::{
    BlockHashReader, BlockIdReader, BlockNumReader, HeaderProvider, StateProviderFactory,
};
use revm::primitives::hardfork::SpecId;
use std::sync::Arc;
use tokio::sync::watch;

use crate::cache::GlobalSharedCache;

type EthEvmEnv = reth_evm::EvmEnvFor<reth_evm_ethereum::EthEvmConfig>;

/// 唯一标识一个区块版本的 epoch 上下文，不可变。
#[derive(Clone)]
pub struct EpochContext {
    /// 单调递增，每个新 committed/reorg tip 对应一个新 epoch_id。
    pub epoch_id: u64,
    pub block_number: u64,
    pub block_hash: B256,
    /// 完整 EVM 区块环境。
    pub block_env: EthEvmEnv,
    /// 对应的 EVM 规格。
    pub spec_id: SpecId,
    /// 用于在该高度打开 StateProvider。
    pub state_provider_factory: Arc<dyn StateProviderFactory + Send + Sync>,
}

impl std::fmt::Debug for EpochContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpochContext")
            .field("epoch_id", &self.epoch_id)
            .field("block_number", &self.block_number)
            .field("block_hash", &self.block_hash)
            .field("spec_id", &self.spec_id)
            .finish_non_exhaustive()
    }
}

impl EpochContext {
    /// EpochManager 初始化占位值。
    pub fn placeholder() -> Self {
        Self {
            epoch_id: 0,
            block_number: 0,
            block_hash: B256::ZERO,
            block_env: EthEvmEnv::default(),
            spec_id: SpecId::default(),
            state_provider_factory: Arc::new(PlaceholderProviderFactory),
        }
    }
}

/// `EpochContext::placeholder()` 的空实现。
struct PlaceholderProviderFactory;

impl BlockHashReader for PlaceholderProviderFactory {
    fn block_hash(&self, _number: BlockNumber) -> reth_errors::ProviderResult<Option<B256>> {
        unimplemented!("placeholder")
    }

    fn canonical_hashes_range(
        &self,
        _start: BlockNumber,
        _end: BlockNumber,
    ) -> reth_errors::ProviderResult<Vec<B256>> {
        unimplemented!("placeholder")
    }
}

impl BlockNumReader for PlaceholderProviderFactory {
    fn chain_info(&self) -> reth_errors::ProviderResult<ChainInfo> {
        unimplemented!("placeholder")
    }

    fn best_block_number(&self) -> reth_errors::ProviderResult<BlockNumber> {
        unimplemented!("placeholder")
    }

    fn last_block_number(&self) -> reth_errors::ProviderResult<BlockNumber> {
        unimplemented!("placeholder")
    }

    fn block_number(&self, _hash: B256) -> reth_errors::ProviderResult<Option<BlockNumber>> {
        unimplemented!("placeholder")
    }
}

impl BlockIdReader for PlaceholderProviderFactory {
    fn pending_block_num_hash(&self) -> reth_errors::ProviderResult<Option<BlockNumHash>> {
        unimplemented!("placeholder")
    }

    fn safe_block_num_hash(&self) -> reth_errors::ProviderResult<Option<BlockNumHash>> {
        unimplemented!("placeholder")
    }

    fn finalized_block_num_hash(&self) -> reth_errors::ProviderResult<Option<BlockNumHash>> {
        unimplemented!("placeholder")
    }
}

impl StateProviderFactory for PlaceholderProviderFactory {
    fn latest(&self) -> reth_errors::ProviderResult<reth_storage_api::StateProviderBox> {
        unimplemented!("placeholder")
    }

    fn state_by_block_number_or_tag(
        &self,
        _number_or_tag: BlockNumberOrTag,
    ) -> reth_errors::ProviderResult<reth_storage_api::StateProviderBox> {
        unimplemented!("placeholder")
    }

    fn history_by_block_number(
        &self,
        _block: BlockNumber,
    ) -> reth_errors::ProviderResult<reth_storage_api::StateProviderBox> {
        unimplemented!("placeholder")
    }

    fn history_by_block_hash(
        &self,
        _block: BlockHash,
    ) -> reth_errors::ProviderResult<reth_storage_api::StateProviderBox> {
        unimplemented!("placeholder")
    }

    fn state_by_block_hash(
        &self,
        _block: BlockHash,
    ) -> reth_errors::ProviderResult<reth_storage_api::StateProviderBox> {
        unimplemented!("placeholder")
    }

    fn pending(&self) -> reth_errors::ProviderResult<reth_storage_api::StateProviderBox> {
        unimplemented!("placeholder")
    }

    fn pending_state_by_hash(
        &self,
        _block_hash: B256,
    ) -> reth_errors::ProviderResult<Option<reth_storage_api::StateProviderBox>> {
        unimplemented!("placeholder")
    }

    fn maybe_pending(
        &self,
    ) -> reth_errors::ProviderResult<Option<reth_storage_api::StateProviderBox>> {
        unimplemented!("placeholder")
    }
}

pub struct EpochManager {
    active_tx: watch::Sender<Arc<EpochContext>>,
    pub active_rx: watch::Receiver<Arc<EpochContext>>,
    debug_fixed_block: Option<BlockNumber>,
}

impl std::fmt::Debug for EpochManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpochManager")
            .field("current_epoch_id", &self.active_rx.borrow().epoch_id)
            .field("current_block_number", &self.active_rx.borrow().block_number)
            .field("debug_fixed_block", &self.debug_fixed_block)
            .finish_non_exhaustive()
    }
}

impl EpochManager {
    fn build_fixed_epoch<P>(
        provider: &P,
        evm_config: &reth_evm_ethereum::EthEvmConfig,
        block_num: BlockNumber,
    ) -> eyre::Result<EpochContext>
    where
        P: HeaderProvider<Header = alloy_consensus::Header>
            + BlockHashReader
            + StateProviderFactory
            + Clone
            + Send
            + Sync
            + 'static,
    {
        let header =
            provider.header_by_number(block_num)?.ok_or_else(|| eyre::eyre!("block {} not found", block_num))?;

        let block_hash = provider
            .block_hash(block_num)?
            .ok_or_else(|| eyre::eyre!("block hash not found for block {}", block_num))?;

        let block_env = evm_config
            .evm_env(&header)
            .map_err(|err| eyre::eyre!("failed to build evm_env: {:?}", err))?;
        let spec_id = *block_env.spec_id();

        Ok(EpochContext {
            epoch_id: 1,
            block_number: block_num,
            block_hash,
            block_env,
            spec_id,
            state_provider_factory: Arc::new((*provider).clone()),
        })
    }

    /// 启动后台任务：监听 canonical state，维护 active epoch。
    ///
    /// `global_cache` is invalidated on every epoch change so that stale
    /// epoch-keyed entries (account and storage slots) are evicted promptly
    /// rather than waiting for TTI expiry.
    pub fn spawn<P>(
        provider: P,
        evm_config: reth_evm_ethereum::EthEvmConfig,
        global_cache: Arc<GlobalSharedCache>,
    ) -> Arc<Self>
    where
        P: CanonStateSubscriptions<Primitives = reth_ethereum_primitives::EthPrimitives>
            + HeaderProvider<Header = alloy_consensus::Header>
            + StateProviderFactory
            + Clone
            + Send
            + Sync
            + 'static,
    {
        let debug_fixed_block = std::env::var("MEV_DEBUG_FIXED_EPOCH")
            .ok()
            .and_then(|value| value.parse::<BlockNumber>().ok());

        let initial = Arc::new(EpochContext::placeholder());
        let (active_tx, active_rx) = watch::channel(initial);

        let manager = Arc::new(Self { active_tx, active_rx, debug_fixed_block });
        let manager_clone = Arc::clone(&manager);
        let fixed_block = debug_fixed_block;

        tokio::spawn(async move {
            if let Some(block_num) = fixed_block {
                // Fixed epoch debug mode: load once, never update.
                match Self::build_fixed_epoch(&provider, &evm_config, block_num) {
                    Ok(epoch) => {
                        let _ = manager_clone.active_tx.send(Arc::new(epoch));
                        tracing::warn!(
                            target: "reth::mev::epoch",
                            block_number = block_num,
                            "MEV_DEBUG_FIXED_EPOCH is set: epoch frozen. \
                             All mev_* requests will use this block's state. NOT for production use."
                        );
                        std::future::pending::<()>().await;
                    }
                    Err(err) => {
                        tracing::error!(
                            target: "reth::mev::epoch",
                            ?err,
                            block_number = block_num,
                            "failed to build fixed epoch, falling back to normal mode"
                        );
                    }
                }
            }

            // Normal mode: subscribe to canonical state notifications.
            let mut notifications = provider.subscribe_to_canonical_state();
            let mut epoch_counter = 0_u64;

            loop {
                match notifications.recv().await {
                    Ok(notification) => {
                        let Some(tip) = notification.tip_checked() else {
                            tracing::warn!(
                                target: "reth::mev::epoch",
                                "received canonical notification with empty new chain"
                            );
                            continue;
                        };

                        let header = tip.header();
                        let block_env = match evm_config.evm_env(header) {
                            Ok(env) => env,
                            Err(err) => {
                                tracing::error!(
                                    target: "reth::mev::epoch",
                                    ?err,
                                    "failed to build evm env for epoch"
                                );
                                continue;
                            }
                        };

                        epoch_counter = epoch_counter.saturating_add(1);
                        let spec_id = *block_env.spec_id();
                        let epoch = Arc::new(EpochContext {
                            epoch_id: epoch_counter,
                            block_number: header.number(),
                            block_hash: tip.hash(),
                            block_env,
                            spec_id,
                            state_provider_factory: Arc::new(provider.clone()),
                        });

                        // Evict all stale epoch-keyed cache entries before
                        // advertising the new epoch.  Entries from the old
                        // epoch_id will never be queried again; releasing them
                        // now prevents unbounded heap growth under burst load.
                        global_cache.on_epoch_change();

                        let _ = manager_clone.active_tx.send(epoch);
                    }
                    Err(err) => {
                        tracing::error!(
                            target: "reth::mev::epoch",
                            ?err,
                            "canonical state subscription ended"
                        );
                        break;
                    }
                }
            }
        });

        manager
    }

    /// 获取当前活跃 epoch 快照（Arc 零拷贝）。
    pub fn current(&self) -> Arc<EpochContext> {
        self.active_rx.borrow().clone()
    }

    /// 判断请求 block_id 是否与 active epoch 匹配。
    pub fn matches_active(&self, block_id: Option<BlockId>) -> bool {
        // In fixed epoch debug mode, all requests go through the worker path.
        if self.debug_fixed_block.is_some() {
            return true;
        }

        match block_id {
            None => true,
            Some(BlockId::Number(num)) => match num {
                BlockNumberOrTag::Latest | BlockNumberOrTag::Pending => true,
                BlockNumberOrTag::Number(n) => n == self.active_rx.borrow().block_number,
                _ => false,
            },
            Some(BlockId::Hash(hash)) => hash.block_hash == self.active_rx.borrow().block_hash,
        }
    }
}
