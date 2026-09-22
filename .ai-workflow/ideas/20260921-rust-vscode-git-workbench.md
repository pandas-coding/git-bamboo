---
title: Rust Git Workbench for VS Code（git4idea 级交互 + LLM agent）
date: 2026-09-21
status: planning
tags: [git, rust, vscode-extension, llm-agent, developer-tools]
---

# Rust Git Workbench for VS Code（git4idea 级交互 + LLM agent）

## Problem

VS Code 内置 git 扩展交互差、功能受限，显示大量 commit 时页面卡死（git4idea 也卡但更轻）。用户想要在 VS Code 中获得 JetBrains git 插件级别的交互体验，并且把 LLM 作为对话式 git 操作的一等公民。目标用户是项目作者本人（发布到 Marketplace 以便多台电脑使用），同时面向公开用户。

## Core Idea

一个 UI 无关的高性能 Git 引擎（Rust、独立常驻进程、自有协议），上接轻薄的编辑器前端。VS Code 前端先落地 git4idea 级日常交互（高性能虚拟化 commit graph 等），未来 Zed 作为第二个前端接入。LLM agent 直接作为引擎的客户端执行对话式 git 操作，与前端共享同一状态源。

## Key Insights

- "大量 commit 卡断"的瓶颈是渲染层（webview 一次性塞 DOM），不是 git 本身——高性能必须拆成两半：引擎性能（Rust：graph/diff/status 计算）+ 数据传输与渲染性能（虚拟滚动、增量拉取、只传视口内数据）。只做前者用户感知不到快。
- 引擎独立进程 + 协议的架构同时满足三个目标：常驻缓存（性能）、Zed 兼容（前端只是另一个客户端）、LLM agent 与前端状态一致（agent 是引擎的客户端，不存在同步地狱）。
- LLM 出错回滚 = 引擎事务快照 + undo，而不是依赖 reflog。reflog 只记 ref 移动、粒度散（一次 rebase 几十条记录）、不覆盖工作区/index 丢失、会被 gc。混合方案：引擎做操作级一键 undo，reflog 兜底外部操作。推论：引擎应尽量是操作的唯一入口（或能检测外部变更）。
- LLM agent 的安全交互模式：计划 → 确认 → 执行，执行前自动挂 undo 点。写操作 agent 依赖 undo 引擎成熟，所以分层进场（T1 只有 commit message 生成等轻量 LLM 功能）。
- 性能目标（可验证）：100k+ commit graph 首屏 < 1s、滚动 60fps；5 万文件 monorepo status < 100ms（fs 事件驱动增量）；UI 操作感知响应 < 50ms。总体标准：明确超过 VS Code 内置 git。
- 平台：Windows/Linux/macOS；发布到 Marketplace；不考虑古老 git 版本。

## Open Questions

- 引擎的 git 实现选型：git CLI 包装（并发管道，git4idea 路线）vs libgit2/git2-rs vs gitoxide，还是混合？——留待研究阶段，需要对比性能、功能覆盖（worktree/引用事务）、维护成本。
- 引擎与前端的协议形态：JSON-RPC over stdio？LSP 风格？消息粒度与订阅模型（增量推送怎么设计）？
- VS Code 扩展打包 Rust 二进制的分发方案（npm 携带预编译多平台二进制 vs 首次下载）。
- Commit graph 渲染技术选型：webview 内 canvas vs 虚拟化 DOM；diff 渲染的虚拟化。
- LLM 集成的模型/服务商抽象（BYO-key？本地模型？），agent 权限分级。
- 与 VS Code 内置 git 扩展的共存/禁用关系。
- Zed 前端的真实可行性（Zed 扩展 UI 能力很弱，可能只能做最小前端）。

## Possible Directions

（功能分级为共识方向，工程方案留待 pan-plan / 研究阶段）

**T1 — 日常核心闭环（MVP）**：stage/unstage（文件/hunk/行）、amend、LLM commit message、毫秒级 status、虚拟滚动 commit graph + 详情面板、分支 CRUD + fuzzy 快速切换、fetch/pull/push、auto-stash、**undo 引擎（事务快照，LLM 安全地基）**。

**T2 — 多分支日常**：交互式 rebase（reorder/squash/drop + 三栏冲突解决）、cherry-pick、merge 冲突流、stash 管理、worktree 管理、多分支对比（ahead/behind）。

**T3 — LLM agent（核心卖点，分层进场）**：先只读（自然语言查 log、解释 commit、生成 rebase 计划），后 agent 写操作（计划 → 确认 → 执行 + 事务回滚）、复杂分支整理、批量 cherry-pick。

## Related Documents

- .ai-workflow/research/20260921-git-workbench-research.md —— 工程选型研究报告（git 引擎选型 / VS Code 前端 / 协议+Zed+LLM；含深度审查补充与范围决策）。**规划时请以此报告为研究结论来源，其中的“范围决策（已定）”表不可推翻，PoC 清单应纳入计划验证步骤。**
- .ai-workflow/plans/20260921-t1-core-engine.md —— Phase 1 实施计划：Rust daemon 核心 + VS Code 扩展骨架 + status/graph/undo 基础（T1 闭环）
