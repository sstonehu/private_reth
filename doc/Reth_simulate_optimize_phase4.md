# MEV Phase 4 详细设计：过期请求快速拒绝 + trace drain 提升

> 版本：v2  
> 依赖：Phase 3（精确 Diff 缓存失效已上线）  
> 目标：切断过期请求导致的级联故障正反馈回路；trace 接口 gap=1 提升至 worker 执行，消除降级

---

## 1. 背景与动机

### 1.1 级联故障现象

通过生产监控（7:50–8:10 时段）发现，在流量激增时 `mev_eth_call` 降级率可达 100%。

**正反馈回路（Phase 3 遗留问题）**：

```
流量激增
  → MEV Bot 处理管道堵塞
  → Bot 发出大量 block_id=N-1 的请求（旧块）
  → 全部降级走原生 eth_call（走 DB，50~200ms）
  → Reth 负载上升，响应变慢
  → Bot 在途任务更多，管道更堵
  → 发出更多旧块请求
  （正反馈，难以自愈）
```

### 1.2 降级的两个根本问题

**问题一：降级结果对 Bot 意义有限**

`mev_eth_call` 用于价格检查/状态查询。Bot 管道堵塞时的降级结果：

- 在错误的状态（N-1 或更旧）上执行，价格信号失真
- 即使勉强得到结果，机会窗口已消失
- Bot 侧没有简单办法判断"这个结果是否还有参考价值"

**问题二：降级本身加剧负载**

原生 `eth_call` 每次需要走 StateProvider 读 DB，大量请求同时降级时：

- 每笔耗时 50~200ms，与 MEV worker 路径（< 5ms）差一个数量级
- 并发降级请求将 Reth DB 读路径打满
- 负载上升 → 响应更慢 → 更多在途请求 → 更多降级（正反馈）

### 1.3 根本解法

**快速拒绝**：对 `mev_eth_call` 的过期请求（block_id 落后于当前 epoch，gap ≥ 1），立即返回特定错误码，不执行任何 EVM 计算或 DB 读取。

Bot 收到错误后：

1. 识别为 EpochMismatch 错误
2. 排空当前 drain 任务（不等待无意义结果）
3. 在新 block 上重新发起请求

**效果**：正反馈转为负反馈。

```
流量激增
  → Bot 发出旧块 eth_call 请求
  → Reth 微秒级返回 -39001 错误（零 DB 读）
  → Bot 立刻收到错误，排空旧任务
  → 负载立即下降，回路断开
```

---

## 2. 设计决策

### 2.1 差异化处理：按接口 × gap 二维路由

Phase 4 对三个接口的 gap 处理规则如下：

| 接口 | gap = 0 | gap = 1（drain） | gap ≥ 2（stale） |
|------|---------|-----------------|-----------------|
| `mev_eth_call` | worker 执行 | **-39001 错误** | **-39001 错误** |
| `mev_debug_traceCall` | worker 执行 | **worker 执行（在 N 上）** | **-39001 错误** |
| `mev_trace_call` | worker 执行 | **worker 执行（在 N 上）** | **-39001 错误** |

**eth_call gap=1 → 错误** 的理由：
- 价格检查结果与状态强绑定，在 N 上执行 N-1 请求会产生错误的价格信号
- Bot 收到错误后可立即排空，切断正反馈回路

**trace gap=1 → worker** 的理由：
- trace 用于路径模拟，在最新状态（N）上执行结果仍有参考价值
- gap=1 是切块时的正常 drain，允许在 N 上执行比直接报错体验更好
- 不再需要降级走原生接口，完全在 worker 内完成，延迟更低

**所有接口 gap≥2 → 错误** 的理由：
- gap≥2 代表 Bot 管道出现异常积压，此时任何接口的旧块请求意义都很小
- 快速拒绝切断正反馈，优先于给出近似结果

> Bot 明确声明：不会通过 `mev_*` 接口查询历史数据。

### 2.2 Gap 语义

