---
title: 网络操作增强 Phase 3：最小冲突流（冲突栏 + Accept + VS Code 合并编辑器）
date: 2026-10-26
status: in-progress
ideas:
  - .ai-workflow/ideas/20261025-network-ops-enhancements.md
group: network-ops-enhancements
phase: 3
tags: [git, vscode-extension, ui]
---

# 网络操作增强 Phase 3：最小冲突流（冲突栏 + Accept + VS Code 合并编辑器）

## Goal

pull/stash pop 出现冲突后，用户在 git-bamboo 内完成解决：提交视图新增独立"冲突"栏（列出冲突文件 + Accept Yours/Theirs + 打开三方合并编辑器入口），复杂冲突借用 VS Code 内置三栏合并编辑器（自研部分仅冲突栏，不自建合并编辑器）。至此网络操作闭环恢复不再是半成品。

## Background

见 idea 文件。用户决策（2026-10-25）：冲突流与网络操作同轮交付；载体采用**提交视图内嵌冲突栏**（不另开独立面板/Tree View）；不做 "Resolve simple" 式自动消解，只做 Accept Yours/Theirs + 手动三方编辑；stash pop 冲突与 pull 冲突共用本流程。姊妹 idea（rebase）后续复用本冲突流。

## Research Summary

- 冲突识别已存在：`crates/engine/src/gix_read.rs` `read_status` L303 起、L405 把 gix `Conflict` 状态映射为 `StatusCode::Conflict`；`write_queue.rs` `classify_discard_paths` L475-540 的 `CONFLICT_CODES` L509（DD/AU/UD/UA/DU/AA/UU）已拒绝 discard 冲突文件。缺：StatusItem 未区分冲突种类（both-modified vs delete/modify），前端无法判断哪些 Accept 动作合法。
- 提交视图桶结构：webview `editors/vscode/webview/commit.js` L23 `BUCKETS = ['staged','changes','untracked']`、L68 `bucketOf`；host 镜像 `editors/vscode/src/commitView.ts` L25 `CONTEXT_BUCKETS`、L47 `bucketOf`、右键目标 `takeContextTarget` L289（桶交叉校验）。
- 冲突文件当前落进 "changes" 桶（`bucketOf` L47-50 无冲突分支）——本阶段为其立独立桶。
- 三方内容访问：`engineClient.getBlob(revision, path)` 支持任意 revision（engineClient.ts L135）；`headContent.ts` 已是 `TextDocumentContentProvider`（ENGINE_SCHEME，L28 注册，L60 revision 校验）——冲突 stage `:1`/`:2`/`:3` 可直接复用此管线，需验证 getBlob 对 `:N` 形态的支持。
- 外部研究（VS Code git 扩展源码）：打开合并编辑器 = 构造 base/ours/theirs URI（stage `:1/:2/:3`）+ 调私有命令 `_open.mergeEditor`（`{ base, input1, input2, output }`，output = 工作区文件 URI，结果原地写回；input 各带 `{ uri, title, detail }`，title "Current"/"Incoming"）；完成检测 = 活动 tab 是 `TabInputTextMerge` → `mergeEditor.acceptMerge` → 保存 → `git add`。**rebase 期间 ours/theirs 与 input 顺序反转** —— 本期 pull 用 merge 语义不受影响，但代码结构需预留 rebase 分支（姊妹 idea 复用）。`git checkout --ours/--theirs` 仅适用 both-modified；rename/delete 类冲突需"保留/删除 + git add/git rm"抉择；二进制文件不能带冲突标记，需显式取某一方。
- 学习教训：webview→host 每条消息 host 侧验证（类型/枚举/路径合法才触发 RPC）；消费时校验目标（刷新可能使冲突列表变化）；empty-string sentinel 问题不引入。

## Steps

1. **StatusItem 冲突种类** — `crates/protocol/src/lib.rs` `StatusItem`（L37-47 区域）扩展：冲突条目携带冲突种类（both-modified / both-added / delete-modify（含方向）/ rename 类），来源为 `gix_read.rs` read_status 的 gix conflict 状态细化（L405 附近）。响应保持向后兼容（非冲突条目字段缺省）。

2. **resolveConflict 引擎 RPC** — `protocol/src/lib.rs` + `server.rs` dispatch + `write_queue.rs` `WriteCommand` 新增 `ResolveConflict { path, resolution }`，resolution 枚举：
   - `ours` → `git checkout --ours -- <path>` + `git add --`；
   - `theirs` → `git checkout --theirs -- <path>` + `git add --`；
   - `keepOurs` / `keepTheirs`（delete-modify 类）：保留工作区版本（或不删除）+ `git add`/`git rm` 相应执行。
   引擎执行前用与 `classify_discard_paths`（L475-540）相同的"执行时新鲜 `git status --porcelain` 分类"确认该 path 当前确为对应冲突种类，种类不匹配返回 `INVALID_ARGUMENT`（不信任客户端快照）。both-modified 以外种类请求 `ours/theirs` 时返回明确错误。**undo 决策：bypass journal**（快照不含工作区内容，恢复 index 反而造成 index/工作区不一致；理由写入代码注释，模式同 discard）。

