---
title: Rebase 与历史洞察 Phase 1：引擎层 rebase 地基（计划/执行/中途态/undo）
date: 2026-10-30
status: done
ideas:
  - .ai-workflow/ideas/20261025-rebase-and-history-insights.md
group: rebase-and-history-insights
phase: 1
tags: [git, rust, undo]
---

# Rebase 与历史洞察 Phase 1：引擎层 rebase 地基（计划/执行/中途态/undo）

## Goal

引擎侧具备交互式 rebase 的全部 RPC 能力：`getRebasePlan`（计算将被重放的 commit 链）、`startRebase`（以外部 todo 执行 `git rebase -i --onto`，支持 pick/reword/squash/drop）、`getRebaseStatus`（检测与解析进行中 rebase 的中途态）、`rebaseContinue`/`rebaseSkip`/`rebaseAbort`。执行前引擎自存 pre-rebase 分支 tip 并走 undo journal（不信任 ORIG_HEAD）。本阶段纯引擎，无任何 UI 改动。

## Background

见 `.ai-workflow/ideas/20261025-rebase-and-history-insights.md`。rebase 是本轮唯一从零开始的引擎大件：无任何宿主现成能力，RPC、执行、中途状态、abort/continue 全需自建。gitoxide 没有 rebase 实现，执行层 shell 出 `git`，todo 控制走 `GIT_SEQUENCE_EDITOR`（GitLens 同款机制）。一期动作集锁定 pick/reword/squash/drop + 冲突停续；edit 二期。

## Research Summary

- **RPC 添加配方**（network-ops 轮验证）：`crates/protocol/src/lib.rs` 类型（现 731 行，`GraphViewport` L100、`BambooError` 错误码 L343-349 已用至 -32010）→ `crates/engine/src/server.rs:378` dispatch 加臂 → 读路径 `gix_read.rs` / 写路径 `write_queue.rs`（`WriteCommand` 枚举 L12-33、`validate_ref_arg` L106、`run_git_checked` L554）→ `engineClient.ts` 包装。新写 RPC 的标准配方含 undo 决策写入代码注释 + 测试覆盖边界。
- **undo 机制**：`crates/engine/src/undo.rs` `Snapshot` L25-36 已含 HEAD 内容 + branch refs + stash_ref + index tree —— rebase 改写 branch refs 正是 Snapshot 捕获的内容，走 `run_write` journal（write_queue.rs L219-261）即可；网络 ops 轮的 `switchBranch`/`Commit` 同模式。
- **执行机制（外部研究）**：`git rebase -i --onto <newbase> <upstream> <branch>` + `GIT_SEQUENCE_EDITOR` 指向非交互写 todo 的程序；程序非零退出 = 干净取消 rebase。reword/squash 的 message 停靠由 `GIT_EDITOR` 在 `COMMIT_EDITMSG` 上触发。`ORIG_HEAD` 不可靠（mid-rebase reset 会覆盖，git 文档明示）→ 引擎自存 tip SHA。
- **中途态检测（GitLens 配方）**：readdir gitdir 判 `rebase-merge`（merge backend）/`rebase-apply`；`rebase-merge` 下读 `head-name`/`orig-head`/`onto`/`msgnum`/`end`/`message`、`git rev-parse --quiet --verify REBASE_HEAD` 取 stopped sha、解析 `done`（已执行）与 `git-rebase-todo`（剩余）。全部 best-effort 读（缺文件不报错）。解析需容忍 `p` 缩写（`rebase.abbreviateCommands`）与 `rebase.instructionFormat`。
- **必须显式 `--no-autosquash`**：用户配置 `rebase.autosquash=true` 会静默折叠 fixup。
- **陷阱**：continue/abort 不设超时上限（大 rebase 超时）、in-flight 去重（并发 continue 只跑一个）、continue 在 rebase 刚结束时返回 "nothing to continue" 应视为成功、abort/continue 失败也可能移动了 HEAD → 之后无条件失效缓存。
- **learnings 红线**：消费时新鲜度校验（客户端只传 id+动作，执行时引擎重算权威状态，stale 输入返回 `INVALID_ARGUMENT -32005` 而非错误执行）；写路径 byte-exact（todo 文件写入不得 lossy）。
- VS Code 侧已有 `mergeEditor.ts:50-55` `isRebaseActive()` 探测（`.git/rebase-merge`/`rebase-apply`），本阶段引擎 RPC 的状态语义与其对齐，Phase 2 接 UI。

## Steps

