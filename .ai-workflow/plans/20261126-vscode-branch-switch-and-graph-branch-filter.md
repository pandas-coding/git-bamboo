---
title: VSCode 分支切换增强、commit 面板图标语义化、graph 分支筛选
date: 2026-11-26
status: in-progress
tags: [git, rust, vscode-extension, ui]
---

# VSCode 分支切换增强、commit 面板图标语义化、graph 分支筛选

## Goal

三件用户提出的 UX 缺口：

1. **分支切换**：状态栏分支名可点击、graph 工具栏分支 pill 可点击，都打开同一个增强选择器（本地/远程分组、当前分支标记、"新建分支…"入口）；选远程分支时检出为本地跟踪分支；graph 提交行右键对带分支 tip 的行提供 "Switch to This Branch"。
2. **图标语义化**：`commitAmend` 由 `$(edit)`（笔）改为 `$(replace)`；`discardAllUntracked` 由与 `discardAllChanges` 相同的 `$(discard)` 改为 `$(trash)`（删除未跟踪文件 vs 回退已跟踪改动）。
3. **graph 分支筛选**：`GraphFilter` 新增 `refs`（多选分支），引擎按选中 refs 收窄 rev walk 的 tips；filter 栏新增分支多选下拉 + 可移除 chips。

## Steps

### 1. 引擎：`GraphFilter.refs`（protocol + gix_read）

- `GraphFilter` 增 `refs: Option<Vec<String>>`（`skip_serializing_if`，旧客户端不受影响）；`is_empty`/`normalized`（trim、去空、去重）/`validate_graph_filter`（≤64 项、每项 ≤256 字符、无控制字符、不以 `-` 开头）同步。
- `read_graph_page_with_limits`：`refs` 非空时 tips 由这些 ref 解析（`rev_parse_single` + peel，解析不到的跳过；全部解析不到 → 空页），否则维持 `collect_tips`。
- 路由：`refs` 是**范围**不是谓词——只有 `refs` 时走原来的窗口化 unfiltered 读（O(limit)）；有 author/message/path 时才走 `graph_filter::filtered_page`（memo key 含 `refs`，tips 生成键含收窄后的 tips）。

### 2. 引擎：`switchBranch` 支持远程分支

`switch_branch` 先用 gix 判定目标：本地分支存在 → `git switch <name>`；名字形如 `<remote>/<branch>` 且 `refs/remotes/<name>` 存在 → 本地同名分支存在就切它，否则 `git switch -c <short> --track <name>`。auto-stash 流程不变。

### 3. TS 宿主

- `types.ts` / `graphSession.ts`：`GraphFilter.refs` 的规范化、比较、webview 入站解析（数组 of string，长度上限）。
- `commands.ts`：增强 `switchBranch` 选择器（分组、当前分支、新建入口、远程分支）；导出 `switchToBranch(client, name)` 供复用。
- `extension.ts`：状态栏 `branchItem.command`；graph 工具栏 `switchBranch` 消息；`gitBamboo.graph.switchBranchAt` 右键命令；palette filter 增 "Branches…" 多选。
- `package.json`：图标替换 + 新命令/菜单项（`branchTip == yes` when 键）。

### 4. Webview（graph）

- 工具栏分支 pill（`head` 消息的 branch）→ `switchBranch` 消息。
- filter 栏分支多选下拉（搜索 + 复选框，本地/远程分组），选中项渲染成可移除 chips；`refs` 参与 `sameFilter`/`normalizeSent`/`parseFilterState`。
- 仅 `refs` 时命中数是下界，显示 "≥ N"。
- 行右键：`data-vscode-context` 带 `branchTip: 'yes'|'no'`，`contextTarget` 携带该行分支名（host 侧校验）。

## Acceptance Criteria

- [x] `cargo test` 全绿（142 engine + 29 protocol；含 refs 范围、refs+谓词组合与分页、refs-only 走窗口化路径、未知 ref、远程分支检出为跟踪分支）
- [x] `npm run typecheck` 与 `npm run compile` 零错误
- [x] JSON-RPC 冒烟（真实引擎进程 + 临时仓库）：`refs:["topic"]` / 多分支并集 / 未知 ref 空页 / refs+message 组合 / `--all` 返回 -32005 / 空白 refs 等价于不过滤
- [ ] F5 手动验收（留用户）：
  1. 状态栏分支名、graph 工具栏分支 pill 均打开选择器；搜索、当前分支标记、分组正确；
  2. 选远程分支 → 检出为本地跟踪分支（`git status` 显示 tracking）；已有同名本地分支时切本地；
  3. "Create new branch…" → 建完自动切过去；
  4. 带分支 pill 的提交行右键出现 "Switch to This Branch"，无分支的行不出现；多分支同名 tip 时先选分支；
  5. filter 栏分支下拉：搜索、勾选多个分支 → 图只剩这些分支可达提交，chips 可单个移除，"All branches" 清空；与 message/author/path 条件叠加；
  6. 仅选分支时命中数显示 "≥ N"；分支被外部删除后 chip 自动消失；
  7. Commit 面板标题栏三个图标（Amend=$(replace)、Discard All Changes=$(discard)、Discard All Untracked=$(trash)）在明暗主题下均可区分。