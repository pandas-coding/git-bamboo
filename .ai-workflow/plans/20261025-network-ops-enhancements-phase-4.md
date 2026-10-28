---
title: 网络操作增强 Phase 4：graph 工具栏网络按钮 + ahead/behind 徽标 + Sync + auto-fetch
date: 2026-10-26
status: done
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

- [x] graph 头部出现四按钮 + 徽标；无 upstream 显示 `↑✱`，点击进入 publish 流程（QuickPick remote）。 \
  实现为头 #toolbar 行（scroller/overlay/splitter 移入相对定位的 #graph-area，几何不变）；`↑✱` 徽标点击发 `netOp {op:'publish'}` 走 netOps.publishBranch 同一 QuickPick。
- [x] pull/push/sync 按钮行为与命令面板入口一致（同一 netOps.ts 实现，无复制逻辑）。 \
  graphSession.runNetOp 经共享 netOpsContext 调 netOps.runPull/runPush/runSync；pull/push 主体已抽取为 pullCurrentBranch/pushCurrentBranch 供两处复用。
- [x] 操作期间按钮 loading 且同排禁用；完成自动复位。 \
  host 在 op 前后回发 netOpState {active, op}；进行中按钮 SVG 旋转动画 + 整排 disabled，finally 中复位。
- [x] 徽标数字在 fetch/pull/push 后自动刷新（refsChanged 驱动）。 \
  引擎侧验证：fetch --all 后 refsChanged+graphInvalidated 确实发出（驱动 200ms 防抖的 sendBranchStatus）；引擎冒烟脚本比对 `git rev-list --left-right --count` 前后一致。
- [x] detached HEAD 时按钮禁用且有 tooltip 说明。 \
  引擎返回 detached 错误（冒烟已验证），host 转 branchStatus {detached:true}，pull/push/sync 禁用 + DETACHED_TITLE tooltip；fetch 保持可用。 
- [x] autoFetch=180 时后台周期 fetch 全静默；设 0 关闭。 \
  extension 定时器直调 client.fetch(null)，无进度通知、无 toast、失败仅 console；窗口非活跃跳过 tick；0 = 不起定时器；配置变更实时重臂。
- [x] 状态栏分支项显示 ↑n↓n。 \
  branchLabel/aheadBehindText 两段组合渲染，refsChanged/headChanged 200ms 防抖刷新；无 upstream/detached 省略。
- [x] 所有 host→webview 初始推送在 ready 握手之后；所有 webview→host 消息 host 侧验证。 \
  初始 branchStatus 挂在 ready 处理器（构造函数零 postMessage）；netOp op 枚举白名单校验，branchStatus 载荷 webview 侧形状校验。
- [x] tsc 通过 + 手工冒烟记录。 \
  tsc --noEmit 与 vite 双构建通过；引擎级冒烟（临时脚本）：ahead/behind 与 git rev-list 一致、fetch 后 refsChanged、detached 报错。VS Code 内 F5 手工冒烟清单见下。

## 手工冒烟清单（F5，VS Code 内）

- graph（sidebar + panel 两宿主）头部四按钮 + 徽标渲染正常；`git rev-list --left-right --count upstream...branch` 与徽标一致。
- 无 upstream 分支显示 `↑✱`，点击出现 remote QuickPick，push -u 后徽标转为 `↑0 ↓0`。
- 落后+领先场景点 Sync：先 pull 后 push 各一条通知；拉取冲突时停在冲突通知（冲突栏闭环）。
- 操作期间按钮转圈且整排禁用；断网点 fetch 看错误通知与按钮复位。
- `git checkout --detach` 后 pull/push/sync 禁用 + detached tooltip，fetch 仍可用，状态栏无 ↑↓。
- 设 `gitbamboo.autoFetch.intervalSeconds` 为 10：网络面板可见周期性 fetch 且无任何通知弹窗；设 0 停止。

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
