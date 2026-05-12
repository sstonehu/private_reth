# Pricer No-Change Pool Shadow Validate 设计

> 范围：`go-service/core/pricer/pricer.go`  
> 目的：验证 `this block no change pool` 优化的正确性，而不是替代其默认性能路径  
> 关联：`private_reth` 的 `mev_*` replay / debug 环境

---

## 1. 背景

当前 pricer 在处理 `cache hit old` 时，存在一条快速路径：

- 若某 `poolKey + unitAmountKey` 的缓存价格来自 `currentBlockNumber - 1`
- 且通过 `callParam.RawIds` 与 `BlockImpactIndex.changedRawIds` 比对，判定该 pool 在当前块 `!isChanged`
- 则直接将上一块价格继承到当前块，并刷新缓存 blockNumber

对应代码位置：

- `go-service/core/pricer/pricer.go`
- 注释：`// 3.1 this block no change pool`

该优化能减少当前块的重复询价，但存在一个正确性风险：

- `!isChanged(rawIds)` 只能证明“命中的 rawId 没有被 BlockImpactIndex 标记为变化”
- 不能自动证明“该 pool 在当前块的报价一定与前一块完全一致”

因此需要一套可落地的校验方案，用于回答：

- 当前 `3.1` 优化是否安全
- 哪些 pool 会出现“`!isChanged` 但真实报价变化”的误判
- mismatch 发生时，应该如何处理缓存与观测

---

## 2. 现状问题

当前 `3.1` 分支在命中 `!isChanged` 后，行为是：

1. 直接 `RefreshPoolBlockNumber(poolKey, currentBlockNumber-1, currentBlockNumber)`
2. 直接将旧 `UnitAmountDetail` 写入本地 `stepPriceMap`
3. 不再发起真实询价

这带来两个问题：

### 2.1 错误会先污染缓存

如果 `!isChanged` 的判断条件不充分，缓存会先被“提升”到当前块。

后果：

- replay / debug 时看到的是已刷新后的缓存
- 难以追溯“真实报价到底有没有变化”
- 若同一 pool 的其他 unitAmount 也复用该缓存块号，错误影响面会扩大

### 2.2 无法验证核心假设

当前优化背后的核心假设是：

> `!isChanged(rawIds)` => 当前块真实询价结果与前一块缓存价格完全一致

现有实现没有验证这个假设，只是直接信任它。

---

## 3. 目标

本设计的目标不是默认替换掉 `3.1` 的性能优化，而是新增一个 **Shadow Validate 模式**：

- 当命中 `!isChanged` 时，不直接刷新缓存
- 仍然继续真实询价
- 询价完成后，对比“真实结果”和“旧缓存结果”
- 若 mismatch，则记录错误并保留 fresh 结果
- 若 match，则再安全地将缓存 blockNumber 刷新到当前块

这样可以在不改变 replay / debug 基准语义的前提下，验证优化是否可靠。

---

## 4. 设计原则

### 4.1 默认路径仍保性能

默认生产主线保留当前 `3.1` 快速继承逻辑，不因 correctness 校验而增加额外 RPC。

### 4.2 校验模式只做 correctness 验证

Shadow Validate 模式的定位是：

- replay
- 离线回放
- 灰度抽样
- 故障排查

而不是长期全量开启的默认生产模式。

### 4.3 mismatch 不得写坏缓存

一旦 fresh quote 与旧缓存不一致：

- 不得 refresh 旧缓存 blockNumber
- 应以 fresh 结果为准继续后续流程
- 必须输出可回放、可检索的错误记录

### 4.4 对比必须严于线上命中条件

当前 `7.1` 的逻辑主要以首个 `testPercent` 的 `StepPrice` 作为“是否相同”的关键依据。

这对性能回写足够，但对 correctness 校验不够。

Shadow Validate 应采用更严格的对比规则：

- 比较整个 `UnitAmountDetail`
- 至少覆盖：
  - `MaxTestPercent`
  - `StepDetails` 的所有 `testAmountKey`
  - 每个 `StepDetail` 中的：
    - `StepPrice`
    - `AmountOut`
    - `StepCost`

