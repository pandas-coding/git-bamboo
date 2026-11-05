---
title: Rebase 与历史洞察 Phase 2：rebase 计划面板 + graph 集成 + 冲突流共享抽取
date: 2026-10-30
status: in-progress
ideas:
  - .ai-workflow/ideas/20261025-rebase-and-history-insights.md
group: rebase-and-history-insights
phase: 2
tags: [git, vscode-extension, ui]
---

# Rebase 与历史洞察 Phase 2：rebase 计划面板 + graph 集成 + 冲突流共享抽取

## Goal

用户在 git-bamboo 内完成端到端交互式 rebase：graph 右键 "Rebase onto…" 弹出**编辑器区单例 webview 计划面板**（todo 列表拖拽排序、动作改 pick/reword/squash/drop、reword/squash 消息内联编辑、确认执行）；执行中冲突停靠走**共享抽取后的冲突流**（看板 + 三方合并编辑器，与 pull/merge 同一套）；graph 顶加进行中状态条（继续/跳过/中止），面板保持打开高亮当前 todo。graph 不画半成品历史。

## Background

见 idea 文件与 Phase 1（引擎 RPC 已就绪）。设计决策（2026-10-29）：JetBrains 路线独立面板；面板承载细节、graph 只负责"有件事没做完"（状态条）；`mergeEditor.ts:50-55` 已有 `isRebaseActive()` 与 `MergeInputOrder='rebase'` 的 ours/theirs 标签支持——rebase 是冲突流的**第二个消费者**，按 learning 必须同变更抽取共享，不可复制。

## Research Summary

- **Webview 模板**：`editors/vscode/src/commitDetail.ts`（编辑器区单例 + serializer 恢复模式参照 `graphPanel.ts`）——ready 握手后再推初始数据、dataSeq 序号守卫防乱序、**入站消息逐字段校验**（webview 是信任边界）。HTML 构建 `webviewHtml.ts`（nonce + CSP）；构建入口 `editors/vscode/vite.webview.config.ts`（新增 `rebase.html` entry）；`assetsInlineLimit: 0`（CSP 禁 inline script）。
- **learnings 红线（均曾在真 round 里踩过）**：
  - 构造器里 postMessage 会被静默丢弃 → todo 下发必须等 webview `{type:'ready'}`；
  - 入站消息进 engine RPC 前逐字段校验（改写历史 = 安全红线）；
  - **消费时校验而非刷新时清空**：引擎通知到达时不清空用户正在编辑的 todo，Execute 时引擎重验（Phase 1 已实现 -32005），客户端把该错误呈现为"计划已过期，请刷新"；
  - 冲突流共享抽取（second-consumer-triggers-shared-extraction）；
  - hover 显隐按钮用 `opacity: 0; pointer-events: none` 模式；图标 16×16 inline SVG（`stroke="currentColor"`、stroke-width 1.4、圆角连接、`aria-hidden` + title）；`$(codicon)` 只用于状态栏。
  - 上下文菜单 when 子句用 `=~` 正则收敛为单条 contribution。
- **右键入口机制**：`contextTargetSlot.ts`（park/take、TTL 30s）+ `package.json` `webview/context` contribution —— graph 行右键既有命令（`gitBamboo.graph.copySha` 等，extension.ts:126-150）同模式。
- **JetBrains UX 参考**：行内动作下拉（Pick/Reword/Squash/Fixup→本组无/Drop）、上下移按钮、确认预览。**exec/break/label 等命令行本组不出现**（引擎 startRebase 只接受四动作，Phase 1 已定）。
- **GitLens 面板参考**：done/remaining 分区渲染（done 灰显 + 计数）、停靠时高亮当前条目。
- 引擎通知既有：`RefsChanged`/`HeadChanged`/`WorktreeChanged`/`GraphInvalidated`（带 epoch）；rebase 推进会 bump epoch → graphSession 既有失效逻辑自动刷新。

## Steps

