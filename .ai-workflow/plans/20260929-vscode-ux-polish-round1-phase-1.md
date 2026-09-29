---
title: UX Polish Round 1 — Phase 1（A1）：Commit Graph 交互修复
date: 2026-09-29
status: ready
ideas:
  - .ai-workflow/ideas/20260929-vscode-ux-polish-round1.md
group: vscode-ux-polish-round1
phase: 1
tags: [git, vscode-extension, ui]
---

# UX Polish Round 1 — Phase 1（A1）：Commit Graph 交互修复

## Goal

修复用户报告的 Commit Graph 感知最强的问题：lane 区宽度从写死 240px 改为随可见 lane 数自适应 + 可拖拽（按宿主分别持久化）；删除双击 commit 的 "detached checkout coming in T2" 占位假动作；用**原生 `webview/context` 菜单 API** 提供 commit 右键菜单（Copy SHA / Copy Message / Create Branch at / 检出）；补 HEAD 行标记与 tag 显示；author/时间列右对齐且不再丢失时间信息。**不改 Rust 引擎与协议**——全部落点在 `editors/vscode` 前端层。

## Background

independent-panel-ui 计划（branch `implement/independent-panel-ui`，未合并）落地后的使用反馈，见 [idea file](../ideas/20260929-vscode-ux-polish-round1.md)。用户已确认（2026-09-29）：右键菜单用原生 `webview/context` API（非自绘）；discard 类破坏性操作不在 A 期；"检出" = `createBranch(base=sha)` + `switchBranch` 两步组合，裸 detached checkout 留 B 期。

## Research Summary

三路研究（codebase / 外部）关键结论：

- **原生 webview/context 菜单**（VS Code 1.64+）：package.json `menus.webview/context` 贡献条目（`command` + `when`，如 `webviewId == gitBamboo.graph`）；webview HTML 元素加 `data-vscode-context='{"webviewSection": "…", "preventDefaultContextMenuItems": true}'`，上下文自 document 根向下合并。GitLens 现行方案（323 个条目），自带主题/键盘导航/无障碍。**注意**：编辑器区 panel 的 `webviewId` 是 `gitBamboo.graphPanel`（createWebviewPanel 的 viewType），侧栏是 `gitBamboo.graph`，when 子句需两者都覆盖。
- **目标 commit 传递机制**：`contextmenu` 事件先于原生菜单弹出——webview 在容器上委托监听 `contextmenu`，定位行 → postMessage `{type:'contextTarget', …}`，host 记录 pending 目标；菜单命令执行时读该目标。此模式确定可行，不依赖命令参数传递语义。
- **引擎 RPC（零引擎改动即可用）**：`createBranch` params `{name, base}`，**base 可为 SHA**（`crates/engine/src/write_queue.rs` L294–303，`validate_ref_arg` 校验）；`switchBranch` `{name, auto_stash? (default true)}`；`getRefs` 返回 tag（`RefKind::Tag`，`crates/engine/src/gix_read.rs` L483–517）；`getHead` 返回 `{head, branch}`。写操作经引擎串行 write queue，两步不原子但安全（失败即报错，不留半检出状态——最坏情况是新分支已建未切换）。
- **当前代码事实**：`graph.js` L15 `GRAPH_WIDTH=240`（唯一 JS 使用点是 draw() L200 的 fallback）；`graph.html` L57–58 `.graph-gap { flex: 0 0 240px }`、L102–106 `#overlay { width: 240px }`；双击链路 = `graph.js` L101–104 + `graphSession.ts` L103–113 + `GraphViewMessage` L20–25 的 `id` 字段；`applyRefs`（graph.js L332–347）L336 过滤 `ref.kind !== 'branch'`（tag/remote 全丢）；`formatTime`（L167–173）只用 `toLocaleDateString()`；`GraphSession`（graphSession.ts L31–56）刻意 host 无关，无 host 标识、无 globalState 使用（整个扩展目前零 globalState）；graph webview 无 ready 握手（commit.js 有），初始宽度下发需补握手；行填充 textContent-only（commit 数据不可信——context 菜单标签同理）；CSP（webviewHtml.ts L25–33）允许行内 DOM SVG 与 unsafe-inline 样式。
- **图标/时间惯例**（外部研究）：Copy SHA 用全 SHA；行内时间相对/短格式 + hover 全格式（Git Graph/GitLens 惯例）；活动栏图标 24×24 单色 SVG。
- Lane 颜色按序号循环**有意不在本期**（idea 已定，随 B 期二进制数据面计划）。

## Steps

### 1. 删除双击 "detached checkout" 占位链路

