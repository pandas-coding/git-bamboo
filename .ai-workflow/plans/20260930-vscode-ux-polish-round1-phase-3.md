---
title: UX Polish Round 1 — Phase 3（B1）：引擎 discard RPC 与 Commit 视图丢弃确认流
date: 2026-09-30
status: ready
ideas:
  - .ai-workflow/ideas/20260929-vscode-ux-polish-round1.md
group: vscode-ux-polish-round1
phase: 3
tags: [git, rust, vscode-extension, ui]
---

# UX Polish Round 1 — Phase 3（B1）：引擎 discard RPC 与 Commit 视图丢弃确认流

## Goal

引擎新增 `discard{paths}` 写 RPC（工作区级销毁操作，**按用户决策设计为不可撤销**，以 IRREVERSIBLE modal 确认承担风险），Commit 视图文件行右键与节级操作接入 Discard。执行后：用户可以在 Commit 视图丢弃单个/整节未暂存改动（tracked 恢复自 index、untracked 删除），全程走引擎唯一写入口。

## Background

A 期（phase 2）明确砍掉 discard 归 B 期：引擎无对应 RPC，host 直接 shell git 会破坏"引擎是唯一写入口"（引擎无法感知该操作）。用户决策（2026-09-30）：B1 discard 采用**方案 A**——不可撤销 + 业界标准 IRREVERSIBLE 确认弹窗（VS Code/GitLens 同路线，tracked discard 无人做真撤销）；方案 B（ODB blob 快照 + undo 级联回滚）不采纳，若未来要做按独立计划立项。见 [idea file](../ideas/20260929-vscode-ux-polish-round1.md)。

## Research Summary

- **引擎写路径全貌**：RPC 在 `crates/engine/src/server.rs:266-408`（`dispatch_with_session`）dispatch → `session.enqueue_write`（`session.rs:118-142`，mpsc(32) 单队列）→ `run_write_queue`（`crates/engine/src/write_queue.rs:91-135`）串行执行。`WriteCommand` 枚举在 wq:10-20；git 命令在 `execute_command`（wq:196-285），**全部 spawn git CLI**（`git add` / `git reset HEAD --` / `git commit` / 3-step switch）——discard 沿用 CLI 模式保持写路径统一。
- **undo 与 discard 的交互（关键论证）**：`Snapshot`（`crates/engine/src/undo.rs:28-40`）只存 HEAD、refs、stash、index tree，**不含工作区文件内容**；`UNDO_BLOCKED` 安全检查（undo.rs:270-281）也只比对 refs/index/HEAD。discard 只改工作区 → **不触碰快照可比对状态，天然不阻塞既有 tx 的 undo**，因此无需写 tx、无需入 undo journal。但 wq 现有不变量（wq:169-177）"git 成功但 undo 落盘失败 → 整体报错"要求 run_write 为 discard 提供**跳过 undo begin/commit 的旁路**（仍串行 + epoch bump）。
- **VS Code/GitLens discard 语义**（来源 microsoft/vscode commands.ts ~2255-2360、gitlens discard.utils.ts）：
  - tracked 改动 = `git checkout -- <path>`（**从 index 恢复，staged 内容幸存**，非从 HEAD）；
  - untracked 文件 `checkout --` 不生效，需 `git clean -f -- <path>`（内置 git 走 trash，B1 引擎侧用 clean——引擎是跨平台进程，trash 语义留给 host 可后续增强）；
  - 冲突文件（unmerged index entries）discard 会失败：**分类时发现 conflict 状态直接报错拒绝**（B1 不做 GitLens 的按侧恢复）；
  - 确认弹窗：`showWarningMessage(msg, { modal: true, detail }, 'Discard …')`；单文件软文案，多文件 **"This is IRREVERSIBLE! FOREVER LOST"** 强文案，主按钮 `Discard All {N} Files`；
  - 执行前重读新鲜 status 防陈旧 UI 快照——**引擎侧在执行时分类**即天然满足（比 UI 侧重读更强）。
- **epoch/通知**：写队列成功后同步 bump epoch（wq:113-122）；工作区文件变化由 fs watcher 发 `WorktreeChanged{paths}`（`session.rs:181-360`）自动刷新 Commit 视图（`extension.ts:364-373`）——discard RPC **无需新增通知**。
- **校验链**：Rust `validate_path_arg`（wq:88-97，非空/无前导 `-`/无控制字符）+ git 调用带 `--`；TS `isValidRepoPath`（`editors/vscode/src/headContent.ts:63-72`）在 `commitView.changePaths`（commitView.ts:176-180）先行过滤。B1 discard 复用同链。
- **既有 learnings 约束**：webview↔host 双向不可信（弹窗不替代校验）；destructive 目标消费时校验（`takeContextTarget` 已按 lastItems 校验 bucket）；确认命令沿用 `contextTargetSlot` take-then-clear。

