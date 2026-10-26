---
title: 网络操作增强 Phase 4：graph 工具栏网络按钮 + ahead/behind 徽标 + Sync + auto-fetch
date: 2026-10-26
status: ready
ideas:
  - .ai-workflow/ideas/20261025-network-ops-enhancements.md
group: network-ops-enhancements
phase: 4
tags: [git, vscode-extension, ui]
---

# 网络操作增强 Phase 4：graph 工具栏网络按钮 + ahead/behind 徽标 + Sync + auto-fetch

## Goal

graph 视图头部出现网络操作区：`[⟳] [↓ Pull] [↑ Push] [⇧ Sync]` 按钮排 + `↑n ↓n`（无 upstream 显示 `↑✱`，点击进入 publish 流程）徽标；操作期间按钮 loading 并禁用同排；auto-fetch 定时静默刷新；状态栏分支项显示 ahead/behind。交付原设计 P3/P4 的"顺手"层。

## Background

见 idea 文件。graph webview 目前没有工具栏（只有 lane 区 + 分隔条），本阶段新建。所有按钮复用 Phase 2 的 `netOps.ts` 共享交互流程 —— 工具栏与命令面板走同一实现，命令面板始终是能力超集。

## Research Summary

- graph webview：`editors/vscode/webview/graph.js`（约 620 行，无网络控件）+ `graph.html`；host 侧 `graphSession.ts` `onMessage` L151-226（所有消息 host 验证：offset 钳制、`COMMIT_ID_RE` L215、宽度范围）；`ready` 握手模式（graph.js L586-612：webview 就绪后 host 才回推数据）。
- 刷新竞争（20260930 核心教训）：网络刷新随时触发，右键目标**不得**在刷新时清除，须消费时校验 + 30s TTL（`contextTargetSlot.ts` 的 take-then-clear 模式）。fetch 完成触发的推送不得早于 webview ready。
- 状态栏：`extension.ts` L305-307 分支状态栏项，L305 有 `TODO(ahead-behind)` 注释；`getBranchStatus`（Phase 1）提供数据。
- 引擎事件：`session.rs` `EngineEvent` L19（RefsChanged/HeadChanged 等，L371 起 emit）—— ahead/behind 在 refsChanged 后刷新。
- 学习教训：webview 图标用 16×16 内联 SVG（stroke=currentColor），不用 `$(codicon)`；hover 显示用 opacity 而非 display:none；webviewSection 正则 when 子句管理菜单。
- 原设计 §4.4：`↑✱` 点击 = publish 流程；Sync = pull→push 串联，任一步失败即停；loading 态防重复触发；detached HEAD 禁用按钮（tooltip 说明）。

## Steps

1. **工具栏 UI** — `graph.html`/`graph.js`：头部新增网络操作行（与 lane 控件并列）：fetch ⟳ / pull ↓ / push ↑ / sync ⇧ 四按钮 + ahead/behind 徽标 pill（`↑2 ↓0`，复用 ref pill 样式基因；无 upstream `↑✱`）。按钮内联 SVG 图标；操作进行中（收到本操作 start 且未 done）按钮转圈 + 同排禁用。

2. **消息协议** — 新增 webview→host：`netOp { op: 'fetch'|'pull'|'push'|'sync' }`（点击按钮，host 验证 op 枚举合法）。新增 host→webview：`netOpState { active: boolean }`（loading 态）、`branchStatus { upstream, ahead, behind }`。徽标点击（`↑✱`）发送 `netOp { op: 'publish' }` 或复用 `openPublish` 语义。所有 host→webview 推送遵守 ready 握手：初始 branchStatus 只在 `ready` 之后发送（早期 postMessage 被静默丢弃 —— 20260930 教训，构造函数内 postMessage 视为评审红旗）。