| gap | 含义 | eth_call | trace 接口 |
|-----|------|----------|-----------|
| 0 | 正常请求（当前 epoch） | worker 执行 | worker 执行 |
| 1 | drain（切块时正常在途请求） | **-39001 错误** | **worker 执行（在 N）** |
| ≥ 2 | stale（管道异常积压） | **-39001 错误** | **-39001 错误** |

### 2.3 错误设计

**错误码**：`-39001`（MEV 自定义，远离以太坊标准保留范围 `-32000` ~ `-32768`）

**错误体**：

```json
{
  "code": -39001,
  "message": "epoch mismatch: stale block_id",
  "data": {
    "requestedBlock": 21500000,
    "currentEpoch": 21500001,
    "gap": 1
  }
}
```

`data` 字段的价值：
- Bot 可直接用 `currentEpoch` 发起重试，无需额外查 `eth_blockNumber`
- 运维排查时可直接从日志中看 gap 分布，无需解析 result

### 2.4 新旧方案对比

| 维度 | Phase 3（降级） | Phase 4 |
|------|---------------|---------|
| eth_call 过期请求处理时间 | 50~200ms（走 DB） | < 1ms（快速拒绝，无 DB 读） |
| trace gap=1 请求处理时间 | 50~200ms（走 DB 降级） | 与正常 worker 相同（< 5ms） |
| trace gap≥2 请求处理时间 | 50~200ms（走 DB） | < 1ms（快速拒绝） |
| Reth 在异常场景下的额外负载 | 高（每请求读 DB） | 极低 |
| 语义清晰度 | 模糊（Bot 不知结果来自哪个状态） | 明确（error = 未执行；worker = 在 N 上执行）|
| 级联故障抵抗力 | 弱（降级加重负载，正反馈） | 强（错误切断回路，负反馈） |
| Bot 侧处理复杂度 | 低（结果直接用，但无意义） | 低（error → discard；trace gap=1 结果可直接用）|

---

## 3. 组件改动

### 3.1 `crates/mev/src/api/server.rs`（核心改动）

共修改三个方法的路由分支，**同时移除 `use reth_rpc_api::{DebugApiServer, TraceApiServer};` 导入**（降级路径删除后已无用）。

**`mev_eth_call`（gap ≥ 1 → 错误）**：

```rust
if !self.epoch_manager.matches_active(block_id) {
    let gap = self.epoch_manager.block_gap(block_id);
    if self.reject_stale_call {
        // Phase 4: 快速拒绝，不做任何 DB 读取
        metrics::record_epoch_mismatch(method::ETH_CALL, gap);
        metrics::record_e2e_latency(method::ETH_CALL, t0.elapsed());
        return Err(epoch_mismatch_error(block_id, self.epoch_manager.active_block_number(), gap));
    }
    // Phase 3 fallback（MEV_REJECT_STALE_CALL=0）
    // ...（保留原降级代码）
}
```

**`mev_debug_traceCall`（gap=1 → worker，gap≥2 → 错误）**：

```rust
if !self.epoch_manager.matches_active(block_id) {
    let gap = self.epoch_manager.block_gap(block_id);
    if gap != Some(1) {
        // gap >= 2 or non-number: fast rejection.
        metrics::record_epoch_mismatch(method::DEBUG_TRACE, gap);
        metrics::record_e2e_latency(method::DEBUG_TRACE, t0.elapsed());
        return Err(epoch_mismatch_error(block_id, self.epoch_manager.active_block_number(), gap));
    }
    // gap == Some(1): drain — fall through to worker path.
    // Execute on current epoch N; path simulation on latest state remains useful.
}
// 正常走 worker path...
```

**`mev_trace_call`（同 `mev_debug_traceCall`）**：

