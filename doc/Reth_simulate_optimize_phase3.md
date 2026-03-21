# MEV Phase 3 详细设计：精确 Diff 缓存失效

> 版本：v1  
> 依赖：Phase 2（GlobalSharedCache + CachedStateProvider 已上线）  
> 目标：切块后首批 `T_batch_core` P99 与稳态 P99 差距 < 20ms

---

## 1. 背景与动机

### 1.1 Phase 2 遗留问题

Phase 2 引入了 `GlobalSharedCache`，key 格式为 `(epoch_id, address)` / `(epoch_id, address, slot)`。  
每当新块到达，`EpochManager` 调用 `invalidate_all()`，将全部缓存条目驱逐。

```
Phase 2 切块时序：
  新块到达
    → invalidate_all()        // 全量驱逐，~60M 条目
    → 广播新 epoch
    → worker 首批请求全部 miss
    → MDBX 全量穿透
    → 逐渐预热（需 2~3 批）
```

**代价**：切块后首批 MDBX 穿透量 ≈ 全量，Worker 处于冷启动状态，首批 P99 可达稳态的 3~5×。

### 1.2 关键发现

`CanonStateNotification::Commit { new }` 中的 `Chain` 包含 `execution_outcome()`，其 `BundleState` 记录了该区块执行后**所有发生变更**的账户和存储槽，以及每个条目的新值。

```
一个普通区块（200 笔交易）
  → 变更账户数：约 300~800 个
  → 变更 storage slot 数：约 1000~3000 个

GlobalSharedCache 共 6000 万条目
  → 需要驱逐：约 4000 条（< 0.01%）
  → 可直接继承：> 99.99%
```

利用此 diff 可做到：
1. **精确失效**：仅驱逐真正变更的条目，未变条目跨 epoch 直接复用
2. **直接预填充**：将 diff 中的新值直接写入缓存，切块后变更账户无需打 DB

---

## 2. 架构变更总览

```
Phase 2:  key = (epoch_id, address)   切块 → invalidate_all() → 全冷启动
Phase 3:  key = address               切块 → invalidate(diff) → 几乎全热
```

| 对比项 | Phase 2 | Phase 3 |
|--------|---------|---------|
| account key | `(epoch_id, Address)` | `Address` |
| storage key | `(epoch_id, Address, U256)` | `(Address, U256)` |
| bytecode key | `B256`（已无 epoch_id）| `B256`（不变）|
| 切块失效范围 | 100%（`invalidate_all`）| diff 变更集（< 0.01%）|
| 切块后命中率 | 0%（冷启动）| > 99%（未变条目延续）|
| 变更账户来源 | SafeUnchangedSet（不完整）| `execution_outcome`（完整权威）|
| ExEx 依赖 | SafeUnchangedSet 构建 | 不再依赖 SafeUnchangedSet |
| 首批 DB 穿透 | 全量 | ≤ diff 变更账户数（约数百条）|

---

## 3. 数据流变更

### 3.1 切块时序（Phase 3）

```
新块到达（CanonStateNotification::Commit { new }）
  ├─ 1. 记录 t0（net_engine_delay 打点）
  ├─ 2. on_epoch_change_diff(notification)
  │       ├─ 遍历 execution_outcome().bundle_accounts_iter()
  │       │     ├─ cache.accounts.invalidate(&address)
  │       │     └─ for each changed slot:
  │       │           cache.storage.invalidate(&(address, slot))
  │       └─ （Commit 通知，精确失效，非 Reorg）
  ├─ 3. pre_fill_diff(notification)
  │       └─ 遍历同一 bundle_accounts_iter()
  │             ├─ 若 account.info 有新值 → cache.accounts.insert(address, new_info)
  │             └─ for each changed slot:
  │                   cache.storage.insert((address, slot), new_value)
  ├─ 4. 广播新 epoch（active_tx.send(epoch)）
  └─ 5. 记录 t1（epoch_manager_delay 打点，含步骤 2+3 耗时）

Reorg 通知（CanonStateNotification::Reorg { .. }）
  └─ invalidate_all()（保守处理，reorg 小概率）
```

### 3.2 Worker 读路径（不变）

Phase 3 读路径无需修改，key 格式变更对 `CachedStateProvider` 透明：