1. **冲突流共享抽取（先做，第二消费者触发）** — 从 `commitView.ts`（冲突栏渲染 + Accept Yours/Theirs + 打开合并编辑器入口，696 行）与 `netOps.ts`（pull 冲突进入冲突栏的流转）中，把"冲突集获取 → 冲突栏呈现 → 解决动作 → 合并编辑器打开"的控制器抽为共享模块（如 `editors/vscode/src/conflictFlow.ts`）：导出冲突状态查询、栏 bucket 数据组装、resolve 命令绑定、`openMergeEditor`（含 `MergeInputOrder` 参数化）。pull/stash 路径与 rebase 路径共同调用，**不得复制逻辑**；`mergeEditor.ts` 的 rebase 标签支持（`MergeInputOrder='rebase'`）在抽取中参数化暴露。行为零变化：现有 pull/stash pop 冲突测试与手测不回归。
2. **面板骨架** — `editors/vscode/src/rebasePanel.ts`：编辑器区单例 webview `gitBamboo.rebasePanel`（commitDetail.ts 模式：`show()` 复用已有 panel 或 `createWebviewPanel` + `registerWebviewPanelSerializer` 恢复）；`webviewHtml.ts` 新 builder；`vite.webview.config.ts` 加 `rebase.html` entry；新建 `editors/vscode/webview/rebase/`（html/js/css）。**ready 握手**：初始 todo/状态推送只在 `{type:'ready'}` 处理器内发出，构造器零 postMessage。
3. **入站消息协议与 host 校验（安全红线）** — 面板→host 消息集：`ready` / `executeRebase { onto, todo[] }` / `rebaseContinue` / `rebaseSkip` / `rebaseAbort` / `refreshPlan` / `panelReady`（UI 状态如折叠持久化如需要）。host 端逐字段校验：`onto` 过 ref 白名单校验；`todo` 数组长度 ≤ 500；每项 `id` 匹配 `/^[0-9a-f]{40,64}$/i`、`action ∈ {pick,reword,squash,drop}`、`message` ≤ 8192 无控制字符；越界一律丢弃 + 通知日志（不回显错误给 webview 以免探测）。校验通过才调 `engineClient` 的 Phase 1 RPC。引擎侧 -32005（stale 链）→ host 向 webview 发 `stale` 消息 → 面板顶部黄条"底层历史已变化，计划过期"+ 刷新按钮（**不清空用户编辑**，由用户决定刷新）。
4. **面板 UI（webview rebase/）**：
   - todo 行：sha 前缀、subject、作者/时间次要文字、当前动作徽标（pick 默认无、reword/squash/drop 有色标）；hover 显隐按钮（opacity 模式）：动作下拉、上移/下移、删除（=drop）；行内拖拽排序（HTML5 DnD 或 pointer 拖拽，与既有 webview 交互风格一致——优先 HTML5 DnD 简单实现，20 条内交互量足够）。
   - reword/squash 选中时行内展开消息编辑 textarea（squash 默认折叠消息 = 保留 git 折叠语义，自定义才填）；drop 行整体降不透明度保留位置可恢复（再点恢复 pick）。
   - 底部：执行按钮（主按钮）+ 取消；执行前 modal 确认（复用既有确认 modal 模式）列出动作摘要（N pick / M squash / K drop / 重排）。
   - 图标/交互遵循 ICONS inline-SVG 与 hover-opacity 既有规范。
5. **graph 右键入口** — `extension.ts` 注册 `gitBamboo.graph.rebaseOnto`：从 `contextTargetSlot` take 停靠的行 commit → 调 `getRebasePlan { onto: <该 commit>, branch: 当前分支 }` → 打开面板（数据随 ready 下发）；graph 行 `webview/context` contribution 加一条（`webviewSection` 正则 when 子句收敛，命令名 `Rebase onto this commit…`）。plan 为空链（upstream..branch 无 commit，如右键了本分支祖先）→ 信息提示不弹面板。
6. **执行流转与停靠处理** — `executeRebase` 调 `startRebase`；响应 `stopped: true`：
   - `reason: conflict` → 面板切换"停靠视图"：顶部红条列出冲突文件数 + "在提交视图解决"入口（指向共享冲突流抽取模块的冲突栏）；conflictFlow 以 `MergeInputOrder='rebase'` 打开三方合并编辑器；用户解决后回面板点"继续"（`rebaseContinue`，引擎前置检查未解决冲突返回 -32005 → 面板提示未完成）。
   - `reason: reword_stop` → 正常继续由引擎 sidecar 消息处理，UI 不需要停靠（Phase 1 机制）。
   - `stopped: false` → 面板顶部绿条"rebase 完成"+ 关闭按钮；graph 因 `GraphInvalidated`/`HeadChanged` 自动刷新。
   - 停靠期间面板每次引擎通知后 `getRebaseStatus` 刷新 done/remaining 分区（done 灰显 + "N/M"计数），高亮 `msgnum` 对应条目；**用户未开始编辑的新面板打开在停靠期** → 直接以 getRebaseStatus 的 done/remaining 渲染（只读停靠视图 + 继续/跳过/中止按钮）。