```rust
if !self.epoch_manager.matches_active(block_id) {
    let gap = self.epoch_manager.block_gap(block_id);
    if gap != Some(1) {
        metrics::record_epoch_mismatch(method::TRACE_CALL, gap);
        metrics::record_e2e_latency(method::TRACE_CALL, t0.elapsed());
        return Err(epoch_mismatch_error(block_id, self.epoch_manager.active_block_number(), gap));
    }
    // gap == Some(1): drain — fall through to worker path.
}
// 正常走 worker path...
```

### 3.2 新增辅助函数（`server.rs`）

```rust
/// 构造 -39001 EpochMismatch JSON-RPC 错误体。
fn epoch_mismatch_error(
    requested: Option<BlockId>,
    current_epoch: u64,
    gap: Option<u64>,
) -> jsonrpsee::types::error::ErrorObject<'static> {
    let requested_block = match requested {
        Some(BlockId::Number(alloy_rpc_types_eth::BlockNumberOrTag::Number(n))) => Some(n),
        _ => None,
    };
    let mut data = std::collections::BTreeMap::new();
    data.insert("requestedBlock", requested_block);
    data.insert("currentEpoch", Some(current_epoch));
    data.insert("gap", gap);
    jsonrpsee::types::error::ErrorObject::owned(-39001, "epoch mismatch: stale block_id", Some(data))
}
```

### 3.3 `crates/mev/src/metrics.rs`（新增指标函数）

新增 `record_epoch_mismatch()`，供三个接口的拒绝路径使用：

```rust
/// Phase 4: 记录因 epoch mismatch 被快速拒绝的次数。
/// - eth_call:       gap=1 和 gap≥2 均拒绝
/// - trace 接口:     仅 gap≥2 拒绝；gap=1 走 worker（不调用此函数）
#[inline]
pub fn record_epoch_mismatch(method: &'static str, gap: Option<u64>) {
    let reason = match gap {
        None => "non_number",
        Some(1) => "drain",
        Some(_) => "stale",
    };
    metrics::counter!("mev_epoch_mismatch_total", "method" => method, "reason" => reason)
        .increment(1);
}
```

> `mev_degraded_gap_total` 在 Phase 4 中将不再增长（无降级路径），保留供历史对比和回退观察。

### 3.4 `crates/mev/src/epoch.rs`（已实现，无需变更）

`block_gap()` 和 `active_block_number()` 已在之前实现，Phase 4 直接复用。

### 3.5 灰度开关（`server.rs` + `lib.rs`）

读取 `MEV_REJECT_STALE_CALL` 环境变量（影响三个接口）：

```rust
// MEV_REJECT_STALE_CALL=0 回退 Phase 3 降级行为；默认 1（Phase 4 快速拒绝）。
let reject_stale_call = std::env::var("MEV_REJECT_STALE_CALL")
    .map(|v| v != "0")
    .unwrap_or(true);
```

在 `mev_eth_call` 路由分支中：

```rust
if !self.epoch_manager.matches_active(block_id) {
    let gap = self.epoch_manager.block_gap(block_id);
    let current = self.epoch_manager.active_block_number();
    if self.reject_stale_call {
        // Phase 4: 快速拒绝
        metrics::record_epoch_mismatch("mev_eth_call", gap);
        return Err(epoch_mismatch_error(block_id, current, gap));
    } else {
        // Phase 3 回退: 降级走原生 eth_call
        metrics::record_degraded_gap("mev_eth_call", gap);
        return self.eth_api.call(request, block_id, state_overrides, block_overrides).await
            .map_err(Into::into);
    }
}
```

---

## 4. 指标与监控

### 4.1 新增指标

| 指标名 | 类型 | 含义 |
|--------|------|------|
| `mev_epoch_mismatch_total{method, reason}` | Counter | **全量 gap 事件计数**（无论结果是拒绝还是 promote 到 worker）。`reason` = `drain`（gap=1）/ `stale`（gap≥2）/ `non_number` |

`method` 标签隐含了请求的处理方式：

| method | reason=drain | reason=stale |
|--------|-------------|-------------|
| `eth_call` | 被快速拒绝（-39001） | 被快速拒绝（-39001） |
| `debug_traceCall` | 被 promote 到 worker | 被快速拒绝（-39001） |
| `trace_call` | 被 promote 到 worker | 被快速拒绝（-39001） |