## Steps

### 1. 引擎：protocol + dispatch（`crates/protocol/src/lib.rs`、`crates/engine/src/server.rs`）

- `protocol/src/lib.rs` 在 `Unstage`（:130）后新增 `Discard { paths: Vec<String> }`（snake_case 字段，与现有风格一致）。
- `server.rs:266-408` dispatch 表新增 `"discard"` 分支：解析 params → `session.enqueue_write(WriteCommand::Discard { paths })`，完全镜像 `stage` 分支（:312-321）。

### 2. 引擎：write_queue 执行与 undo 旁路（`crates/engine/src/write_queue.rs`）

- `WriteCommand` 枚举（wq:10-20）新增 `Discard { paths: Vec<String> }`；`op_summary`（wq:39-41）保持不含 path 的惯例，summary 为 `"discard changes"`。
- **执行体**（`execute_command`，wq:196-285 内新 match 分支）：
  1. `validate_path_arg`（wq:88-97）逐 path 校验；
  2. **执行时新鲜分类**：用 `git status --porcelain -- <paths>`（或复用 gix status 读路径，取现有代码更近者）将 paths 分为 tracked（含 deleted/modified/added-staged）与 untracked-only；发现 `DD/AU/UD/UA/DU/AA/UU`（conflict 组合）→ `bail!` 拒绝并指明文件；
  3. tracked 非空 → `git checkout -- <tracked paths>`；untracked 非空 → `git clean -f -- <untracked paths>`；
  4. 非 git 常规退码处理沿用 wq:288-290（stderr → `BambooError::git_error` -32002）。
- **undo 旁路**：`run_write`（wq:148-186）按命令分支——`Discard` 跳过 `undo.begin()/undo.commit()`（不写 tx、不入 journal），但**保留队列串行 + 成功后 `bump_epoch`**（wq:113-122 同路径）。加注释说明：discard 只改工作区，不进入快照可比对集（undo.rs:28-40），故不产生 tx 也不阻塞既有 undo——这是方案 A 的架构落点。
- 测试（跟随现有引擎测试布局，若 `crates/engine/tests/` 已有 write_queue 集成测试则同址新增）：tracked-modified 恢复自 index（staged 内容幸存）、untracked 被删除、conflict 拒绝、单 RPC 混合 paths、`UNDO` 在 discard 之后仍可回滚 discard 之前的 tx（快照不受影响）。

### 3. TS host：确认流 + RPC 转发（`editors/vscode/src/commitView.ts`、`editors/vscode/src/engineClient.ts`）

- `engineClient.ts` 新增 `discard(paths: string[])` 请求方法（镜像现有 `stage`/`unstage` 的调用形式）。
- `commitView.ts`：
  - `CommitViewMessage` 增 `discard` 消息分支：复用 `changePaths` 的校验模式（`isValidRepoPath` 过滤）后**先弹确认再发 RPC**；
  - 确认文案对齐 VS Code 惯例（`commands.ts:95-107` 的 `deleteBranch` modal 是现成模板）：单文件 `Are you sure you want to discard changes in '{basename}'?`，detail `This is IRREVERSIBLE! Your current working set will be FOREVER LOST.`，按钮 `Discard Changes` / `Cancel`（modal 默认）；
  - 节级 Discard All：按钮文案 `Discard All {N} Files`（N 取该节 paths 数），paths 来自 `lastItems` 按桶过滤（changes + untracked 两节各自提供；**staged 节不提供 discard**——语义是从 index 恢复会把 staged 内容抹回 staged 前状态，B1 明确不做，弹窗不出现即无歧义）。
- 右键菜单命令 `gitBamboo.commitView.discardFile`：`runContextCommand('discardFile')` 走 `takeContextTarget()`（已含 lastItems 新鲜度 + bucket 校验）→ 弹单文件确认 → `client.discard([target.path])`。action↔bucket 交叉校验：仅 `file-changes` / `file-untracked` 桶允许（镜像 stageFile 的写法，commitView.ts `runContextCommand`）。

### 4. Webview + 菜单贡献（`editors/vscode/package.json`、`editors/vscode/src/extension.ts`）

