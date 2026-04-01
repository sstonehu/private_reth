# MEV 优化记录（Changelog）

> 模块：`crates/mev`（`reth-mev` crate）  
> 目标：单批次 1 万条路径 EVM 模拟，P99 ≤ 205ms  
> 维护：本文件为所有优化阶段的总览记录，具体实现详见各 Phase 详设文档

---

## 目录

- [优化总览](#优化总览)
- [Phase 1：EVM Worker Pool](#phase-1evm-worker-pool)
- [Phase 2：GlobalSharedCache 全局读缓存](#phase-2globalsharedcache-全局读缓存)
- [Phase 3：精确 Diff 缓存失效](#phase-3精确-diff-缓存失效)
- [Phase 4：过期请求快速拒绝 + trace drain 提升](#phase-4过期请求快速拒绝--trace-drain-提升)
- [补丁：热路径 Metrics 开销消除](#补丁热路径-metrics-开销消除)
- [环境变量总览](#环境变量总览)
- [Grafana 监控面板说明](#grafana-监控面板说明)

---

## 优化总览

| Phase | 解决的核心问题 | 关键技术 | 状态 | 延迟改善 |
|-------|--------------|---------|------|---------|
| Phase 1 | 每次请求重建 EVM 上下文 | Worker Pool + Worker-L1 Cache | ✅ 已上线 | 同批次复用后首批外 P99 大幅下降 |
| Phase 2 | Worker 间重复读 DB（无共享缓存） | GlobalSharedCache（moka）+ singleflight | ✅ 已上线 | 稳态 P99 ≤ 205ms |
| Phase 3 | 切块后全量 cache 失效 → 首批冷启动 | 精确 diff 失效 + pre_fill_diff | ✅ 已上线 | 首批 P99 趋近稳态，差距 < 20ms |
| Phase 4 | 过期请求降级引发级联故障 | 快速拒绝（-39001）+ trace gap=1 提升 worker | ✅ 已上线 | 消除降级路径，级联故障正反馈断路 |
| 热路径补丁 | Metrics 采集在 EVM 热路径产生大量原子竞争 | 任务级本地计数器 + 单次 flush | ✅ 已实施 | 减少 ~50–100ms 潜在热路径开销 |

---

## Phase 1：EVM Worker Pool

**详设文档**：[Reth_simulate_optimize_phase1.md](./Reth_simulate_optimize_phase1.md)

### 问题

原生 `eth_call` 对每条请求独立创建 `revm::State` 和 `StateProvider`，在 1 万条/批场景下重建开销累积显著。无复用机制，请求间无任何状态共享。

### 方案

- 常驻 **EVM Worker Pool**（`MevWorkerPool`，`crossbeam-channel` 任务队列）
- 每个 Worker 在同一 epoch 内跨请求共享 **Worker-L1 本地缓存**（clean reads 的 `HashMap`）
- 新增三个 JSON-RPC 接口：`mev_eth_call` / `mev_debug_traceCall` / `mev_trace_call`
- 旧块请求**降级**走原生接口（Phase 4 前的兜底策略）

### 新增文件

```
crates/mev/src/
├── lib.rs              # RPC 模块注册入口
├── epoch.rs            # EpochContext, EpochManager
├── worker/
│   ├── mod.rs          # MevWorkerPool, WorkerTask
│   ├── worker.rs       # MevWorker 主循环
│   └── cache.rs        # WorkerL1Cache（per-worker HashMap）
├── provider.rs         # CachedStateProvider（revm::Database 适配）
└── api/
    ├── server.rs       # RPC 实现体 + 路由逻辑
    └── types.rs        # CallKind 等辅助类型
```

### 新增环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `MEV_WORKER_COUNT` | `40` | Worker 线程数，建议设为物理核心数 60%~80% |

### 验收标准

- 三个接口结果与原生接口完全一致
- Worker-L1 命中率可观测（同 epoch 内连续批次有显著提升）
- 切块后 Worker-L1 数据清空，无状态污染

---

## Phase 2：GlobalSharedCache 全局读缓存

**详设文档**：[Reth_simulate_optimize_phase2.md](./Reth_simulate_optimize_phase2.md)

### 问题

Phase 1 的 Worker-L1 是 per-worker 私有缓存。同一 epoch 内 Worker A 读过的数据对 Worker B 不可见，热点 key（如头部 DEX pool）被 60 个 worker 各自独立打 DB，产生大量冗余读。

### 方案

引入跨所有 Worker 共享的 **GlobalSharedCache**（基于 `moka`，支持并发 LRU + 权重限制）：

```
EVM read(key)
  → Worker-L1 hit    → return
  → Worker-L1 miss
      → GlobalSharedCache hit → backfill L1 → return
      → GlobalSharedCache miss（singleflight，仅一 worker 打 DB）
          → DB read → backfill GlobalCache + L1
```

- **singleflight**：并发 miss 同一 key 时只发出一次 DB 请求（`moka::try_get_with`）
- **权重分配**：account 25% / storage 70% / bytecode 5%（按实际访问比例）
- Phase 2 中 key 格式含 `epoch_id`（切块时全量失效；Phase 3 移除）

### 关键数据

| 维度 | 数值 |
|------|------|
| GlobalCache 默认容量 | 16 GB（`MEV_GLOBAL_CACHE_MAX_MB=16384`） |
| Storage 条目估算（16GB） | ~6100 万条 |
| 实测稳态利用率 | < 1%（典型约 0.57%） |
| 稳态 P99 目标 | ≤ 205ms |

### 新增环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `MEV_GLOBAL_CACHE_MAX_MB` | `16384` | GlobalSharedCache 总内存上限（MB） |
| `MEV_STATS_INTERVAL_SECS` | `30` | 周期性统计日志输出间隔（秒） |
| `MEV_DEBUG_FIXED_EPOCH` | 未设置 | 仅调试用，冻结 epoch 在指定块高 |

---

## Phase 3：精确 Diff 缓存失效

**详设文档**：[Reth_simulate_optimize_phase3.md](./Reth_simulate_optimize_phase3.md)

### 问题

Phase 2 切块时调用 `invalidate_all()`，将全部 ~6000 万条缓存条目驱逐。切块后首批请求全部 miss → MDBX 全量穿透 → Worker 处于冷启动，首批 P99 是稳态的 3~5 倍。

### 关键发现

`CanonStateNotification::Commit` 中的 `execution_outcome()` 包含了该区块执行后所有变更的账户和 storage slot：

```
一个典型区块（200 笔交易）
  → 变更账户数：约 300~800 个
  → 变更 storage slot 数：约 1000~3000 个
  → 占 GlobalCache 总条目比例：< 0.01%
  → 可直接继承（无需重读 DB）：> 99.99%
```

### 方案

1. **移除 key 中的 `epoch_id`**：account key = `Address`，storage key = `(Address, U256)`
2. **切块时精确失效**：仅 `invalidate(changed_key)` diff 中真正变更的条目
3. **切块时预填充（pre_fill_diff）**：将 diff 新值直接写入 GlobalCache，变更账户无需打 DB
4. **灰度开关**：`MEV_DIFF_CACHE=0` 可一键回退 Phase 2 的 `invalidate_all()`

### 架构变更对比

| 对比项 | Phase 2 | Phase 3 |
|--------|---------|---------|
| account key | `(epoch_id, Address)` | `Address` |
| storage key | `(epoch_id, Address, U256)` | `(Address, U256)` |
| 切块失效范围 | 100%（`invalidate_all`） | diff 变更集（< 0.01%） |
| 切块后首批命中率 | 接近 0% | 接近 100% |
| 首批 P99 | 稳态 3~5× | ≈ 稳态（差距 < 20ms） |

### 新增 Metrics

| 指标 | 类型 | 说明 |
|------|------|------|
| `mev_epoch_diff_accounts_total` | gauge | 当前块变更账户数 |
| `mev_epoch_diff_storage_slots_total` | gauge | 当前块变更存储槽数 |
| `mev_epoch_current_block_number` | gauge | 当前 epoch 对应的区块号（用于 Grafana tooltip 定位） |

### 新增环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `MEV_DIFF_CACHE` | `1`（启用） | `0` 回退 Phase 2 全量失效；生产遇问题时无需重新部署即可回退 |

---

## Phase 4：过期请求快速拒绝 + trace drain 提升

**详设文档**：[Reth_simulate_optimize_phase4.md](./Reth_simulate_optimize_phase4.md)

### 问题

Phase 3 上线后通过生产监控（7:50–8:10 时段）发现，流量激增时仍出现 100% 降级率。原因是**正反馈级联故障**：

```
流量激增
  → MEV Bot 处理管道堵塞
  → Bot 发出大量 block_id=N-1 的请求（旧块）
  → 全部降级走原生 eth_call（走 DB，50~200ms）
  → Reth 负载上升，响应变慢
  → Bot 在途任务更多，管道更堵
  → 发出更多旧块请求（正反馈，难以自愈）
```

### Gap 语义澄清

MEV Bot 的请求中，`block_id` 相对于当前 epoch 的 gap 分为两类：

| gap | 含义 | 性质 |
|-----|------|------|
| `gap=0` | 请求块号 = 当前 epoch | 正常 |
| `gap=1` | 请求块号 = epoch - 1 | **正常排空（drain）**：Bot 旧任务自然消化 |
| `gap≥2` | 请求块号 ≤ epoch - 2 | **异常积压（stale）**：管道严重堵塞，需告警 |

Bot **从不**通过 `mev_*` 接口查询历史数据，`gap≥1` 均为管道排空中的过渡请求。

### 方案：差异化处理

| 接口 | gap=0 | gap=1 | gap≥2 |
|------|-------|-------|-------|
| `mev_eth_call` | → worker（N） | → **-39001 错误** | → **-39001 错误** |
| `mev_debug_traceCall` | → worker（N） | → **worker（N）** ← 提升 | → **-39001 错误** |
| `mev_trace_call` | → worker（N） | → **worker（N）** ← 提升 | → **-39001 错误** |

**设计理由**：
- `mev_eth_call`：价格检查对状态敏感，过期状态无意义，且降级会加剧负载。全部拒绝可断路正反馈。
- trace 接口：路径模拟对状态敏感度低，gap=1 在最新 N 状态上执行结果仍有参考价值，无需拒绝。

### 错误码设计

```json
{
  "code": -39001,
  "message": "EpochMismatch: requested block N-1 (gap=1), active epoch is N"
}
```

Bot 侧协议：收到 `-39001` 后排空当前 drain 任务，不重试旧 `block_id`，等待新块后重新发起。

### 代码变更

| 文件 | 变更内容 |
|------|---------|
| `api/server.rs` | `mev_eth_call`：gap≥1 → 返回 -39001；trace：gap=1 → worker，gap≥2 → -39001 |
| `epoch.rs` | 新增 `active_block_number()` 方法 |
| `metrics.rs` | 新增 `record_epoch_mismatch()` 和 `mev_epoch_mismatch_total` 指标 |
| `lib.rs` | 读取 `MEV_REJECT_STALE_CALL` 环境变量并传递给 Server |

### 新增 Metrics

| 指标 | Labels | 说明 |
|------|--------|------|
| `mev_epoch_mismatch_total` | `method`（eth_call/debug_trace/trace）, `reason`（drain/stale/non_number） | 全量 gap 事件计数，替代旧的 `mev_degraded_gap_total` |

> **注**：Phase 4 后 `mev_degraded_path_total` 和 `mev_degraded_gap_total` 将始终为 0，可从告警规则中移除。

### 新增环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `MEV_REJECT_STALE_CALL` | `1`（启用） | `0` 回退 Phase 3 降级行为；Bot 侧适配前可先设 `0` 灰度 |

---

## 补丁：热路径 Metrics 开销消除

> 实施时间：Phase 4 完成后  
> 涉及文件：`crates/mev/src/provider.rs`、`crates/mev/src/worker/worker.rs`

### 问题

`CachedStateProvider` 实现 `revm::Database` 时，在每次 EVM 状态读取（`basic`/`storage`/`code_by_hash`）的 L1 命中/未命中路径上直接调用 `metrics::counter!().increment(1)`。

**开销分析**（以 60 worker、10,000 条/批为例）：

| 层面 | 实际操作 | 量级 |
|------|---------|------|
| `metrics::counter!()` 宏 | 全局 `DashMap` 查找（哈希 + 桶锁） | 每次 EVM 读各 1 次 |
| `.increment(1)` | `AtomicU64::fetch_add`（`Relaxed`） | 每次 EVM 读各 1 次 |
| 60 worker 并发 | 写同一 `AtomicU64` → cache-line 竞争（false sharing） | 峰值每秒 100 万次 |

估算每批额外开销：50~100ms，占 205ms 目标的 25%~50%。

### 方案

引入 `ProviderStats` 结构体（任务级本地计数器，纯 `u64`，无共享），在任务完成后一次性 flush 到 Prometheus：

```
旧：每次 EVM 读 → DashMap 查找 + AtomicU64 写（O(EVM 读次数)）
新：每次 EVM 读 → 本地 u64 += 1（无锁无竞争）
    任务结束 → 最多 9 次 DashMap 查找 + AtomicU64 写（O(1) per task）
```

### 代码变更

**`provider.rs`**：

```rust
/// 任务级本地计数器，无原子操作，无共享状态。
#[derive(Debug, Default)]
pub struct ProviderStats {
    pub l1_hits_account: u64,
    pub l1_hits_storage: u64,
    pub l1_hits_bytecode: u64,
    pub l1_misses_account: u64,
    pub l1_misses_storage: u64,
    pub l1_misses_bytecode: u64,
    pub db_reads_account: u64,
    pub db_reads_storage: u64,
    pub db_reads_bytecode: u64,
}

impl ProviderStats {
    /// 任务结束时调用一次，最多 9 次 Prometheus 操作（跳过为零的计数器）。
    pub fn flush(&self) { ... }
}
```

**`worker.rs`**（`execute_task`）：

```rust
// 构建时初始化
let worker_provider = CachedStateProvider {
    ...,
    stats: ProviderStats::default(),
};

// 执行完成后统一 flush
let result = match &task.kind { ... };
db.database.stats.flush();  // ← 单次批量写出，替代热路径原子操作
result
```

### 性能收益

| 对比 | 旧方案 | 新方案 |
|------|--------|--------|
| 每次 EVM 读的 metrics 操作 | DashMap 查找 + AtomicU64 | 本地 `+= 1`（L1 cache） |
| 每个任务的 Prometheus 写次数 | O(EVM 读次数) ≈ 数千次 | 最多 9 次 |
| 60 worker 并发写竞争 | 高（同一 AtomicU64） | 消除（各自本地计数） |
| 数据精度 | 相同（最终结果一致） | 相同 |

---

## 环境变量总览

| 变量名 | 默认值 | 引入阶段 | 作用 |
|--------|--------|---------|------|
| `MEV_WORKER_COUNT` | `40` | Phase 1 | Worker 线程数；建议为物理核心数的 60%~80% |
| `MEV_GLOBAL_CACHE_MAX_MB` | `16384`（16 GB） | Phase 2 | GlobalSharedCache 总内存上限（MB） |
| `MEV_STATS_INTERVAL_SECS` | `30` | Phase 2 | 周期性统计日志间隔（秒） |
| `MEV_DEBUG_FIXED_EPOCH` | 未设置 | Phase 2 | 仅调试：冻结 epoch 在指定块高，⚠️ 禁用于生产 |
| `MEV_DIFF_CACHE` | `1` | Phase 3 | `0` = 回退 Phase 2 全量失效；灰度/故障回退开关 |
| `MEV_REJECT_STALE_CALL` | `1` | Phase 4 | `0` = 回退 Phase 3 降级行为；Bot 适配前可先关闭 |

### 典型生产配置（systemd）

```ini
[Service]
Environment=MEV_WORKER_COUNT=32
Environment=MEV_GLOBAL_CACHE_MAX_MB=8192
Environment=MEV_STATS_INTERVAL_SECS=60
Environment=MEV_DIFF_CACHE=1
Environment=MEV_REJECT_STALE_CALL=1
```

### 快速回退命令

```bash
# 回退 Phase 3（diff 缓存）→ Phase 2（全量失效）
systemctl edit reth
# 追加: Environment=MEV_DIFF_CACHE=0

# 回退 Phase 4（快速拒绝）→ Phase 3（降级行为）
# 追加: Environment=MEV_REJECT_STALE_CALL=0

systemctl daemon-reload && systemctl restart reth
```

---

## Grafana 监控面板说明

> Dashboard 文件：[myReth_grafana.json](./myReth_grafana.json)

### 面板结构

| Row | 面板 | 核心指标 |
|-----|------|---------|
| 请求速率 | 各接口 QPS、eth_call gap 事件 | `mev_worker_tasks_total`、`mev_epoch_mismatch_total` |
| Gap 数量分布（Phase 4） | drain/stale 速率汇总、按接口细分 | `mev_epoch_mismatch_total{method, reason}` |
| 延迟分布 | E2E P50/P99 | `mev_e2e_latency_seconds` |
| 缓存状态 | L1 命中率、Global 命中率、DB 穿透 | `mev_worker_l1_hits_total`、`mev_global_cache_db_reads_total` |
| Worker 队列 | 队列深度、epoch 切换次数 | `mev_worker_queue_depth`、`mev_worker_epoch_switches_total` |
| Epoch Diff 统计 | 每块变更账户数/存储槽数趋势（含 blockNumber） | `mev_epoch_diff_accounts_total`、`mev_epoch_diff_storage_slots_total`、`mev_epoch_current_block_number` |

### Phase 4 后的指标变化

| 旧指标 | 状态 | 替代指标 |
|--------|------|---------|
| `mev_degraded_path_total` | 始终为 0（可保留观察） | — |
| `mev_degraded_gap_total` | 始终为 0（可保留观察） | `mev_epoch_mismatch_total` |
| "降级率 %" 面板 | 已替换 | "Epoch Mismatch 速率"（drain/stale 分层） |

### 关键告警建议

```yaml
# stale 请求持续增加（管道积压）
alert: MevStalePipelineBacklog
expr: rate(reth_mev_epoch_mismatch_total{reason="stale"}[1m]) > 5
severity: warning

# DB 穿透率突增（缓存命中率下降）
alert: MevCacheDBReadSpike
expr: rate(reth_mev_global_cache_db_reads_total[1m]) > 10000
severity: info
```

---

## 设计决策备忘

### 为何不复用 Engine 的 ExecutionCache

详见[主架构文档 §13](./mev-path-simulation-architecture-v3.md#13-设计决策记录为何不复用-engine-的-executioncache)。核心原因：

- `Arc::clone` 共享：Engine 写 N+1 会覆盖 MEV 正在读的 N 的数据（正确性破坏）
- 深拷贝：`fixed_cache` 无遍历 API，无法独立复制
- 结论：MEV 维护独立的 `GlobalSharedCache`，参照 `insert_state` 逻辑实现 `pre_fill_diff`

### 为何不做双 epoch 缓存

在评估 Phase 4 时曾考虑同时维护 epoch N 和 N-1 的缓存状态：
- 内存开销翻倍（需同时维护两套 GlobalCache 和 Worker Pool）
- 状态管理复杂度高（滚动更新、N-2 驱逐时机）
- MEV Bot 从不查询历史数据，gap≥1 请求均为过渡排空，无需精确历史结果
- 结论：快速拒绝（Phase 4）比双 epoch 缓存更简单、更有效

### 关于 GlobalCache 容量

当前默认 16GB，约 6100 万 storage 条目。实测稳态利用率约 0.57%，缓存远未达到上限。DB 穿透来自：
1. cache cold miss（初次访问）→ Phase 3 切块后已大幅降低
2. diff 精确失效后重读（变更 slot 约 1000~3000/块）→ 属于正常行为

结论：缓存容量不是瓶颈，可根据实际内存资源酌情调低（如 4~8 GB）。
