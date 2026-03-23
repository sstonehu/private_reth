use alloy_consensus::BlockHeader;
use alloy_eips::{BlockId, BlockNumHash, BlockNumberOrTag};
use alloy_primitives::{BlockHash, BlockNumber, B256};
use reth_chain_state::{CanonStateNotification, CanonStateSubscriptions};
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
    /// Whether Phase 3 diff-based cache invalidation is active.
    /// Set MEV_DIFF_CACHE=0 to fall back to Phase 2 full invalidation.
    diff_cache_enabled: bool,
}

impl std::fmt::Debug for EpochManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpochManager")
            .field("current_epoch_id", &self.active_rx.borrow().epoch_id)
            .field("diff_cache_enabled", &self.diff_cache_enabled)
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
    /// `global_cache` applies precise diff invalidation and prefill on every
    /// canonical notification so unchanged entries stay warm across epochs.
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

        // MEV_DIFF_CACHE=0 falls back to Phase 2 full invalidation on every block.
        // All other values (including unset) keep Phase 3 diff-based invalidation.
        let diff_cache_enabled =
            std::env::var("MEV_DIFF_CACHE").map(|v| v != "0").unwrap_or(true);

        tracing::info!(
            target: "reth::mev::epoch",
            diff_cache_enabled,
            "EpochManager starting"
        );

        let initial = Arc::new(EpochContext::placeholder());
        let (active_tx, active_rx) = watch::channel(initial);

        let manager = Arc::new(Self { active_tx, active_rx, debug_fixed_block, diff_cache_enabled });
        let manager_clone = Arc::clone(&manager);
        let fixed_block = debug_fixed_block;
        let diff_cache = diff_cache_enabled;

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
                        // ── t0: canonical notification arrived ───────────────
                        // Captures the wall-clock time at which the Engine API
                        // has finished processing the block and reth's internal
                        // pipeline has committed it to canonical chain.
                        // net_engine_delay = t0 - block.timestamp
                        //   ≈ network propagation + Engine API (newPayload /
                        //     forkchoiceUpdated) + reth pipeline time.
                        let t0_wall = std::time::SystemTime::now();
                        let t0_instant = std::time::Instant::now();

                        let Some(tip) = notification.tip_checked() else {
                            tracing::warn!(
                                target: "reth::mev::epoch",
                                "received canonical notification with empty new chain"
                            );
                            continue;
                        };

                        let header = tip.header();

                        // Segment 1: network + Engine API delay.
                        // Measured at notification arrival, before any MEV
                        // processing, so it does NOT include EpochManager work
                        // or (future Phase 3) cache pre-warming.
                        let net_engine_delay_secs = t0_wall
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| (d.as_secs_f64() - header.timestamp() as f64).max(0.0))
                            .unwrap_or(0.0);

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

                        let warmup_start = std::time::Instant::now();
                        if diff_cache {
                            global_cache.on_epoch_change_diff(&notification);
                            global_cache.pre_fill_diff(&notification);
                        } else {
                            // Phase 2 fallback: full invalidation (MEV_DIFF_CACHE=0).
                            global_cache.on_epoch_change();
                        }
                        let warmup_secs = warmup_start.elapsed().as_secs_f64();
                        metrics::histogram!("mev_epoch_warmup_duration_seconds")
                            .record(warmup_secs);
                        metrics::gauge!("mev_epoch_warmup_duration_latest_seconds")
                            .set(warmup_secs);

                        if let CanonStateNotification::Commit { ref new } = notification {
                            let diff_accounts =
                                new.execution_outcome().bundle_accounts_iter().count() as f64;
                            let diff_slots: f64 = new
                                .execution_outcome()
                                .bundle_accounts_iter()
                                .map(|(_, account)| account.storage.len() as f64)
                                .sum();
                            metrics::gauge!("mev_epoch_diff_accounts_total")
                                .set(diff_accounts);
                            metrics::gauge!("mev_epoch_diff_storage_slots_total").set(diff_slots);
                            // Record the committed block number so Grafana tooltips can show
                            // which block produced this diff, enabling precise event correlation.
                            metrics::gauge!("mev_epoch_current_block_number")
                                .set(header.number() as f64);
                        }

                        let _ = manager_clone.active_tx.send(epoch);

                        // ── t1: epoch published, MEV workers can now serve ──
                        // Segment 2: EpochManager processing delay.
                        // = build_block_env + build_epoch + on_epoch_change_diff + pre_fill_diff.
                        let epoch_manager_delay_secs = t0_instant.elapsed().as_secs_f64();

                        // Total delay = net_engine + epoch_manager
                        let total_delay_secs = net_engine_delay_secs + epoch_manager_delay_secs;

                        metrics::histogram!("mev_net_engine_delay_seconds")
                            .record(net_engine_delay_secs);
                        metrics::gauge!("mev_net_engine_delay_latest_seconds")
                            .set(net_engine_delay_secs);

                        metrics::histogram!("mev_epoch_manager_delay_seconds")
                            .record(epoch_manager_delay_secs);
                        metrics::gauge!("mev_epoch_manager_delay_latest_seconds")
                            .set(epoch_manager_delay_secs);

                        // Keep the aggregate metric for dashboards that track
                        // overall MEV readiness latency end-to-end.
                        metrics::histogram!("mev_epoch_block_delay_seconds")
                            .record(total_delay_secs);
                        metrics::gauge!("mev_epoch_block_delay_latest_seconds")
                            .set(total_delay_secs);

                        tracing::debug!(
                            target: "reth::mev::epoch",
                            block_number = header.number(),
                            block_delay_us             = (net_engine_delay_secs    * 1_000_000.0) as u64,
                            mev_epoch_manager_delay_us = (epoch_manager_delay_secs * 1_000_000.0) as u64,
                            mev_block_delay_us         = (total_delay_secs         * 1_000_000.0) as u64,
                            "new epoch ready"
                        );
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

    /// 返回请求块号与 active epoch 的差值（仅对 explicit Number 有意义）。
    ///
    /// 用于降级路径上区分两种情况：
    /// - gap = 1：正常排空——新块到来时旧 in-flight 请求自然滞后一块，属预期行为。
    /// - gap ≥ 2：管道积压异常——Bot 处理管道堵塞，需告警。
    ///
    /// 返回 None 表示 block_id 不是 explicit Number（latest / hash / none 等），
    /// 这类请求走 matches_active 的其他分支，不会因块号失配而降级，无需分类。
    pub fn block_gap(&self, block_id: Option<BlockId>) -> Option<u64> {
        let active = self.active_rx.borrow().block_number;
        match block_id {
            Some(BlockId::Number(BlockNumberOrTag::Number(n))) => Some(active.saturating_sub(n)),
            _ => None,
        }
    }

    /// 返回当前 active epoch 的块号，供 EpochMismatch 错误体使用。
    pub fn active_block_number(&self) -> u64 {
        self.active_rx.borrow().block_number
    }
}
