---
title: UX Polish Round 1 — Phase 4（B2）：commit 详情与 diff 视图
date: 2026-09-30
status: ready
ideas:
  - .ai-workflow/ideas/20260929-vscode-ux-polish-round1.md
group: vscode-ux-polish-round1
phase: 4
tags: [git, rust, vscode-extension, ui]
---

# UX Polish Round 1 — Phase 4（B2）：commit 详情与 diff 视图

## Goal

引擎新增 `getCommitDetail{id}` 只读 RPC（完整 message、committer、parent、变更文件列表）；graph 视图单击/右键打开 commit 详情面板（复用独立 webview panel 架构），文件行内 "Open Changes" 调 `vscode.diff` 展示该 commit 与其第一 parent 的文件 diff。执行后：用户在 graph 中点任意提交即可看到详情与逐文件 diff——补上 idea 里"单击 commit 无任何反馈"的缺口，纯只读零销毁风险。

## Background

idea 将 commit 详情视图明确归 B 期。phase 2 已把 graph 右键菜单框架、webview panel 架构（`graphPanel.ts` / `webviewHtml.ts` / `headContent.ts` ready 握手）铺好，本阶段在其上做第二个只读视图。研究确认：**难点小、管道化为主**——引擎 `getBlob{revision, path}` 已支持任意 revision（`crates/engine/src/gix_read.rs:493-543`），TS 侧写死 HEAD；commit 全 message 与 diff-tree 均为新只读 RPC。

## Research Summary

- **引擎读路径**：dispatch（`crates/engine/src/server.rs:266-408`）只读 RPC 直接起 task 查询，不入写队列。`Commit` 结构（`crates/protocol/src/lib.rs:8-16`）的 `message` 只是 summary line（`commit_ref.message_summary()`，gix_read.rs:159）——详情需要全 message + committer + 文件列表。
- **getBlob 已有任意 revision 支持**：gix_read.rs:493-543 rev-parses 任意 revision 且防御性拒绝 option-like revision/path。TS 侧 `engineClient.getBlob` 已带 revision 参数（engineClient.ts:118-125），但 `headContent.ts:37-40` 硬编码 `'HEAD'`、`openResource` 的 URI query 固定 `HEAD`（:51）。
- **diff 视图业界模式**（来源 microsoft/vscode `uri.ts` + `fileSystemProvider.ts`）：自定义 scheme URI 的 query 编码 `{path, ref}` JSON（`fromGitUri` = `JSON.parse(uri.query)`）；ref `''` = 工作区文件（直接传 `file:` Uri）；内容经只读 ContentProvider 提供（`git show` 等价物）；标题 `'{basename} ({shortRef}) ↔ {basename} ({shortRef2})'`；ref 中缺失文件 → `FileSystemError.FileNotFound`。B2 用 `vscode.diff` 命令拼两个 `git-bamboo:` URI。
- **既有 learnings 约束**：新 webview 初始数据必须挂 `ready` 握手（`20260930-vscode-webview-early-postmessage-dropped.md`）；第二个消费者出现即抽取共享（`20260930-second-consumer-triggers-shared-extraction.md`）——本阶段是 webview panel 构建的第二个消费者，`webviewHtml.ts` / 消息派发的复用在此期完成抽取；graph 侧 commit id 严格 `[0-9a-f]{40,64}` 校验（graphSession.ts:34）沿用。
- **lane 颜色/ahead-behind 等开放问题不在本范围**（idea Open Questions，另有归属）。

## Steps

### 1. 引擎：`getCommitDetail` RPC（`crates/protocol/src/lib.rs`、`crates/engine/src/server.rs`、`crates/engine/src/gix_read.rs`）

