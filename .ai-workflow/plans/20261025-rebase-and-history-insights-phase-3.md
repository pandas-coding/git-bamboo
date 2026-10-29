---
title: Rebase 与历史洞察 Phase 3：JetBrains 式 blame（引擎 getBlame + 标注/悬停/导航）
date: 2026-10-30
status: ready
ideas:
  - .ai-workflow/ideas/20261025-rebase-and-history-insights.md
group: rebase-and-history-insights
phase: 3
tags: [git, rust, vscode-extension, ui]
---

# Rebase 与历史洞察 Phase 3：JetBrains 式 blame（引擎 getBlame + 标注/悬停/导航）

## Goal

编辑器内三层 blame 闭环：**gutter 标注**（作者/时间的行级 decoration，`before` attachment）+ **悬停详情**（该行所属 commit 完整信息 + diff 入口）+ **行级导航**（CodeLens 动作：跳到引入该行的 commit、annotate previous revision 链式回溯）。引擎新增 `getBlame` 读 RPC（gix-blame + commit-graph，`git blame --porcelain` 回退）。热力图与文件历史视图不做（idea 定案：后补）。

## Background

见 idea 文件。2026-10-29 定案：纯标注走 decoration、行级操作走 CodeLens（`20261029-codelens-hunk-actions.md` 实测 CodeLens 优于 gutter decoration，但那是"操作"维度；标注文字仍只能 decoration 承载——VS Code API 的 `gutterIconPath` 只支持图片）。blame 范围锁最小闭环。

## Research Summary

- **gix-blame 现状**（gix 0.90 / gix-blame 0.20，2026-10 验证）：`Repository::blame_file(path, suspect, options)`，gated 于 `blame` feature（`gix-blame` 目前仅是传递依赖，需显式启用/直接依赖）。`Outcome.entries: Vec<BlameEntry>`（`start_in_blamed_file: u32` **0-based 行号**、`len`、`commit_id`、`source_file_name` 改名时设置）、`blob: Vec<u8>`（行切分是调用方责任）。
  - **必须传 commit-graph**：`gix_blame::file` 的 `cache: Option<Graph>` 参数——Chromium 实测 14.9s → 290ms（无 bloom-filter 增益，未合并，最坏文件仍慢于 git blame）。
  - `Start::Contents { first_suspect, contents }` 可 blame 未提交的工作区内容（脏缓冲）。
  - **陷阱**：`Options.ranges` 非法输入（空/逆序/重叠）会 panic（PR #2981 未合）——本计划**整文件 blame 不用 ranges**，规避该面；无 `--ignore-rev`；无 incremental blame。
- **回退路径**：gix blame 出错（大/病态文件、API 边界）→ 引擎 shell `git blame --porcelain -- <path>` 解析（display 路径允许 lossy，边界明确标注）。
- **VS Code 装饰 API**：gutter 文字无官方 API；GitLens 用 `DecorationRenderOptions.before` attachment + **负右边距**（`margin: '0 26px -1px 0'`、按格式计算 width）把标注推到行号左侧——已知 CSS hack，按格式串计算宽度缓解。hover 有两机制：decoration `hoverMessage`（悬停行文本才触发）与 `registerHoverProvider`（富 Markdown，JetBrains 式推荐）。
- **性能策略（GitLens 配方）**：viewport 切片渲染（行数 > 1000 且唯一 commit > 500 时按 visibleRanges 切片 re-setDecorations）、150ms trailing debounce、`(path, rev)` 键缓存 + WorktreeChanged/HeadChanged 失效、per-commit renderOptions 缓存避免重复分配样式、唯一样式数控制。
- **CodeLens 学习迁移**（`20261029-codelens-hunk-actions.md`）：状态无跟踪派生（lens 目标从当前打开文档 + 引擎数据推导，不记 open-intent）、3s TTL 缓存 + 动作/通知即失效、document selector 按 scheme 过滤注册、palette 隐藏命令（`"when": false`）。
- **引擎 blob 读已存在**：`getBlob`（server.rs:385）+ `headContent.ts` 的 `ENGINE_SCHEME` 文档 provider（`HEAD`/sha/`:N` stage）——"打开文件在历史版本"的载体现成；commitDetail 面板已能按 sha 展示 commit 详情（跳转引入 commit 的落点现成）。
- **annotate previous 语义**（JetBrains）：打开文件在 blame ref 的父版本（或该行引入 commit 的父版本）并重新标注，保持光标行。gix 无内建；实现 = 对 `ref^` 重跑 blame + 行号经 diff 映射或简单保持 + clamp（GitLens 即保持光标行）。

