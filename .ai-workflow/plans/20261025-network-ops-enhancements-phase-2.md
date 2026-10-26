---
title: 网络操作增强 Phase 2：扩展命令面板网络 UX 闭环
date: 2026-10-26
status: ready
ideas:
  - .ai-workflow/ideas/20261025-network-ops-enhancements.md
group: network-ops-enhancements
phase: 2
tags: [git, vscode-extension]
---

# 网络操作增强 Phase 2：扩展命令面板网络 UX 闭环

## Goal

命令面板的 fetch/pull/push 全部零输入（zero-typing）：不再手敲 remote 名、push/pull 前做 upstream 检测、无 upstream 走 publish 流程、push 被拒后给出 [Pull & Push (Rebase/Merge)] / [Force Push…] 闭环恢复按钮组、force push 有 modal 确认。交付后即达原设计的"可用"标准（P1/P2）。

## Background

见 idea 文件。本阶段为原设计 P2 + 恢复闭环所需的 P4 子集（force 确认 modal、forceMode/pull strategy 设置）；auto-fetch 与工具栏留给 Phase 4。引擎 RPC（Phase 1）就绪后，客户端编排逻辑全部落在交互层（原设计权衡：engine 保持纯 RPC）。

## Research Summary

- 现有痛点：`editors/vscode/src/commands.ts` — `pickRemote()` L39-44 是 InputBox 手输 remote；`fetchRemotes` L136-141、`pull` L143-148、`push` L150-174 为三个命令入口。
- RPC 客户端：`editors/vscode/src/engineClient.ts` — `request` L91（泛型返回 result 字段）、`onNotification` L114；需按 Phase 1 新 RPC 补类型化方法。
- 命令注册：`editors/vscode/src/extension.ts` L57-64 注册 fetch/pull/push；刷新管线已存在：`refsChanged`/`worktreeChanged` → `commitView.refresh()`（L327-329），fetch 成功后 graph 刷新无需新增工作。
- 认证边界：engine 以子进程跑系统 git，凭据走 git credential helper；非交互认证失败需引导终端。
- 学习教训：host 侧不得 shell out git（破坏 epoch/undo 感知）；所有编排用 RPC。
- 外部研究（VS Code 原生 git 行为）：无 upstream → "publish branch" 提示 + remote QuickPick → `push -u`；force 默认 lease 档；确认 modal 有 lease/force 两档差异化文案。

## Steps

1. **engineClient 类型化方法** — `engineClient.ts`：新增 `getRemotes()`、`getBranchStatus(branch?)`、`stashList()` 及 stash/新签名 push/pull 的 request 封装；RpcError 处理增强为 code-aware（-32008 STASH_CONFLICT、-32009 PUSH_REJECTED、-32010 AUTH_REQUIRED 可被上层 switch）。

2. **共享网络操作模块** — 新建 `editors/vscode/src/netOps.ts`：把 fetch/pull/push 的完整交互流程（upstream 检测、publish、恢复按钮组、进度通知）实现为可导出函数，命令面板与 Phase 4 工具栏共用（学习教训：以"抽取共享模块供两处消费"表述，禁止复制实现）。

3. **Fetch 命令重写** — `commands.ts` L136-141：QuickPick 选项 = `All remotes`（默认，调 `fetch(null)` → `--all`）+ `getRemotes()` 列表（单 remote 精确 fetch）；成功后一条简短完成通知；`refsChanged` 通知自然触发刷新。

4. **Pull 命令重写** — `commands.ts` L143-148，走 `netOps.ts`：
   - 读 `getBranchStatus`：无 upstream → 提示 "Branch 'x' has no upstream" + 按钮 [Set upstream & pull…] → 第 5 步 publish 流程（publish 成功后接着 pull）。
   - 有 upstream → 用设置 `gitbamboo.pull.strategy`（见第 7 步）作为 strategy，`withProgress` 进度通知 → `pull`。
   - 成功：完成通知（输出含 fast-forward / up-to-date / 合并摘要）。
   - 冲突失败（-32008 或含 conflict 的 GIT_ERROR）：错误通知 + [Open Conflict Files] 按钮 —— 本阶段该按钮先落地为打开提交视图并聚焦（Phase 3 完成冲突栏后自然生效）；**不自动 abort**（与 discard 冲突拒绝语义一致，原设计明确）。
   - dirty worktree 时若设置 `gitbamboo.pull.autoStash` 为 true → 带 `auto_stash: true` 调 pull（Phase 1 引擎已支持；默认 false）。
