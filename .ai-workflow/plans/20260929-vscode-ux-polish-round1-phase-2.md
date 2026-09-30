---
title: UX Polish Round 1 — Phase 2（A2）：Commit 视图批量操作与竹子主题品牌图标
date: 2026-09-29
status: done
completion: 2026-09-30
ideas:
  - .ai-workflow/ideas/20260929-vscode-ux-polish-round1.md
group: vscode-ux-polish-round1
phase: 2
tags: [git, vscode-extension, ui]
---

# UX Polish Round 1 — Phase 2（A2）：Commit 视图批量操作与竹子主题品牌图标

## Goal

Commit 视图补齐日常批量操作（分节 Stage All / Unstage All 行内按钮 + 文件行右键菜单），并以竹子主题 SVG 替换 Activity Bar 容器图标（消除与内置 git `$(git-branch)` 的混淆）、补 package.json `icon` 字段（marketplace 128×128 PNG）。**不改 Rust 引擎**；discard（丢弃工作区改动）**明确不在本期**——引擎无对应 RPC（详见 Research Summary），归 B 期随写操作 RPC 一起做。

## Background

independent-panel-ui 落地后的反馈与用户决策（2026-09-29）：A 期做 stage all / unstage all + 文件级右键；右键菜单用原生 `webview/context` API；discard 砍掉归 B 期；图标用竹子 + commit 树意象（具体形态实现时出草图再定，见 idea Open Questions）。见 [idea file](../ideas/20260929-vscode-ux-polish-round1.md)。

## Research Summary

- **host 侧批量管道现成**：`commitView.ts` `onMessage`（L93–124）的 `stage`/`unstage` 消息本就是 `paths: string[]`，`changePaths()`（L108–124）已做路径校验（`isValidRepoPath`，信任边界）+ 引擎调用 + 刷新——"Stage All" 只是 webview 侧把整节路径塞进数组，**host 侧零改动**。
- **无 discard RPC**：引擎唯一"reset"是 unstage（`git reset HEAD -- <paths>`，`crates/engine/src/write_queue.rs` L277）。丢弃工作区改动需要 `git checkout --`——host 直接 shell git 会破坏"引擎是唯一写入口"的架构原则（引擎无法感知/undo 该操作），故归 B 期由引擎新增 RPC 实现（届时含确认弹窗）。
- **原生 webview/context**：Commit 视图是 WebviewView，view type 即 `webviewId == gitBamboo.commit`；文件行加 `data-vscode-context`（分 `fileStaged` / `fileChanges` / `fileUntracked` 三个 section，让菜单条目按桶显隐 stage vs unstage）；目标文件经 `contextmenu` 委托监听 → `contextTarget` 消息同步到 host（先于原生菜单弹出，可靠）。同 Phase 1 Step 4 的模式，实现时可把共享逻辑（pending 目标注册表）抽成小工具复用。
- **图标规范**（VS Code 官方）：活动栏容器图标 **24×24、居中、单色**（VS Code 自行上色并施加 Default 60% / Hover·Active 100% 透明度状态——SVG 内不要烘焙透明度/多色/悬停态）；SVG 推荐；相对路径引用。顶层 `"icon"` 字段：**PNG ≥128×128**（256 可选 Retina），SVG 不被 marketplace 接受。`editors/vscode/.vscodeignore` 不排除 `media/`，新目录自动打包。
- **状态栏限制**：`StatusBarItem.text` 只支持 codicon（`$(name)`），**不支持自定义图片**——idea 中"状态栏同步替换"在 API 层面不可行，本期保持 `$(git-branch)` codicon（混淆主因是活动栏，已由 SVG 解决），计划内明确记录此限制。
- **当前代码事实**：`commit.html` 三节 section header（L228–245，`.section-header` + `.count` 徽章，头点击切换折叠）；`commit.js` `createFileRow()`（L85–125）行内已有 `+`/`−`/`⇄` 悬停按钮（单路径消息）；`createButton()`（L76–83）已有 stopPropagation 模式可复用于节级按钮；分桶 `bucketOf()`（L89–93）与渲染 `render()` 维护 `BUCKETS` 顺序。行内 DOM SVG 不受 CSP 限制（`webviewHtml.ts` L25–33）；`vscode.env.clipboard` 是宿主侧剪贴板标准入口（webview 内 clipboard 受限）。

## Steps

### 1. Commit 视图分节批量按钮（Stage All / Unstage All）

- `editors/vscode/webview/commit.html` L228–245：三个 `.section-header` 内、count 徽章右侧各加一个悬停可见的行内按钮（复用 `.action-btn` 样式或新 `.section-action`）：
  - Staged 节：`−` Unstage All（title "Unstage All Changes"）
  - Changes 节：`+` Stage All（title "Stage All Changes"）
  - Untracked 节：`+` Stage All（title "Stage All Untracked Files"）
