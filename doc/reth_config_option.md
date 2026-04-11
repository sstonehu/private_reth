# Reth Config Options

本文汇总 `private_reth/doc` 下各阶段文档中出现过的配置项，便于统一查阅。

适用范围说明：

- 本文只整理 `doc/` 目录中已经明确写出的配置项
- 优先记录当前仍有效的配置
- 对已移除或仅存在于文档说明中的项，会单独标注状态

## 当前有效环境变量

以下环境变量在文档中被明确声明，且当前仍属于 MEV 模块的有效运行配置。

| 变量名 | 默认值 | 引入阶段 | 作用 | 备注 |
|--------|--------|----------|------|------|
| `MEV_WORKER_COUNT` | `40` | Phase 1 | EVM Worker Pool 的 worker 线程数 | 建议按物理核心数的 60%~80% 设置 |
| `MEV_GLOBAL_CACHE_MAX_MB` | `16384`（16 GB） | Phase 2 | `GlobalSharedCache` 总内存上限（MB） | 内部再按固定比例分配到各子缓存 |
| `MEV_STATS_INTERVAL_SECS` | `30` | Phase 2 | 周期性统计日志输出间隔（秒） | 设为 `0` 无效，最小生效值为 `1` |
| `MEV_DEBUG_FIXED_EPOCH` | 未设置 | Phase 2 | 冻结 EpochManager 在指定块高 | 仅调试可用，禁止生产启用 |
| `MEV_DIFF_CACHE` | `1`（启用） | Phase 3 | 控制是否启用 diff cache 精确失效 | `0` 回退到 Phase 2 的全量失效 |
| `MEV_REJECT_STALE_CALL` | `1`（启用） | Phase 4 | 控制 `mev_eth_call` 是否快速拒绝旧 epoch 请求 | `0` 回退到 Phase 3 的 degrade 行为 |

## 各配置项详情

### `MEV_WORKER_COUNT`

- 默认值：`40`
- 引入阶段：Phase 1
- 作用：控制 EVM Worker Pool 的并行 worker 数量
- 调整建议：通常按物理核心数的 60%~80% 设置，为 Tokio、Engine、MDBX 等留出余量
- 典型场景：CPU 紧张时下调；任务排队严重时适度上调

### `MEV_GLOBAL_CACHE_MAX_MB`

- 默认值：`16384`
- 引入阶段：Phase 2
- 作用：限制 `GlobalSharedCache` 的总内存预算
- 备注：这是总预算，不是单个子缓存预算

文档中明确给出的固定分配比例：

| 子缓存 | 固定比例 | 默认值（16 GB） |
|--------|----------|-----------------|
| `storage` | `70%` | 约 `11.5 GB` |
| `accounts` | `25%` | 约 `4 GB` |
| `bytecodes` | `5%` | 约 `800 MB` |

### `MEV_STATS_INTERVAL_SECS`

- 默认值：`30`
- 引入阶段：Phase 2
- 作用：控制周期性 `tracing::info` 统计日志输出频率
- 备注：文档说明 `0` 不生效，最小有效值为 `1`

### `MEV_DEBUG_FIXED_EPOCH`

- 默认值：未设置
- 引入阶段：Phase 2
- 作用：将 `EpochManager` 冻结在指定块高，所有 `mev_*` 请求都使用该块状态
- 典型用途：离线回放、压测、稳定复现 worker 路径
- 风险说明：节点同步仍继续，但 MEV 服务状态固定，不再反映最新链状态
- 使用建议：仅测试环境开启，生产禁止使用

示例：

```bash
MEV_DEBUG_FIXED_EPOCH=21000000 reth node --http --http.api eth,debug,trace,mev
```

### `MEV_DIFF_CACHE`

- 默认值：`1`
- 引入阶段：Phase 3
- 作用：控制是否启用 `on_epoch_change_diff + pre_fill_diff` 的精确失效方案
- 配置语义：
  - `1`：启用 Phase 3 diff cache
  - `0`：回退到 Phase 2 的 `invalidate_all()`
- 典型用途：生产灰度、故障快速回退

### `MEV_REJECT_STALE_CALL`

- 默认值：`1`
- 引入阶段：Phase 4
- 作用：控制 `mev_eth_call` 遇到旧 epoch 请求时，是快速拒绝还是回退 degrade
- 配置语义：
  - `1`：Phase 4 生效，旧请求返回 `-39001`
  - `0`：回退到 Phase 3 的原生 `eth_call` degrade 路径
- 典型用途：Bot 适配 `-39001` 前先灰度关闭；适配完成后再开启

## 文档中提到的固定内部参数

以下项在文档里被当作“配置相关参数”描述，但当前仍是代码内固定值，不是环境变量。

| 名称 | 当前值 | 状态 | 说明 |
|------|--------|------|------|
| `TASK_QUEUE_CAPACITY` | `65536` | 硬编码 | Worker 有界队列容量上限 |
| `GlobalSharedCache` 三段比例 | `accounts 25% / storage 70% / bytecodes 5%` | 固定分配 | 由 `MEV_GLOBAL_CACHE_MAX_MB` 派生，不单独配置 |

## 已移除或不再生效的配置

以下项在文档里明确提到，但当前语义是“已移除”或“不再需要”，应视为不可配置。

| 名称 | 状态 | 说明 |
|------|------|------|
| `time_to_idle` / `TTI` | 已移除 | Phase 3 文档明确说明已删除，不再作为缓存生命周期配置 |

## 推荐阅读顺序

如果只想快速了解当前生效配置，建议按以下顺序阅读原文：

1. `mev-path-simulation-architecture-v3.md`
2. `mev-optimization-changelog.md`
3. `Reth_simulate_optimize_phase4.md`
4. `Reth_simulate_optimize_phase3.md`
5. `Reth_simulate_optimize_phase2.md`

## 原始来源

本汇总主要来自以下文档章节：

- `doc/Reth_simulate_optimize_phase2.md`
- `doc/Reth_simulate_optimize_phase3.md`
- `doc/Reth_simulate_optimize_phase4.md`
- `doc/mev-optimization-changelog.md`
- `doc/mev-path-simulation-architecture-v3.md`