- `editors/vscode/webview/graph.js` L101–104：删除 `renderRows()` 行创建处的 `dblclick` 监听及其注释。
- `editors/vscode/src/graphSession.ts` L103–113：删除 `onMessage` 中 `checkoutCommit` 分支（含 `showInformationMessage('… coming in T2')`）。
- `editors/vscode/src/graphSession.ts` L20–25：`GraphViewMessage` 接口删除 `id?: string`（仅 checkoutCommit 使用）。
- 验证：`grep -rn "checkoutCommit" editors/vscode/` 无结果。

### 2. Lane 区宽度自适应（CSS 变量化 + 按 lane 数收缩）

- `editors/vscode/webview/graph.html`：
  - L57–58 `.row .graph-gap` → `flex: 0 0 var(--graph-width, 240px)`。
  - L102–106 `#overlay` → `width: var(--graph-width, 240px)`（其余属性不动）。
- `editors/vscode/webview/graph.js`：
  - 删 L15 `GRAPH_WIDTH` 常量；新增模块级 `let manualWidth = null`（用户拖拽覆盖值）与 `let graphWidth = 240`（当前生效值）。
  - 新增 `computeAutoWidth()`：遍历 `commits` Map（trimCache 保留 ~3× limit 行，代表性足够）取 `maxLane`；`width = LANE_X_START + (maxLane + 1) * LANE_X_STEP + 14`（右侧留白），clamp 到 `[44, 240]`。多数历史 2–4 lane → ~50–80px。
  - 新增 `applyWidth()`：`manualWidth ?? computeAutoWidth()` → `graphWidth`；`document.documentElement.style.setProperty('--graph-width', graphWidth + 'px')`；置 `needsRedraw = true`（canvas 以 `clientWidth` 读实际宽度，draw() L200 的 `|| GRAPH_WIDTH` fallback 改为 `|| graphWidth`）。
  - 触发点：`graphPage` 消息处理（缓存写入后）与 `invalidate` 后调用 `applyWidth()`（auto 模式下新数据可能改变 maxLane）。
  - 宽度变化时 canvas resize 已由现有 draw() 的 dpr 重设逻辑覆盖（读 `clientWidth`）。

### 3. 拖拽分隔条 + 按宿主持久化（globalState）

- `editors/vscode/webview/graph.html`：滚动容器内新增 `#splitter`——绝对定位竖条，`left` 由 JS 同步为 `calc(var(--graph-width))`，视觉宽 4px、命中区 10px（透明 padding 扩展），`cursor: col-resize`，`role="separator"` + `aria-orientation="vertical"` + `tabindex="0"`。置于 canvas（`#overlay`，pointer-events:none）右侧同层，z-index 高于行。
- `editors/vscode/webview/graph.js`：
  - `pointerdown`（主键）→ `e.preventDefault()`（禁文本选择/合成 dblclick）→ **try/catch 包裹** `setPointerCapture(e.pointerId)`（pointer 已消失时抛 NotFoundError，不捕获会把拖拽状态卡死）；`pointermove` → clamp `[44, containerWidth * 0.6]` → 设 `manualWidth` + `applyWidth()`（实时视觉反馈）；**`pointerup`（或 `lostpointercapture`）→ 仅此时** `postMessage({type:'graphWidthChanged', width: manualWidth})`（避免持久化写风暴）。
  - 键盘：`ArrowLeft/Right` ±8px（Shift ±32）、Home/End 到 min/max、Enter 重置——均更新并即时应用，Enter 时 `postMessage({type:'graphWidthChanged', width: null})`。分隔条 `dblclick` 同 Enter（重置 auto）。
- `editors/vscode/src/graphSession.ts`：
  - 构造函数新增参数 `host: 'sidebar' | 'panel'` 与 `context: vscode.ExtensionContext`（或注入读写回调，实现者可选更简形式）。globalState 键：`gitBamboo.graphWidth.sidebar` / `gitBamboo.graphWidth.panel`（分宿主持久化）。
  - 消息处理新增：
    - `graphWidthChanged`（width: number | null）→ `context.globalState.update(key, width)`（null = 清除，恢复 auto）。
- **初始宽度握手**：`graph.js` 初始化末尾（L349 附近，`requestViewport()` 旁）新增 `vscode.postMessage({type:'ready'})`；`graphSession.ts` 处理 `ready` → `context.globalState.get<number | null>(key, null)` → `postMessage({type:'graphWidth', width})`（null 表示 auto）。`graph.js` 处理 `graphWidth` 消息：设 `manualWidth`（可为 null）+ `applyWidth()`。握手时序安全：ready 由 webview 发出，此时消息通道必然已通。
- `GraphViewMessage` 接口补 `width?: number | null`；`graphWebview.ts`（L37–60 `resolveWebviewView`）与 `graphPanel.ts`（`attachSession` L63–76）构造 `GraphSession` 时分别传 `host: 'sidebar'` / `'panel'` 与 context。

