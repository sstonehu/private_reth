# MEV Phase 6 详细设计：Worker / IPC 卡点分解指标

> 版本：v1  
> 依赖：Phase 5（`withAccessList` 已上线）  
> 目标：在不先扩大 worker 数的前提下，用低开销 Prometheus 指标定位 `mev_*` 请求 wall-clock 延迟到底卡在 IPC/RPC、worker queue、worker 执行、cache/DB/provider，还是结果返回路径

---

## 1. 背景与问题

### 1.1 现象

生产观测中出现以下组合：

- `eth_call` 请求速率从约 `800 req/s` 上升到 `3000+ req/s` 后，Reth 侧 `eth_call p99` 从约 `70ms` 上升到 `280ms+`。
- 机器 CPU 使用率仍较低，Reth 进程实际仅使用少数几个核。
- `MEV_WORKER_COUNT=60`，并且 systemd `CPUAffinity=1-31,65-95` 已生效，worker 数量与可用物理/逻辑核心数量接近。
- MDBX DB reads 没有与延迟成比例上涨，多级缓存大体有效。
- perf/off-CPU 采样显示 `mev-worker-*` 大量时间在 `futex_wait_queue`，但当前 release binary 被 strip，无法将等待栈精确映射回 Rust 源码。

因此不能简单把问题归因为 CPU 不足，也不应直接扩大 worker。Phase 6 的目标是把 Reth `mev_*` 请求的 wall time 拆成可观测阶段，明确 worker 被什么卡住。

### 1.2 需要验证的假设

| 假设 | 典型表现 | 需要新增的证据 |
|------|----------|----------------|
| H1：IPC/RPC ingress 过载 | IPC request p99 高，API in-flight 高，但 worker active 不满 | API in-flight、API prepare/dispatch/await 分段 |
| H2：worker queue 排队 | queue wait 高，worker active 接近 worker 数 | per-task queue wait histogram |
| H3：worker 内部执行慢 | worker execute/transact 高 | worker execute/transact 分段 |
| H4：cache / singleflight / DB 等待 | provider/cache get_or_load 高，DB read 不一定高 | provider op 分 source 耗时 |
| H5：结果返回路径阻塞 | worker result send 或 API return 高 | result_send / api_return 分段 |

---

## 2. 设计原则

1. **只观测，不改变行为**：Phase 6 不修改 worker 调度策略、不扩大 worker、不改变 cache 语义。
2. **优先 wall-clock 分解**：目标不是替代 perf，而是补齐长期可与 Grafana 流量对齐的阶段耗时。
3. **热路径低开销**：每个请求只记录有限数量的 `Instant::now()` 和 histogram；避免每次 state access 都访问 Prometheus registry。
4. **标签低基数**：允许 `method`、`kind`、`op`、`source` 等固定集合标签；禁止 `worker_id` 用于 histogram，避免 60 倍序列膨胀。
5. **可直接诊断 IPC 请求过多**：指标必须能区分“请求卡在 Reth IPC/API 层”与“请求已经进入 worker 但 worker 内部卡住”。

---

## 3. 请求路径分解

```
Go pricer / sender
    │ IPC JSON-RPC
    ▼
Reth RPC server
    │ reth_rpc_server_connections_request_time_seconds{transport="ipc"}
    ▼
mev API handler
    │ api_prepare_seconds
    │ api_dispatch_seconds
    │ api_worker_await_seconds
    │ api_return_seconds
    ▼
MevWorkerPool queue
    │ worker_queue_wait_seconds
    ▼
MevWorker thread
    │ worker_switch_epoch_seconds
    │ worker_build_db_seconds
    │ worker_apply_overrides_seconds
    │ worker_nonce_basic_seconds
    │ worker_transact_seconds
    │ worker_trace_build_seconds
    │ worker_stats_flush_seconds
    │ worker_result_send_seconds
    ▼
CachedStateProvider / GlobalSharedCache / MDBX
    │ provider_basic_seconds{source}
    │ provider_storage_seconds{source}
    │ provider_code_seconds{source}
    ▼
oneshot result → API response → IPC response
```

---

## 4. 指标设计

### 4.1 API / IPC ingress 层

**位置**：`crates/mev/src/api/server.rs`

| 指标 | 类型 | 标签 | 含义 |
|------|------|------|------|
| `mev_api_inflight` | Gauge | `method` | 当前 `mev_*` API handler 在途请求数 |
| `mev_api_prepare_seconds` | Histogram | `method` | handler 开始到 `WorkerTask` 构造完成 |
| `mev_api_dispatch_seconds` | Histogram | `method` | 调用 `MevWorkerPool::dispatch()` 的耗时 |
| `mev_api_worker_await_seconds` | Histogram | `method` | dispatch 成功后等待 worker oneshot result 的耗时 |
| `mev_api_return_seconds` | Histogram | `method` | 收到 worker result 到 handler 返回前的耗时 |

