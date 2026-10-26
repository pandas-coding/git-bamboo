---
title: 网络操作增强 Phase 1：引擎层网络/stash RPC 地基
date: 2026-10-26
status: in-progress
ideas:
  - .ai-workflow/ideas/20261025-network-ops-enhancements.md
group: network-ops-enhancements
phase: 1
tags: [git, rust]
---

# 网络操作增强 Phase 1：引擎层网络/stash RPC 地基

## Goal

引擎侧具备支撑全部网络操作 UI 与 stash UI 的 RPC 能力：push 返回真实输出并支持 force 三档与 set-upstream、pull 支持策略与 auto-stash、fetch None 语义为 `--all`、新增 `getRemotes`/`getBranchStatus` 读 RPC、新增独立的 stash push/pop/drop/list RPC（从 switchBranch 的 auto-stash 中抽取共享）。本阶段纯引擎，无任何 UI 改动。

## Background

见 `.ai-workflow/ideas/20261025-network-ops-enhancements.md`。原设计文档 `docs/network-ops-interaction-design.md` 已删除，本计划组自包含其全部设计决策。引擎已有 `fetch`/`pull`/`push` RPC 但存在协议缺陷：push 返回 `Value::Null`、`force` 是布尔（无法表达 lease/force 两档）、pull 无策略参数、无 upstream 检测、stash 只存在于 switchBranch 内部。

## Research Summary

- 现有协议结构：`crates/protocol/src/lib.rs` — `OutputResult` L116、`StatusCode` L49-57（含 `Conflict`）、`Fetch` L248-250、`Pull` L253-257、`Push` L258-263（`force: bool`）、`BambooError` 错误码常量 L343-349（-32001…-32007）。
- 现有分发：`crates/engine/src/server.rs` — `dispatch_with_session` L370-560（`fetch` L504、`pull` L512、`push` L523-534 返回 `Ok(Value::Null)`）。
- 现有写队列：`crates/engine/src/write_queue.rs` — `WriteCommand` 枚举 L12-33（`Fetch`/`Pull`/`Push` L25-27）、`execute_command` L277-355、网络命令 shell 执行 L356-386、`switch_branch`（auto-stash）L580-640、参数校验 `validate_path_arg` L90 / `validate_ref_arg` L106 / `validate_pushspec` L137、`run_git_checked` L554。
- undo：`crates/engine/src/undo.rs` — `Snapshot` L25-36 已含 `stash_ref`；`run_write`（write_queue.rs L219-261）为非 discard 写自动 journal。已有学习：网络刷新会使 undo 的外部 ref 检测更频繁触发 `UNDO_BLOCKED`，属预期行为。
- 外部研究：bare `--force-with-lease`（无期望值）会被后台 fetch 削弱；但 git shelling 下精确期望值需额外读远端 ref，一期先用 lease 无参形式，风险由"确认 modal + 可配置 forceMode"兜底。stash pop 冲突时 git 不丢弃 stash —— 需要显式错误码暴露这一语义。
- 学习教训（`.ai-workflow/learnings/`）：新增写 RPC 的标准配方 = protocol 枚举 → server.rs dispatch 镜像 → write_queue 分支 → undo 决策写入代码注释 → 测试覆盖边界。

## Steps

1. **Push 协议升级** — `crates/protocol/src/lib.rs` L258-263：`force: bool` 改为 `force: ForceMode` 枚举（`none` | `lease` | `force`），加 serde 兼容反序列化：旧布尔 `true` 映射 `lease`、`false` 映射 `none`（协议版本不跳变）；新增字段 `set_upstream: bool`（serde default false）。server.rs L523-534：push 响应从 `Ok(Value::Null)` 改为返回 `OutputResult`（与 fetch/pull 一致）。`write_queue.rs` `execute_command` 网络分支 L356-386：`ForceMode::Lease` → `--force-with-lease`、`ForceMode::Force` → `--force`、`set_upstream=true` → `-u`；`force: force` 仅由客户端 modal 确认后可达（engine 不做确认，保持纯 RPC）。

2. **Pull 协议升级** — `protocol/src/lib.rs` L253-257：新增 `strategy: PullStrategy` 枚举（`default` | `merge` | `rebase` | `ff-only`，serde default `default`）与 `auto_stash: bool`（serde default false）。`write_queue.rs`：`default` 不追加任何 flag（尊重用户 `pull.rebase`/`pull.ff` git 配置）；`rebase` → `--rebase`；`merge` → `--no-rebase`；`ff-only` → `--ff-only`。`auto_stash=true` 时 pull 前后走第 5 步抽取出的 stash 原语（push dirty worktree → pull → pop，pop 失败时错误信息明确"changes retained in stash"，复用 switch_branch L584-586 的既有语义）。

3. **Fetch None → `--all`** — `write_queue.rs` L356-386 的 Fetch 分支：`remote: None` 时执行 `git fetch --all`（图形客户端"刷新所有远端"的心智；单 remote 精确 fetch 由客户端显式传 remote 保留）。

4. **`getRemotes` 读 RPC** — `protocol/src/lib.rs` 新增请求/响应类型（`[{ name, url, default }]`，`default` = `remote.<name>.default`? 实际按 git 语义取 `remote.default` 配置指向者，无配置时取第一个）；`gix_read.rs` 实现（读 remote 配置，gix 或 `git remote -v` 解析）；`server.rs` dispatch 注册（读路径，不入写队列）。