### 4.2 指标变化说明

| 指标 | Phase 3 行为 | Phase 4 行为 |
|------|-------------|-------------|
| `mev_degraded_path_total` | 有值 | **永远为 0**（所有三个接口均无降级） |
| `mev_degraded_gap_total` | 有值 | **永远为 0** |
| `mev_epoch_mismatch_total` | 不存在 | **新增**，记录所有 gap 事件（拒绝 + promote）|

> `mev_degraded_*` 指标在 Phase 4 中不会增长，但保留以便：
> ① 在灰度期（`MEV_REJECT_STALE_CALL=0`）时仍可观察降级回退状态；
> ② 与 Phase 3 历史数据对比；
> ③ 回退时恢复监控。

### 4.3 告警建议

| 条件 | 动作 |
|------|------|
| `rate(mev_epoch_mismatch_total{reason="stale"}[1m]) > 5` | 告警：Bot 管道出现异常堆积（gap ≥ 2 超出预期） |
| `rate(mev_epoch_mismatch_total{reason="drain"}[1m])` 持续上升 | 观察：切块期 drain 量增加，Bot 管道开始积压 |
| `mev_degraded_path_total` 仍有增量 | 告警：`MEV_REJECT_STALE_CALL=0` 仍在生效，Phase 4 未启用 |

### 4.4 Grafana 面板

**用 "Gap 数量分布" 面板替换 Phase 3 的 "降级 Gap 分析" 面板**：

```promql
# 主面板：按 method + reason 分组，完整展示所有 gap 事件
sum(rate(mev_epoch_mismatch_total[1m])) by (method, reason)
```

```promql
# 简化面板：只看 reason，判断 drain/stale 比例
sum(rate(mev_epoch_mismatch_total[1m])) by (reason)
```

使用 stacked bars，颜色约定：
- `reason=drain`：黄色（观察）——eth_call 被拒；trace 被 promote 到 worker（均属正常）
- `reason=stale`：红色（告警）——所有接口 gap≥2，管道出现异常积压
- `reason=non_number`：灰色（罕见）

> **Phase 3 的 "降级 Gap 分析"（`mev_degraded_gap_total`）可从 Grafana 下线**，Phase 4 后该指标不再增长。

---

## 5. 迁移路径

```
Step 1（开发）
  ├─ server.rs：修改 mev_eth_call 降级分支 → 快速拒绝（含 MEV_REJECT_STALE_CALL 回退）
  ├─         ：修改 mev_debug_traceCall 降级分支 → gap=1 fall-through worker，gap≥2 拒绝
  ├─         ：修改 mev_trace_call 降级分支 → 同上
  ├─         ：新增 epoch_mismatch_error() 函数
  ├─         ：移除 use reth_rpc_api::{DebugApiServer, TraceApiServer}（降级路径删除后无用）
  ├─ metrics.rs：新增 record_epoch_mismatch()
  └─ epoch.rs：补充 active_block_number()（若未暴露）

Step 2（Bot 侧适配，与 Step 1 并行）
  ├─ Bot 能识别并正确处理 -39001 EpochMismatch 错误
  ├─ 收到错误后：排空当前 drain 任务，不重试旧 block_id
  ├─ trace 接口 gap=1 请求：结果在 N 上执行，直接使用（无需改动处理逻辑）
  └─ 可选：利用 data.currentEpoch 直接发起新请求

Step 3（灰度）
  ├─ 初始以 MEV_REJECT_STALE_CALL=0 部署（Phase 3 行为，观察指标变化）
  ├─ Bot 适配完成后切换为 MEV_REJECT_STALE_CALL=1
  ├─ 观察 mev_epoch_mismatch_total + Bot 侧成功率 + trace 接口 worker 路径延迟
  └─ 确认 mev_epoch_mismatch_total{reason="stale"} 维持低值后全量开启

Step 4（清理，稳定运行数天后）
  ├─ 移除 mev_eth_call 中的 Phase 3 回退代码（降级分支彻底删除）
  ├─ 移除 debug_api / trace_api 字段（若确认不再需要）
  ├─ mev_degraded_gap_total 可退出 Grafana 监控（不再增长）
  └─ 更新设计文档，标记 Phase 4 完成
```