**诊断读法**：

- `api_inflight` 高、`worker_active` 不满、`worker_queue_wait` 低：卡在 IPC/RPC/API 层。
- `api_worker_await` 高且 `worker_queue_wait` 高：卡在 worker queue。
- `api_worker_await` 高且 `worker_execute/transact` 高：卡在 worker 执行或 provider/cache。
- `api_return` 高：结果序列化/响应路径可能阻塞。

### 4.2 Worker queue 层

**位置**：`crates/mev/src/worker/mod.rs`、`crates/mev/src/worker/worker.rs`

`WorkerTask` 新增低开销元数据：

```rust
pub struct WorkerTask {
    // existing fields ...
    pub enqueued_at: std::time::Instant,
    pub method: &'static str,
}
```

| 指标 | 类型 | 标签 | 含义 |
|------|------|------|------|
| `mev_worker_recv_seconds` | Histogram | `backlog` | worker `recv()` 耗时；`backlog=yes` 表示 `recv` 前队列已有积压，用于判断队列消费/调度是否卡住 |
| `mev_worker_queue_wait_seconds` | Histogram | `method`, `kind` | task 入队到 worker `recv()` 后开始处理的等待时间 |
| `mev_pool_queue_depth` | Gauge | 无 | 现有队列深度，保留 |
| `mev_pool_queue_full_total` | Counter | 无 | 现有队列满计数，保留 |

`kind` 固定为：

- `basic`
- `debug_trace`
- `trace_call`

**注意**：现有 `mev_pool_queue_depth` 会在 dispatch 和 worker recv 后刷新，`mev_worker_queue_wait_seconds` 是 per-task 排队直接证据。若 `mev_worker_recv_seconds{backlog="yes"}` 在队列有积压时也明显升高，才更支持 worker 取队列 / 调度 / channel 侧存在瓶颈。

### 4.3 Worker 执行总览

**位置**：`crates/mev/src/worker/worker.rs`

| 指标 | 类型 | 标签 | 含义 |
|------|------|------|------|
| `mev_worker_active` | Gauge | `method`, `kind` | 当前正在处理 task 的 worker 数 |
| `mev_worker_handle_seconds` | Histogram | `method`, `kind` | worker 从拿到 task 到 result send 完成的总耗时 |
| `mev_worker_switch_epoch_seconds` | Histogram | `method`, `kind` | `switch_epoch()` 耗时 |
| `mev_worker_execute_seconds` | Histogram | `method`, `kind` | `execute_task()` 总耗时 |
| `mev_worker_result_send_seconds` | Histogram | `method`, `kind` | `task.result_tx.send(result)` 耗时 |

**诊断读法**：

- `worker_active ~= MEV_WORKER_COUNT` 且 `queue_wait` 高：worker 槽位被占满。
- `worker_active` 不满但 `api_inflight` 高：IPC/RPC/API ingress 卡住，任务没有稳定进入 worker。
- `result_send` 高：worker 返回到 API/Tokio 消费路径异常。

### 4.4 `execute_task()` 内部分段

**位置**：`crates/mev/src/worker/worker.rs`

| 指标 | 类型 | 标签 | 含义 |
|------|------|------|------|
| `mev_worker_build_db_seconds` | Histogram | `method`, `kind` | 构造 `CachedStateProvider` / `State` 的耗时 |
| `mev_worker_apply_overrides_seconds` | Histogram | `method`, `kind` | block/state overrides 应用耗时 |
| `mev_worker_nonce_basic_seconds` | Histogram | `method`, `kind` | `db.basic(tx_env.caller)` 读取 nonce 的耗时 |
| `mev_worker_transact_seconds` | Histogram | `method`, `kind` | `evm.transact()` 耗时 |
| `mev_worker_trace_build_seconds` | Histogram | `method`, `kind` | debug / parity trace result 构建耗时；basic 为 0 或不记录 |
| `mev_worker_stats_flush_seconds` | Histogram | `method`, `kind` | provider stats flush 到 Prometheus 的耗时 |

**重点**：

- `nonce_basic_seconds` 是当前 `transact()` 前的单独 provider 访问。如果它高，说明即使真正 EVM 执行前也已卡在 provider/cache/DB。
- `transact_seconds` 高但 CPU 低时，需要继续看 provider/cache 分段，而不是认为 EVM CPU-bound。
- `stats_flush_seconds` 应接近 0；若升高，说明 Prometheus registry / metrics 写入自身有压力。

