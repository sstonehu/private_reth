//! Block impact 识别模块：从 CanonStateNotification 中提取 changed_raw_ids。
//!
//! # 设计约束（见 design-reth-block-impact.md §2.1）
//!
//! reth 侧**不维护**已知池地址 / poolId / dexId 集合，也不维护 ABI 或 topic 清单。
//! handler 对监听地址上的所有 log 按固定规则提取候选 ID，直接写入 changed_raw_ids。
//! 过滤职责由 pricer 的 `pool.rawIds ∩ changed_raw_ids` 匹配承担：
//!   - false positive（假 ID 进入集合）：pricer 最多额外重计算少量不相关 pool，结果仍正确。
//!   - false negative（漏掉真实 pool）：不会发生——reth 侧只要"多写"，不会"少写"。
//!
//! # 先验知识（reth 侧仅需）
//!
//! | 内容 | 来源 |
//! |------|------|
//! | S2/S3 共享合约地址（代码内置常量）| 合约升级时更新 |
//! | FluidDexLite `LogSwap` topic0（1 个常量，代码内嵌）| ABI 变更时更新 |

use alloy_primitives::{address, Address, B256};
use reth_chain_state::CanonStateNotification;
use reth_ethereum_primitives::EthPrimitives;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, OnceLock},
};

// ── 共享合约主网常量地址 ──────────────────────────────────────────────────────
const BALANCER_V2_VAULT: Address =
    address!("BA12222222228d8Ba445958a75a0704d566BF2C8");
const BALANCER_V3_VAULT: Address =
    address!("bA1333333333a1BA1108E8412f11850A5C319bA9");
const UNISWAP_V4_POOL_MANAGER: Address =
    address!("000000000004444c5dc75cB358380D2e3dE08A90");
const FLUID_DEX_LITE: Address =
    address!("bBCB91440523216E2b87052A99F69c604A7B6e00");
const CORE_SWAP_POOL_MANAGER: Address =
    address!("e0e0e08A6A4B9dc7Bd67bCB7AAde5cf48157D444");
const CORE_SWAP_V3_POOL_MANAGER: Address =
    address!("00000000000014AA86C5d3C41765bb24E11bD701");

// ── FluidDexLite LogSwap topic0（唯一内嵌常量，区分两种 data 布局） ─────────────
static FLUID_LOG_SWAP_TOPIC0: OnceLock<B256> = OnceLock::new();
fn fluid_log_swap_topic0() -> &'static B256 {
    FLUID_LOG_SWAP_TOPIC0
        .get_or_init(|| alloy_primitives::keccak256(b"LogSwap(uint256,uint256)"))
}

// ────────────────────────────────────────────────────────────────────────────
// Handler 接口
// ────────────────────────────────────────────────────────────────────────────

pub trait ImpactLogHandler: Send + Sync {
    /// 从单条 log 提取候选 rawId，直接写入 changed_raw_ids。
    /// 不做已知集合过滤——pricer 侧负责最终匹配。
    fn process_log(
        &self,
        log: &alloy_primitives::Log,
        changed_raw_ids: &mut HashSet<String>,
    );
}

// ────────────────────────────────────────────────────────────────────────────
// S2-A: BalancerV2
// topics[1] = bytes32 poolId，poolId[0:20] = pool 合约地址
// 非池事件（InternalBalanceChanged 等）topics[1] 为 ABI address 编码：
//   前12字节为0，取 [0:20] 后不是合法 pool 地址，pricer 侧自动过滤。
// ────────────────────────────────────────────────────────────────────────────
#[derive(Debug)]
pub struct BalancerV2Handler;
impl ImpactLogHandler for BalancerV2Handler {
    fn process_log(&self, log: &alloy_primitives::Log, changed: &mut HashSet<String>) {
        if log.topics().len() < 2 {
            return;
        }
        let pool_addr = Address::from_slice(&log.topics()[1].as_slice()[0..20]);
        changed.insert(pool_addr.to_checksum(None));
    }
}