## Steps

1. **引擎 `getBlame` RPC** — `crates/protocol/src/lib.rs` 新增：
   - 请求 `{ path: String, rev: Option<String>, contents: Option<String> }`：`rev` 缺省 = HEAD；`contents` 提供时为脏缓冲 blame（`Start::Contents { first_suspect: <rev 解析>, contents }`）。
   - 响应 `{ rev, entries: [{ start_line: u32, len: u32, commit: { id, author, author_time, committer_time, summary } }], via: "gix" | "git-porcelain" }`——引擎侧 join commit 元数据（走既有 redb commit 缓存或 gix 批量读），客户端一次往返拿全渲染数据，**禁止逐 entry 回查**。
   - `path` 过 `validate_path_arg`；`rev` 过 ref 校验；`contents` 大小上限（如 16 MiB，超限返回 `INVALID_ARGUMENT` 提示先保存/太大）。
2. **gix 实现与回退** — `crates/engine/src/gix_read.rs`：启用 gix `blame` feature（Cargo.toml 直接依赖 `gix-blame` 与版本对齐）；`repo.blame_file` 或 `gix_blame::file`（**显式传 `repo.commit_graph_if_enabled()` 的 Graph**）；行切分 `blob`（`split_inclusive('\n')`）校验 entry 覆盖总行数；gix 错误 → 回退 `git blame --porcelain -- <path>`（--contents 从 stdin 传脏缓冲）解析 porcelain 输出。session-gated + epoch 一致性照旧，`server.rs:378` 注册读路径。**代码注释写明**：display 路径 lossy 可接受、写路径（无）不受影响——边界显式。
3. **引擎测试** — `engine_tests.rs`：fixture 仓库多 commit 多行改动 → 行归属断言（某行属于引入它的 commit）；rename 文件 blame（`source_file_name` 路径）；脏缓冲 blame（`contents` 传入未提交修改）；回退路径（强制 gix 失败的注入开关或 feature-flag 测试）porcelain 解析等价性；`via` 字段正确。
4. **TS 装饰层** — `editors/vscode/src/blame.ts`：
   - `TextEditorDecorationType`（`before` attachment：contentText = `作者 简短时间`、负右边距 + 按格式计算宽度、per-commit renderOptions 缓存、颜色用 `gitDecoration*` 主题变量语义取 foreground 弱化色）；行 Range 从 `start_line(0-based)+1` 构造。
   - **开关**：`gitBamboo.blame.toggle` 命令（editor/title menu + palette 可见），per-editor 开关态；默认关闭。
   - 触发：toggle 开 → 活动编辑器 blame 渲染；文档切换/滚动 → 150ms debounce 重渲染；行数 > 1000 时 viewport 切片（visibleRanges 切 + 滚动时 re-setDecorations）。
   - 缓存：`(path, rev)` 键、3s TTL；失效钩子接 `WorktreeChanged`/`HeadChanged`（既有 engineClient 通知）。scheme 过滤：仅 `file` 与 `git-bamboo`（引擎 scheme 历史版本文档可标注——annotate previous 的载体）。