- protocol 新增 `CommitDetail` 结构：`{ id, message: String（完整带换行）, author_name/email/time, committer_name/email/time, parent_ids: Vec<String>, files: Vec<CommitFile> }`；`CommitFile { path, old_path: Option<String>（rename 时）, status: String（A/D/M/R/C）, additions: u32, deletions: u32 }`。
- server.rs dispatch 新增 `"getCommitDetail"`：params `{ id: String }`，SHA 校验（regex `^[0-9a-f]{40,64}$`，host 与引擎双侧，镜像 graph 的做法）→ 只读 task 直查，**不入写队列**。
- gix_read.rs：参照 `read_blob`（:493-543）的 rev-parse 防御模式新增 `read_commit_detail`：
  - 全 message：`commit.message()` 原始 bytes（UTF-8 lossy）；
  - 文件列表 + 行数：`git diff-tree --no-commit-id --numstat --name-status -r -M <id>`（与写队列 CLI 风格一致，避免在 gix 里手写 diff 遍历）；numstat 与 name-status 按行配对解析为 `CommitFile`（R 状态的 `{old => new}` 路径用 `-z` 或逐字段解析规避空格陷阱——实现时优先 `--numstat -z` 组合并核对输出格式）。
  - root commit（无 parent）：diff-tree 用空树 OID（`4b825dc642cb6eb9a060e54bf8d69288fbee4904`）作 baseline。

### 2. TS host：`getCommitDetail` 客户端 + revision 参数化（`editors/vscode/src/engineClient.ts`、`editors/vscode/src/headContent.ts`）

- `engineClient.ts` 新增 `getCommitDetail(id: string): Promise<CommitDetail>`（镜像现有只读方法）。
- `headContent.ts` 参数化 revision（:37-40 `readHeadRevision`、:51 `openResource` query）：URI query 由固定 `'HEAD'` 改为携带 `ref` 字段（保持对既有 `HEAD` 调用方的兼容——query 解析处默认 `'HEAD'`）。`getBlob` 调用透传该 ref。**这同时是将来一切"以任意 revision 打开内容"（含 B3 checkout 预览）的地基。**

### 3. TS host：commit 详情 panel（新 `editors/vscode/src/commitDetail.ts` + `editors/vscode/webview/commitDetail.html/.js`）

- 镜像 `graphPanel.ts` / `commitView` 的生命周期管理：单例或按 id 复用的 panel（`window.createWebviewPanel('gitBamboo.commitDetail', title = 提交短 SHA，ViewColumn.Beside`）。
- **共享抽取**：`webviewHtml.ts` 的 HTML 组装、`ready` 握手、CSP/nonce 构造成为第二消费者——按 learning 在本期完成必要抽取（若纯函数可直接复用则免抽取，实现时判断，标准：不复制粘贴任何超过 ~10 行的宿主代码）。
- 数据流：panel `ready` → host 发 `commitDetail`（步骤 1 的结构体）→ webview 渲染：
  - 头部：author avatar 占位/名字、author/committer 时间（复用 webview 现有 `formatTime` 类似实现）、完整 message（保留换行，pre-wrap）；
  - 文件列表行：status 图标（沿用 phase 2 codicon 风格内联 SVG 惯例，`20260930-codicon-style-inline-svg-webview-icons.md`）+ path（rename 显示 old → new）+ `+additions −deletions`（着色）；
  - 每行 "Open Changes" 按钮（hover reveal 用 opacity + `:focus-visible` 惯例）→ postMessage `{ type: 'openChanges', path, oldPath? }`。
- host 收 `openChanges`：**入站校验**（path 经 `isValidRepoPath`；ref 用面板持有且已校验的 SHA，绝不信 webview 传来的 revision 字段——SHA 由 host 注入 DOM 前即校验）→ `vscode.executeCommand('vscode.diff', leftUri, rightUri, title)`：
  - left = `git-bamboo:` Uri（ref = 第一 parent sha，步骤 2 参数化后直接可用）、right = `git-bamboo:` Uri（ref = 本 commit sha）；
  - 新增文件 → 左侧用空内容 Uri（ContentProvider 对 ref 中缺失 path 返回空 bytes，避免 FileNotFound 破坏 diff——研究结论：VS Code 内置 git 对 empty tree 返回空 bytes、仅纯缺失才 FileNotFound）；
  - title `'{basename} ({shortSha}^) ↔ {basename} ({shortSha})'`。
- root commit：left 侧空内容。

### 4. Graph 视图接入（`editors/vscode/src/graphSession.ts`、`editors/vscode/src/extension.ts`、`editors/vscode/webview/graph.js`、`editors/vscode/package.json`）