5. **Push 命令重写（publish flow）** — `commands.ts` L150-174，走 `netOps.ts`：
   - 读 `getBranchStatus`：无 upstream → 信息提示 "Branch 'x' is not published yet" → QuickPick 选 remote（第一项 = default remote，列 getRemotes 全部）→ `push(remote, branch, force=none, set_upstream=true)`。
   - 有 upstream：ahead=0 时提示 "Already up to date with <upstream>"（仍允许走 force 入口）；否则 `push(force=none)` → 完成通知 "Pushed <branch> → <upstream>"。
6. **Push 拒绝闭环恢复** — `netOps.ts`：push 返回 -32009 → `showErrorMessage("Push rejected: <upstream> has commits you don't have")` + 按钮组：
   - [Pull & Push (Rebase)] → `pull(strategy=rebase)` 成功后自动重试 `push(force=none)`；
   - [Pull & Push (Merge)] → 同上 strategy=merge；
   - [Force Push…] → 第 8 步确认流程（默认 lease 档）。
   - 任一 pull 出现冲突：**停在那一步**，交给冲突通知（不盲目继续 push）。
7. **设置项** — `editors/vscode/package.json` contributes.configuration 新增：`gitbamboo.pull.strategy`（默认 `"default"`：default/merge/rebase/ff-only）、`gitbamboo.pull.autoStash`（默认 false）、`gitbamboo.push.forceMode`（默认 `"lease"`：lease/force）、`gitbamboo.push.confirmForce`（默认 true）。另新增命令面板入口 `gitBamboo.pullWithStrategy`：QuickPick "Rebase / Merge / Fast-forward only / Default (git config)" 单次覆盖策略。
8. **Force push 确认 modal** — `netOps.ts`：force 只能从明确入口（`gitBamboo.forcePush` 命令或拒绝流程按钮）到达，普通 push 永不隐式 force。`confirmForce=true` 时 modal："Force push '<branch>' to '<upstream>'?"，detail 按 forceMode 差异化：lease 档文案强调"仅当远端仍指向上次所见时覆盖（--force-with-lease）"；force 档文案明确"无条件覆盖，远端分叉提交将丢失（--force）"。确认后调 `push(force=lease|force)`。
9. **认证失败引导** — -32010 错误 → 通知附 [Open Terminal & Fetch] 按钮，引导在集成终端跑一次 `git fetch` 完成 credential helper 认证。本期不自建 credential 交互（GitStudio 亦依赖系统 git 凭据，原设计边界）。
10. **进度通知** — pull/push/fetch 均包 `vscode.window.withProgress`（客户端已知阶段 connecting → transferring → done 的 `report()` 文案）；不做 engine 端 `--progress` 解析（原设计 §2.6 延后决策，维持）。

## Acceptance Criteria

- [ ] 命令面板 fetch/pull/push 全程无 InputBox 手输 remote。
- [ ] 新分支（无 upstream）push → 出现 publish QuickPick（默认 origin）→ 成功后 upstream 建立，再 push 为一键。
- [ ] 制造 non-fast-forward 拒绝（本地落后 + 分叉）→ 错误通知出现三个按钮，选 [Pull & Push (Rebase)] 后自动完成 pull+push 闭环。
- [ ] force push 前 modal 出现，lease/force 文案不同；普通 push 永不产生 --force*。
- [ ] pull 冲突后不自动 abort，出现 [Open Conflict Files] 按钮。
- [ ] `gitbamboo.pull.strategy` 设置生效；`pullWithStrategy` 可单次覆盖。
- [ ] 关闭网络（或用坏 URL remote）pull → 认证/网络错误不裸抛，有可读通知。
- [ ] `npm run typecheck`（或等价 tsc）通过；手工 GUI 冒烟覆盖上述场景。

## Dependencies

- Phase 1（引擎 RPC：getRemotes/getBranchStatus/PUSH_REJECTED/force 枚举/set_upstream/pull strategy/auto_stash）。
- [Open Conflict Files] 按钮的完整效果依赖 Phase 3 冲突栏，本阶段先接占位聚焦行为。

## Related Documents

- .ai-workflow/ideas/20261025-network-ops-enhancements.md
- .ai-workflow/plans/20261025-network-ops-enhancements-phase-1.md
- 后续：phase-3（冲突流）、phase-4（工具栏复用 netOps.ts）