---

## 5. 总体方案

### 5.1 模式定义

新增一个仅用于校验的运行模式，建议命名为：

- `PRICER_VALIDATE_NO_CHANGE_POOL=1`

语义：

- `0` 或未设置：保持当前行为
- `1`：启用 Shadow Validate

---

### 5.2 当前流程（简化）

```text
cache hit old
  -> 命中 currentBlockNumber-1
  -> 判断 !isChanged(rawIds)
      -> 是：直接 RefreshPoolBlockNumber + copy cache 到 local
      -> 否：走真实询价
```

---

### 5.3 新流程（Shadow Validate）

```text
cache hit old
  -> 命中 currentBlockNumber-1
  -> 判断 !isChanged(rawIds)
      -> 否：保持现有 3.2 路径，走真实询价
      -> 是：
           若未开启 validate:
               继续当前 3.1 行为
           若开启 validate:
               标记为 noChangeCandidate
               不 refresh cache
               不 copy old detail 到 local
               继续走真实询价

真实询价完成
  -> 对 noChangeCandidate:
       比较 fresh localDetail 与 cachedDetail
       -> match:
            refresh cached blockNumber 到 currentBlockNumber
            local 可复用 cached 或保留 fresh，二者语义等价
            记录 validate_ok
       -> mismatch:
            不 refresh 旧缓存
            local 保持 fresh 结果
            记录 validate_mismatch
```

---

## 6. 代码落点

### 6.1 候选识别阶段

位置：

- `go-service/core/pricer/pricer.go`
- `// 3.1 this block no change pool`

当前行为：

- 直接 refresh blockNumber
- 直接 copy cache 到 local

调整后：

- 若命中 `!isChanged` 且开启 validate：
  - 将该 `callParam` 归类到新的 `noChangeCandidates`
  - 不直接 refresh cache
  - 不直接写 local
  - 继续进入真实询价路径

建议额外记录以下上下文：

- `poolKey`
- `unitAmountKey`
- `rawIds`
- `cachedBlockNumber`
- `currentBlockNumber`

---

### 6.2 结果对比阶段

位置建议：

- `go-service/core/pricer/pricer.go`
- `7.1 处理 cacheHitOld` 之后，或并列新增 `7.1.x process noChangeCandidates`

原因：

- 该阶段 fresh quote 已进入 `stepPriceMap`
- `PriceMapCache` 中仍保留旧缓存
- 对比上下文已经齐全

需要实现：

1. 读取 `cachedDetail`
2. 读取 `localDetail`
3. 做 strict compare
4. 根据 compare 结果决定：
   - refresh 旧缓存 blockNumber
   - 记录 ok / mismatch
   - 是否同步 local / cache

---

## 7. 对比规则

### 7.1 比较对象

最小比较单元：

- `poolKey + unitAmountKey`

比较内容：

- `BlockNumber` 不参与“价格相等”判定
  - 因为校验目标正是验证：上一块价格是否可安全继承到当前块
- 其余内容应比较：
  - `MaxTestPercent`
  - `StepDetails` 全量 key 集合
  - 对每个 `testAmountKey` 比较：
    - `Percent`
    - `StepPrice`
    - `StepCost`
    - `AmountOut`

### 7.2 mismatch 定义

满足以下任一条件即视为 mismatch：

- `StepDetails` key 集合不同
- 任一 `testAmountKey` 缺失
- 任一 `StepPrice` 不同
- 任一 `AmountOut` 不同
- 任一 `StepCost` 不同
- `MaxTestPercent` 不同

### 7.3 match 定义

仅当上述字段全部一致时，才判定为 match。

---

## 8. mismatch 时的处理

### 8.1 数据处理

若 mismatch：

- 不 refresh 旧缓存 blockNumber
- 不保留“旧缓存提升到当前块”的结果
- 后续链路以 fresh `localDetail` 为准
- fresh 结果按现有逻辑写入 cache

### 8.2 日志与落盘

建议记录一条结构化错误日志，字段至少包含：