---

## 6. 环境变量

| 变量名 | 默认值 | 作用 | 备注 |
|--------|--------|------|------|
| `MEV_REJECT_STALE_CALL` | `1`（启用）| **灰度开关**：`1`（或未设置）= Phase 4 启用，三个接口都按 Phase 4 stale 规则处理；`0` = 回退 Phase 3 降级行为（分别走原生 `eth_call` / `debug_traceCall` / `trace_call`） | Bot 侧适配完成前建议先设 `0` 灰度，确认后切 `1` |

**快速灰度配置**：

```ini
# /etc/systemd/system/reth.service.d/override.conf
[Service]
# Bot 适配前先保持 Phase 3 降级行为
Environment=MEV_REJECT_STALE_CALL=0
```

**正式启用**：

```ini
[Service]
Environment=MEV_REJECT_STALE_CALL=1
```

---

## 7. 风险与应对

| 风险 | 概率 | 应对 |
|------|------|------|
| Bot 侧未处理 `-39001`，将其当成执行失败或网络错误 | 中 | 灰度期先设 `MEV_REJECT_STALE_CALL=0`；确认 Bot 适配后再切 `1` |
| eth_call drain（gap=1）被拒绝后 Bot 重试逻辑死循环 | 低 | Bot 收到错误后应丢弃（不重试旧 block_id），协议规范见第 8 节 |
| trace gap=1 在 N 上执行，客户端用 N-1 的 block env 误解结果 | 低 | Bot 声明：不使用 mev_* 接口查询历史数据，gap=1 的 trace 结果可直接用于路径模拟 |
| 误拒绝正常请求（gap=0 被错判为 gap=1）| 极低 | `matches_active()` 逻辑不变，仅修改降级路径 |
| 大量 `-39001` 导致 Bot 侧日志/告警风暴 | 低 | Bot 应将 `reason=drain` 静默处理；仅 `reason=stale`（gap≥2）触发告警 |

---

## 8. Bot 侧协议规范

Bot 上线 Phase 4 前需满足以下条件：

1. **识别 `-39001` 错误码**：不将其视为合约执行失败、余额不足或网络错误
2. **eth_call drain 任务直接丢弃**：收到 `-39001` 后，对应的 drain 任务不重试（不带原 `block_id` 重试）
3. **trace gap=1 结果可直接使用**：trace 接口 gap=1 请求在最新状态（N）上执行，结果对路径模拟有效，Bot 无需特殊处理
4. **利用 `data.currentEpoch`**（可选）：可直接使用错误体中的 `currentEpoch` 发起新请求，节省一次 `eth_blockNumber` 查询
5. **不重试同 block_id**：对同一 `block_id` 重试 eth_call 只会再次收到 `-39001`
6. **`reason=drain` 静默处理**：gap=1 是切块时的正常现象；仅 `reason=stale`（gap≥2）才代表管道出现异常积压

---

## 9. 实施说明（追加，不变更设计）

> 本节待实施完成后填写，记录工程实际差异与测试结果。

---

## 10. Codex 实现 Prompt

> 将以下 prompt 完整粘贴给 Codex，配合本文档和当前代码库使用。

---

````

## 11. 实施结果（追加，不变更设计）

本节仅记录本轮 Phase 4 的实际开发结果、验证结果和工程实现差异，不修改前文设计。

### 11.1 本次实际改动文件

- `crates/mev/src/epoch.rs`
- `crates/mev/src/metrics.rs`
- `crates/mev/src/api/server.rs`
- `crates/mev/src/lib.rs`

### 11.2 功能落地结果