### 4. Commit 行右键菜单（原生 webview/context）

- `editors/vscode/webview/graph.js`：
  - 行创建处（L94–105，同 click 监听位置）：`el.setAttribute('data-vscode-context', JSON.stringify({ webviewSection: 'commitRow', preventDefaultContextMenuItems: true }))`——值是静态 JSON（安全，非 innerHTML）。
  - 容器级委托监听 `contextmenu`（rowsEl 上，冒泡捕获）：`e.target.closest('.row')` → 取对应 commit → `postMessage({ type: 'contextTarget', commit: { id, message, author_name } })`（**先于原生菜单弹出**，host 侧 pending 目标因此可靠）。commit 无（骨架行）则不设目标。
- `editors/vscode/package.json` 新增命令（commands 数组，L20–93 区域追加）：
  - `gitBamboo.graph.copySha` — "Copy Commit SHA"（全 SHA）
  - `gitBamboo.graph.copyMessage` — "Copy Commit Message"
  - `gitBamboo.graph.createBranchAt` — "Create Branch at Commit…"
  - `gitBamboo.graph.checkoutNewBranch` — "Checkout Commit (New Branch)…"
  - 四者 `commandPalette` 条目 `when: "false"`（无 pending 目标时 palette 调用无意义）。
- `editors/vscode/package.json` 新增 `"menus" → "webview/context"`：
  - 全部条目 `when: "webviewSection == commitRow && (webviewId == gitBamboo.graph || webviewId == gitBamboo.graphPanel)"`
  - 分组：`1_checkout` → checkoutNewBranch；`2_branch` → createBranchAt；`9_clipboard` → copySha、copyMessage（组内 `@1`/`@2` 排序）。
- `editors/vscode/src/graphSession.ts`：
  - 模块级导出 pending 目标注册表：`contextTarget` 消息 → 记录 `{ session, commit }`（弱引用最近一次右键来源；session dispose 时清除）。导出 `takeGraphContextTarget(): { client: EngineClient; commit: {id, message, author_name} } | undefined` 供命令层读取。
- `editors/vscode/src/extension.ts`（L188–203 命令注册区追加，沿用统一错误包装 + `noSessionWarning` 守卫风格）：
  - `copySha` → `vscode.env.clipboard.writeText(target.commit.id)`（webview 内 clipboard 受限，统一走 host）。
  - `copyMessage` → `writeText(target.commit.message)`（数据来自行数据，无需引擎往返）。
  - `createBranchAt` → `vscode.window.showInputBox`（prompt 带短 SHA，校验非空；ref 合法性由引擎 `validate_ref_arg` 把守）→ `client.request('createBranch', { name, base: target.commit.id })` → 成功 `showInformationMessage`（分支创建后 refsChanged 自动刷新 graph）。复用 `src/commands.ts` L53–77 的输入模式。
  - `checkoutNewBranch` → 同上输入 → `createBranch({name, base: sha})` → 成功后 `switchBranch({name, auto_stash: true})` → 成功提示。两步非原子：第二步失败时明示"分支 X 已创建但未切换"（`showWarningMessage`）。不加额外确认弹窗（建分支式检出无数据丢失风险，auto-stash 保护脏工作区；裸 detached checkout 的确认弹窗属 B 期）。

### 5. HEAD 行标记

- `editors/vscode/src/graphSession.ts`：构造函数新增 `client.getHead()` 初始拉取 + 订阅 `client.onNotification('headChanged', …)` 重拉，结果 `postMessage({ type: 'head', sha, branch })`。`GraphViewMessage` 侧为出站消息新增对应类型（host→webview 消息目前无接口声明，按现有 `graphPage`/`refs` 的实现风格处理即可）。
- `editors/vscode/webview/graph.js`：处理 `head` 消息存 `headSha`；`fillRow`（L131 起）：`commit.id === headSha`（引擎返回全 SHA，直接等值比较）时在 `.branch-tags` 最前插入 `HEAD` 标记 pill（class `head-pill`：加粗、强调色边框，样式参考 Git Graph 的 head dot+ring 视觉语言），并给行加 `head-row` class（预留行级底色/边线）。收到 head 消息后清空全部行的 `dataset.commitId` 触发重填（同 `applyRefs` L338–340 的既有手法）。
- `editors/vscode/webview/graph.html`：新增 `.head-pill` / `.row.head-row` 样式（用 `--vscode-*` 变量：`--vscode-focusBorder`、`--vscode-badge-foreground` 等）。

### 6. tag（与 remote 分支）显示

