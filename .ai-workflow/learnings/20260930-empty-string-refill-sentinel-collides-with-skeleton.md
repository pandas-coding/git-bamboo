---
title: 用空字符串作 DOM"强制重填"哨兵会与骨架值撞车短路
date: 2026-09-30
category: surprise
plans:
  - .ai-workflow/plans/20260929-vscode-ux-polish-round1-phase-1.md
tags: [vscode-extension, dom, virtual-list, context-menu]
---

# 用空字符串作 DOM"强制重填"哨兵会与骨架值撞车短路

## Context

git-bamboo 的 Commit Graph 用虚拟列表渲染行：`renderRows()` 比较 `el.dataset.commitId !== commitId` 决定是否重跑 `fillRow`。commit 不存在时 `commitId` 为 `''`（骨架行）。刷新时需要强制重填已渲染行（如 refs/HEAD 更新、invalidate），惯用手法是清空 `dataset.commitId`。

## Insight

`dataset.commitId = ''` 对**曾持有真实数据、现在应回到骨架态**的行不触发重填：行上残留的 commitId 被清成 `''`，而比较右侧（无 commit 时）也是 `''`，相等即跳过 `fillRow`。后果是 `fillRow` 曾经设置的属性——`data-vscode-context`（原生右键菜单目标）、`title`、`head-row` class——永不清理：invalidate 后骨架行仍弹右键菜单，且可能命中 stale 的右键目标（对旧 commit 执行检出）。空字符串在这里同时扮演"骨架值"和"重填哨兵"两个语义，互相冲突。

## Evidence

Phase 1 计划原文引用"同 applyRefs 的既有手法"（清 dataset 触发重填），实现照做。Review 发现 invalidate 后骨架行的 `data-vscode-context` 残留，原生菜单照常弹出，违反"右键骨架行不出现菜单"的验收标准；修复改用永不与真实值撞车的哨兵（`'\u0000'`）并统一走 `fillRow` 骨架路径清理。

## Recommendation

- 虚拟列表/diff 渲染中，"清空字段强制重填"的哨兵必须选一个**任何真实数据都不可能等于**的值，不能复用"空=无数据"的哨兵。
- 复制既有代码的重置惯用法前，先走一遍比较路径，确认重置真的会让填充逻辑重跑。
- 属性级残留（尤其影响安全面如菜单目标）是这类 bug 的典型产物；重置逻辑应集中在一处（这里是 `fillRow` 的骨架分支），而不是在多个事件处理器里手写各清各的。
