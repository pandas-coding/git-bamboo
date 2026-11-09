---
title: Rebase 与历史洞察 Phase 4：graph 搜索过滤（作者/消息/路径，过滤即查询条件）
date: 2026-10-30
status: in-progress
ideas:
  - .ai-workflow/ideas/20261025-rebase-and-history-insights.md
group: rebase-and-history-insights
phase: 4
tags: [git, rust, vscode-extension, ui]
---

# Rebase 与历史洞察 Phase 4：graph 搜索过滤（作者/消息/路径，过滤即查询条件）

## Goal

graph 工具栏出现过滤框：输入作者 / 消息文本 / 路径条件（一期三条件集），commit 历史按条件过滤，虚拟滚动、lane 渲染、右键上下文菜单全部保持正常工作。过滤在引擎侧作为 graph 查询条件实现（"过滤即查询条件"），不在客户端过滤已物化的列表——保住 100k+ commit / 60fps 的性能目标。

## Background

见 idea 文件。2026-10-29 定案：过滤即查询条件；UI 放 graph 工具栏内过滤框（命令面板只做入口）；一期条件集仅作者/消息文本/路径，日期/分支后补。graph 现状：`GraphViewport` 协议**没有任何过滤参数**——需协议扩展 + `gix_read.rs` 过滤遍历 + webview 工具栏输入三层贯通。

## Research Summary

- **现状坐标**：协议 `crates/protocol/src/lib.rs` — `GraphViewport` L100、`GraphPage` L109、`GraphRequest` L168；引擎 `gix_read.rs::read_graph_page`（refs+HEAD tips、topological + lane assignment、anchor offset、`PRELOAD_MARGIN = 500`、epoch 校验）；缓存 `cache.rs`（redb commit/lane 缓存，epoch 失效，**键不含过滤条件**）。TS：`graphSession.ts`（410 行：16ms viewport debounce、requestSeq stale 丢弃、EPOCH_MISMATCH 单次重试 epoch probe L178-186、失效合并、NET_OPS 工具栏 dispatch 表）。webview：`graph.js`（745 行，24px 行高、`ROW_BUFFER 20`、`total_approx * ROW_HEIGHT` spacer、viewport 页 round-trip、canvas Bézier lane 覆盖层、`#toolbar` 工具栏行）。
- **过滤语义（git-log 权威）**：作者/消息 = 大小写不敏感子串（git `--author/--grep` 是 regex，但 GUI 输入框语义取子串更符合直觉且免 regex 注入——**有意偏离 git 默认**）；路径 = 提交相对任一变更文件路径含子串（变更集 = 与第一父的 tree-diff；根提交 = 全量）。多条件 AND。
- **路径过滤是性能大头**（tree-diff TREESAME 检查正是 git changed-path bloom-filter 加速的对象；gix 侧未合）。引擎侧用 commit-graph + redb 元数据缓存做作者/消息极廉；路径过滤逐 commit tree-diff 昂贵 → 过滤结果列表必须**按 (filter, epoch) 记忆化**。
- **分页经济学**：过滤后 `total_approx` 需变精确计数（虚拟滚动滚动条定位）；gix 现行 page+total_approx 结构保留，`total_approx` 语义变为"精确命中数"（全量遍历一次后列表固定，计数天然精确）。
- **learnings 红线**：
  - 虚拟列表行复用的 refill 哨兵：空串 `''` 与 skeleton 值碰撞导致 stale `data-vscode-context`/title/class 残留——**过滤轮是这个 bug 的高危场景**（有数据的行变 skeleton 行），必须用不与真实数据相等的哨兵（如 `'\u0000'`）且属性清理集中在 `fillRow` skeleton 分支单一路径。残留的 stale 右键目标指向旧 commit = checkout 安全事故。
  - 消费时校验：Execute 类操作的目标在消费时由引擎重验（Phase 1 已做）；graph 右键命令走 `contextTargetSlot`（park/take、TTL 30s）既有机制不受过滤影响，但行数据必须来自当前过滤页的权威数据。
  - webview→host 消息 host 侧逐字段校验（过滤文本 ≤ 256、无控制字符）。

## Steps

1. **协议扩展** — `protocol/src/lib.rs`：`GraphFilter { author: Option<String>, message: Option<String>, path: Option<String> }`（serde default None，`GraphRequest` 加 `filter: Option<GraphFilter>`，缺省 None = 现行为，协议向后兼容）；每字段长度 ≤ 256、无控制字符的校验落在 server 解析层（`parse_params` 后校验，越界 `INVALID_ARGUMENT -32005`）。协议测试：带 filter 往返 + 缺省兼容。
2. **引擎过滤遍历** — `gix_read.rs`：
   - `read_graph_page` 接 filter：**遍历照旧走全部 commit（lane/拓扑不能跳）**，页发射层按条件筛；命中 commit 进入页面，未命中跳过但 lane 结构照算（被过滤 commit 的父子连线穿过——渲染为祖先连线，不产生行）。
   - **作者/消息谓词**：commit 元数据（author 名/邮箱、message）——元数据读走 redb 缓存（cache.rs），未命中缓存的 commit 走 gix 解码并回填。
   - **路径谓词**：与第一父 tree-diff 的变更路径集合含子串（根提交取全量 diff）。昂贵 → **结果记忆化**：`FilteredView { key: (filter, refs, epoch), ids: Vec<ObjectId> }` 挂 session（内存，非 redb），首个请求全量遍历建列表，后续页/滚动/重请求直接切片；epoch bump 或 refs 变化即失效重建。redb commit/lane 缓存保持 filter 无感知（键不含 filter，`FilteredView` 独立）。
   - `total`：过滤激活时返回**精确命中数**；防失控：遍历计数超上限（如 200k commit 或树 diff 超时预算）→ 截断并置 `truncated: true` 标志（响应新增字段，serde default false），UI 显示"≥ N 命中"。