1. **新建 `crates/engine/src/rebase.rs` 模块** — 承载 rebase 专属逻辑，供 write_queue 与 server 调用：
   - `RebaseTodoEntry { id: String, action: RebaseAction, message: Option<String> }`，`RebaseAction` 枚举 `Pick | Reword | Squash | Drop`（serde 小写字符串）。
   - **todo 序列化**：`serialize_todo(&[RebaseTodoEntry]) -> String`，每行 `pick <sha> <subject>` / `reword <sha> <subject>` / `squash <sha> <subject>` / `drop <sha> <subject>`；行尾 `\n`；写文件用 `Vec<u8>` byte-exact 路径（不经 lossy UTF-8）。
   - **todo 解析**（读回 done/git-rebase-todo 用）：容忍 `pick|p|reword|r|squash|s|fixup|f|drop|d|edit|e` 动作词与任意 sha 长度（7-64 hex）；`label`/`reset`/`merge`/`exec`/`break`/`noop`/`update-ref` 行保留原文透传（不解析为 entry，Phase 1 不支持但不得丢弃）。
   - **rebase-merge 状态解析** `read_rebase_state(git_dir) -> RebaseState`（全部 best-effort，单文件缺失 → 字段 None 不报错）：`{ backend: merge|apply, branch, onto, orig_head, msgnum, end, stopped_sha(REBASE_HEAD), done: Vec<String 原始行>, remaining: Vec<String 原始行> }`。
2. **`getRebasePlan` 读 RPC**（graph 右键 "Rebase onto X" 的数据源）：
   - 请求 `{ onto: String, branch: Option<String> }`（branch 缺省 = 当前 HEAD 分支）。响应 `{ onto, upstream, commits: [{ id, subject, author, time }] }`，`upstream = merge-base(onto, branch)`，`commits` = `upstream..branch` 按拓扑逆序（旧→新，即重放顺序）。
   - `onto` 过 `validate_ref_arg`（write_queue.rs L106 模式）；branch 同；merge-base 无解（无共同祖先）→ `INVALID_ARGUMENT`。gix_read.rs 实现（gix merge-base + revwalk），入 `server.rs:378` dispatch 读路径（不入写队列，session-gated + epoch 一致性照旧）。
3. **`getRebaseStatus` 读 RPC** — 请求 `{}`，响应 = `read_rebase_state` 的直接映射（idle 时 `{ state: "idle" }`；进行中 `{ state: "stopped"|"running", ...RebaseState, done_entries, remaining_entries }`，entries 用第 1 步解析器）。读路径、session-gated。**外部启动的 rebase 也能被检测**（直接读 gitdir，不依赖引擎自身簿记）。
4. **`startRebase` 写 RPC（核心）**：
   - 请求 `{ onto, branch: Option<String>, todo: Vec<RebaseTodoEntry> }`。
   - **消费时新鲜度校验（安全红线）**：执行前重算 `upstream..branch` 的 commit 集合，要求 `todo` 的 id 集合与该集合**完全相等**（drop 也是显式条目）、每个 id 为 40-64 hex；任何偏差 → `INVALID_ARGUMENT -32005`（客户端据此提示 stale，不误执行）。onto/branch 过 `validate_ref_arg`；`todo.len()` ≤ 500；message（reword/squash 自定义消息）过 message 校验（长度 ≤ 8192、无控制字符，network-ops 轮 message 校验同款）。
   - **undo**：`run_write` journal pre 快照（Snapshot 捕获 branch refs/HEAD）；同时把 pre-rebase branch tip SHA 存入 `Snapshot` 扩展字段（新增 `pre_rebase_tip: Option<String>` + `rebase_branch: Option<String>`），undo 恢复时若 rebase 仍在进行先 `git rebase --abort` 再 `git reset --hard <tip>`（branch 指向）。undo 决策（journal，理由）写入 `WriteCommand` 变体注释。
   - **执行**：`WriteCommand::StartRebase` 入写队列；spawn `git rebase -i --onto <onto> <upstream> <branch>`，环境变量：
     - `GIT_SEQUENCE_EDITOR` = 引擎自调用（`std::env::current_exe()` + `--internal-sequence-editor <sidecar-todo-path>`）：该模式下引擎把 sidecar 文件内容 byte-exact 覆盖到 argv 传入的 todo 文件路径后 exit 0；sidecar 为 startRebase 在 tempdir 预写的序列化 todo（**绝不让 webview 输入直接成为 git 输入** —— 引擎重验后才落 sidecar）。
     - `GIT_EDITOR` = `--internal-message-editor <sidecar-msg-path>`：reword/squash 停靠时，git 在 `COMMIT_EDITMSG` 上调用；若 sidecar 存在对应 commit 的自定义消息则覆盖写入后 exit 0，否则直接 exit 0（保留 git 默认消息，即 squash 常规折叠）。匹配方式：停靠时读 `.git/rebase-merge/stopped-sha` 对应 todo entry 的 message；无自定义消息的停靠全部 no-op 通过。
     - 显式 `--no-autosquash`（防用户配置静默折叠）、`GIT_TERMINAL_PROMPT=0`。
   - **冲突/停靠返回**：rebase 停止（冲突或 reword 停靠）时 git 以非零退出 → 不作为错误上抛完整失败，而是返回结构化结果 `OutputResult`-风格：`{ stopped: true, reason: conflict|reword_stop, stopped_sha, msgnum, end }`；成功完成返回 `{ stopped: false }`。新增 `BambooError` 码不需要——停靠是正常语义，用响应字段表达。真错误（ref 无效、git 崩溃 stderr）走既有 `GIT_ERROR -32002`。