5. **stash 独立 RPC + switchBranch 共享抽取** — 按学习教训"第二个消费者出现时抽取共享模块"：
   - `write_queue.rs` L580-640：把 stash push/pop 的原语（`run_git_checked` 调用、"失败时 stash 保留"错误语义）抽取为共享函数，`switch_branch` 与新 stash 命令共同调用，不得复制。
   - `WriteCommand` L12-33 新增变体：`StashPush { message: Option<String>, include_untracked: bool, staged_only: bool }`（`git stash push [-u] [-S] [-m]`）、`StashPop { stash_ref: Option<String>, apply: bool, reinstate_index: bool }`（pop/apply，`reinstate_index` → `--index` 恢复暂存态；`stash_ref: None` = 最新）、`StashDrop { stash_ref }`。
   - `protocol/src/lib.rs` + `server.rs` 镜像注册；`gix_read.rs` 新增 `stashList` 读 RPC（`git stash list` 解析为 `[{ ref, message, date }]`）。
   - **stash pop 冲突错误码**：`BambooError` L343-349 新增 `STASH_CONFLICT: i32 = -32008`，pop/apply 冲突或"local changes would be overwritten"时返回（语义：git 未丢弃 stash，错误 data 注明 "stash retained"）。冲突文件已在工作区，直接交给 Phase 3 冲突流。

6. **`getBranchStatus` 读 RPC** — `protocol/src/lib.rs` 新增请求 `{ branch: Option<String> }`（缺省 = 当前分支）/ 响应 `{ branch, upstream: Option<String>, ahead: u32, behind: u32 }`。`gix_read.rs` 实现：upstream 检测（分支配置的 `branch.<name>.remote`/`merge`，无则 null）；ahead/behind 用 `git rev-list --left-right --count upstream...branch`。独立 RPC 而非塞进 getRefs（refs 高频全量，双向 walk 贵且仅 HEAD 分支需要 —— 原设计权衡，维持）。detached HEAD 返回 `upstream: null` + 特殊标记或错误（选错误 `INVALID_ARGUMENT`，客户端据此禁用按钮）。

7. **push 拒绝的结构化错误** — `BambooError` 新增 `PUSH_REJECTED: i32 = -32009`：write_queue 执行 push 后检查 stderr/输出含 `rejected (fetch first)` / `(non-fast-forward)` 时返回该码（错误 data 携带 remote 分支名）。engine 不做恢复编排（pull→push 串联属客户端交互层，原设计权衡，维持）。同时新增 `AUTH_REQUIRED: i32 = -32010`（stderr 含 `could not read Username`/`Authentication failed`），供客户端引导终端认证。

8. **undo 决策（写入代码注释）** — pull/push/stash push/pop/drop 保持走 `run_write` journal（动 refs/index，快照可恢复：`Snapshot` L25-36 已含 `stash_ref`/`index_tree`）；**fetch 显式 bypass undo journal**（远端跟踪 ref 镜像外部真相，undo 恢复无意义），bypass 理由写入代码注释，模式参照 discard（write_queue.rs 现有 bypass 分支）。

9. **参数校验** — remote/branch/stash_ref/message 均过 `validate_ref_arg`/`validate_pushspec`（L106-137）或新增等价校验（message 长度上限 + 无控制字符）；stash_ref 限定 `stash@{N}` 形态，防注入。

10. **引擎测试** — `crates/engine/tests/engine_tests.rs`（现有 auto-stash 测试 L648-680 旁）新增：force 三档 argv 断言、set_upstream 后 `git rev-parse --abbrev-ref @{upstream}` 断言、pull 四策略 argv、fetch None 为 `--all`、getRemotes/getBranchStatus（有/无 upstream、ahead/behind 数值）、stash push/pop/drop/list 往返、stash pop 冲突返回 -32008 且 stash 仍在、push 拒绝返回 -32009、旧布尔 force 的 serde 兼容往返。

## Acceptance Criteria

- [ ] `cargo test -p bamboo-engine`（含新增测试）全绿。
- [ ] `cargo clippy --workspace` 无新告警。
- [ ] push RPC 返回 `OutputResult`（非 null），`force: "lease"`/`"force"`/`"none"` 产生对应 argv。
- [ ] 旧客户端发送 `force: true`（布尔）仍被正确解析为 `lease`（serde 兼容测试）。
- [ ] `getRemotes`、`getBranchStatus`、`stashList` RPC 可用；stash pop 冲突返回 -32008 且 stash 未被丢弃。
- [ ] fetch `remote: null` 执行 `git fetch --all`。
- [ ] 每个新 WriteCommand 的 undo 决策（journal 或 bypass）在代码注释中有明确说明。
- [ ] switchBranch auto-stash 行为不变（现有测试 L648-680 不改动仍通过）。

## Dependencies

无（T1 核心引擎已完成）。

## Related Documents

- .ai-workflow/ideas/20261025-network-ops-enhancements.md
- 后续阶段：phase-2（扩展命令面板 UX）、phase-3（冲突流）、phase-4（graph 工具栏）、phase-5（hunk 暂存 + stash UI）