1. `MEV_REJECT_STALE_CALL` 已接入三个接口的 stale 分支：
   - `mev_eth_call`：开关开启时 `gap >= 1` 立即返回 `-39001`
   - `mev_debug_traceCall` / `mev_trace_call`：开关开启时 `gap = 1` 走 worker，`gap >= 2` 返回 `-39001`
2. 保留 Phase 3 回退路径：
   - `MEV_REJECT_STALE_CALL=0` 时，三个接口都回退到各自原生 RPC 降级逻辑
3. 新增并接入 `reject_stale_call` 配置：
   - 读取环境变量：`MEV_REJECT_STALE_CALL`
   - 注入 `MevApiServer` 字段并进入 `Debug` 输出
   - 启动日志增加 `reject_stale_call` 字段，文案更新为 Phase 4

### 11.3 代码级变更摘要

#### A. `epoch.rs`

- 在 `block_gap()` 之后新增：
  - `active_block_number() -> u64`

#### B. `metrics.rs`

- 更新 `MethodCounters.degraded` 字段注释，明确 Phase 4 后 `mev_eth_call` 不再计入 degraded。
- 新增：
  - `record_epoch_mismatch(method, gap)`
  - 指标：`mev_epoch_mismatch_total{method,reason}`
  - `reason` 分类：`non_number` / `drain` / `stale`

#### C. `api/server.rs`

- `MevApiServer` 新增字段：
  - `reject_stale_call: bool`
- `mev_eth_call` 的 stale 分支改为：
  - `reject_stale_call=true`：快拒绝 + 记录 `mev_epoch_mismatch_total`
  - `reject_stale_call=false`：保留原降级（`record_degraded_*` + native `eth_call`）
- 新增辅助函数：
  - `epoch_mismatch_error(requested, current_epoch, gap)`

#### D. `lib.rs`

- 新增环境变量读取：
  - `MEV_REJECT_STALE_CALL=0` 回退 Phase 3
  - 其余值（含未设置）开启 Phase 4 快拒绝
- 构造 `MevServer` 时传入 `reject_stale_call`
- 启动日志文案和字段同步更新

### 11.4 验证结果

- `cargo check -p reth-mev`：通过
- `cargo nextest run -p reth-mev`：通过（8 passed, 0 skipped）

### 11.5 与设计文档的工程实现差异说明

1. `epoch_mismatch_error()` 的返回类型实际使用
   `jsonrpsee::types::error::ErrorObject<'static>`，
   由 `RpcResult<T> = Result<T, ErrorObjectOwned>` 直接承接；语义与设计一致（仍返回 `-39001` 与 data 字段）。
2. 错误 `data` 构造未使用 `serde_json::json!`，而是使用可序列化映射结构构造后交给
   `ErrorObject::owned`；输出字段保持一致：
   - `requestedBlock`
   - `currentEpoch`
   - `gap`
你是一名 Rust 专家，正在为 Reth（高性能以太坊执行客户端）的 MEV 模块实现 Phase 4：
mev_eth_call 过期请求快速拒绝。

详细设计见：`doc/Reth_simulate_optimize_phase4.md`
总体架构见：`doc/mev-path-simulation-architecture-v3.md`

## 任务目标

将 `mev_eth_call` 的"降级走原生 eth_call"逻辑替换为"立即返回 -39001 EpochMismatch 错误"，
以切断大流量场景下的级联故障正反馈回路。`mev_debug_traceCall` 和 `mev_trace_call` 的逻辑
**完全不动**。

## 需要修改的文件（共 4 个）

---

### 1. `crates/mev/src/epoch.rs`

**在文件末尾 `block_gap()` 方法之后（当前文件约 456 行）添加一个新方法**：

```rust
/// 返回当前 active epoch 的块号，供 EpochMismatch 错误体使用。
pub fn active_block_number(&self) -> u64 {
    self.active_rx.borrow().block_number
}
```

无需修改任何现有逻辑。

---

