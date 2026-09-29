---
title: "精确行号 + 当前代码事实"式计划研究使实现零返工
date: 2026-09-30
category: pattern
plans:
  - .ai-workflow/plans/20260929-vscode-ux-polish-round1-phase-1.md
tags: [planning, research, codebase-exploration, vscode-extension]
---

# "精确行号 + 当前代码事实"式计划研究使实现零返工

## Context

git-bamboo 的 UX polish 计划采用三路研究（codebase / 外部文档 / 引擎源码）后，把"当前代码事实"直接写进计划：目标函数的精确行号、常量名、既有链路（如双击 = graph.js L101 + graphSession.ts L103 + 消息接口 L20）、引擎 RPC 的参数与校验点（write_queue.rs L294–303 base 可为 SHA）。

## Insight

计划里每条断言都锚定到可验证的代码坐标时，实现变成机械翻译：步骤即 diff 的说明书，无需中途再做代码考古。返工几乎全部消失，review 发现的问题也集中落在计划未覆盖的维度（运行时时序、信任边界），而不是计划研究过的维度——两个阶段互补得非常干净。

## Evidence

Phase 1 计划的行号/事实全部命中（GRAPH_WIDTH=240、双击链路三文件位置、getRefs 的 RefKind::Tag、CSP 允许行内 SVG 等），10 个 commit 约 25 分钟完成，主路径零返工。对照之下，计划唯一失败的两处引用都是**把既有代码模式当可靠事实引用**而未审计模式本身：照抄 sendRefs 的构造函数推送（早于 webview 加载，消息被丢）、照抄 applyRefs 的"清 dataset 重填"手法（哨兵与骨架值撞车）——两处均在 review 阶段才暴露。

## Recommendation

- 计划研究阶段就核实并写下：目标代码的精确位置（文件+行号）、相关常量/接口签名、依赖方（引擎 RPC）的参数与校验行为、平台约束（CSP、API 版本）。
- 引用既有代码模式作为实现手法时，**模式本身也要过一遍研究**：走通它的执行路径、确认前置条件仍成立（本例中分别是"接收方监听器已就绪"与"重填比较会短路"）。"代码里已经这么干"不是正确性证据。
- 验收标准里保留静态可验项（typecheck/grep）与 GUI 冒烟项的区分标记，让 agent 与人各验各的。