### 4.5 Provider / Cache / DB 层

**位置**：`crates/mev/src/provider.rs`、必要时 `crates/mev/src/cache/mod.rs`

第一阶段只在 `CachedStateProvider` 的三个 `Database` 方法中做低成本分段：

| 指标 | 类型 | 标签 | 含义 |
|------|------|------|------|
| `mev_provider_op_seconds` | Histogram | `op`, `source` | provider 单次访问耗时 |
| `mev_provider_access_total` | Counter | `op`, `source` | provider 访问次数 |

`op` 固定为：

- `basic`
- `storage`
- `code_by_hash`

`source` 固定为：

- `l1_hit`
- `global_or_singleflight`
- `db_read`

实现口径：

- L1 命中时记录 `source="l1_hit"`。
- L1 miss 后调用 `global.get_or_load_*`，总耗时记录为 `source="global_or_singleflight"`。
- 若闭包实际执行 DB 读取，另记录 `source="db_read"` 的 loader 耗时。

如果 Phase 6 第一轮发现 `global_or_singleflight` 高而 `db_read` 不高，再进入第二轮细拆 `GlobalSharedCache` / moka / singleflight。

---

## 5. Histogram bucket 建议

Reth 当前 metrics crate 导出 summary/histogram 的具体 bucket 由 recorder 决定。若可以配置 bucket，建议使用以下边界：

```text
micro stages:
  [0.000001, 0.000005, 0.00001, 0.00005, 0.0001, 0.0005, 0.001, 0.005, 0.01]

request stages:
  [0.0001, 0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0]
```

若沿用 `metrics::histogram!` 当前 summary 导出，也接受 `quantile=0.5/0.95/0.99/0.999` 的现有格式。

---

## 6. 代码改动范围

### 6.1 `crates/mev/src/worker/mod.rs`

- `WorkerTask` 增加 `enqueued_at`、`method`。
- `dispatch()` 保留现有 queue depth / full 指标。
- 若调用方没有传 `enqueued_at`，在 API 构造任务时统一写入 `Instant::now()`。

### 6.2 `crates/mev/src/api/server.rs`

- 每个 `mev_*` handler 加 `api_inflight` guard。
- 将 handler 内流程按 prepare / dispatch / worker await / return 分段。
- 构造 `WorkerTask` 时写入 `method` 和 `enqueued_at`。

### 6.3 `crates/mev/src/worker/worker.rs`

- worker `recv()` 后立即记录 `queue_wait`。
- `handle_task()` 外层记录 active / handle total。
- `switch_epoch()` 记录耗时。
- `execute_task()` 内部记录 build_db / apply_overrides / nonce_basic / transact / trace_build / stats_flush。
- `result_tx.send()` 记录 result send 耗时。

### 6.4 `crates/mev/src/provider.rs`

- 在 `basic()` / `storage()` / `code_by_hash()` 中记录 `provider_op_seconds` 和 `provider_access_total`。
- 继续保留现有 `ProviderStats` 按 task flush 的命中/DB read counter。
- 避免每次 provider access 都做多次 Prometheus registry lookup；若实测 registry 开销明显，可改为 task-local duration accumulator，随 `ProviderStats::flush()` 一次性上报。

### 6.5 `crates/mev/src/metrics.rs`

- 新增统一 helper，避免各文件散落 metric 名称：

```rust
pub fn record_duration(name: &'static str, labels: &[(&'static str, &'static str)], elapsed: Duration);
pub fn inc_gauge(name: &'static str, labels: &[(&'static str, &'static str)]);
pub fn dec_gauge(name: &'static str, labels: &[(&'static str, &'static str)]);
```

如果 metrics crate 不便支持动态 metric name helper，则保留显式 `metrics::histogram!` / `metrics::gauge!` 调用，但指标名必须集中列在 `metrics.rs` 常量区。

---

## 7. Grafana 面板设计

新增 Row：**MEV Worker / IPC Breakdown（Phase 6）**

| 面板 | PromQL / 指标 | 目的 |
|------|---------------|------|
| API in-flight | `mev_api_inflight{method="eth_call"}` | 判断 IPC/API 层是否堆积 |
| API 分段 p99 | `mev_api_prepare/dispatch/worker_await/return_seconds{quantile="0.99"}` | 定位 handler 内部卡点 |
| Worker active | `sum(mev_worker_active) by (method, kind)` | 判断 60 worker 是否占满 |
| Worker recv p99 | `mev_worker_recv_seconds{backlog="yes",quantile="0.99"}` | 队列已有积压时，判断 worker 从队列取任务是否卡住 |
| Worker queue wait p99 | `mev_worker_queue_wait_seconds{quantile="0.99"}` | 直接证明 worker queue 排队 |
| Worker exec 分段 p99 | `mev_worker_*_seconds{quantile="0.99"}` | 定位 switch/nonce/transact/flush/result |
| Provider source p99 | `mev_provider_op_seconds{quantile="0.99"}` by `op,source` | 区分 L1/global/DB |
| IPC request p99 对照 | `reth_rpc_server_connections_request_time_seconds{transport="ipc",quantile="0.99"}` | 与 Reth 原生 RPC 指标对齐 |

