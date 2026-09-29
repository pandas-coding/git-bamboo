---
title: Git Bamboo VS Code 插件样式与交互优化（A/B 两期）
date: 2026-09-29
status: planning
ideas:
  - .ai-workflow/ideas/20260921-rust-vscode-git-workbench.md
tags: [git, vscode-extension, ui]
---

# Git Bamboo VS Code 插件样式与交互优化（A/B 两期）

## Problem

独立面板 UI（independent-panel-ui 计划）落地后的实际使用反馈：commit graph 图区写死 240px，侧栏视图里把 commit message 挤到不可读；双击 commit 弹出 "detached checkout … coming in T2" 占位提示，危险操作（checkout）被绑在了随意的手势上；整体交互贫瘠，只有 commit/amend，远不到 GitStudio/GitLens 基本盘；activity bar 图标用 `$(git-branch)` codicon，与 VSCode 内置 git 混淆。

## Core Idea

分两期修补：A 期不动 Rust 引擎，纯插件层解决用户感知最强的问题（graph 宽度自适应 + 可拖拽、删双击、右键菜单框架 + 引擎现有 RPC 能撑住的菜单项、竹子主题 SVG 图标、HEAD/tag 显示、Commit 视图批量操作）；B 期扩引擎写操作 RPC（checkout/reset/cherry-pick/revert）和 commit 详情 diff 视图，把右键菜单补全到 GitStudio 水平。

## Key Insights

- **graph 宽度**：根因是 `GRAPH_WIDTH=240` 与 CSS `.graph-gap/#overlay` 三处硬编码。方案：宽度随可见提交的最大 lane 数动态收缩（多数历史只有 2~4 条 lane，平时 ~60px），加 4px 拖拽分隔条，宽度按宿主（侧栏 vs 编辑器面板）分别持久化到 globalState。
- **双击占位提示**：是 T1 遗留假动作（引擎无 checkout RPC）。直接删除；checkout 属右键菜单，detached checkout 必须带确认框。**A 期即可提供安全的"检出"体验**：`createBranch(base=sha)`（write_queue.rs 明确支持 base 为 SHA）+ `switchBranch(name)`，两步组合走现有 RPC；裸 detached checkout 留 B 期。
- **右键菜单形态**：webview 内自绘 context menu（`contextmenu` 事件 + 自绘 UI，经 host 消息派发动作），比 postMessage→QuickPick 快且可定制——GitLens 同路线。
- **A 期可用的引擎 RPC**：`createBranch(base=sha)`、`switchBranch`、`getRefs`、`getGraph`、`stage/unstage/commit`——Copy SHA/Message、Create Branch at、检出（建分支式）、tag 显示（getRefs 已返回 tag，前端 `applyRefs` 过滤掉了）、HEAD 行标记（getHead）全部 A 期可做。
- **图标**：竹子主题 SVG + commit 树意象（如竹节构成节点/分叉），activity bar 容器 icon 用 SVG 文件路径、状态栏同步替换、补 `package.json` "icon" 字段（128×128 PNG for marketplace）。
- **顺带发现的缺口**（按影响排序）：单击 commit 无任何反馈（详情视图 → B 期）；无 HEAD 指示；tag 不显示；author/时间列未右对齐且 `formatTime` 丢时间信息；ahead/behind 缺失（引擎不暴露 tracking）；graph 无搜索；Commit 视图无 stage all/unstage all/discard 与文件级右键；lane 颜色按序号循环不稳色；无键盘导航。
- **用户对齐结论（2026-09-29）**：允许分两期，但重点问题（用户报的 4 条）必须全在 A 期；commit 详情视图接受放 B 期；图标用竹子 + commit 树意象 SVG。

## Open Questions

- A 期右键菜单的具体条目集与分组（检出方式命名、"Copy SHA" 短 SHA vs 全 SHA 等细节），留待计划阶段定。
- lane 颜色按分支稳定着色涉及引擎 lane 计算输出，是否 A 期先不动（保持现状），B 期随二进制数据面计划一起做。
- ahead/behind 依赖引擎 tracking 信息，归 B 期还是更后，未定。
- 竹子 SVG 的具体形态（竹节抽象程度），实现时出草图再定。

## Possible Directions

- A 期范围收敛方案（已采纳）：用户报的 4 条全修 + 低成本高感知附加项（HEAD 标记、tag 显示、meta 排版、Commit 视图批量操作）。
- 备选（未采纳）：一步到位连引擎 RPC 一起做——周期长，用户感知最重的问题被拖住，不取。

## Related Documents

- 母 idea：`.ai-workflow/ideas/20260921-rust-vscode-git-workbench.md`
- 前置计划（本次反馈来源）：`.ai-workflow/plans/20260927-independent-panel-ui.md`
- 实施计划（A 期，group `vscode-ux-polish-round1`）：
  - `.ai-workflow/plans/20260929-vscode-ux-polish-round1-phase-1.md`（A1：graph 宽度自适应/分隔条、删双击、右键菜单、HEAD/tag、meta 排版）
  - `.ai-workflow/plans/20260929-vscode-ux-polish-round1-phase-2.md`（A2：Commit 视图批量操作/文件右键、竹子图标与 marketplace icon）