// ────────────────────────────────────────────────────────────────────────────
// S2-B: BalancerV3
// topics[1] = address pool（ABI 左填零编码），取后20字节
// Buffer 事件的 wrappedToken 地址不在 pool.rawIds 中，pricer 侧自动过滤。
// ────────────────────────────────────────────────────────────────────────────
#[derive(Debug)]
pub struct BalancerV3Handler;
impl ImpactLogHandler for BalancerV3Handler {
    fn process_log(&self, log: &alloy_primitives::Log, changed: &mut HashSet<String>) {
        if log.topics().len() < 2 {
            return;
        }
        let pool_addr = Address::from_slice(&log.topics()[1].as_slice()[12..32]);
        changed.insert(pool_addr.to_checksum(None));
    }
}

// ────────────────────────────────────────────────────────────────────────────
// S2-C: UniswapV4
// topics[1] = bytes32 PoolId（keccak256 哈希）
// Transfer/Approval 的 topics[1] 为 ABI address 编码（12字节前缀0），
// 与 keccak256 PoolId 碰撞概率极低，pricer 侧自动过滤。
// ────────────────────────────────────────────────────────────────────────────
#[derive(Debug)]
pub struct UniswapV4Handler;
impl ImpactLogHandler for UniswapV4Handler {
    fn process_log(&self, log: &alloy_primitives::Log, changed: &mut HashSet<String>) {
        if log.topics().len() < 2 {
            return;
        }
        changed.insert(format!("{:#x}", log.topics()[1]));
    }
}

// ────────────────────────────────────────────────────────────────────────────
// S3-A: FluidDexLite
//
// 所有事件均可精确解码 dexId，两种固定格式：
//
//   topics[0] == LogSwap_topic0  →  data[24:32] = swapData 低64位 = dexId
//   其他事件（LogDeposit/LogWithdraw/LogUpdate*）
//     → data[96:104] = dexKey(token0,token1,salt) 之后的 bytes8 = dexId
//
// 结构不符的协议级事件：解出随机 u64，pricer 侧无匹配，自动忽略。
// ────────────────────────────────────────────────────────────────────────────
#[derive(Debug)]
pub struct FluidDexLiteHandler;
impl ImpactLogHandler for FluidDexLiteHandler {
    fn process_log(&self, log: &alloy_primitives::Log, changed: &mut HashSet<String>) {
        let data = log.data.data.as_ref();
        let dex_id: u64 = if log.topics().first() == Some(fluid_log_swap_topic0()) {
            // LogSwap：swapData = data[0:32]，dexId = 低64位（data[24:32]）
            if data.len() < 32 {
                return;
            }
            u64::from_be_bytes(data[24..32].try_into().unwrap())
        } else {
            // LogDeposit/LogWithdraw/LogUpdate*：
            //   word0=token0[0:32], word1=token1[32:64], word2=salt[64:96], word3=dexId(bytes8)[96:104]
            if data.len() < 104 {
                return;
            }
            u64::from_be_bytes(data[96..104].try_into().unwrap())
        };
        // format: "0x000000000000001a"（16 hex chars = 8 bytes）
        changed.insert(format!("{dex_id:#018x}"));
    }
}

