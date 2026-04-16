# 多策略上下文压缩补齐方案

## 摘要

本文档给出 Claw Code 在 Rust 工作区内补齐多策略上下文压缩的实现方案。目标不是逐行复刻 Claude Code 内部实现，而是在 Claw 现有架构上补齐 6 类压缩能力，并通过分层测试保证行为正确：

- `autoCompact`
- `microCompact`
- `partialCompact`
- `reactiveCompact`
- `snipCompact`
- `sessionMemoryCompact`

方案采用 Rust 原生重写，优先复用 Claw 现有运行时、session 持久化和 slash command 体系，不复制 Anthropic 专有的 cache-edit API，也不引入 Claude Code 的完整 UI 选择器和完整 session memory 子系统。

## 1. 现状与差距

### 1.1 Claw Code 当前压缩能力

Claw 目前只有单一摘要压缩链路：

- 运行时入口在 `rust/crates/runtime/src/conversation.rs` 的 `maybe_auto_compact()`
- 核心压缩逻辑在 `rust/crates/runtime/src/compact.rs`
- 摘要再压缩在 `rust/crates/runtime/src/summary_compression.rs`
- 手动入口只有 `/compact`

当前模型是：

1. 正常完成一轮请求
2. 在回合结束后判断是否超阈值
3. 超阈值时把旧消息汇总成一个 system summary

当前缺失：

- 请求前的轻量压缩
- 超窗后的恢复性压缩重试
- 局部压缩
- 历史片段裁剪
- 会话记忆型压缩
- 独立的压缩编排层

### 1.2 Claude Code 参考实现的关键特点

参考实现的主链路位于以下文件：

- `restored-src/src/query.ts`
- `restored-src/src/services/compact/autoCompact.ts`
- `restored-src/src/services/compact/microCompact.ts`
- `restored-src/src/services/compact/compact.ts`
- `restored-src/src/services/compact/sessionMemoryCompact.ts`

从调用顺序上看，Claude Code 的压缩不是单个函数，而是 query 主循环中的多层编排：

1. `snipCompact`
2. `microCompact`
3. `partial/context collapse`
4. `autoCompact`
5. `reactiveCompact` 作为超窗后恢复路径
6. `sessionMemoryCompact` 作为 full compact 的优先替代分支

其中 `reactiveCompact.ts` 和 `snipCompact.ts` 在 restored tree 中没有直接落盘，本文档对这两类能力的行为约束基于 `query.ts`、`QueryEngine.ts`、`utils/messages.ts` 的调用契约反推。

## 2. 目标与边界

### 2.1 目标

在 Claw 的 Rust 架构上建立可测试的多策略压缩体系，使上下文管理具备以下能力：

- 在请求发出前主动减小高噪音上下文
- 在真实超窗错误出现后自动恢复并重试
- 支持按 prompt 序号做局部压缩
- 支持裁剪历史低价值内容而非一律摘要化
- 支持轻量的 session memory sidecar 方案
- 维持现有 `/compact` 的兼容语义

### 2.2 非目标

首版明确不做以下内容：

- 不实现 Anthropic 专有 cache-edit microcompact
- 不实现 Claude 的完整 transcript projection 双视图
- 不引入 UI message selector
- 不复制完整 SessionMemory 抽取服务
- 不新增复杂配置 UI，只保留 Rust 侧默认值和必要的 env/test override

## 3. 总体设计

### 3.1 新的压缩编排顺序

在 `ConversationRuntime::run_turn_with_messages()` 中引入统一的压缩编排层。请求生命周期改为：

1. 写入本轮 user message
2. 在真正构造 API request 前执行：
   - `snipCompact`
   - `microCompact`
   - `autoCompact`
3. 发起模型请求
4. 如果出现真实超窗错误：
   - 尝试 `reactiveCompact`
   - 单次重建 request 并重试
5. 正常完成后增量刷新 `sessionMemoryCompact` 所需的 sidecar memory

### 3.2 策略触发顺序

固定顺序如下：

1. `snipCompact`
2. `microCompact`
3. `autoCompact`
4. `reactiveCompact` 仅在模型返回真实超窗错误后触发
5. `partialCompact` 仅在手动 `/compact` 参数化模式下触发
6. `sessionMemoryCompact` 作为 `auto/reactive/manual full compact` 的优先分支

这样做的原因：

- `snipCompact` 和 `microCompact` 成本最低，应优先尝试
- `autoCompact` 是全摘要型兜底，不应抢在轻量策略前面
- `reactiveCompact` 必须只在真实错误后触发，否则会和主动压缩重叠
- `partialCompact` 是用户定向操作，不应自动触发

## 4. 核心数据结构改造

### 4.1 `ConversationMessage` 需要补的元数据