```
EVM 读 account(addr)
  1. Worker-L1 命中 → 返回
  2. GlobalSharedCache.accounts.get(&addr) 命中 → 返回，回填 Worker-L1
  3. miss → MissCoordinator singleflight → MDBX → 回填缓存
```

---

## 4. 组件改动详情

### 4.1 GlobalSharedCache（`crates/mev/src/cache/mod.rs`）

#### 4.1.1 Cache Key 变更

```rust
// Phase 2（移除）
type AccountKey  = (u64, Address);        // (epoch_id, address)
type StorageKey  = (u64, Address, U256);  // (epoch_id, address, slot)  ← revm 用 U256 表示 slot

// Phase 3（新增）
type AccountKey  = Address;
type StorageKey  = (Address, U256);       // slot 类型保持 U256，与 revm Database trait 一致
// BytecodeKey = B256  （不变）
```

#### 4.1.2 新增 API

```rust
impl GlobalSharedCache {
    /// Phase 3 精确失效：仅驱逐 diff 中变更的条目。
    /// Commit 通知调用此方法；Reorg 仍调用 invalidate_all()。
    pub fn on_epoch_change_diff(&self, notification: &CanonStateNotification) {
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

    /// Phase 3 预填充：将 diff 新值直接写入缓存，切块后变更账户无需打 DB。
    /// 必须在 on_epoch_change_diff() 之后调用，确保先驱逐再填充。
    pub fn pre_fill_diff(&self, notification: &CanonStateNotification) {
        let CanonStateNotification::Commit { new } = notification else {
            return; // Reorg 不做预填充，已 invalidate_all
        };
        for (address, account) in new.execution_outcome().bundle_accounts_iter() {
            if let Some(info) = account.info.as_ref() {
                // account.status == Destroyed 时 info 为 None，不填充
                self.accounts.insert(address, info.clone().into());
            }
            for (slot, change) in account.storage.iter() {
                self.storage.insert((address, *slot), change.present_value);
            }
        }
    }
}
```

#### 4.1.3 Phase 2 兼容过渡

`on_epoch_change()` 保留为 Feature Flag 控制的旧路径，便于灰度切换：

```rust
/// Phase 2 全量失效（Feature Flag: mev-phase2-cache）
pub fn on_epoch_change(&self) {
    self.accounts.invalidate_all();
    self.storage.invalidate_all();
}
```

---

### 4.2 EpochManager（`crates/mev/src/epoch.rs`）

#### 4.2.1 接收完整 Notification

当前实现只从 notification 中取 `tip_checked()`，Phase 3 需传递完整 notification 至 GlobalSharedCache：

```rust
// Phase 2（当前）
let Some(tip) = notification.tip_checked() else { ... };
global_cache.on_epoch_change();  // 全量失效

// Phase 3（改动）
// 保留原有 tip 取用，额外传 notification 引用给缓存操作
let Some(tip) = notification.tip_checked() else { ... };
global_cache.on_epoch_change_diff(&notification);   // 精确失效
global_cache.pre_fill_diff(&notification);          // 预填充新值
```

#### 4.2.2 Warmup 指标

```rust
let warmup_start = std::time::Instant::now();

global_cache.on_epoch_change_diff(&notification);
global_cache.pre_fill_diff(&notification);

let warmup_secs = warmup_start.elapsed().as_secs_f64();
metrics::histogram!("mev_epoch_warmup_duration_seconds").record(warmup_secs);
metrics::gauge!("mev_epoch_warmup_duration_latest_seconds").set(warmup_secs);
```

`mev_epoch_manager_delay_us`（已有，从 canonical 通知到 epoch 广播的总耗时）自动包含 warmup 时间，无需额外修改。

---

### 4.3 CachedStateProvider（`crates/mev/src/cache/provider.rs`）

读路径的 key 构造需跟随 Phase 3 移除 `epoch_id`：

```rust
// Phase 2
fn basic_account(&mut self, addr: Address) -> Result<Option<AccountInfo>, Self::Error> {
    let key = (self.epoch_id, addr);  // 带 epoch_id
    ...
}

// Phase 3
fn basic_account(&mut self, addr: Address) -> Result<Option<AccountInfo>, Self::Error> {
    let key = addr;  // 直接用 addr
    ...
}
```