// ────────────────────────────────────────────────────────────────────────────
// S3-B: coreSwap / coreSwapV3
//
// 两种固定格式：
//   topics[0] == 0（raw_log0）  →  data[20:52] = poolId（caller 20B 之后）
//   topics[0] != 0（标准 ABI）  →  data[0:32]  = poolId（第一个 bytes32 参数）
//
// poolId 为 keccak256 哈希（256位空间），随机32字节命中真实 poolId 概率≈零。
// pricer 侧自动过滤不匹配的 poolId。
// ────────────────────────────────────────────────────────────────────────────
#[derive(Debug)]
pub struct CoreSwapHandler;
impl ImpactLogHandler for CoreSwapHandler {
    fn process_log(&self, log: &alloy_primitives::Log, changed: &mut HashSet<String>) {
        let data = log.data.data.as_ref();
        let pool_id = if log.topics().is_empty() || log.topics()[0] == B256::ZERO {
            // raw_log0：自定义紧密打包，caller(20B) + poolId(32B) + ...
            if data.len() < 52 {
                return;
            }
            B256::from_slice(&data[20..52])
        } else {
            // 标准 ABI 事件：poolId 为第一个非 indexed bytes32 参数 → data[0:32]
            if data.len() < 32 {
                return;
            }
            B256::from_slice(&data[0..32])
        };
        changed.insert(format!("{pool_id:#x}"));
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 注册表
// ────────────────────────────────────────────────────────────────────────────

/// 共享合约地址 → handler 的映射表。
/// reth 侧仅需注册 Vault / PoolManager 等少数共享合约地址（来自环境变量）。
pub struct BlockImpactRegistry {
    handlers: HashMap<Address, Box<dyn ImpactLogHandler>>,
}

impl std::fmt::Debug for BlockImpactRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockImpactRegistry")
            .field("handler_count", &self.handlers.len())
            .field("addresses", &self.handlers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl BlockImpactRegistry {
    /// 使用代码内置的共享合约主网地址构建注册表。
    pub fn mainnet_defaults() -> Arc<Self> {
        let mut handlers: HashMap<Address, Box<dyn ImpactLogHandler>> = HashMap::new();

        macro_rules! register_handler {
            ($addr:expr, $handler:expr, $name:literal) => {{
                let addr = $addr;
                handlers.insert(addr, Box::new($handler));
                tracing::info!(
                    target: "reth::mev::impact",
                    addr = ?addr,
                    handler = $name,
                    "registered impact handler"
                );
            }};
        }

        register_handler!(BALANCER_V2_VAULT, BalancerV2Handler, "BalancerV2Handler");
        register_handler!(BALANCER_V3_VAULT, BalancerV3Handler, "BalancerV3Handler");
        register_handler!(UNISWAP_V4_POOL_MANAGER, UniswapV4Handler, "UniswapV4Handler");
        register_handler!(FLUID_DEX_LITE, FluidDexLiteHandler, "FluidDexLiteHandler");
        register_handler!(
            CORE_SWAP_POOL_MANAGER,
            CoreSwapHandler,
            "CoreSwapHandler(coreSwap)"
        );
        register_handler!(
            CORE_SWAP_V3_POOL_MANAGER,
            CoreSwapHandler,
            "CoreSwapHandler(coreSwapV3)"
        );

        // 预热 FluidDexLite topic0 常量（避免首个 block 时延）
        let _ = fluid_log_swap_topic0();

        Arc::new(Self { handlers })
    }

    pub fn get(&self, addr: &Address) -> Option<&dyn ImpactLogHandler> {
        self.handlers.get(addr).map(|h| h.as_ref())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 核心计算函数
// ────────────────────────────────────────────────────────────────────────────

/// 从 CanonStateNotification 计算 changed_raw_ids。
///
/// S1：全量 state diff 地址 → changed_raw_ids（不过滤，pricer 侧匹配）
/// S2/S3：receipts logs → shared contract handler → changed_raw_ids
pub fn compute_changed_raw_ids(
    notification: &CanonStateNotification<EthPrimitives>,
    registry: &BlockImpactRegistry,
) -> Vec<String> {
    let new_chain = match notification {
        CanonStateNotification::Commit { new } => new.as_ref(),
        CanonStateNotification::Reorg { new, .. } => new.as_ref(),
    };

    let outcome = new_chain.execution_outcome();
    let mut changed: HashSet<String> = HashSet::new();

    // ── S1：全量 state diff ───────────────────────────────────────────────
    // 所有有 storage/nonce/balance 变化的合约地址直接写入。
    // 包含：per-pool 合约（S1-A/B/C）、共享合约（S1-D）、hook 合约（S2-B/C 补充）。
    // 地址使用 EIP-55 checksum 格式，与 Go 侧 NormalizeAddress 输出一致。
    // pricer 侧 pool.rawIds 中若有对应地址则命中，否则安全忽略。
    for (addr, _) in outcome.bundle_accounts_iter() {
        changed.insert(addr.to_checksum(None));
    }

    // ── S2/S3：receipts logs ──────────────────────────────────────────────
    // 仅处理已注册共享合约地址上的 log，提取候选 pool/dex ID。
    // 直接访问 reth_ethereum_primitives::Receipt::logs 公开字段，无需引入 Receipt trait。
    for block_receipts in outcome.receipts_iter() {
        for receipt in block_receipts {
            for log in &receipt.logs {
                if let Some(handler) = registry.get(&log.address) {
                    handler.process_log(log, &mut changed);
                }
            }
        }
    }

    changed.into_iter().collect()
}