当前 `rust/crates/runtime/src/session.rs` 中的 `ConversationMessage` 元数据不足以支撑多策略压缩。首版需要新增以下字段，并保持对旧 session 文件的兼容加载：

- `message_id: String`
- `timestamp_ms: u64`
- `compaction_meta: Option<CompactionMarker>`

其中 `CompactionMarker` 用于描述 system 消息是否是：

- `compact_boundary`
- `microcompact_boundary`
- `snip_boundary`

新增原因：

- `microCompact` 的 time-based 触发依赖最近 assistant 时间戳
- `snipCompact` 需要稳定的消息标识
- `partialCompact` 需要把 `/history` 序号映射回真实 user message
- `reactiveCompact` 需要记录本轮是否已经做过恢复重试

### 4.2 向后兼容要求

旧 `.json` / `.jsonl` session 文件在缺少新字段时必须自动补默认值：

- `message_id` 缺失时基于加载顺序生成
- `timestamp_ms` 缺失时退化到 session 级时间戳
- `compaction_meta` 缺失时视为普通消息

任何旧 session 不应因为新增字段而无法恢复。

### 4.3 运行时配置

新增 `RuntimeCompactionConfig`，作为 `RuntimeFeatureConfig` 的一部分。首版建议包含：

- `auto_compact_threshold_tokens`
- `snip_target_tokens`
- `micro_trigger_count`
- `micro_keep_recent`
- `micro_gap_minutes`
- `session_memory_min_tokens`
- `session_memory_min_text_messages`
- `session_memory_max_tokens`
- `reactive_retry_limit`

默认值写死在 Rust 侧即可，测试使用 env override。

## 5. 六种策略的落地方案

### 5.1 `autoCompact`

#### 目标

在请求发出前主动执行 full compact，避免等本轮结束后才压缩。

#### 现状问题

Claw 当前在回合结束后才压缩，导致：

- 请求前无法主动减小上下文
- `reactiveCompact` 没有合理接入点
- `microCompact` 和 `snipCompact` 无法与 full compact 形成梯度

#### 新行为

- 将自动压缩的判定移动到 request 构造前
- 不再只依赖累计 usage，而是基于当前 session 的估算 token 体积
- 超阈值时先尝试 `sessionMemoryCompact`
- `sessionMemoryCompact` 不可用时再走现有 `compact_session()`

#### 约束

- 仍保留现有 `TurnSummary.auto_compaction` 字段
- 但其含义改为“本轮请求前是否发生自动压缩”

### 5.2 `microCompact`

#### 目标

在不做 full summary 的情况下，低成本清理高噪音工具结果。

#### 首版实现范围

首版只实现 Rust 原生的“内容清空型 microcompact”，不实现 Claude 的 cache-edit 版。

#### 压缩对象

优先覆盖高噪音、低复用价值的工具结果：

- `read_file`
- `bash`
- `grep`
- `glob`
- `edit_file`
- `write_file`

#### 触发条件

两类触发并存：

1. 数量触发
   - 可压缩 tool result 数量超过 `micro_trigger_count`
2. 时间触发
   - 距最近 assistant 超过 `micro_gap_minutes`

#### 行为

- 保留最近 `micro_keep_recent` 条可压缩结果
- 将更早 tool result 的内容替换为固定哨兵文本
- 不删除消息壳
- 不修改 user prompt
- 不打断 `tool_use -> tool_result` 配对关系

#### 首版哨兵文本

统一使用单行文本，例如：

`[Old tool result content cleared by microcompact]`

#### 输出

- 在 session 中追加一个 `microcompact_boundary` system marker
- 记录：
  - 本次清空了多少条工具结果
  - 估算释放了多少 tokens

### 5.3 `partialCompact`

#### 目标

允许用户只压缩某个 prompt 之前或之后的上下文，而不是总是全量 compact。

#### 交互方式

不做 UI message selector，直接扩展现有 `/compact`。

首版语法：

- `/compact`
- `/compact --up-to-prompt <N>`
- `/compact --from-prompt <N>`

其中 `<N>` 对应现有 `/history [count]` 输出里的 user prompt 序号。

#### 映射规则

- 只允许 pivot 到非 meta 的 user message
- 解析 `<N>` 后，先通过 `/history` 序号映射到 session 中的 user prompt
- 再映射到真实消息索引

#### 两种方向的行为

`--up-to-prompt <N>`

- 压缩第 `N` 个 prompt 之前的上下文
- 保留第 `N` 个 prompt 及其之后的上下文

`--from-prompt <N>`

- 压缩第 `N` 个 prompt 之后的上下文
- 保留第 `N` 个 prompt 之前的上下文

#### 首版约束

- 不自动重放 pivot prompt
- 不自动续问
- 只做上下文重写