`self.epoch_id` 字段在 Phase 3 可移除（或保留用于 Worker-L1 分桶，Worker-L1 层的 epoch 隔离仍有意义）。

> **Worker-L1 的 epoch_id**：Worker-L1 是 worker 本地缓存，切块时清空旧 epoch 分桶的开销极小（内存释放），可保留 epoch_id 用于 Worker-L1 隔离，不影响 GlobalSharedCache 的 key 变更。

---

## 5. 正确性保证

### 5.1 为何可以移除 epoch_id

`execution_outcome()` 是 reth 执行引擎生成的权威结果，包含**该区块所有状态写操作**的完整集合。任何未出现在 `bundle_accounts_iter()` 中的地址，其状态在新 epoch 与旧 epoch 完全一致，缓存条目继续有效。

### 5.2 特殊情况处理

| 情况 | 处理方式 |
|------|----------|
| 账户被销毁（`Destroyed`）| `account.info == None`，不预填充；`invalidate` 已驱逐旧值；读时 miss → DB 返回 `None` → 负缓存 |
| Reorg | `invalidate_all()` + 不预填充，保守处理，正确性优先 |
| 预填充值与 DB 不一致 | 不可能：`execution_outcome` 是执行结果，与 DB committed state 同源 |
| pre_fill 期间 worker 并发读同一 key | 读到旧值（invalidate 后、fill 前的窗口）→ worker 会 miss → 从 DB 读到正确新值，无误 |

### 5.3 验证方案

上线前必须做**链上重放对照测试**：

```
对 100 个连续区块，每块执行后：
  1. 用 Phase 3 路径（diff 失效 + 预填充）模拟 1000 条路径
  2. 用原生 eth_call 模拟相同 1000 条路径
  3. 逐条比较结果（gas used、return data、revert reason）
  → 必须 100% 一致，零差异
```

---

## 6. 新增指标

| 指标名 | 类型 | 含义 |
|--------|------|------|
| `mev_epoch_warmup_duration_seconds` | Histogram | diff 失效 + pre-fill 耗时 |
| `mev_epoch_warmup_duration_latest_seconds` | Gauge | 最新一次 warmup 耗时 |
| `mev_epoch_diff_accounts_total` | Gauge | 每个 epoch diff 变更的账户数 |
| `mev_epoch_diff_storage_slots_total` | Gauge | 每个 epoch diff 变更的 storage slot 数 |

### 6.1 指标埋点位置（EpochManager）

```rust
let diff_accounts = new.execution_outcome()
    .bundle_accounts_iter()
    .count() as f64;
let diff_slots: f64 = new.execution_outcome()
    .bundle_accounts_iter()
    .map(|(_, a)| a.storage.len() as f64)
    .sum();

metrics::gauge!("mev_epoch_diff_accounts_total").set(diff_accounts);
metrics::gauge!("mev_epoch_diff_storage_slots_total").set(diff_slots);
```

### 6.2 mev_block_delay 组成（Phase 3）

```
mev_block_delay = block_delay + mev_epoch_manager_delay
                = (网络 + Engine API)
                + (构建 block_env + diff 失效 + diff 预填充)
                            ↑
                   Phase 2 此处仅有 invalidate_all()，约 1~5ms
                   Phase 3 此处增加 pre_fill，约 5~30ms
                   但切块后首批延迟大幅降低，总体 SLO 改善
```

---

## 7. 性能预期

| 指标 | Phase 2 | Phase 3 预期 |
|------|---------|--------------|
| 切块后 GlobalSharedCache 命中率（首批）| ~0%（冷启动）| > 99%（未变账户）|
| 切块后 DB 穿透量 | 全量（数百万次）| ≤ diff 变更条目数（约数千次）|
| `mev_epoch_warmup_duration_seconds` | N/A | 5~30ms（diff 大小决定）|
| 首批 `T_batch_core` P99 vs 稳态 P99 | 3~5× | < 1.2×（差距 < 20ms）|
| 切块后 MDBX 读速率峰值 | ~15K reads/s | < 500 reads/s |

---

## 8. 迁移路径

