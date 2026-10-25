---
title: 交互式 rebase（计划面板）+ JetBrains 式 blame 与历史搜索
date: 2026-10-25
status: raw
tags: [git, ui, vscode-extension]
---

# 交互式 rebase（计划面板）+ JetBrains 式 blame 与历史搜索

## Problem

git-bamboo 完全没有 rebase、blame 和历史搜索能力——这是与 GitStudio（交互式 rebase 拖拽排序、blame 带代码年龄热力图、强过滤搜索是其招牌）和 JetBrains git 插件（编辑器内 blame 标注 + 导航 + log 强过滤）的共同差距。用户明确希望这一轮纳入交互式 rebase，且 blame/搜索参考 JetBrains 的集成水准。

## Core Idea

交互式 rebase 采用**独立计划面板**（JetBrains 路线）：graph 右键 "Rebase onto…" 弹出面板，列出将被重放的 commit 链，面板内拖拽排序、改动作（pick/reword/squash/drop 等），确认后执行；执行中的冲突复用网络操作轮的冲突流。同期实现 JetBrains 式 blame（编辑器内行级标注 + 悬停详情 + 向历史导航）和 graph 搜索过滤。

## Key Insights

- **rebase 的操作模型是 todo 清单**：git 交互式 rebase 本质是一份按顺序重放的 commit 待办清单（pick/reword/edit/squash/drop），GUI 的拖拽排序只是这份清单的图形化。独立面板贴合这个模型：安全、易做冲突回滚、确认前不碰真历史；graph 直接操作（GitStudio 路线）酷但要先解决跨 lane 拖拽语义与实时布局重算，暂不取（用户 2026-10-25 选定面板路线）。
- **rebase 是本轮唯一从零开始的引擎大件**：merge 能借 VS Code 合并编辑器，rebase 没有任何宿主现成能力——RPC、执行、中途状态（stopped for edit / conflict）、abort/continue 全需自建。
- **冲突流复用**：rebase 中途的冲突与 merge/pull 冲突走同一套看板 + 三方编辑体验，两份 idea 的冲突基建共享。
- **引擎 undo 是 rebase 的安全网**：rebase 是高风险历史改写，确认执行前自动挂 undo 点，失败/反悔一键回滚——这是对 gitStudio reflog 式 undo 的差异化优势。
- **JetBrains 式 blame 的三层结构**（对标参考）：
  1. 编辑器内行级标注（gutter 注解，显示作者/时间）；
  2. blame 悬停详情（该行所属 commit 的完整信息、diff 入口）；
  3. blame 导航（从某行跳到引入它的 commit、annotate previous revision 沿历史回溯）。
- **blame 计算是引擎侧新读能力**：gix 有 blame 实现，需要新增 RPC；行级 blame 性能要在大文件上验证。
- **graph 搜索过滤**：按作者/消息文本/路径/分支过滤 commit 历史是历史洞察的入口，与 graph 虚拟滚动天然兼容（过滤即查询条件）。
- **多 commit 框选批量操作**（cherry-pick/revert 批量、GitStudio 有）与 rebase 面板的选中链模型相近，可考虑共用交互基因，但 cherry-pick/revert 引擎 RPC 仍是空白，范围待定。

## Open Questions

- blame 的渲染载体：VS Code decoration API（gutter 标注）能力边界如何？热力图（代码年龄着色，GitStudio 特色）是否纳入？
- blame 是否需要"文件历史视图"（该文件的历史 commit 列表）作为配套，还是先只做编辑器内标注 + 导航？
- graph 搜索的过滤条件集合与 UI 位置（graph 工具栏内 vs 命令面板）？
- rebase 面板的动作集：pick/reword/squash/drop 是否一期全做？edit（停下来改提交）中途态复杂，可否后置？
- cherry-pick/revert（含批量）是否借这轮 rebase 的框架顺带落地，还是完全独立排期？
- rebase 中途状态的可见性：进行中的 rebase 在 graph 上如何呈现（进行态标记、continue/abort 入口）？

## Possible Directions

- rebase 交互形态三选一（已定）：独立计划面板（JetBrains 路线，用户选定）；备选未采纳：graph 内直接操作（GitStudio 路线）、混合计划模式（graph 切换可编辑视图）。
- blame 范围两档：最小版（gutter 标注 + 悬停 + 导航）先落地，热力图/文件历史视图视效果后补；或一步到位。

## Related Documents

- 母 idea：`.ai-workflow/ideas/20260921-rust-vscode-git-workbench.md`（T2：交互式 rebase、多分支对比的原规划）
- 姊妹 idea：`.ai-workflow/ideas/20261025-network-ops-enhancements.md`（冲突流基建来源，rebase 冲突复用）
- 设计文档：`docs/network-ops-interaction-design.md`
- 对标研究：本 idea 的差距分析基于 2026-10-25 对 GitStudio（github.com/GitStudioHQ/gitstudio / gitstudio.dev）与 JetBrains git 插件的调研。