### 5.4 `reactiveCompact`

#### 目标

当模型请求因为真实上下文超限而失败时，自动做恢复性压缩并单次重试。

#### 触发条件

在 `ConversationRuntime` 请求模型失败时拦截：

- `ApiError::ContextWindowExceeded`
- provider 返回 413
- 文案包含 `request is too large`
- 文案包含 `context window`

#### 恢复顺序

单次请求最多 reactive 重试一次。顺序为：

1. 如果本轮尚未做过强制轻量回收，先尝试 force `snipCompact` / `microCompact`
2. 如仍然超限，则尝试：
   - `sessionMemoryCompact`
   - fallback 到 full compact
3. 重建 request
4. 重试同一轮

#### 循环保护

- 每轮只允许一次 reactive retry
- 第二次仍超限则直接抛错
- 不能形成“压缩 -> 重试 -> 再压缩 -> 再重试”的无限循环

### 5.5 `snipCompact`

#### 目标

在 full compact 之前，以“裁掉低价值历史段”的方式回收上下文。

#### 说明

由于 restored tree 中没有 `snipCompact.ts` 原文件，首版按保守策略实现。

#### 首版行为

- 仅裁旧 assistant 文本和旧 tool result
- 不裁 user prompt
- 不裁最近 tail
- 不打断 tool pair
- 不做复杂 projection，只做 destructive rewrite

#### 触发场景

- 在请求前先于 `microCompact` 执行
- 面向“上下文接近阈值，但尚未需要 full compact”的场景

#### 策略

基于以下信号选择最早的低价值候选：

- 距当前最远
- 文本很长
- 纯工具输出或纯 assistant 描述
- 不在 protected recent tail 中

#### 结果

- 生成 `snip_boundary` marker
- 记录：
  - 被 snip 的 message ids
  - 估算释放的 tokens

### 5.6 `sessionMemoryCompact`

#### 目标

在 full compact 之前，优先用独立的轻量 memory sidecar 表示历史决策与上下文。

#### 首版底座

不复制 Claude 的完整 SessionMemory 子系统。首版采用 session sidecar 文件：

- 存储位置：与 session 文件同目录
- 文件名：`<session-id>.memory.md`

#### 内容结构

固定 Markdown 分段：

- `Goals`
- `Decisions`
- `Files`
- `Current work`
- `Pending work`
- `Recent prompts`

#### 刷新时机

每轮成功结束后增量刷新 sidecar 内容。

#### 压缩时机

作为以下路径的首选分支：

- `autoCompact`
- `reactiveCompact`
- 手动 `/compact`

#### 保留规则

压缩后保留最近 tail，并满足：

- 至少保留 `session_memory_min_tokens`
- 至少保留 `session_memory_min_text_messages`
- 不拆 tool pair
- 若 memory 文件为空、无有效内容或压缩后仍超阈值，则回退 full compact

## 6. 运行时接入点

### 6.1 主要改造文件

运行时侧预计新增或改造的核心文件：

- `rust/crates/runtime/src/conversation.rs`
- `rust/crates/runtime/src/session.rs`
- `rust/crates/runtime/src/compact.rs`
- `rust/crates/runtime/src/config.rs`

建议新增的模块：

- `rust/crates/runtime/src/compaction_pipeline.rs`
- `rust/crates/runtime/src/micro_compact.rs`
- `rust/crates/runtime/src/snip_compact.rs`
- `rust/crates/runtime/src/reactive_compact.rs`
- `rust/crates/runtime/src/session_memory_compact.rs`

### 6.2 新的运行时入口建议

建议形成统一入口，避免压缩逻辑散落在 CLI 和 commands crate：

- `prepare_messages_for_request(...) -> PreparedContext`
- `compact_full(...)`
- `compact_partial_by_prompt(...)`
- `try_session_memory_compact(...)`
- `try_reactive_compact(...)`

### 6.3 `ConversationRuntime` 的职责变化

`ConversationRuntime::run_turn_with_messages()` 需要承担：

- 请求前压缩编排
- 模型错误恢复重试
- 会话 memory 刷新

现有 `maybe_auto_compact()` 的逻辑要下沉或重写，不能继续保持“回合结束后自动压缩”的旧语义。

## 7. Slash Command 与 CLI 接口变更

### 7.1 `commands` crate 变更

`rust/crates/commands/src/lib.rs` 中现有：

- `SlashCommand::Compact`
- `SlashCommandSpec.argument_hint = None`

建议改为带参数结构：

- `SlashCommand::Compact { up_to_prompt: Option<usize>, from_prompt: Option<usize> }`

帮助文案改为：

- `argument_hint: Some("[--up-to-prompt N|--from-prompt N]")`

### 7.2 兼容性要求

以下命令必须继续可用：