5. **`rebaseContinue` / `rebaseSkip` / `rebaseAbort` 写 RPC**：
   - 请求均为 `{}`（操作当前 rebase）。执行前先 `read_rebase_state` 确认有进行中 rebase，无 → `INVALID_ARGUMENT`（"no rebase in progress"）。
   - continue 前置检查：`git diff --name-only --diff-filter=U` 存在未解决冲突 → 返回 `INVALID_ARGUMENT`（提示先解决；UI 侧引导走冲突流）。continue 需带 `GIT_EDITOR` no-op 环境（停靠消息用 `--allow-empty-message`？否——git rebase --continue 在无停靠编辑时不调 editor；若停在 reword，继续时同样接 `--internal-message-editor` sidecar）。
   - `git rebase --continue` 返回 "nothing to continue"（刚完成的竞态）→ 视为成功返回 `{ stopped: false, finished: true }`。
   - **in-flight 去重**：三个命令共享写队列天然串行，但 continue/skip/abort 各自加 per-command in-flight 标志防客户端连点；abort/continue 后无条件 bump epoch + 发既有引擎通知（refs/head 变更通知复用 `HeadChanged`/`RefsChanged`，graph 与 commit view 自动刷新）。
   - **undo 决策**：这三个命令**bypass 新 journal 条目**（abort 本身就是恢复操作；continue 推进已 journal 的 rebase 事务，post 快照在 rebase 完成时补记）。决策写入代码注释。
6. **server.rs + protocol 镜像注册** — 第 2-5 步的每个 RPC：`protocol/src/lib.rs` 请求/响应类型（serde default 齐全，旧客户端兼容无破坏——全是新增方法）、`server.rs:378` dispatch 臂、`engineClient.ts` typed wrapper（本阶段最小：方法存在即可，Phase 2 才接 UI；但 wrapper 顺手写好避免 Phase 2 混引擎改动）。
7. **引擎测试** — `crates/engine/tests/engine_tests.rs`（3146 行，`test_repo()` fixture）新增：
   - plan 计算：3 commit 分叉 → `upstream..branch` 恰为 3 条、顺序旧→新。
   - 执行往返（真实 rebase，非 argv 断言）：drop 一条 → 分支历史少一条；squash 两条 → 消息合并、树内容正确；reword 自定义消息 → `git log -1 --format=%s` 断言；reorder → 顺序反转。
   - 停靠与续行：构造冲突 rebase → `startRebase` 返回 `stopped: true, reason: conflict` → 手动 checkout --theirs + add → `rebaseContinue` → 完成。
   - 中途态：冲突停靠时 `getRebaseStatus` 返回 msgnum/end/stopped_sha/done/remaining 正确；**外部启动** rebase（测试直接跑 `git rebase` 制造停靠）同样可检测。
   - abort：冲突停靠 → `rebaseAbort` → branch 回到 pre-rebase tip（SHA 相等断言）。
   - undo：完成一次 rebase 后 `undo` RPC → branch tip 恢复 pre-rebase SHA；进行中停靠 + undo → abort + reset。
   - 校验边界：todo id 不在链内 → -32005；todo 集合与链不等（漏一条）→ -32005；无 rebase 时 continue → -32005；有未解决冲突时 continue → -32005；message 带控制字符 → -32005。
   - 协议测试（`crates/protocol/tests/protocol_tests.rs`）：新类型 serde 往返、旧字段缺省兼容。

## Acceptance Criteria

- [ ] `cargo test -p bamboo-engine` 全绿（存量 58+ 不动，新增全部通过）。
- [ ] `cargo clippy --workspace` 无新告警。
- [ ] `getRebasePlan` / `getRebaseStatus` / `startRebase` / `rebaseContinue` / `rebaseSkip` / `rebaseAbort` 六个 RPC 经 `engineClient.ts` wrapper 可调用（含协议测试 serde 往返）。
- [ ] 真实 rebase 往返测试覆盖：drop / squash / reword 自定义消息 / reorder / 冲突停靠→解决→continue / abort 恢复 tip。
- [ ] 外部（git 命令行直接）启动的 rebase 停靠可被 `getRebaseStatus` 检测。
- [ ] stale todo（id 集合与当前链不等）返回 `INVALID_ARGUMENT`，不执行任何 git 命令（sidecar 未写入）。
- [ ] rebase 完成后 `undo` 恢复 pre-rebase 分支 tip（引擎 undo 面板一键回滚可用）。
- [ ] 每个 WriteCommand 变体的 undo 决策（journal / bypass 及理由）写入代码注释。

## Dependencies

无（T1 核心引擎与 network-ops 全部 phase 已完成；冲突 UI 在 Phase 2 接入）。

## Related Documents

- .ai-workflow/ideas/20261025-rebase-and-history-insights.md
- 姊妹组：.ai-workflow/plans/20261025-network-ops-enhancements-phase-1.md（RPC 配方与校验模式的参照）
- 后续阶段：phase-2（rebase 计划面板 + graph 集成 + 冲突流共享抽取）、phase-3（JetBrains 式 blame）、phase-4（graph 搜索过滤）
