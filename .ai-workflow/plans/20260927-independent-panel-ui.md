---
title: Independent Panel UI — Git Bamboo 从 SCM 面板独立为 GitLens 式自有面板（统一 Commit 视图）
date: 2026-09-27
status: in-progress
ideas:
  - .ai-workflow/ideas/20260921-rust-vscode-git-workbench.md
group: git-workbench
phase: 2
tags: [git, vscode-extension, ui]
---

# Independent Panel UI — Git Bamboo 从 SCM 面板独立（方案 3：统一 Commit Webview）

## Goal

Git Bamboo 的全部 UI 从 VS Code 内置 SCM 面板迁移到自有 Activity Bar 容器（`gitBamboo`），完全移除 `vscode.scm` API 依赖。完成后：SCM 面板完全归还给内置 git 扩展（不再混排）；Git Bamboo 容器内含两个视图——**Commit**（新 webview：commit 输入 + 文件变更列表 + 提交按钮一体化，JetBrains/GitStudio 式）与 **Commit Graph**（现有 webview 移容器），且 **Commit Graph 支持双模式**：侧栏紧凑视图 + `openGraph` 打开的编辑器区全宽面板（JetBrains tool window 式，GitLens/Git Graph 插件同款模式）。内置 git 扩展保持启用、共存不动。

## Background

用户反馈：当前插件作为 SCM Provider 与内置 git 混排在同一面板，难区分、使用不便。见 [idea file](../ideas/20260921-rust-vscode-git-workbench.md)。

研究结论（2026-09-27 三路研究，含 GitLens 源码验证）：

- **GitLens 完全不使用 SCM API**（~10M 用户验证的架构）：全部 UI 用 TreeView/WebviewView 构建，且新版持续把视图从 TreeView 迁往 webview——webview 路线是行业验证过的演进方向。
- **`vscode.diff` 不依赖 SCM API**：接受任意两个 URI。现有 `ENGINE_SCHEME` TextDocumentContentProvider + 引擎 `getBlob` 方案可原样保留。
- **`SourceControlInputBox` 是 SCM API 独占**——必须自建 commit 输入；与其做独立小输入视图（方案 2/4），不如与文件列表合并成一体（JetBrains commit 面板 / GitStudio commit 视图形态）。
- **原研究报告选择 SCM 集成的理由均不与独立面板冲突**：内置 git 不禁用；差异化锚点在引擎性能 + LLM，不在视图位置。

已定决策（用户确认 2026-09-27）：①完全移除 SCM Provider；②**Changes + Commit 合并为统一 Commit WebviewView（方案 3，JetBrains/GitStudio 式）**；③与内置 git 共存；④**Commit Graph 双模式**——侧栏紧凑视图 + 编辑器区 WebviewPanel 全宽视图（GitLens/Git Graph 模式），同一份 webview bundle 复用；⑤**二进制数据面（ArrayBuffer 零拷贝 + Rust 预计算 lane 几何 + ±500 预取）不在本计划**——涉及引擎协议层，另立计划（见 Related Documents 的后续指引）。**不采用** TreeView 方案（无法嵌入输入框、布局不自控、无虚拟化、hunk 级交互无演进路径）。

补充：graph 的容器形态（侧栏 vs 编辑器面板）**不影响滚动性能**——同一套 webview 技术；性能升级杠杆（二进制数据面、预取窗口）见决策⑤的后续计划。

## Research Summary

关键代码事实（实现者可据此做定向阅读，不必通读）：