```
Step 1（开发）
  ├─ GlobalSharedCache：新增 on_epoch_change_diff() + pre_fill_diff()
  ├─ CachedStateProvider：key 构造移除 epoch_id（accounts/storage）
  └─ EpochManager：替换 on_epoch_change() → on_epoch_change_diff() + pre_fill_diff()

Step 2（测试）
  ├─ 单元测试：diff 失效正确性（变更账户被驱逐，未变账户保留）
  ├─ 链上重放对照测试：100 区块 × 1000 路径，零差异
  └─ 压测：对比 Phase 2 首批 / 稳态 P50/P95/P99

Step 3（灰度）
  ├─ Feature flag：mev-phase3-diff-cache（默认关闭）
  ├─ 开启后观察 mev_epoch_diff_accounts_total + mev_epoch_warmup_duration_seconds
  └─ 确认 GlobalSharedCache 命中率 > 95% 后关闭旧路径

Step 4（清理）
  ├─ 移除 on_epoch_change()（旧 Phase 2 全量失效）
  ├─ 移除 CachedStateProvider 中的 epoch_id 字段（若 Worker-L1 也不再需要）
  └─ 更新设计文档
```

---

## 9. 风险与应对

| 风险 | 概率 | 应对 |
|------|------|------|
| `execution_outcome` diff 不完整（遗漏变更账户）| 极低（reth 执行引擎权威结果）| 上线后抽样 DB 对照；开启完整重放测试 |
| pre_fill 与 DB commit 时序差：pre_fill 写入后 DB 尚未 flush | 不存在：canonical 通知在 DB commit 之后触发 | N/A |
| 大区块（高 gas）diff 条目过多，pre_fill 耗时过长 | 低（EIP-1559 gas limit 限制条目数上界）| 设置 `MAX_DIFF_ENTRIES` 阈值，超出时回退 invalidate_all() |
| Worker-L1 与 GlobalSharedCache key 不一致 | 中（需同步改动）| Worker-L1 可保留 epoch_id（按 epoch 清空），GlobalSharedCache 移除 epoch_id，两层独立管理 |
| Reorg 时 pre_fill 的旧值污染 | 无：Reorg 走 invalidate_all()，不执行 pre_fill | N/A |

---

## 10. Codex 实现 Prompt

> 将以下 prompt 完整粘贴给 Codex，配合本文档和当前代码库使用。

---

````

## 11. 开发实施说明（追加，不变更设计）

本节仅记录本轮 Phase 3 的实际落地结果与工程差异，不修改上文设计内容。

### 11.1 本次实际改动文件

- `crates/mev/src/cache/mod.rs`
- `crates/mev/src/provider.rs`
- `crates/mev/src/epoch.rs`
- `crates/mev/src/worker/worker.rs`（编译适配，见 11.4）

### 11.2 实施结果

1. `GlobalSharedCache` 已从 epoch 命名空间切换为无 epoch key：
   - `accounts: Cache<Address, Option<AccountInfo>>`
   - `storage: Cache<(Address, U256), U256>`
   - `bytecodes: Cache<B256, Bytecode>` 保持不变
2. `account_weigher` / `storage_weigher` 已按 Phase 3 口径调整：
   - account = `200`
   - storage = `192`
3. 已移除 `time_to_idle`（TTI）配置。
4. `get_or_load_account` / `get_or_load_storage` 已移除 `epoch_id` 参数。
5. 已新增并接入：
   - `on_epoch_change_diff(...)`
   - `pre_fill_diff(...)`
6. `on_epoch_change()`（Phase 2 全量失效）保留为回退路径。
7. `CachedStateProvider` 已移除 `epoch_id` 字段，并更新 account/storage 读取调用。
8. `EpochManager` 已替换切块缓存逻辑为：
   - `on_epoch_change_diff(&notification)`
   - `pre_fill_diff(&notification)`
   并补充指标：
   - `mev_epoch_warmup_duration_seconds`
   - `mev_epoch_warmup_duration_latest_seconds`
   - `mev_epoch_diff_accounts_total`
   - `mev_epoch_diff_storage_slots_total`

### 11.3 测试改动与结果

1. 已删除 `test_epoch_namespace_isolation`（Phase 3 语义不再适用）。
2. 已新增 `test_diff_invalidation`，覆盖：
   - 仅变更账户/slot 的精确失效
   - 变更项预填充新值
   - 未变更项保持缓存
3. 其余 Phase 2 测试已同步改为无 `epoch_id` 的缓存 API 调用。

执行结果：