- **单击**：graph.js 行 click → postMessage `{ type: 'openCommitDetail', id }` → graphSession 校验 SHA（graphSession.ts:34 正则）→ 打开/聚焦步骤 3 的 panel。（idea："单击 commit 无任何反馈"就此关闭；若担心单击误触，实现时可用单击选中 + 双击打开的折中，默认单击直开。）
- **右键菜单**：`gitBamboo.graphView.openCommitDetail`（title "Open Commit Details"，codicon `$(eye)`）加进 `graph/context` 的 `gitBamboo.graphMenu` 组，context target 走现有 `takeGraphContextTarget()`（graphSession 已导出 wrapper）——第二个消费者已在 phase 2 抽好 `contextTargetSlot`，直接用。
- `package.json`：command + palette `when: "false"` + `menus → view/item`（现有 graph 条目旁）。
- 详情数据 push 时机：panel 打开时 host 先 `getCommitDetail` 再随 `ready` 握手一次性推（learning：constructor-time push 会被丢弃）。

### 5. 构建与冒烟

- 引擎：`getCommitDetail` 测试——普通 commit（message 多行保留）、merge commit（parent_ids 长度 2，diff 用第一 parent）、root commit（空树 baseline）、rename 文件（R 状态 + old_path）、不存在/非法 SHA 报错。
- `npm run typecheck` / `compile` 零错误。
- F5 冒烟（留用户）：
  1. graph 单击任意提交 → 详情 panel 打开在侧栏旁，完整 message 含换行、author/committer 时间正确；
  2. 文件行 Open Changes → diff 视图标题 `basename (sha^) ↔ basename (sha)`，内容正确；新增文件左侧空；重命名行显示 old → new；
  3. root commit（仓库首个提交）→ diff 左侧空、不报 FileNotFound；
  4. 两种主题下 SVG 状态图标与 +/− 着色正常；
  5. panel 复用：连点不同提交，panel 内容切换而非叠开。

## Acceptance Criteria

- [ ] 引擎 `getCommitDetail` 测试全绿（多行 message、merge、root、rename、非法 SHA）
- [ ] `tsc --noEmit` 与 `npm run compile` 零错误
- [ ] graph 单击提交打开详情 panel（ready 握手后一次性数据推送）
- [ ] 文件行 Open Changes 产生正确 diff（含 root/add/rename 边界、无 FileNotFound 报错）
- [ ] ref 注入 diff Uri 前经 host 校验，webview 无法伪造 revision（入站只传 path，SHA 由 host 持有）
- [ ] webview panel 宿主代码无 >10 行复制粘贴（共享抽取完成）

## Dependencies

- 实施约定：B2 在新分支 `implement/vscode-ux-polish-round1-phase-4` 上开发（与 B1 分支并行，均自 `main` 切出），完成并验收后各自合并回 `main`（用户约定，2026-09-30）。若 B1/B2 有先后合并顺序，先合并者直接 fast-forward，后者做普通 merge（勿 rebase 已合并分支）。
- Phase 1/2 已合并 main（`contextTargetSlot`、graph 右键菜单框架、webview panel 架构）。
- 与 Phase 3（B1）无相互依赖，可并行实施；`headContent` 参数化若先于 B1 落地不影响 B1。
- 引擎侧纯新增只读 RPC，零既有行为变更。

## Related Documents

- .ai-workflow/ideas/20260929-vscode-ux-polish-round1.md
- .ai-workflow/plans/20260929-vscode-ux-polish-round1-phase-1.md（同组 Phase 1）
- .ai-workflow/plans/20260929-vscode-ux-polish-round1-phase-2.md（同组 Phase 2）
- .ai-workflow/plans/20260930-vscode-ux-polish-round1-phase-3.md（同组 Phase 3 = B1：discard RPC，无依赖可并行）
- .ai-workflow/learnings/20260930-codicon-style-inline-svg-webview-icons.md
- .ai-workflow/learnings/20260930-hover-reveal-buttons-need-opacity-not-display-none.md
- .ai-workflow/learnings/20260930-vscode-webview-early-postmessage-dropped.md
- .ai-workflow/learnings/20260930-second-consumer-triggers-shared-extraction.md
- .ai-workflow/learnings/20260930-webview-host-two-way-trust-boundary.md