- `editors/vscode/package.json`：`categories` L14（含 `"SCM Providers"` 需移除）；`commands` L21–93；`menus` L95–152（`scm/title`、`scm/resourceState`、`scm/resourceState/contextual`、`commandPalette`——前三者全删）；`views` L158–167（graph webview 挂在 `scm` 容器）；`viewsContainers` L168–177（`gitBamboo` activitybar 容器已声明但空置）。
- `editors/vscode/src/scmProvider.ts`（219 行，整文件废弃）：L37 `vscode.scm.createSourceControl`；L41–43 资源组；L50–52 `ENGINE_SCHEME` content provider（**保留**）；L105–143 状态分桶逻辑（`item.staged` → staged，`untracked` → untracked，`ignored` 过滤——**逻辑保留，迁往 webview 侧**）；L145–178 commit/amend（**保留语义，输入源改 webview**）；L180–194 批量 stage/unstage（**保留，改为 webview 消息触发**）；L196–218 `openResource`/`readHeadRevision`（**保留，签名改为路径参数**）；L58–66 debounce refresh 模式（**复用**）。
- `editors/vscode/src/extension.ts`（210 行）：L13 import；L101–102 实例化 `BambooSCMProvider`；L136–144 四处 `scm.refresh()` 引擎通知钩子（**改指向 Commit 视图**）；L90–93 graph provider 注册（**不动**）。
- `editors/vscode/src/graphWebview.ts`：`viewId = 'gitBamboo.graph'`；`resolveWebviewView`（L47–69）持有 view 生命周期；L91–168 视口请求/epoch 重试/防抖/失效逻辑；L176–200 `buildHtml()` 读 `out/webview/graph.html`、注入 CSP、重写相对资源引用——**该逻辑抽为共享工具**，且视口/失效逻辑需抽为与宿主无关的共享核心（Step 6）。
- `editors/vscode/webview/graph.js`：已具备固定行高虚拟滚动（±20 行缓冲）、Canvas 覆盖层视口裁剪、rAF 合帧、锚点保持、骨架行——双模式重构**不改此文件**（同一 bundle 两种宿主）。
- `editors/vscode/vite.webview.config.ts`：`root: webview/`、单入口 `graph.html`、输出 `out/webview/`、`assetsInlineLimit: 0`（CSP 关键）。
- `editors/vscode/webview/graph.html`：主题适配范例（`var(--vscode-font-family)`、`--vscode-list-hoverBackground` 等 CSS 变量用法可直接借鉴）。
- 命令注册模式：`extension.ts` L188–203 `registerCommand` 统一错误包装。
- 引擎侧零改动：本计划只动前端声明层与 extension host 代码，`getStatus`/`stage`/`unstage`/`commit` RPC 与通知协议（`refsChanged`/`worktreeChanged`/`indexChanged`/`headChanged`）原样使用。

## Steps

### 1. `package.json` 视图与容器重组（声明层）

修改 `editors/vscode/package.json`：

- **L14 `categories`**：改为 `["Other"]`（移除 `"SCM Providers"`）。
- **L158–167 `views`**：删除 `"scm"` 键，改为 `"gitBamboo"` 键下两个 webview 视图（顺序即面板自上而下）：
  1. `{ "id": "gitBamboo.commit", "name": "Commit", "when": "git-bamboo:active", "type": "webview" }`（新——日常提交流入口，放最上）
  2. `{ "id": "gitBamboo.graph", "name": "Commit Graph", "when": "git-bamboo:active", "type": "webview" }`（原样移入）
- **L168–177 `viewsContainers`**：容器 `gitBamboo` 已存在，`icon` 改为 `$(git-branch)`（与内置 git 的 `$(source-control)` 区分）。
- **L21–93 `commands`**：删除 `gitBamboo.stage`、`gitBamboo.unstage`、`gitBamboo.openResource` 三个声明（它们不再出现在菜单/palette——交互全部由 Commit 视图 webview 消息驱动，VS Code 不要求 webview 内部动作声明命令）；保留 `gitBamboo.commit`（语义改为"聚焦 Commit 视图"）、`gitBamboo.commitAmend`（palette 直达，空消息 = `--no-edit`）；新增 `gitBamboo.refreshChanges`（"Refresh Changes"，icon `$(refresh)`）。
- **L95–152 `menus`**：删除 `scm/title`、`scm/resourceState`、`scm/resourceState/contextual` 全部三个键；新增：
  - `"view/title"`：
    - `gitBamboo.refreshChanges`（`when: "view == gitBamboo.commit"`，`group: "navigation"`）
    - `gitBamboo.commitAmend`（`when: "view == gitBamboo.commit"`，`group: "navigation"`，icon `$(edit)`）
    - `gitBamboo.undo`（`when: "view == gitBamboo.graph"`，`group: "navigation"`，icon `$(discard)`）
    - `gitBamboo.fetch`（`when: "view == gitBamboo.commit"`，`group: "navigation"`，icon `$(sync)`）
  - `commandPalette`：删除 `stage`/`unstage`/`openResource` 条目；`gitBamboo.commit` 改为 `when: "git-bamboo:active"`（不再是 `"when": "false"`）；`commitAmend` 维持 `when: "git-bamboo:active"`。

### 2. 新建 webview 源码 `editors/vscode/webview/commit.html` + `commit.js`