- `cargo check -p reth-mev`：通过
- `cargo nextest run -p reth-mev`：通过（8/8）

### 11.4 与设计文档的小差异（工程实现层）

1. 额外改动了 `crates/mev/src/worker/worker.rs`：
   - 因 `CachedStateProvider` 移除 `epoch_id` 字段，构造体初始化需同步删除该字段赋值；
   - 为纯编译适配，不改变执行语义。
2. `on_epoch_change_diff` / `pre_fill_diff` 的泛型约束显式写为
   `N: reth_node_api::NodePrimitives`，以满足 `CanonStateNotification<N>` 的 trait bound。
3. `pre_fill_diff` 插入账户时使用 `Some(info.clone().into())`，用于类型收敛到缓存值类型。

### 11.5 约束检查

- 未新增依赖（`Cargo.toml` 无新增条目）。
- 未改动 `WorkerL1Cache` 设计。
- 未改动 `bytecodes` key 设计（仍为 `B256`）。
你是一名 Rust 专家，正在为 Reth（高性能以太坊执行客户端）的 MEV 模块实现 Phase 3：精确 Diff 缓存失效。

## 任务目标

将 `crates/mev/` 中的 `GlobalSharedCache` 从「epoch_id 命名空间 + 全量 invalidate_all」升级为「无 epoch_id + 精确 diff 失效 + diff 预填充」。

详细设计见：`doc/Reth_simulate_optimize_phase3.md`
总体架构见：`doc/mev-path-simulation-architecture-v3.md`

## 需要修改的文件

### 1. `crates/mev/src/cache/mod.rs`（GlobalSharedCache）

**变更说明**：

1. 移除 `accounts` 和 `storage` 缓存 key 中的 `epoch_id`：
   - `Cache<(u64, Address), Option<AccountInfo>>` → `Cache<Address, Option<AccountInfo>>`
   - `Cache<(u64, Address, U256), U256>` → `Cache<(Address, U256), U256>`
   - `bytecodes: Cache<B256, Bytecode>` 不变

2. 移除 `account_weigher` 和 `storage_weigher` 的 key 中的 `u64`，调整字节估算：
   - account weigher: key `Address=20` + value `Option<AccountInfo>≈80` + overhead `100` ≈ 200（不变）
   - storage weigher: key `(Address=20, U256=32)=52` + value `U256=32` + overhead `100` ≈ 184 → 用 192

3. 移除 `time_to_idle`（TTI）配置。Phase 3 通过精确失效管理条目生命周期，TTI 不再必要。

4. 更新 `get_or_load_account` / `get_or_load_storage` 签名，移除 `epoch_id` 参数：
   ```rust
   pub fn get_or_load_account<F>(&self, address: Address, load: F) -> Result<...>
   pub fn get_or_load_storage<F>(&self, address: Address, slot: U256, load: F) -> Result<...>
   ```

5. 新增两个方法（精确参考 `engine/tree/src/tree/cached_state.rs` 的 `insert_state` 实现，含 `was_destroyed()` 处理逻辑）：

   ```rust
   /// 精确失效：仅驱逐 diff 变更的条目。Commit 调用；Reorg 调用 on_epoch_change()。
   pub fn on_epoch_change_diff<N>(&self, notification: &CanonStateNotification<N>)
   where
       N: reth_node_types::NodePrimitives,
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

   /// 预填充：将 diff 新值写入缓存，切块后变更账户无需打 DB。
   /// 必须在 on_epoch_change_diff() 之后调用。
   pub fn pre_fill_diff<N>(&self, notification: &CanonStateNotification<N>)
   where
       N: reth_node_types::NodePrimitives,
   {
       let CanonStateNotification::Commit { new } = notification else { return };
       for (address, account) in new.execution_outcome().bundle_accounts_iter() {
           if account.status.was_destroyed() {
               // 销毁账户：info 已置 None，invalidate 已处理，不预填充
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
   ```

6. 保留 `on_epoch_change()` 方法（Phase 2 全量失效），作为回退路径：
   ```rust
   pub fn on_epoch_change(&self) {
       self.accounts.invalidate_all();
       self.storage.invalidate_all();
   }
   ```

7. 更新所有现有测试，将 `get_or_load_account(epoch_id, addr, ...)` 改为 `get_or_load_account(addr, ...)`，移除 `test_epoch_namespace_isolation` 测试（该语义在 Phase 3 不再适用，改为测试 diff 失效正确性）。