---

## 8. 诊断矩阵

| 现象 | 结论 | 下一步 |
|------|------|--------|
| `api_inflight` 高，`worker_active` 不满，`queue_wait` 低 | IPC/RPC/API 层阻塞 | 看 tokio / RPC server / IPC 连接请求速率 |
| `api_worker_await` 高，`queue_wait` 高，`worker_active ~= 60` | worker queue 排队 | 优先削峰或评估 worker 周转，不直接认定 CPU 不足 |
| `queue_wait` 低，`execute/transact` 高 | worker 执行慢 | 看 provider/cache/DB 与 CPU profile |
| `transact` 高，CPU 高 | EVM CPU-bound | 优化 EVM 执行或减少请求 |
| `transact` 高，CPU 低，`provider_op` 高 | provider/cache/DB wait | 继续拆 moka/singleflight/MDBX |
| `global_or_singleflight` 高，`db_read` 低 | cache/singleflight/锁竞争 | 进入 GlobalSharedCache 细拆 |
| `db_read` 高 | cache miss / MDBX 读路径 | 查 epoch warmup、diff prefill、DB read table |
| `result_send` 或 `api_return` 高 | worker 返回或响应路径卡住 | 查 oneshot 消费、JSON 序列化、IPC write |

---

## 9. 验收标准

1. 在高流量窗口内，可以用 Grafana 明确回答：
   - 请求是否卡在 IPC/API 层；
   - 是否卡在 worker queue；
   - 是否卡在 worker execute；
   - 是否卡在 provider/cache/DB；
   - 是否卡在 result return。
2. `mev_api_worker_await_seconds.p99` 可以被近似分解为：
   ```
   worker_queue_wait + worker_handle
   ```
3. `worker_handle` 可以被进一步解释为：
   ```
   switch_epoch + execute_task + result_send
   ```
4. `execute_task` 的主要耗时可以被 `nonce_basic / transact / trace_build / stats_flush / provider_op` 解释。
5. 开启 Phase 6 指标后，正常流量下 CPU 和 p99 延迟没有可观测恶化。

---

## 10. 实施顺序

1. **P0：WorkerTask 元数据 + API/worker 总分段**
   - `api_inflight`
   - `api_worker_await`
   - `worker_queue_wait`
   - `worker_active`
   - `worker_handle`
   - `worker_execute`

2. **P1：execute_task 内部分段**
   - `switch_epoch`
   - `build_db`
   - `apply_overrides`
   - `nonce_basic`
   - `transact`
   - `trace_build`
   - `stats_flush`
   - `result_send`

3. **P2：provider/cache 分 source**
   - `provider_op_seconds{op,source}`
   - `provider_access_total{op,source}`

4. **P3：Grafana 面板与告警**
   - worker queue wait p99
   - api in-flight
   - provider db_read / global_or_singleflight p99

---

## 11. 回滚与风险

| 风险 | 应对 |
|------|------|
| 指标过多导致 registry 开销 | P0/P1 先上线；provider access 若有压力，改 task-local accumulator |
| histogram 序列过多 | 不加 `worker_id` 标签；`kind/source/op` 使用固定低基数 |
| `Instant` 增加轻微开销 | 仅在阶段边界记录，不在每 opcode 内记录 |
| 指标解释与 Reth 原生 RPC 指标不一致 | Grafana 同屏展示 `reth_rpc_server_connections_request_time_seconds{transport="ipc"}` 作对照 |
| stripped binary 仍难 profile | Phase 6 指标用于长期定位；需要源码级 perf 时另行部署带符号二进制 |

---

## 12. 与自定义批量 IPC 的关系

Phase 6 不实现 `mev_callBatch` / `mev_callBundleBatch`，但为其提供决策依据：

- 如果卡点在 IPC/API ingress，而 worker queue / execute 正常，则优先规划批量 IPC 接口。
- 如果卡点在 worker queue / execute，则批量 IPC 只能降低入口开销，不能解决 worker 周转瓶颈。
- 如果卡点在 provider/cache，则应先优化 cache/singleflight/DB，再考虑接口批量化。