3. **冲突栏（webview）** — `commit.html`/`commit.js`：`BUCKETS` L23 扩为 `['conflicts','staged','changes','untracked']`（冲突栏置顶，标题如 "Merge Conflicts" + 计数）；`bucketOf` L68 冲突条目（status conflict）优先归 `conflicts`。行内动作：每文件 [Accept Yours][Accept Theirs][Resolve in Merge Editor]；delete/modify 类只显示对应的 keep 动作。右键菜单按学习教训用 `webviewSection` 正则 when 子句（`/^file-conflicts$/`）扩展，Accept 动作菜单化。图标用 16×16 内联 SVG、stroke=currentColor。

4. **冲突栏（host 镜像）** — `commitView.ts`：`CONTEXT_BUCKETS` L25 加 `conflicts`；`bucketOf` L47 加冲突分支（与 commit.js 镜像同步，两处必须同规则）；新增消息处理：`resolveConflict { path, resolution }`、`openMergeEditor { path }` — host 侧验证：path 为合法 repo 相对路径（复用 L211 既有校验）、resolution 枚举合法；消费时新鲜 status 交叉校验（复用 L286-293 takeContextTarget 桶校验模式：目标仍是 conflicts 桶才执行）。执行 resolveConflict 后 `commitView.refresh()`（indexChanged/worktreeChanged 通知已有管线）。

5. **合并编辑器接入** — 新建 `editors/vscode/src/mergeEditor.ts`（或扩展 headContent.ts）：
   - 验证并复用 `getBlob(':1'|':2'|':3', path)` 供三方内容（getBlob 不支持 `:N` 时在引擎侧扩展 getBlob 的 revision 解析，禁止 host shell out）。
   - `openMergeEditor(path)`：构造 base=`:1`、ours=`:2`、theirs=`:3` 的 ENGINE_SCHEME URI + 工作区文件 URI 作为 output，调 `vscode.commands.executeCommand('_open.mergeEditor', { base, input1: {uri, title:'Current'}, input2: {uri, title:'Incoming'}, output })`。代码结构预留 rebase 时的 input 顺序反转分支（本期不启用）。
   - 完成检测：注册 `window.tabGroups` 监听，活动 tab 为 `TabInputTextMerge` 且用户执行 accept 后保存 → 对 output 文件调既有 stage RPC（`changePaths` 管线，commitView.ts L187）。
   - 二进制冲突文件：不打开合并编辑器，直接提示二选一（Accept Yours/Theirs）。

6. **网络流接入** — Phase 2 的 [Open Conflict Files] 按钮：聚焦提交视图 + 展开冲突栏（netOps.ts 冲突错误分支发 webview 消息）；-32008 stash 冲突（Phase 5 消费）同样路由。冲突栏存在期间，Commit 按钮置灰或提示先解决冲突（防止带冲突标记提交 —— 现状 engine 侧 commit 对冲突文件的行为需在实现时确认并补拒绝逻辑）。

7. **提交防护** — `write_queue.rs` commit 路径：执行前 `git status --porcelain` 若存在未解决冲突条目则拒绝（返回明确错误），不依赖前端拦截。

8. **测试** — 引擎：resolveConflict ours/theirs/keep 的端到端（构造冲突 → resolve → status 无冲突 → 可提交）、种类不匹配拒绝、bypass undo 注释存在性（grep 验收）；扩展：typecheck + 手工冒烟（构造 pull 冲突 → 冲突栏出现 → Accept 一半 + 合并编辑器解决另一半 → commit 通过）。

## Acceptance Criteria

- [ ] 构造 both-modified 冲突后，提交视图出现置顶冲突栏，文件不再混入 changes。
- [ ] Accept Yours/Theirs 一键解决单文件（status 转为 staged）。
- [ ] [Resolve in Merge Editor] 打开 VS Code 三栏合并编辑器，accept + 保存后文件自动 staged。
- [ ] delete/modify 冲突只提供 keep 动作；对它请求 ours/theirs 被引擎拒绝。
- [ ] pull 冲突 → [Open Conflict Files] 聚焦冲突栏；不自动 abort。
- [ ] 存在未解决冲突时 commit 被引擎拒绝。
- [ ] 每条新增 webview→host 消息在 host 有类型/枚举/路径验证；右键目标消费时新鲜校验。
- [ ] `cargo test` 与 tsc 通过。

## Dependencies

- Phase 1（stash pop 冲突错误码 -32008、WriteCommand 扩展模式）。
- Phase 2（[Open Conflict Files] 按钮与 netOps.ts 错误分支接线）。
- 冲突栏依赖现有提交视图结构（20260930 phase-3/4 已交付）。

## Related Documents

- .ai-workflow/ideas/20261025-network-ops-enhancements.md
- .ai-workflow/plans/20261025-network-ops-enhancements-phase-1.md
- .ai-workflow/plans/20261025-network-ops-enhancements-phase-2.md
- .ai-workflow/ideas/20261025-rebase-and-history-insights.md（后续 rebase 冲突复用本流）