- `editors/vscode/webview/commit.js`：
  - 按钮点击复用 `createButton()` 的 `stopPropagation` 模式（**不得触发节折叠**——现有折叠监听在 `.section-header` click 上，L186–192）。
  - 处理函数：收集该节当前全部 `item.path`（渲染时已按桶维护的数组），空节禁用按钮；`vscode.postMessage({ type: 'stage'|'unstage', paths: [...] })`——复用现有消息类型，host 侧 `commitView.ts` `changePaths()` 零改动。
  - 渲染时同步按钮禁用态（节空 → `disabled`）。
- VS Code 惯例参照：`$(add)`/`$(remove)` 语义（内置 git 的 stage/unstage 图标语言）；文本按钮 `+`/`−` 与现有行内按钮一致，无需引 codicon 资源。

### 2. Commit 视图文件行右键菜单（原生 webview/context）

- `editors/vscode/webview/commit.js` `createFileRow()`（L85–125）：行元素加
  `row.setAttribute('data-vscode-context', JSON.stringify({ webviewSection: 'file' + <Bucket 首字母大写>, preventDefaultContextMenuItems: true }))`（`fileStaged` / `fileChanges` / `fileUntracked`——静态 JSON，安全）。
- 容器级委托监听 `contextmenu`（文件列表容器上）：`e.target.closest('.file-row')` → `postMessage({ type: 'contextTarget', path, status, bucket })`。若 Phase 1 已落地 `contextTarget` 注册表工具，抽到共享模块（如 `src/webviewContextTarget.ts`）供两个视图复用；否则在本文件内实现同构逻辑。
- `editors/vscode/src/commitView.ts`：`onMessage` 新增 `contextTarget` 分支，记录 pending 目标 `{ path, status, bucket }`（供命令层读取；view dispose 时清除）。
- `editors/vscode/package.json` 新增命令（commands 数组追加）：
  - `gitBamboo.commitView.openChanges` — "Open Changes"（untracked/added 时 title 语义即打开文件；命令实现按 status 分流，同现有 `openResource` 逻辑）
  - `gitBamboo.commitView.stageFile` — "Stage Changes"
  - `gitBamboo.commitView.unstageFile` — "Unstage Changes"
  - `gitBamboo.commitView.copyPath` — "Copy Path"（仓库相对路径 → `vscode.env.clipboard.writeText`）
  - 全部 `commandPalette` 条目 `when: "false"`（目标依赖右键上下文）。
- `editors/vscode/package.json` 新增 `"menus" → "webview/context"`，全部 `when: "webviewId == gitBamboo.commit && webviewSection == …"`：
  - `fileChanges` / `fileUntracked` 行：Open Changes（`2_open`）、Stage Changes（`1_stage`）、Copy Path（`9_clipboard`）
  - `fileStaged` 行：Open Changes（`2_open`）、Unstage Changes（`1_stage`）、Copy Path（`9_clipboard`）
  - **不提供 Discard**（B 期引擎 RPC + 确认弹窗，届时参考内置 git 的确认文案与"Discard All N Files"模式）。
- `editors/vscode/src/extension.ts` 命令注册区（L188–203 风格）：四个命令读 commitView 的 pending 目标 → 分别调既有 `openResource(path, status)`、`changePaths('stage'|'unstage', [path])`（或直接 postMessage 等价路径）、clipboard。无目标时静默返回（或 `noSessionWarning` 风格提示）。

### 3. 竹子主题 Activity Bar 图标（SVG）

- 新建 `editors/vscode/media/` 目录。**先出草图**：实现时产出 2–3 个 24×24 竹子 + commit 树意象变体（竹节构成节点/分叉的走向），交给用户选定后再定稿（idea Open Questions 明确此项留实现期）。
- 定稿要求：24×24 画布、图形居中、**单一纯色 fill**（颜色任意，VS Code 会重着色）、不烘焙透明度/多色/悬停态；笔触粗细适配 16px 视觉缩放后仍可辨。
- `editors/vscode/package.json` L142：`viewsContainers.activitybar[0].icon` 从 `"$(git-branch)"` 改为 `"media/bamboo.svg"`。
- 状态栏（`extension.ts` L158–175）保持 `$(git-branch)` codicon——`StatusBarItem.text` 不支持自定义图片（API 限制，已记录）；混淆问题由活动栏 SVG 解决。

### 4. Marketplace 图标（package.json "icon"）