5. **悬停详情** — `registerHoverProvider`（file + git-bamboo scheme）：当前行命中 blame entry → Markdown（trusted）：作者/时间（绝对+相对）、sha、完整 subject、命令链接 `gitBamboo.blame.showCommit`（打开 commitDetail 面板该 sha）与 `gitBamboo.blame.diffLine`（该行与上一版本的 diff——落点：打开该 commit 的 diff 视图，复用既有 commit detail diff 入口，一期不做到行级精确滚动，subject 级即可）。
6. **CodeLens 行级动作** — `registerCodeLensProvider`（同 scheme 集）：**无状态派生**——lens 只在 blame 开启的编辑器、当前光标行（`onDidChangeTextEditorSelection` 驱动重算，不跟踪其他 intent）渲染两条：`跳转引入 commit`（→ commitDetail 打开 entry.commit.id）与 `回溯上一版本`（→ 第 7 步）。palette 命令 `gitBamboo.blame.jumpToCommit` / `gitBamboo.blame.annotatePrevious` 注册为 `"when": false` 隐藏。实现文件头注释记录 decoration-vs-CodeLens 分层决策（learning 要求）。
7. **annotate previous revision 回溯链** — 命令 `gitBamboo.blame.annotatePrevious { uri, line }`：解析当前文档 rev（file scheme → HEAD；git-bamboo scheme → 文档 rev）→ 取**当前行 blame entry 的引入 commit I** 的父 `I^`（行级语义，JetBrains 同款：每步回到引入该行的链上一环）→ 打开 `git-bamboo` scheme 文档（`headContent.ts` 既有 provider，`I^:<path>`）于保持的行号（clamp 到文件行数，高亮该行）→ 该文档自动进入 blame 开启态（scheme 过滤已覆盖）。链式：在新文档上再次执行即继续回溯。行号映射一期用保持+clamp（GitLens 同款简化），diff 精确映射列为后续增强（非本 phase 验收项）。
8. **package.json 注册** — `gitBamboo.blame.toggle/showCommit/diffLine/jumpToCommit/annotatePrevious` 命令 + `editor/title` menu（toggle 图标按钮）+ palette（toggle 可见，其余 `when` 隐藏）。

## Acceptance Criteria

- [ ] `cargo test -p bamboo-engine` 全绿（含 blame 归属/rename/脏缓冲/回退测试）；`cargo clippy` 无新告警。
- [ ] `getBlame` 响应含每 entry 的 commit 元数据（author/time/summary），客户端单次往返可渲染（无逐行回查）。
- [ ] gix 路径显式传 commit-graph（代码断言/注释可见）；`via` 字段区分实现路径。
- [ ] **手测**：toggle blame → gutter 出现作者/时间标注；颜色弱化不干扰代码阅读。
- [ ] **手测**：悬停行 → 该行 commit 详情 + 跳 commit 面板/diff 入口可用。
- [ ] **手测**：CodeLens 当前行"跳转引入 commit" → commitDetail 打开正确 commit。
- [ ] **手测**：annotate previous 连续回溯 3 层 → 每层打开父版本只读文档并自动标注，行号合理保持。
- [ ] **手测**：脏缓冲（未保存修改）blame 不报错（contents 路径）；保存后重渲染。
- [ ] **手测**：大文件（>1000 行、多变更）滚动流畅（viewport 切片生效，无明显卡顿；50ms 级交互）。
- [ ] engine 通知（切换分支/外部 commit）后 blame 缓存失效重渲染（TTL + 通知双通道）。

## Dependencies

无（与 phase-1/2 无代码耦合，可并行；建议排在 rebase 两阶段之后交付以先落大件）。

## Related Documents

- .ai-workflow/ideas/20261025-rebase-and-history-insights.md
- .ai-workflow/learnings/20261029-codelens-hunk-actions.md（CodeLens/decoration 分层依据）
- 同组：phase-1/2（rebase）、phase-4（graph 过滤）