- `blockNumber`
- `poolKey`
- `unitAmountKey`
- `rawIds`
- `cachedBlockNumber`
- `currentBlockNumber`
- `cached.maxTestPercent`
- `fresh.maxTestPercent`
- `cached.stepDetails`
- `fresh.stepDetails`

建议同时支持按需落盘：

- `test_output/no_change_pool_mismatch.jsonl`

用途：

- replay 时直接 diff
- 排查具体 pool 的误判条件
- 后续回归测试复用

---

## 9. 与 Reth 的联调要求

Shadow Validate 需要在固定块高上做真实询价，因此对 `private_reth` 运行模式有要求。

### 9.1 replay / 固定块回放

推荐：

- `MEV_DEBUG_FIXED_EPOCH=<blockNumber>`

用途：

- 保证 `mev_*` 请求始终命中同一个 epoch
- 避免 replay 时因为节点推进到新块而引入额外噪音

### 9.2 不拒绝旧块 `mev_eth_call`

若 replay 里仍会请求旧块，需保证：

- `MEV_REJECT_STALE_CALL=0`

否则：

- `mev_eth_call` 的 stale 请求会直接返回 `-39001`
- 无法完成“旧缓存 vs fresh quote”的对照验证

> 注：当前该开关已影响三个接口；关闭时会分别回退到原生 `eth_call` / `debug_traceCall` / `trace_call` 路径。

---

## 10. 运行模式建议

### 10.1 模式 A：生产默认

- 不开启 `PRICER_VALIDATE_NO_CHANGE_POOL`
- 保留当前 `3.1` 快速路径
- 目标：性能优先

### 10.2 模式 B：离线正确性校验

- 开启 `PRICER_VALIDATE_NO_CHANGE_POOL=1`
- reth 侧设置固定 epoch
- 对指定 replay 输入做 A/B 对照
- 目标：验证 `!isChanged => 价格不变`

### 10.3 模式 C：生产抽样灰度

- 仅对小流量或特定 pool 开启 validate
- mismatch 仅记录，不阻塞主链路
- 目标：观察真实生产分布下是否存在误判

---

## 11. 推荐实施顺序

### 第一步：离线 replay 验证

- 增加 `PRICER_VALIDATE_NO_CHANGE_POOL`
- 跑固定 `simulatorEvent.json`
- 统计：
  - `validate_total`
  - `validate_ok`
  - `validate_mismatch`

### 第二步：完善落盘与检索

- 输出结构化 mismatch 日志
- 支持按 `poolKey` / `rawId` 检索

### 第三步：生产抽样灰度

- 仅抽样开启 validate
- 评估 mismatch 比例
- 再决定是否需要调整 `BlockImpactIndex` 口径或 `rawIds` 覆盖范围

---

## 12. 预期收益

引入 Shadow Validate 后，可以同时获得两类收益：

### 12.1 正确性收益

- 能直接验证 `3.1` 的核心假设
- mismatch 不会提前污染缓存
- 发生问题时可精确定位到具体 pool / unitAmount / rawIds

### 12.2 工程收益

- replay 有标准校验手段
- 后续优化 `BlockImpactIndex` 时有回归基线
- 能为是否保留、收紧或扩大 `3.1` 适用范围提供数据支持

---

## 13. 非目标

本设计当前不覆盖以下内容：

- 不改变默认生产链路的性能策略
- 不试图在 reth 侧直接判断 pool 是否“价格无变化”
- 不将 `no-change pool` 判断迁移到节点内部
- 不替代现有 `cacheHitOld` / `privateMiss` 的整体缓存框架

---

## 14. 结论

`3.1 this block no change pool` 是高价值优化，但其前提假设必须被验证。

相比“命中 `!isChanged` 就立刻 refresh cache”的现状，更稳妥的方案是：

- 在 replay / 灰度模式下
- 对 `!isChanged` 的 pool 仍然发起真实询价
- 再将 fresh quote 与旧缓存做严格比较
- mismatch 时记录错误并以 fresh 结果为准

这是一种 **正确性优先、性能可控、可灰度、可回归** 的 Shadow Validate 方案。