- `/compact`
- `claw --resume SESSION.jsonl /compact`

新增命令：

- `claw --resume SESSION.jsonl /compact --up-to-prompt 3`
- `claw --resume SESSION.jsonl /compact --from-prompt 3`

### 7.3 非法输入校验

必须覆盖：

- 同时传 `--up-to-prompt` 和 `--from-prompt`
- `N <= 0`
- `N` 超过当前 prompt 数量
- pivot 指向不可压缩消息

错误文案要沿用现有 slash command 统一校验风格。

## 8. 测试方案

测试必须拆成三层，否则很难稳定回归。

### 8.1 `runtime` 单测

建议新增以下用例：

- `auto_compact_pre_request_when_threshold_exceeded`
- `micro_compact_clears_old_tool_results_but_preserves_recent_tail`
- `micro_compact_time_based_trigger_uses_last_assistant_timestamp`
- `snip_compact_never_removes_user_prompts_or_breaks_tool_pairs`
- `partial_compact_up_to_prompt_preserves_tail_and_summarizes_head`
- `partial_compact_from_prompt_preserves_head_and_summarizes_tail`
- `session_memory_compact_prefers_memory_sidecar_and_falls_back_when_empty`
- `reactive_compact_retries_once_on_context_window_exceeded`
- `legacy_sessions_without_message_id_and_timestamp_still_load`

### 8.2 `commands` 单测

建议新增：

- `/compact` 无参仍然兼容
- `/compact --up-to-prompt 3`
- `/compact --from-prompt 3`
- 非法组合报错
- help/usage 文案更新

### 8.3 `rusty-claude-cli` 集成测

建议新增：

- resumed session 执行 partial compact 后 session 文件正确更新
- memory sidecar 被正确生成与刷新
- prompt-mode JSON 仍带 `auto_compaction`
- mock provider 返回超窗错误时，reactive compact 能自动重试并成功
- 现有 `compact_output.rs` 不回归

### 8.4 建议执行命令

```bash
cargo test -p runtime compact --manifest-path rust/Cargo.toml
cargo test -p commands compact --manifest-path rust/Cargo.toml
cargo test -p rusty-claude-cli compact --manifest-path rust/Cargo.toml
cargo test -p rusty-claude-cli resume_slash_commands --manifest-path rust/Cargo.toml
```

## 9. 分阶段实施顺序

### 阶段 1：基础元数据

- 给 `ConversationMessage` 增加 `message_id`
- 增加 `timestamp_ms`
- 增加 `compaction_meta`
- 完成旧 session 兼容加载

### 阶段 2：auto compact 前移

- 将自动压缩从“回合结束后”改为“请求前”
- 保持现有 full compact 行为不变
- 校准 `TurnSummary.auto_compaction`

### 阶段 3：session memory sidecar

- 生成 `<session-id>.memory.md`
- 建立最小可用 memory 内容结构
- 接入 `auto/reactive/manual`

### 阶段 4：micro compact

- 支持数量触发
- 支持时间触发
- 追加 `microcompact_boundary`

### 阶段 5：snip compact

- 建立保守型 destructive snip
- 追加 `snip_boundary`
- 与 micro/auto 的顺序编排稳定下来

### 阶段 6：reactive compact

- 拦截真实超窗错误
- 单次恢复重试
- 增加循环保护测试

### 阶段 7：partial compact CLI 暴露

- 扩展 `/compact` 参数
- 接入 `/history` prompt 序号映射
- 增加 CLI 与 commands 层校验

## 10. 风险与约束

### 10.1 首版风险

- `reactiveCompact` 和 `snipCompact` 不是逐行源码复刻，而是契约对齐
- 当前 Claw 没有 Claude 的 read-file state cache，首版 snip 只能保守处理
- session 元数据扩展会影响所有 session 序列化测试
- 自动压缩前移会改变少量现有行为和观察点

### 10.2 约束

- 不得破坏旧 session 文件加载
- 不得引入 tool pair orphan
- 不得让 reactive compact 进入无限重试
- `/compact` 无参必须保持兼容

## 11. 结论

Claw 现在的问题不是“摘要质量不够”，而是“只有单一路径的 full summary compact”。要补齐与 Claude Code 相近的上下文治理能力，关键不是继续堆 prompt，而是建立压缩编排层和分层恢复路径：

- 请求前轻量回收：`snip + micro`
- 请求前主动 full compact：`auto`
- 失败后恢复性重试：`reactive`
- 用户定向压缩：`partial`
- 持久化历史提纯：`sessionMemory`

首版按本文档推进后，Claw 将从“单一摘要压缩”升级为“多策略上下文管理系统”，并且测试面可以覆盖到 runtime、commands、CLI 三层，不再依赖单点手工验证。