3. **server.rs dispatch** — `getGraph` 既有臂（L378 内）透传 filter 至 `read_graph_page`；校验在第 1 步解析层完成。
4. **TS session 层** — `graphSession.ts`：`getGraph` 请求 payload 带 filter 状态；filter 变化 = 新查询（requestSeq 照旧丢弃 stale 响应）；epoch 重试路径保留 filter（probe 重试带同一 filter）；`GraphInvalidated`/refs 变化时**保过滤条件**重查（用户不会因分支切换丢失输入的过滤）。`engineClient.ts` 类型镜像 `GraphFilter`。
5. **webview 工具栏** — `graph.html` `#toolbar` 加过滤输入区（放大镜 inline-SVG 图标 + 单输入框 + 清除按钮；一期单框三字段语义：纯前缀触发？否——**单输入框 + 三 chip 切换**太复杂，一期取：单输入框，旁边下拉选择条件类型（作者/消息/路径），默认消息；多条件 AND 通过再次选择叠加 chip 展示，chip 可单独移除）。300ms debounce 后 postMessage `{type:'setFilter', filter}` → host 校验（长度/控制字符）→ session 更新 → 引擎重查 → 首页下发 `{total, truncated}` → 工具栏右侧显示"N commits"（truncated 时"≥N"）。清除按钮 = filter None 重查。
6. **虚拟列表适配** — `graph.js`：过滤响应 total 变化走既有 spacer/页请求逻辑（无结构性改动）；**refill 哨兵改造**：审查 fillRow 的"清空 dataset 强制重填"惯用法，空串哨兵全部换 `'\u0000'` 类不碰撞值，skeleton 属性清理集中单一路径（learning 配方）；验收专测：过滤收窄后滚动，任意行右键目标 sha 与该行显示 sha 一致（无 stale context 残留）。
7. **命令面板入口** — `gitBamboo.graph.filter` 命令（palette 可见）：QuickInput 三段（条件类型 → 文本 → 可选追加）→ 结果等价于工具栏输入（调同一 session API）；一期最简：单条件 QuickInput 即可，多条件走工具栏 chip。
8. **引擎测试** — `engine_tests.rs`：fixture（多作者/多消息/多路径改动）——作者过滤命中集、消息子串大小写不敏感、路径过滤含/不含子串、多条件 AND、merge commit 第一父 tree-diff 语义、根提交路径语义；分页稳定性（同 filter 下 skip/limit 两页拼接 = 全集顺序一致）；filter + epoch bump → FilteredView 失效重建；缺省 None 行为与现测试完全一致（存量 graph 测试不改动通过）；截断标志路径（小上限注入）；越界 filter（超长/控制字符）→ -32005。

## Acceptance Criteria

- [x] `cargo test -p bamboo-engine` 全绿（存量 graph 测试零改动通过 + 新增过滤测试）；`cargo clippy` 无新告警。
- [x] 协议：`GraphRequest.filter` 缺省 None 时行为与现状逐字节一致（兼容性测试）。
- [ ] **手测**：工具栏过滤作者/消息/路径 → graph 即时收窄，滚动条与实际内容一致（total 精确）。
- [ ] **手测**：过滤态下虚拟滚动流畅（无整列表重渲染卡顿）；滚动中右键任意行，菜单动作作用于该行显示的 commit（无 stale 目标）。
- [ ] **手测**：过滤态下分支切换/engine 通知 → 过滤条件保留、结果正确刷新。
- [ ] **手测**：清除过滤 → 恢复全量视图，行为与过滤前一致。
- [ ] 路径过滤在大仓库（含 rename 历史）不超时；超限场景显示"≥N"截断提示而非假死。
- [x] webview 入站 filter 文本经 host 校验（超长/控制字符被拒）。

## Dependencies

无硬依赖（可与 phase-1/2 并行；建议最后做——复用 rebase 轮对 gix_read/cache 的熟悉度，且其验收依赖大仓库手测环境）。

## Related Documents

- .ai-workflow/ideas/20261025-rebase-and-history-insights.md
- .ai-workflow/learnings/20260930-empty-string-refill-sentinel-collides-with-skeleton.md（refill 哨兵红线）
- .ai-workflow/ideas/20260921-rust-vscode-git-workbench.md（性能目标：100k+ commit 首屏 <1s、60fps 滚动）
- 同组：phase-1/2（rebase）、phase-3（blame）