Commit 的 UI（纯 HTML/CSS/JS，与 `graph.html`/`graph.js` 同构，主题全用 `--vscode-*` CSS 变量）：

**布局**（自上而下）：
1. **commit message 区**：`<textarea>`（placeholder "Commit message (Ctrl+Enter to commit)"，自动增高，`var(--vscode-input-background)` 背景）
2. **文件列表**：三节分组，复刻 `scmProvider.ts` L105–143 的分桶规则（`item.staged` → Staged；`status === 'untracked'` → Untracked；其余 → Changes；`ignored` 过滤）。每节：可折叠头（组名 + 数量徽章）；行为紧凑 JetBrains 式布局：状态字母徽章（M/A/D/R，着色区分）+ 文件名 + 目录路径（`--vscode-descriptionForeground` 弱化）+ 行内按钮（staged 节为 `[−]` unstage + diff；unstaged/untracked 节为 `[+]` stage + 打开/diff）。rename 行 tooltip 显示 old_path。
3. **底栏**：`Commit` 主按钮 + `Amend` 次按钮 + 右侧统计（"N files" / "N staged"）。

**交互**：
- `[+]`/`[−]` → `postMessage({ type: 'stage', paths } | { type: 'unstage', paths })`
- 行点击 / diff 按钮 → `{ type: 'openResource', path, status }`（untracked/added → 打开文件，否则 → HEAD diff）
- `Ctrl+Enter` 或 Commit 按钮 → `{ type: 'commit', message, amend: false }`；Amend 按钮 → `{ type: 'commit', message, amend: true }`（空 message = `--no-edit`，与引擎现有语义一致）
- 接收 host 消息 `{ type: 'status', items }` → 重分桶重渲染（保留展开/折叠状态与 textarea 内容——只替换列表 DOM）；接收 `{ type: 'committed' }` → 清空 textarea
- **虚拟滚动不在本期范围**：数百文件量级直接渲染；列表 DOM 结构按"行高固定、可后续引入窗口化"设计（与 graph.js 的行渲染模式一致，避免日后重写）
- 空状态：仓库无变更时显示 "No changes" 占位（`--vscode-descriptionForeground`）

### 3. 新建 `editors/vscode/src/commitView.ts` — host 侧 Provider

`CommitViewProvider implements vscode.WebviewViewProvider`（view id `gitBamboo.commit`）：

- `resolveWebviewView`：`enableScripts: true`；`localResourceRoots` 指向 `out/webview` 与 `webview`（同 `graphWebview.ts` L63–68 模式）；HTML 用 `buildHtml()` 生成——**把 `graphWebview.ts` L176–200 的 `buildHtml` 抽取为共享工具**（新文件 `editors/vscode/src/webviewHtml.ts`，导出 `buildWebviewHtml(context, webview, htmlFileName)`：读 `out/webview/{name}`（dev 回退 `webview/{name}`）、注入 CSP、重写资源引用；`graphWebview.ts` 改为调用它，行为不变）。
- 消息处理 `onDidReceiveMessage`：
  - `stage`/`unstage` → `client.request('stage'|'unstage', { paths })` → 成功后 `refreshStatus()`（沿用 `scmProvider.ts` L180–194 逻辑与错误文案）
  - `openResource` → 调 `headContent.ts` 的 `openResource(path, status)`（见 Step 4）
  - `commit` → `client.request('commit', { message, amend })` → 成功回发 `{ type: 'committed' }` 并 `refreshStatus()`（沿用 L145–178 语义：空消息拒绝、amend 空消息放行）
- `refreshStatus()`：50ms debounce（沿用 L58–66 模式）→ `client.request('getStatus')` → `view.webview.postMessage({ type: 'status', items })`（原始 `StatusItem[]`，分桶在 webview 侧做）。webview 隐藏时也照发（`retainContextWhenHidden: true`，注册时传入）。
- 构造函数保存 `EngineClient` + root `vscode.Uri` + `RepoState`，并暴露 `refresh()` 供 `extension.ts` 的引擎通知钩子调用。

### 4. 抽取 `editors/vscode/src/headContent.ts` — diff 基础设施

从 `scmProvider.ts` 原样迁移（该文件随后删除）：

