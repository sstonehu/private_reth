//! `reth-mev` — MEV 路径模拟加速 RPC 扩展（Phase 1）。
//!
//! 提供 `mev_eth_call` / `mev_debug_traceCall` / `mev_trace_call` 三个接口，
//! 请求经 [`EpochManager`] 路由后，投递至独立的 OS 线程 Worker Pool 并行执行 EVM，
//! 减少对 Tokio async 线程的阻塞。
//!
//! 使用 [`install_mev_rpc`] 将模块挂载到 Reth 的 `extend_rpc_modules` 钩子。
#![allow(missing_docs)]
#![allow(clippy::doc_markdown, clippy::missing_const_for_fn)]
#![allow(clippy::module_inception, clippy::large_enum_variant)]

pub mod api;
pub mod cache;
pub mod epoch;
pub mod metrics;
pub mod provider;
pub mod worker;

use crate::{
    api::{
        server::{MevApiServer as MevServer, MevCallConfig},
        MevApiServer as _,
    },
    cache::GlobalSharedCache,
    epoch::EpochManager,
    metrics::MevCounters,
    worker::{MevWorkerPool, DEFAULT_POOL_SIZE},
};
use reth_node_api::{BlockTy, FullNodeComponents, HeaderTy, NodeTypes, ReceiptTy, TxTy};
use reth_node_builder::rpc::RpcContext;
use reth_rpc_convert::{RpcConvert, RpcTypes};
use reth_rpc_eth_api::{
    helpers::{Call, EthCall, EthTransactions, TraceExt},
    EthApiTypes, RpcNodeCore,
};

pub use crate::worker::DEFAULT_POOL_SIZE as DEFAULT_MEV_POOL_SIZE;
pub use epoch::EpochContext;

/// `NodeBuilder::extend_rpc_modules` 的注册入口。
pub fn install_mev_rpc<Node, EthApi>(ctx: RpcContext<'_, Node, EthApi>) -> eyre::Result<()>
where
    Node: FullNodeComponents<Evm = reth_evm_ethereum::EthEvmConfig>,
    Node::Types: NodeTypes<
        Primitives = reth_ethereum_primitives::EthPrimitives,
        ChainSpec: reth_chainspec::EthereumHardforks,
    >,
    Node::Provider: reth_storage_api::FullRpcProvider<
            Header = HeaderTy<Node::Types>,
            Block = BlockTy<Node::Types>,
            Receipt = ReceiptTy<Node::Types>,
            Transaction = TxTy<Node::Types>,
        > + reth_storage_api::AccountReader
        + reth_storage_api::ChangeSetReader
        + reth_chain_state::CanonStateSubscriptions<
            Primitives = reth_ethereum_primitives::EthPrimitives,
        > + reth_storage_api::StateProviderFactory
        + Clone
        + Send
        + Sync
        + 'static,
    Node::Network: reth_network_api::NetworkInfo + reth_network_api::Peers + Clone + 'static,
    EthApi: RpcNodeCore<Evm = reth_evm_ethereum::EthEvmConfig>
        + EthApiTypes<
            NetworkTypes: RpcTypes<TransactionRequest = alloy_rpc_types_eth::TransactionRequest>,
            RpcConvert: RpcConvert<
                Evm = reth_evm_ethereum::EthEvmConfig,
                Network = EthApi::NetworkTypes,
            >,
        > + EthCall
        + Call
        + EthTransactions
        + TraceExt
        + Clone
        + Send
        + Sync
        + 'static,
{
    let provider = ctx.registry.provider().clone();
    let evm_config = ctx.registry.evm_config().clone();
    let eth_api = ctx.registry.eth_api().clone();
    let debug_api = ctx.registry.debug_api();
    let trace_api = ctx.registry.trace_api();

    let call_config = MevCallConfig {
        call_gas_cap: eth_api.call_gas_limit(),
        evm_memory_limit: eth_api.evm_memory_limit(),
    };

    let num_workers = std::env::var("MEV_WORKER_COUNT")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_POOL_SIZE);
    let cache_max_mb = std::env::var("MEV_GLOBAL_CACHE_MAX_MB")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(16_384);
    let global_cache = GlobalSharedCache::new(cache_max_mb);

    // EpochManager holds a reference to global_cache so it can call
    // on_epoch_change() each time the canonical head advances.
    let epoch_manager = EpochManager::spawn(provider, evm_config.clone(), global_cache.clone());

    let worker_pool = MevWorkerPool::new(num_workers, evm_config, global_cache.clone());

    // Shared counters: passed into server for per-request recording and into the
    // periodic reporter for delta-based log summaries.
    let counters = MevCounters::new();
    let stats_interval_secs = std::env::var("MEV_STATS_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(30);
    metrics::spawn_periodic_reporter(
        counters.clone(),
        global_cache,
        std::time::Duration::from_secs(stats_interval_secs),
    );

    let mev_module = MevServer {
        epoch_manager,
        worker_pool,
        call_config,
        counters,
        eth_api,
        debug_api,
        trace_api,
    }
    .into_rpc();
    ctx.modules.merge_configured(mev_module)?;

    tracing::info!(
        target: "reth::mev",
        num_workers,
        cache_max_mb,
        call_gas_cap = call_config.call_gas_cap,
        stats_interval_secs,
        "mev RPC module installed (Phase 2: GlobalSharedCache enabled)"
    );

    Ok(())
}