- `editors/vscode/media/icon.png`：128×128（可选再出 256×256 Retina 版）PNG，竹子主题定稿视觉的完整色版本（marketplace icon 不受单色约束，可用品牌配色 + 简单底色）。
- 从定稿 SVG 栅格化：`rsvg-convert -w 128 -h 128 media/bamboo-full.svg -o media/icon.png`（或 ImageMagick `convert`/inkscape，取环境可用者）；**PNG 提交进仓库**（构建管线不引图片生成步骤）。
- `editors/vscode/package.json` 顶层（与 `"categories"` 同级）加 `"icon": "media/icon.png"`。
- 确认 `.vscodeignore` 不排除 `media/`（现状已不排除）；`npm run compile` 后检查 vsix 打包含图标（可选 `npx vsce package` 试打验证）。

### 5. 构建与冒烟验证

- `npm run typecheck`（`tsc --noEmit` 零错误）、`npm run compile`（vite 成功）。
- `grep -n '"icon"' package.json` 顶层字段存在；`media/bamboo.svg` 与 `media/icon.png` 存在。
- VS Code F5 冒烟（agent 环境无 GUI 的项留用户手动验证）：
  1. 三节各出现悬停批量按钮；点 Stage All → 整节移入 Staged 且计数刷新；Unstage All 反向；空节按钮禁用；按钮点击不触发节折叠
  2. 右键文件行：按桶显示正确菜单（Changes/Untracked 行含 Stage，Staged 行含 Unstage），Open Changes 打开 diff/文件，Copy Path 得相对路径；右键空白区无菜单
  3. Activity Bar 出现竹子 SVG 图标：深/浅主题着色正确，默认态半透明、悬停/激活全显
  4. 状态栏仍为 codicon 分支指示（无回归）
  5. 内置 git 扩展的 Activity Bar 图标与 Git Bamboo 不再混淆（视觉区分明显）

## Acceptance Criteria

- [x] `tsc --noEmit` 零错误；`npm run compile` 成功 ✅（静态可验）
- [x] `package.json` 顶层 `"icon": "media/icon.png"` 存在，PNG 为 128×128 ✅（静态可验，vsix 打包含图标）
- [x] `viewsContainers` 图标指向 `media/bamboo.svg`（24×24 单色规范）✅（静态可验，单色已程序化验证）
- [x] 三节 Stage All / Unstage All 可用、空节禁用、不误触折叠 ✅（F5 冒烟通过，2026-09-30）
- [x] 文件行右键菜单按桶显隐正确，Open/Stage/Unstage/Copy Path 全可用 ✅（F5 冒烟通过）
- [x] 活动栏竹子图标在深浅主题下正确显示且与内置 git 区分 ✅（F5 冒烟通过）
- [x] 既有单文件 stage/unstage/commit/amend/diff 流程无回归 ✅（F5 冒烟通过）

## Post-Review Fixes（2026-09-30）

pan-review（security / code-quality / simplicity）无 Critical 发现；2 条 Warning + 10 条 Suggestion 全部修复于 commit `74e4ee8`：
- context-target park/take/clear 抽取为共享模块 `src/contextTargetSlot.ts`（graphSession 与 commitView 复用）；消费时按 lastItems 校验新鲜度、action↔bucket 交叉校验、畸形 payload 清除旧目标
- 菜单 when 子句改用 `=~` regex（`file-<bucket>` section），9 条 → 4 条
- 批量按钮路径改为点击时从 DOM 派生（删 bucketPaths 并行状态）；命令注册循环化；CSS 去重
- 按钮改为 opacity + `:focus-visible`/`:focus-within` 显隐，键盘可达
- 行内 `+`/`−`/`⇄` 与 `▾` 文本符号替换为 codicon 风格内联 SVG（随主题变色）

F5 复验通过（2026-09-30）：右键菜单按桶显隐（regex when）、键盘可达性、SVG 图标深浅主题显示均正常。

## Dependencies

- independent-panel-ui 计划的代码（branch `implement/independent-panel-ui`，未合并）——`implement/vscode-ux-polish-round1-phase-2` 自该分支切出（或自 Phase 1 分支切出若其先落地，二者文件面重叠小）。
- 与 Phase 1（A1）**无硬依赖可并行**；唯一交集是 `contextTarget` 注册表工具——若 Phase 1 先落地则复用其抽取的共享模块，否则各自实现后续合并时统一。
- 引擎 `stage`/`unstage`（批量 paths）RPC 现已存在，零引擎改动。

## Related Documents

- .ai-workflow/ideas/20260929-vscode-ux-polish-round1.md
- .ai-workflow/plans/20260929-vscode-ux-polish-round1-phase-1.md（同组 Phase 1）
- .ai-workflow/plans/20260927-independent-panel-ui.md（前置）
- （B 期待建：引擎写操作 RPC（含 discard/reset/checkout）+ commit 详情视图计划）