7. **graph 状态条** — `graph.js` 工具栏下方加状态横幅 div：`graphSession.ts` 订阅引擎通知（既有 `WorktreeChanged`/`HeadChanged`，200ms debounce）→ `getRebaseStatus` → 非空闲时渲染"Rebase 进行中 3/7，停在 abc1234"（`$(codicon)` 图标不适用于 webview——webview 内用 inline SVG）+ 继续/跳过/中止三个按钮（走 host 校验后 RPC）；与 commit view 冲突栏共用同一状态位（停靠冲突时状态条同时提示"有冲突待解决"）。rebase 结束/中止 → 状态条消失。graph **不改变既有渲染逻辑**（无半成品历史分支）。
8. **package.json / 命令注册** — 新命令 `gitBamboo.rebasePanel.execute/continue/skip/abort/close`、`gitBamboo.graph.rebaseOnto`；`menus`：graph 行 `webview/context`、编辑器 tab 上下文可选；命令面板仅暴露 `gitBamboo.graph.rebaseOnto` 有参数形态?否——面板命令 palette 隐藏（`"when": false` 模式，hunk staging 轮同款），入口唯一走 graph 右键。
9. **验收手测脚本化清单**（plan-status-hygiene learning：手测项必须显式记录通过/失败）—— 见 Acceptance Criteria。

## Acceptance Criteria

- [ ] `npm run compile`（或既有等价 lint/build 流程）无错误；`cargo` 侧零改动（本阶段纯 TS/UI）。
- [ ] **手测**：graph 右键 "Rebase onto…" 弹面板，todo 列表与 `git log upstream..branch` 一致。
- [ ] **手测**：拖拽重排 + drop + squash + reword 自定义消息组合执行，`git log` 结果与计划一致（对照命令行 git 验证）。
- [ ] **手测**：执行中产生冲突 → 面板停靠视图 + 提交视图冲突栏出现 rebase 冲突 → 三方合并编辑器（rebase 侧标签正确）→ 解决 + 继续 → 完成，graph 自动刷新。
- [ ] **手测**：停靠点"中止"→ 分支回到 pre-rebase tip（SHA 对比）；graph 状态条消失。
- [ ] **手测**：rebase 进行中 reload VS Code → 面板 serializer 恢复为停靠只读视图。
- [ ] **手测**：执行前底层历史变化（另一终端 commit）→ Execute 返回 stale 提示，用户编辑未被清空。
- [ ] **手测**：外部命令行启动 `git rebase -i` 停靠 → graph 状态条出现（检测不依赖引擎簿记）。
- [ ] pull/stash 冲突流回归：现有冲突解决路径行为不变（抽取后共用同一代码）。
- [ ] 入站消息校验：伪造越界 payload（非 hex id、未知 action、超长）被 host 丢弃且不触发引擎 RPC。

## Dependencies

- Phase 1（引擎六个 rebase RPC + engineClient wrapper）。
- network-ops 组全部 phase（冲突流抽取的来源代码）。

## Related Documents

- .ai-workflow/ideas/20261025-rebase-and-history-insights.md
- .ai-workflow/plans/20261025-rebase-and-history-insights-phase-1.md
- .ai-workflow/plans/20261025-network-ops-enhancements-phase-3.md（冲突流原始实现，抽取对象）
- 并行阶段：phase-3（blame）、phase-4（graph 过滤）