- `ENGINE_SCHEME = 'git-bamboo'` 常量 + `registerTextDocumentContentProvider` 注册（原 L44–53）+ `readHeadRevision`（原 L210–218，经 `client.getBlob('HEAD', relPath)`）。
- `openResource(path: string, status: string)`（原 L196–208 逻辑，签名从 `SourceControlResourceState` 改为路径 + 状态字符串）：untracked/added/ignored → `vscode.open(fileUri)`；否则 → `vscode.diff(ENGINE_SCHEME-URI, fileUri, "basename (Working Tree — HEAD)")`。
- 路径工具 `resolveRepoPath`/`toRelativePath`（原 L122–130）导出供 commitView 复用。
- 导出小类 `HeadContentProvider implements vscode.Disposable`。

### 5. 命令重注册与 `extension.ts` 改造

- 修改 `editors/vscode/src/extension.ts`：
  - 删 L13 import 与 L101–102 的 `BambooSCMProvider` 实例化；删 L66–67 相关注释（"commit / stage / unstage / openResource are registered by BambooSCMProvider"）
  - 新增（全部推入 `context.subscriptions`）：`HeadContentProvider` 实例化；`CommitViewProvider` + `registerWebviewViewProvider('gitBamboo.commit', …, { webviewOptions: { retainContextWhenHidden: true } })`
  - L136–144 四处 `scm.refresh()` → `commitView.refresh()`（`refsChanged`/`worktreeChanged`/`indexChanged`/`headChanged` 全部指向 commitView 的 debounced refreshStatus）
  - `gitBamboo.commit` palette handler 改为 `vscode.commands.executeCommand('gitBamboo.commit.focus')`（带无 session 守卫，沿用 L40–47 `noSessionWarning`）
  - `gitBamboo.commitAmend` 从"读 SCM input box"改为直接 `client.request('commit', { message: '', amend: true })`（palette 场景无输入框，恒为 `--no-edit` 语义）
  - 新增 `gitBamboo.refreshChanges` → `commitView.refresh()`
  - L90–93 graph provider 注册与 `gitBamboo.openGraph`（L48–59）**不动**——view id 未变，`gitBamboo.graph.focus` 在新容器下同样有效
- 删除 `editors/vscode/src/scmProvider.ts`；确认 `grep -rn "scmProvider\|vscode\.scm" editors/vscode/src/` 无残留。

### 6. Commit Graph 双模式：共享核心 + 编辑器区 Panel

把 graph 改成 JetBrains tool window 式的双入口，同一份 webview bundle 两种宿主：

- **抽取 `editors/vscode/src/graphSession.ts`（新）**：从 `graphWebview.ts` 抽出与宿主无关的核心——视口请求防抖、requestSeq 陈旧丢弃、EPOCH_MISMATCH 单次重试、invalidate 合并、`sendRefs`、引擎通知订阅（`graphInvalidated`/`refsChanged`）、`onDidReceiveMessage` 分发。构造参数：`(client, state, webview: vscode.Webview, onDispose: () => void)`——侧栏 view 与编辑器 panel 都能接。原 `GraphWebviewProvider` 瘦身为侧栏宿主：`resolveWebviewView` 里 new 一个 `GraphSession`，HTML 生成改用 Step 3 的 `webviewHtml.ts`。
- **新建 `editors/vscode/src/graphPanel.ts`（新）**：`openGraphPanel(context, client, state): vscode.WebviewPanel` —— `vscode.window.createWebviewPanel('gitBamboo.graphPanel', 'Commit Graph', vscode.ViewColumn.Beside, { enableScripts: true, retainContextWhenHidden: true, localResourceRoots: [out/webview, webview] })`；HTML 同样经 `webviewHtml.ts`；接一个 `GraphSession`；`onDidDispose` 解绑 session。单例：面板已存在时 `reveal()` 而非重复开。
- **序列化恢复**：`vscode.window.registerWebviewPanelSerializer('gitBamboo.graphPanel', …)` —— 恢复时重建 `GraphSession`（graph 无需状态回传，直接重新请求当前视口即可）。编辑器面板不需要在 package.json 声明 views（`createWebviewPanel` 是运行时 API）。
- **`extension.ts` 的 `gitBamboo.openGraph` handler（L48–59）**：从 `executeCommand('gitBamboo.graph.focus')` 改为调 `openGraphPanel(...)`（保留无 session 守卫与错误提示）。侧栏紧凑视图继续存在，用户两个入口都可开。

### 7. 构建管线：`vite.webview.config.ts` 加第二入口