### 2. `crates/mev/src/metrics.rs`

**在 `record_degraded_gap()` 函数之后（当前约第 133 行）新增一个函数**：

```rust
/// Phase 4: 记录 mev_eth_call 因 epoch mismatch 被快速拒绝的次数。
///
/// - `gap = None`  : block_id 非 explicit Number（理论上不会触发拒绝，保险兜底）。
/// - `gap = Some(1)`: drain——切块时正常在途请求，属预期现象。
/// - `gap = Some(n≥2)`: stale——Bot 管道出现积压，需告警。
#[inline]
pub fn record_epoch_mismatch(method: &'static str, gap: Option<u64>) {
    let reason = match gap {
        None => "non_number",
        Some(1) => "drain",   // normal: in-flight drain during epoch transition
        Some(_) => "stale",   // gap ≥ 2: pipeline backup, needs immediate alert
    };
    metrics::counter!("mev_epoch_mismatch_total", "method" => method, "reason" => reason)
        .increment(1);
}
```

同时，将 `MethodCounters` 结构体中 `degraded` 字段的注释更新为：

```rust
/// Requests degraded to native fallback (stale block_id).
/// Phase 4: only applies to mev_debug_traceCall / mev_trace_call;
/// mev_eth_call stale requests are now fast-rejected (see mev_epoch_mismatch_total).
pub degraded: AtomicU64,
```

---

### 3. `crates/mev/src/api/server.rs`

#### 3.1 在 `MevApiServer` 结构体中新增字段

在 `pub trace_api: reth_rpc::TraceApi<EthApi>,` 之后添加：

```rust
/// Phase 4: if true, mev_eth_call with stale block_id returns -39001 immediately
/// instead of degrading to the native eth_call path.
/// Controlled by MEV_REJECT_STALE_CALL env var (default: true).
pub reject_stale_call: bool,
```

#### 3.2 更新 `Debug` impl

在 `finish_non_exhaustive()` 之前添加（已有 `field("counters", ...)` 之后）：

```rust
.field("reject_stale_call", &self.reject_stale_call)
```

#### 3.3 替换 `mev_eth_call` 的降级逻辑

**找到以下现有代码块**（server.rs 约第 114–123 行）：

```rust
if !self.epoch_manager.matches_active(block_id) {
    metrics::record_degraded_path(method::ETH_CALL, c);
    metrics::record_degraded_gap(method::ETH_CALL, self.epoch_manager.block_gap(block_id));
    let overrides =
        alloy_rpc_types_eth::state::EvmOverrides::new(state_overrides, block_overrides);
    let result =
        self.eth_api.call(request, block_id, overrides).await.map_err(Into::into);
    metrics::record_e2e_latency(method::ETH_CALL, t0.elapsed());
    return result;
}
```

**替换为**：

```rust
if !self.epoch_manager.matches_active(block_id) {
    let gap = self.epoch_manager.block_gap(block_id);
    if self.reject_stale_call {
        // Phase 4: fast rejection — no DB read, no EVM execution.
        metrics::record_epoch_mismatch(method::ETH_CALL, gap);
        metrics::record_e2e_latency(method::ETH_CALL, t0.elapsed());
        return Err(epoch_mismatch_error(block_id, self.epoch_manager.active_block_number(), gap));
    }
    // Phase 3 fallback (MEV_REJECT_STALE_CALL=0): degrade to native eth_call.
    metrics::record_degraded_path(method::ETH_CALL, c);
    metrics::record_degraded_gap(method::ETH_CALL, gap);
    let overrides =
        alloy_rpc_types_eth::state::EvmOverrides::new(state_overrides, block_overrides);
    let result =
        self.eth_api.call(request, block_id, overrides).await.map_err(Into::into);
    metrics::record_e2e_latency(method::ETH_CALL, t0.elapsed());
    return result;
}
```

#### 3.4 在文件末尾（`}` 之前）添加辅助函数

在 `server.rs` 最后一个 `}` 之前（impl 块外部）添加：