- `editors/vscode/webview/graph.js` `applyRefs`（L332–347）：不再过滤非 branch ref。按 kind 分桶入同一 tips 索引：`branch`（现有）、`tag`、`remote_branch` 均进 `branchTips` Map（值携带 kind，或并列 Map，实现者取更简者）。
- `fillRow` 的 ref pill 渲染（L131–145）：顺序 branch → tag → remote_branch；tag pill：`tag` class + 行内 SVG tag 图标（inline DOM SVG，CSP 无碍）+ 名称 textContent + `tagColor()`（L177–186 已有稳定色）；remote pill：斜体、`--vscode-descriptionForeground` 弱化。`sendRefs`（graphSession.ts L147–156）已把全部 refs 推给 webview，host 侧零改动；`refsChanged` 通知链路已存在，切换分支后自动刷新。

### 7. meta 列排版与时间格式

- `editors/vscode/webview/graph.html` L85–94：`.row .meta` 改为固定宽度右对齐两列（author + time）：`flex: 0 0 auto; display: flex; gap`，`.author { text-align: right; overflow: hidden; text-overflow: ellipsis; max-width: 14ch; }`、`.time { flex: 0 0 auto; text-align: right; }`——各行时间列右缘对齐、author 超长截断省略。
- `editors/vscode/webview/graph.js` `formatTime`（L167–173）：改为含时间格式 `toLocaleString(undefined, { year: 'numeric', month: 'short', day: '2-digit', hour: '2-digit', minute: '2-digit' })`；行填充时设 `el.title = commit.author_name + ' ' + full local string`（hover 完整信息，Git Graph 惯例）。

### 8. 构建与冒烟验证

- `npm run typecheck`（`tsc --noEmit` 零错误）、`npm run compile`（vite 构建成功）。
- `grep -rn "checkoutCommit\|GRAPH_WIDTH" editors/vscode/` 无结果。
- VS Code 扩展开发宿主（F5）冒烟（agent 环境无 GUI 的项标注留用户手动验证）：
  1. 侧栏 graph：lane 区收缩到可见 lane 数决定宽度（浅历史 ~50–80px），message 列可读
  2. 拖 4px 分隔条：实时跟随、松手持久化；重开视图宽度保持；侧栏与编辑器面板宽度各自独立
  3. 分隔条双击/Enter：恢复 auto 宽度
  4. 双击 commit 行：无任何动作（占位提示消失）
  5. 右键 commit 行：原生菜单出现 Copy SHA / Copy Message / Create Branch at… / Checkout (New Branch)…；侧栏与编辑器面板两宿主均可用；右键骨架行/空白区不出现
  6. Copy SHA → 粘贴得全 SHA；Copy Message → 得 commit message
  7. Create Branch at… → 输入名 → 分支出现在 graph pill；Checkout (New Branch)… → 工作区切换、状态栏/HEAD 标记更新、脏文件经 auto-stash 保留
  8. HEAD 行出现标记 pill；tag 与 remote 分支 pill 正确渲染、颜色区分
  9. author/时间列右缘对齐，时间含时分，hover 显示完整信息

## Acceptance Criteria

- [ ] `grep -rn "checkoutCommit\|GRAPH_WIDTH" editors/vscode/` 无结果 ✅（静态可验）
- [ ] `tsc --noEmit` 零错误；`npm run compile` 成功 ✅
- [ ] lane 区宽度随可见 lane 数自适应（不再恒 240px），message 列在侧栏可读 ◐
- [ ] 分隔条可拖拽（含键盘操作），宽度按宿主（侧栏/面板）分别持久化到 globalState，重开恢复 ◐
- [ ] 双击 commit 无动作、无 "coming in T2" 提示 ◐
- [ ] 右键 commit 出原生菜单（两宿主均可）：Copy SHA（全 SHA）/ Copy Message / Create Branch at / Checkout (New Branch) 检出流程可用，两步失败时有明确提示 ◐
- [ ] HEAD 行有标记；tag/remote 分支 pill 显示且颜色区分 ◐
- [ ] author/时间列右对齐、时间含时分、hover 全信息 ◐

（◐ = 需 VS Code F5 冒烟，agent 环境无 GUI。）

## Dependencies

- independent-panel-ui 计划的代码（branch `implement/independent-panel-ui`，领先 main 11 commits 未合并）——本计划在其之上开发（`implement/vscode-ux-polish-round1-phase-1` 自该分支切出）。
- 引擎 RPC `createBranch`（base 可为 SHA）/ `switchBranch` / `getRefs`（含 tag）/ `getHead` 现已存在，零引擎改动。

## Related Documents

- .ai-workflow/ideas/20260929-vscode-ux-polish-round1.md
- .ai-workflow/plans/20260927-independent-panel-ui.md（前置）
- .ai-workflow/plans/20260929-vscode-ux-polish-round1-phase-2.md（同组 Phase 2，无相互依赖可并行）
- （B 期待建：引擎写操作 RPC（checkout/reset/cherry-pick/revert、discard）+ commit 详情视图计划）