- `rollupOptions.input` 从单 `graph.html` 改为 `[graph.html, commit.html]`（`root`/`outDir`/`assetsInlineLimit: 0`/`emptyOutDir` 等不变；一次构建产出 `out/webview/graph.html` 与 `out/webview/commit.html`）。
- `build.js` 不需要改（它只是跑这个 config）。

### 8. 构建与冒烟验证

- `npm run typecheck`（`tsc --noEmit` 零错误）与 `npm run compile`（vite 两个入口构建成功，`out/webview/commit.html` + JS 产物存在）。
- 在 VS Code 扩展开发宿主（F5）中冒烟（若 agent 环境无法起 VS Code，则明确标注哪些项留给用户手动验证）：
  1. 内置 SCM 面板只剩内置 git 的 provider，无 "Git Bamboo" 条目
  2. Activity Bar 出现 Git Bamboo（`$(git-branch)` 图标），展开后两视图：Commit 在上、Commit Graph 在下
  3. Commit：修改/暂存/新增文件分三节显示（数量徽章、状态字母、折叠头）；`[+]` stage、`[−]` unstage 即时生效且列表刷新；点击行打开 HEAD vs 工作区 diff（untracked 直接打开文件）；rename tooltip 正确
  4. 提交流程：textarea 输入 → Ctrl+Enter → 提交成功、列表刷新、textarea 清空；空消息点 Amend → `--no-edit` amend 成功；空消息点 Commit → 被拒绝并提示
  5. Commit Graph 在新面板中正常渲染与滚动（回归确认移容器未破坏 webview）
  6. 原有命令回归：undo / switchBranch / createBranch / deleteBranch / fetch / pull / push / openGraph 在 palette 中可用；palette 的 commit 命令聚焦 Commit 视图
  7. 终端 `git commit` 外部变更 → Commit 视图列表 200ms 内刷新（沿用引擎通知链路 + 50ms debounce）
  8. 深浅色主题切换 → Commit 视图配色跟随（CSS 变量验证）
  9. `gitBamboo.openGraph` 打开编辑器区全宽 Commit Graph 面板（可拖到右侧分栏钉住）；重启 VS Code 后面板可恢复；再次执行 openGraph 不重复开窗而是 reveal 已有面板；侧栏紧凑 graph 视图仍可用且与面板互不干扰

二进制数据面与 ±500 预取**不在本计划验证范围**（见决策⑤后续指引）。

## Acceptance Criteria

实现完成于 2026-09-27（branch `implement/independent-panel-ui`）。✅ = 代码/静态验证通过；◐ = 已实现，留待 VS Code F5 冒烟验证（agent 环境无 GUI）。

- [x] `grep -rn "vscode\.scm\|createSourceControl\|SourceControlResourceState" editors/vscode/src/` 无结果（SCM API 依赖清零）✅
- [ ] SCM 面板中不再出现 "Git Bamboo" provider（完全归还内置 git）◐
- [ ] Git Bamboo Activity Bar 容器（`$(git-branch)` 图标）含两个视图：Commit、Commit Graph，均正常渲染 ◐
- [ ] Commit 一体化完成：message 输入 + 三节文件列表 + 底部 Commit/Amend；stage/unstage/diff 全部行内操作可用 ◐
- [ ] commit 与 amend（含空消息 `--no-edit`）从 Commit 视图可完成，成功后列表刷新、输入框清空 ◐
- [ ] 提交/外部 git 操作后 Commit 视图列表经引擎通知自动刷新 ◐
- [x] `tsc --noEmit` 零错误；`npm run compile` 成功且产出 `out/webview/commit.html` ✅
- [ ] `gitBamboo.openGraph` 打开编辑器区全宽 graph 面板：单例 reveal、序列化跨重启恢复、侧栏紧凑视图与面板互不干扰 ◐
- [ ] undo/branch/fetch/pull/push/openGraph 既有命令行为无回归 ◐

## Dependencies

- T1 引擎与扩展骨架已完成（见 `.ai-workflow/plans/20260921-t1-core-engine.md`）——本计划只动前端声明层与 extension host 代码，不改 Rust 引擎与协议。

## Related Documents

- .ai-workflow/ideas/20260921-rust-vscode-git-workbench.md
- .ai-workflow/plans/20260921-t1-core-engine.md
- .ai-workflow/research/20260921-git-workbench-research.md
- （后续待建：`graph-binary-data-plane` 计划 —— ArrayBuffer 零拷贝数据面 + Rust 预计算 lane 几何 + ±500 预取，见本计划 Background 决策⑤）