- `package.json` commands 新增 `gitBamboo.commitView.discardFile`（title "Discard Changes"，category Git Bamboo）+ commandPalette `when: "false"`。
- `menus → webview/context` 新增一条（复用 phase 2 的 regex 模式）：
  ```json
  { "command": "gitBamboo.commitView.discardFile",
    "when": "webviewId == gitBamboo.commit && webviewSection =~ /^file-(changes|untracked)$/",
    "group": "3_discard" }
  ```
  （`3_discard` 排在 `1_stage`/`2_open` 之后；不提供 staged 桶条目。）
- `extension.ts` 命令注册区（commitView 循环，`extension.ts` L186-202 现状）：`discardFile` 单独注册（与三个循环项不同体——有确认流），handler 镜像 `noSessionWarning`/`noCommitTargetWarning` 安全网。
- 节级 Discard All：`commitView.ts` 暴露 `discardBucket(bucket: 'changes' | 'untracked')`（从 `lastItems` 取该桶全部 paths → 弹 `Discard All {N} Files` 确认 → RPC）；`extension.ts` 注册 `gitBamboo.commitView.discardAllChanges` / `discardAllUntracked` 两命令 + `view/title` 菜单条目（`when: view == gitBamboo.commit`，codicon `$(discard)`，与现有 `gitBamboo.refreshChanges` 同区）。
- UI 惯例：danger 操作用 `$(discard)` codicon（非 webview 面）；webview 内无需新图标（右键菜单由 VS Code 原生渲染）。

### 5. 构建与冒烟

- `cargo test -p bamboo-engine`（或现有引擎测试命令）+ `cargo build`；`npm run typecheck` / `npm run compile` 零错误。
- F5 冒烟（agent 环境无 GUI 项留用户）：
  1. 右键 Changes 行 → 菜单出现 Discard Changes（`3_discard` 组）→ 确认弹窗 modal + IRREVERSIBLE detail → 确认后文件恢复 index 版本且视图刷新（`WorktreeChanged`）；
  2. 右键 Untracked 行 → 确认后文件被删除；
  3. 右键 Staged 行 → **无 Discard 菜单项**；
  4. view title 的 Discard All → `Discard All {N} Files` 文案 → 整节清空；Cancel 无任何变化；
  5. discard 后立即 Ctrl+Z 走 `gitBamboo.undo`（若有前置 tx）：discard 不阻塞 undo；
  6. 冲突状态下右键 conflict 文件 → 引擎报错信息可见（error message surface）。

## Acceptance Criteria

- [ ] `cargo test` 引擎 discard 测试全绿（tracked/untracked/混合/conflict 拒绝/discard 后 undo 仍可用）
- [ ] `tsc --noEmit` 与 `npm run compile` 零错误
- [ ] 右键菜单按桶显隐正确（changes/untracked 有、staged 无），确认弹窗为 modal 且含 IRREVERSIBLE 文案
- [ ] tracked discard 恢复 index 版本（staged 内容幸存）、untracked 被删除
- [ ] discard 全程经引擎写队列（host 无 shell git），串行 + epoch bump，不写 undo tx
- [ ] discard 后既有 undo 流程无回归（F5 冒烟）

## Dependencies

- 实施约定：B1 在新分支 `implement/vscode-ux-polish-round1-phase-3` 上开发，完成并验收后合并回 `main`（用户约定，2026-09-30）。
- phase 1/2 已合并 main（branch `implement/vscode-ux-polish-round1-phase-2` @ `c37143b`），含 `contextTargetSlot.ts`、regex when 菜单、`runContextCommand` bucket 交叉校验——B1 直接复用。
- 引擎现有写队列/undo/fs watcher 架构不变；**零既有 RPC 语义变更**。

## Related Documents

- .ai-workflow/ideas/20260929-vscode-ux-polish-round1.md
- .ai-workflow/plans/20260929-vscode-ux-polish-round1-phase-1.md（同组 Phase 1）
- .ai-workflow/plans/20260929-vscode-ux-polish-round1-phase-2.md（同组 Phase 2）
- .ai-workflow/learnings/20260930-webview-host-two-way-trust-boundary.md
- .ai-workflow/learnings/20260930-validate-context-targets-at-consumption.md
- .ai-workflow/plans/20260930-vscode-ux-polish-round1-phase-4.md（同组 Phase 4 = B2：commit 详情视图）
- （B3 待建：detached checkout / reset / cherry-pick / revert 写 RPC + 确认流）