3. **host 侧编排** — `graphSession.ts` `onMessage`（L151-226）扩展：`netOp` 分支验证枚举后调用 Phase 2 `netOps.ts` 的导出函数（fetch/pull/push/sync 交互流程完全复用：upstream 检测、publish、拒绝恢复按钮组、进度通知、冲突通知）。同步期间发 `netOpState` 控制 loading；完成/失败后复位。

4. **Sync 串联** — `netOps.ts` 内实现 `sync()`：pull → 成功 → push，任一步失败（含冲突、拒绝后的用户未选择）即停，不自动重试循环。

5. **ahead/behind 数据流** — `graphSession.ts`/`extension.ts`：refsChanged 通知（L327 已接）后追加 `getBranchStatus()`（仅当前分支，可 200ms 防抖）→ postMessage `branchStatus`；detached HEAD（Phase 1 返回 INVALID_ARGUMENT 或 upstream null + detached 标记）时禁用 pull/push/sync 按钮、徽标显示 detached 态。

6. **状态栏徽标** — `extension.ts` L305-307：清除 TODO，branchItem 文案追加 `↑n↓n`（无 upstream 省略），数据源同上 getBranchStatus 刷新。

7. **auto-fetch** — `extension.ts`：设置 `gitbamboo.autoFetch.intervalSeconds`（package.json contributes.configuration，默认 180，0 = 关闭）；扩展宿主内定时器（活动窗口激活时启动，休眠时暂停）周期调 `fetch(null)` → `--all`；**完全静默**：失败不弹窗仅输出日志（手动 fetch 仍有一条完成通知 —— 静默原则区分手动/自动）。

8. **刷新竞争加固** — 网络刷新会使图上右键目标失效：确认现有 `contextTargetSlot` 消费时校验逻辑（`graphSession.ts` onMessage 的 contextTarget 分支，L195-215 附近）在网络触发的频繁 refsChanged 下仍成立（30s TTL + 消费时新鲜校验），并为本阶段新增的 netOp 消息补同等校验（操作进行中禁止再排队同类 netOp）。

9. **测试/验收** — tsc + 手工冒烟：徽标数字与 `git rev-list --left-right --count` 一致；`↑✱` 点击走 publish；Sync 在落后+领先场景完成 pull→push；auto-fetch 打开网络面板可见周期性 refsChanged 刷新且无通知弹窗。

## Acceptance Criteria

- [ ] graph 头部出现四按钮 + 徽标；无 upstream 显示 `↑✱`，点击进入 publish 流程（QuickPick remote）。
- [ ] pull/push/sync 按钮行为与命令面板入口一致（同一 netOps.ts 实现，无复制逻辑）。
- [ ] 操作期间按钮 loading 且同排禁用；完成自动复位。
- [ ] 徽标数字在 fetch/pull/push 后自动刷新（refsChanged 驱动）。
- [ ] detached HEAD 时按钮禁用且有 tooltip 说明。
- [ ] autoFetch=180 时后台周期 fetch 全静默；设 0 关闭。
- [ ] 状态栏分支项显示 ↑n↓n。
- [ ] 所有 host→webview 初始推送在 ready 握手之后；所有 webview→host 消息 host 侧验证。
- [ ] tsc 通过 + 手工冒烟记录。

## Dependencies

- Phase 1（getBranchStatus / fetch --all）。
- Phase 2（netOps.ts 共享交互流：publish、拒绝恢复、冲突通知）。
- Phase 3（pull 冲突后的冲突栏闭环，工具栏 pull 才算真正可用）。

## Related Documents

- .ai-workflow/ideas/20261025-network-ops-enhancements.md
- .ai-workflow/plans/20261025-network-ops-enhancements-phase-1.md
- .ai-workflow/plans/20261025-network-ops-enhancements-phase-2.md
- .ai-workflow/plans/20261025-network-ops-enhancements-phase-3.md
- .ai-workflow/learnings/20260930-vscode-webview-early-postmessage-dropped.md
- .ai-workflow/learnings/20260930-validate-context-targets-at-consumption.md