```rust
/// 构造 -39001 EpochMismatch JSON-RPC 错误，供 Phase 4 快速拒绝使用。
fn epoch_mismatch_error(
    requested: Option<BlockId>,
    current_epoch: u64,
    gap: Option<u64>,
) -> jsonrpsee::core::Error {
    let requested_block = match requested {
        Some(BlockId::Number(alloy_rpc_types_eth::BlockNumberOrTag::Number(n))) => Some(n),
        _ => None,
    };
    jsonrpsee::core::Error::Call(jsonrpsee::types::ErrorObject::owned(
        -39001,
        "epoch mismatch: stale block_id",
        Some(serde_json::json!({
            "requestedBlock": requested_block,
            "currentEpoch": current_epoch,
            "gap": gap,
        })),
    ))
}
```

---

### 4. `crates/mev/src/lib.rs`

#### 4.1 读取环境变量

在 `stats_interval_secs` 读取之后、`MevServer { ... }` 构造之前，添加：

```rust
// MEV_REJECT_STALE_CALL=0 falls back to Phase 3 degradation for all mev_* calls.
// All other values (including unset) enable Phase 4 stale handling rules.
let reject_stale_call =
    std::env::var("MEV_REJECT_STALE_CALL").map(|v| v != "0").unwrap_or(true);
```

#### 4.2 传入构造函数

在 `MevServer { ... }` 构造块中追加字段：

```rust
let mev_module = MevServer {
    epoch_manager,
    worker_pool,
    call_config,
    counters,
    eth_api,
    debug_api,
    trace_api,
    reject_stale_call,   // ← 新增
}
.into_rpc();
```

#### 4.3 更新启动日志

将现有 `tracing::info!` 块中的 `"mev RPC module installed (Phase 2: GlobalSharedCache enabled)"` 替换为
`"mev RPC module installed (Phase 4: eth_call fast rejection enabled)"` ，
并在字段列表中追加 `reject_stale_call`：

```rust
tracing::info!(
    target: "reth::mev",
    num_workers,
    cache_max_mb,
    call_gas_cap = call_config.call_gas_cap,
    stats_interval_secs,
    reject_stale_call,
    "mev RPC module installed (Phase 4: eth_call fast rejection enabled)"
);
```

---

## 类型速查

| 类型 / 函数 | 来源 |
|-------------|------|
| `BlockId` | `alloy_rpc_types_eth` |
| `BlockNumberOrTag` | `alloy_rpc_types_eth` |
| `jsonrpsee::core::Error` | `jsonrpsee::core` |
| `jsonrpsee::types::ErrorObject` | `jsonrpsee::types` |
| `serde_json::json!` | `serde_json`（已在依赖中） |

---

## 关键约束

1. **不修改 `mev_debug_traceCall` 和 `mev_trace_call`**：这两个方法的降级逻辑完全保持原样。
2. **`reject_stale_call=false` 时行为与 Phase 3 完全一致**：回退路径代码逻辑不变，只是保留原有代码。
3. **`mev_eth_call` 正常路径（worker pool）完全不动**：只改动 `!matches_active` 分支内的代码。
4. **不引入新依赖**：`serde_json` 已存在于 `Cargo.toml`；`jsonrpsee::types` 已在 `server.rs` 的依赖范围内。

## 验收标准

- `cargo check -p reth-mev`：零错误零警告
- `cargo nextest run -p reth-mev`：全部通过
- `MEV_REJECT_STALE_CALL=1`（默认）时：
  - `mev_eth_call` 的过期请求立即返回 `-39001`
  - `mev_debug_traceCall` / `mev_trace_call` 的 `gap=1` 请求继续走 worker，`gap>=2` 返回 `-39001`
- `MEV_REJECT_STALE_CALL=0` 时：三个接口行为都与 Phase 3 完全一致（分别降级走原生 `eth_call` / `debug_traceCall` / `trace_call`）
- 启动日志中出现 `reject_stale_call=true/false`
````
