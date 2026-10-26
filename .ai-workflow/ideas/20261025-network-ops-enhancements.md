---
title: 网络操作增强：push/pull/fetch 闭环 + 冲突流 + hunk 暂存 + stash
date: 2026-10-25
status: planning
tags: [git, ui, vscode-extension]
---

# 网络操作增强：push/pull/fetch 闭环 + 冲突流 + hunk 暂存 + stash

## Problem

git-bamboo 当前的网络操作（push/pull/fetch）要求用户手敲 remote 名、无 upstream 检测、push 被拒绝后无恢复流程；同时日常提交流程存在两处把用户逼回 VS Code 内置 git 视图的死角：只有整文件 stage/unstage（无 hunk 级）、stash 完全没有 UI（引擎的 auto-stash 逻辑只藏在 switchBranch 内部）。一旦 pull/push 落地，冲突会立刻从"远期规划"变成日常撞上的墙——而当前 merge/冲突解决完全空白。

## Core Idea

按已有设计文档（docs/network-ops-interaction-design.md）实现网络操作闭环，并将**最小冲突流绑定进本期**：冲突看板（文件列表 + Accept Yours/Theirs）+ 借用 VS Code 内置三栏合并编辑器解决复杂冲突。同期补上 hunk 级暂存和 stash 管理 UI，消除日常使用死角。

## Key Insights

- **冲突流借力宿主**：GitStudio 自研三栏合并编辑器是其最死守的领地（单独拆出 Merge Studio 产品）；VS Code 自带 ours/result/theirs 合并编辑器，git-bamboo 可以直接借用，把对方最贵的护城河变成我们最便宜的一块。自建部分只剩冲突看板（文件列表、Accept Yours/Theirs、打开三方 diff 的入口）。
- **冲突流必须与网络操作同轮交付**：设计文档 4.2/4.3.3 所有"失败后交给冲突流程"的路径当前指向不存在的流程；pull 上线而冲突无解，闭环恢复就是半成品（用户 2026-10-25 决策）。
- **Commit 与 Push 保持独立按钮**，不做 GitStudio 式的 Commit & Push 一体按钮（用户 2026-10-25 明确决策，刻意与 GitStudio 不同）。
- **hunk 暂存是感知最强的差距**：日常提交流程里它比 force push 三档、单次策略覆盖等所有花哨设计都高频；先做 hunk 级，行级暂存可后置。
- **stash 是引擎已有 80% 能力的低垂果实**：stash push/pop 逻辑在 switchBranch 的 auto-stash 流程中已存在，只差 stash 列表/create/pop/apply 的前端暴露和独立 RPC。
- **undo 是安全网**：引擎事务快照级 undo（优于 gitStudio 的 reflog 方案：粒度散、不覆盖工作区丢失、会被 gc）覆盖 discard/冲突抉择等破坏性操作。
- **webview 交互安全**（来自 20260930 周期教训）：graph 工具栏网络按钮的每条 webview→host 消息必须在 host 侧验证后才触发引擎 RPC；fetch 完成触发的刷新通知不得早于 webview 就绪（早 postMessage 会被静默丢弃）；网络刷新可能使正在途中的右键动作目标失效，需在消费时校验目标。

## Open Questions

- hunk 暂存与行级暂存的分期：hunk 先行、行级放后一轮，还是一起做？
- 冲突看板的载体形态：graph webview 内嵌区块、独立 webview 面板，还是 Tree View？
- "Resolve simple" 式的简单自动消解（gitStudio 有）是否纳入本期，还是只做 Accept Yours/Theirs + 手动三方编辑？
- pull 前是否提供 auto-stash 选项（复用 switchBranch 的流程）？
- stash pop/apply 冲突时的交互（与冲突看板共用还是独立提示）？

## Possible Directions

- 冲突流最小闭环（已倾向）：看板 + Accept Yours/Theirs + 调用 VS Code 合并编辑器，不自研三方编辑器。
- 备选（未采纳）：自研三栏合并编辑器——成本高、无差异化收益，不取。

## Related Documents

- 设计文档：`docs/network-ops-interaction-design.md`（本 idea 的交互基础，其 4.2/4.3.3 冲突路径由本 idea 补齐）
- 母 idea：`.ai-workflow/ideas/20260921-rust-vscode-git-workbench.md`（T1/T2 规划来源）
- 姊妹 idea：`.ai-workflow/ideas/20261025-rebase-and-history-insights.md`（rebase 冲突复用本 idea 的冲突流）
- 实施计划：`.ai-workflow/plans/20260921-t1-core-engine.md`、`.ai-workflow/plans/20260930-vscode-ux-polish-round1-phase-*.md`
- 本 idea 实施计划（2026-10-26 定稿，覆盖已删除的 `docs/network-ops-interaction-design.md` 全部设计决策）：
  - `.ai-workflow/plans/20261025-network-ops-enhancements-phase-1.md`（引擎 RPC 地基）
  - `.ai-workflow/plans/20261025-network-ops-enhancements-phase-2.md`（命令面板 UX 闭环）
  - `.ai-workflow/plans/20261025-network-ops-enhancements-phase-3.md`（最小冲突流）
  - `.ai-workflow/plans/20261025-network-ops-enhancements-phase-4.md`（graph 工具栏 + auto-fetch）
  - `.ai-workflow/plans/20261025-network-ops-enhancements-phase-5.md`（hunk 暂存 + stash UI）