8. 新增单元测试 `test_diff_invalidation`：
   - 插入若干 account/storage 条目
   - 构造一个只改动部分账户的 mock `CanonStateNotification::Commit`
   - 调用 `on_epoch_change_diff` + `pre_fill_diff`
   - 验证：变更账户的旧值已不存在，新值已就位；未变账户的缓存条目仍然存在

---

### 2. `crates/mev/src/provider.rs`（CachedStateProvider）

**变更说明**：

1. 移除 `epoch_id: u64` 字段
2. 更新三个读方法，去掉 `epoch_id` 参数：
   ```rust
   // basic()
   let info = self.global.get_or_load_account(address, || { ... })?;

   // storage()
   let value = self.global.get_or_load_storage(address, index, || { ... })?;
   ```
3. `DatabaseRef` 实现不变

---

### 3. `crates/mev/src/epoch.rs`（EpochManager）

**变更说明**：

1. `EpochManager::spawn` 的循环体中，在现有的 `global_cache.on_epoch_change()` 调用处，替换为：

   ```rust
   let warmup_start = std::time::Instant::now();

   global_cache.on_epoch_change_diff(&notification);
   global_cache.pre_fill_diff(&notification);

   let warmup_secs = warmup_start.elapsed().as_secs_f64();
   metrics::histogram!("mev_epoch_warmup_duration_seconds").record(warmup_secs);
   metrics::gauge!("mev_epoch_warmup_duration_latest_seconds").set(warmup_secs);
   ```

2. 在同一循环体内，在 warmup 完成后追加 diff 规模指标：

   ```rust
   if let CanonStateNotification::Commit { ref new } = notification {
       let diff_accounts = new.execution_outcome().bundle_accounts_iter().count() as f64;
       let diff_slots: f64 = new
           .execution_outcome()
           .bundle_accounts_iter()
           .map(|(_, a)| a.storage.len() as f64)
           .sum();
       metrics::gauge!("mev_epoch_diff_accounts_total").set(diff_accounts);
       metrics::gauge!("mev_epoch_diff_storage_slots_total").set(diff_slots);
   }
   ```

3. `notification` 的类型是 `CanonStateNotification<N>`（从 `provider.subscribe_to_canonical_state()` 获取），`tip_checked()` 取 `&self`，不消耗 `notification`，可安全在 `on_epoch_change_diff` / `pre_fill_diff` 之后继续使用。

---

## 类型速查

| 类型 | 来源 crate |
|------|-----------|
| `CanonStateNotification<N>` | `reth_provider` |
| `BundleAccount` | `reth_revm::db` (re-exported from revm) |
| `BundleAccount.info: Option<AccountInfo>` | `revm::state` |
| `BundleAccount.storage: BTreeMap<U256, StorageSlot>` | `revm` |
| `StorageSlot.present_value: U256` | `revm` |
| `AccountStatus::was_destroyed()` | `revm_state` |
| `ExecutionOutcome::bundle_accounts_iter()` | `reth_evm_execution_types` |

---

## 关键约束

1. **不修改 Worker-L1 Cache**：`WorkerL1Cache` 中的 key 仍使用 `Address`（已无 epoch_id），无需变更。
2. **不修改 `bytecodes` 缓存**：key 仍为 `B256`，不变。
3. **保持 `moka::sync::Cache` API**：`insert(key, value)` 同步插入，`invalidate(&key)` 同步失效，`invalidate_all()` O(1) 异步失效。
4. **编译检查**：改动完成后执行 `cargo check -p reth-mev` 确保无编译错误，再执行 `cargo nextest run -p reth-mev` 确保测试通过。
5. **不引入新依赖**：所有新增代码仅使用已有依赖。

## 验收标准

- `cargo check -p reth-mev` 零错误零警告
- `cargo nextest run -p reth-mev` 全部通过（含新增的 `test_diff_invalidation`）
- `test_epoch_namespace_isolation` 替换为 `test_diff_invalidation`（diff 失效语义测试）
- Prometheus 指标中新出现 `mev_epoch_warmup_duration_seconds`、`mev_epoch_diff_accounts_total`、`mev_epoch_diff_storage_slots_total`
````
