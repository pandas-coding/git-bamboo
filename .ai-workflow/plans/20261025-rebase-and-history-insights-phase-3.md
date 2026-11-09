---
title: Rebase 与历史洞察 Phase 3：JetBrains 式 blame（引擎 getBlame + 标注/悬停/导航）
date: 2026-10-30
status: done
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

- [x] `cargo test -p bamboo-engine` 全绿（含 blame 归属/rename/脏缓冲/回退测试）；`cargo clippy` 无新告警。
  —— `cargo test -p bamboo-engine` **124 绿**（其中 11 个 blame 测试）；`cargo test --workspace` 全绿
  （protocol 25 绿）；`cargo clippy --workspace --all-targets` 只剩 1 条**既有**告警
  （`engine_tests.rs:4138` while-let，非本次改动）。blame 测试覆盖：逐行归属、显式 rev、
  rename（`source_path`）、脏缓冲（null id + 已提交行不变）、**工作区 vs 修订版内容选择**、
  CRLF 转换、工作区已删除文件、unborn HEAD/路径不存在、非法参数（含路径逃逸）、
  以及 porcelain 回退在上述全部形态上的等价性。
- [x] `getBlame` 响应含每 entry 的 commit 元数据（author/time/summary），客户端单次往返可渲染（无逐行回查）。
  —— 引擎侧经 redb commit 缓存做一次 join（`entries[].commit: BlameCommit`，含
  `author_name`/`author_email`/`author_time`/`committer_time`/`summary`；null id 合成 "Not Committed Yet"），
  `blame.ts` 只从这一份响应渲染 gutter/hover/lens。未提交行的 `BlameCommit` 无命令链接。
- [x] gix 路径显式传 commit-graph（代码断言/注释可见）；`via` 字段区分实现路径。
  —— `gix_read.rs` 的 `repo.commit_graph_if_enabled()` 显式传入 `blame::file`（带注释说明
  "不传则静默不用"）；回退（gix 报错或 entry 未铺满 blob）报 `via = "git-porcelain"`，
  正常路径报 `"gix"`；等价性测试断言两条路径的归属与元数据完全一致。
- [x] **手测**：toggle blame → gutter 出现作者/时间标注；颜色弱化不干扰代码阅读。
- [x] **手测**：悬停行 → 该行 commit 详情 + 跳 commit 面板/diff 入口可用。
- [x] **手测**：CodeLens 当前行"跳转引入 commit" → commitDetail 打开正确 commit。
- [x] **手测**：annotate previous 连续回溯 3 层 → 每层打开父版本只读文档并自动标注，行号合理保持。
- [x] **手测**：脏缓冲（未保存修改）blame 不报错（contents 路径）；保存后重渲染。
- [x] **手测**：大文件（>1000 行、多变更）滚动流畅（viewport 切片生效，无明显卡顿；50ms 级交互）。
- [x] engine 通知（切换分支/外部 commit）后 blame 缓存失效重渲染（TTL + 通知双通道）。

> **手测项收口（2026-11-10）**：上述 7 条已由用户在真实 VS Code 会话中执行并确认通过，故逐条
> 勾选、`status` 转 `done`。本轮未走 `scripts/make-blame-test-repo.sh` 的夹具（`/tmp/git-bamboo-blame-manual`
> 无留存产物），所以这里按"整批通过"记录，不逐条补写未经本人执行的观察值——按
> `plan-status-hygiene` 的要求，勾选的依据是执行者的确认，不是"测试过了"的类推。夹具脚本及其
> `MANUAL-TESTS.md`（三层 annotate-previous 链、被相似度检测识别的 rename、单文件三种归属、
> 1500 行大文件、脏工作区 + 未跟踪文件、切换分支触发失效）保留，供日后回归复跑。
> 自动项在 `8a3ae88` 时三平台 CI 全绿：Rust Tests `ubuntu-latest` / `macos-latest` /
> `windows-latest` + Cargo Audit + Security audit 均 success。

## Implementation Notes（2026-11-09，实现期偏离与补充）

- `GetBlameResult = { rev, via, entries }`：plan 草图只画了 `entries`。元数据按草图**内联**在
  `entries[].commit`（`BlameCommit`），而不是 `commits: {id: BlameCommit}` 映射——一个文件通常
  只有两三个 commit，去重省下的字节远小于客户端查表的复杂度，故保持草图形状。`via` 是验收项 3，
  `rev` 供 annotate-previous 沿链续走。
- **blame 的是哪一份字节**按 `git blame` 语义三选一（plan 未定义，实现中发现必须定义，否则
  entry 行号与客户端实际渲染的文档错位）：给了 `contents` → 客户端缓冲，against `rev`/HEAD；
  无 `contents` 无 `rev` → 工作区文件 against HEAD；无 `contents` 有 `rev` → 该修订版 blob。
  工作区已删除 → 空 entries（不回退去 blame HEAD 的副本）。推论：`git-bamboo:` 只读修订版
  文档必须**显式**传 `rev`，省略即等于"我要工作区那份"。
- `core.autocrlf` 转换（plan 未提）：Windows 检出是 CRLF、blob 是 LF，不转换会让整文件读成
  "Not Committed Yet"。只查 `core.autocrlf`，且仅在 blob 自身无 CR 时转换，使 gix 与
  porcelain 两条路径在不发生转换的仓库里保持等价。**边界**：`.gitattributes` 的 `text=`/`eol=`、
  LFS、`ident`、自定义 clean filter 不生效——完整 filter 管线（`filter_pipeline`）每次 blame
  都要读 index，对一个纯展示功能代价过高，代码注释已写明。
- 路径收敛校验：blame 是唯一把客户端 path 直接拼到工作区根去 `open` 的读（其余读都把 path
  交给 git），故拒绝绝对路径 / `..` / Windows 前缀，返回结构化 `INVALID_ARGUMENT`。
- `CachedCommit` 增加 `committer_time: Option<i64>`（`#[serde(default)]`，旧缓存条目仍可读；
  `None` 视为**部分命中**并重新解码，不当 0 用）：blame 的元数据 join 与 graph 读共用同一条目。
- 脏缓冲 `contents` 上限 8 MiB：服务端帧上限是 16 MiB 且超限直接断连，压在其下才能带内返回
  `INVALID_ARGUMENT` + "先保存、改 blame 工作区副本" 的提示。扩展侧另设 2 MiB 文本上限，
  保证 JSON 转义后必然小于帧上限；超限时退化为 blame 已保存文件并在状态栏说明一次。
- 回退条件不止"gix 报错"：gix 成功但 hunks 未连续铺满被 blame 的内容时同样回退 porcelain
  （`validate_hunk_tiling` + 等价性测试），宁可换实现也不交付错位归属。porcelain 自己的输出
  也要过同一条校验——它后面没有可回退的实现了，故校验失败直接返回结构化错误。
- 评审后修正（同分支）：
  - **锁作用域**：`read_blame_with` 拆成三段，porcelain 子进程在**不持有** `gix_repo`/`cache`
    锁时运行（与 `read_commit_detail` 的"读不得跨进程持锁"约定一致），否则一个卡住的 git
    会拖住整个 session 的读与写队列。
  - **符号链接**：worktree 读取用 `symlink_metadata`，链接一律不跟随（返回空标注）——上游
    收敛校验是词法的，被跟踪的链接（`notes/secret -> ../../.ssh/id_rsa`）能过校验并被
    `fs::read` 读出仓库外内容。同一读取补上 8 MiB 上限（worktree 文件不受帧上限约束），
    并把"存在但读不了"（权限/IO）从静默空标注改成结构化错误。
  - **porcelain 路径名**：加 `-c core.quotepath=false`，否则非 ASCII 路径被 C 转义
    （`"notes-\303\251.md"`），hover 不可读且客户端无法用它打开 annotate-previous；`-z` 无效。
    另加 `GIT_OPTIONAL_LOCKS=0`：blame 可能每个键入停顿都触发，读不应去抢 `index.lock`。
  - **stdin 死锁**：`--contents=-` 的缓冲改由子线程写入。stdout/stderr 在
    `wait_with_output` 之前无人排空，最多 8 MiB 的写入与 64 KiB 管道会互相阻塞。
  - **`via` 语义**：早退路径（unborn HEAD、路径不在树中、worktree 无可标注内容）原先硬编码
    `Gix`，现报告"本来会跑的那条"（强制 porcelain 的等价性测试因此才真正校验到该字段）。
  - rename 注释纠正：`git blame` **默认**跟随重命名（已实测），故两条路径的 `source_path`
    一致（等价性测试断言）；差异只在 copy 检测，两边都不开。
  - **扩展侧缓存键**（评审发现的真缺陷）：`rev: null`（工作区字节）与 `rev: 'HEAD'`（HEAD 的
    blob）曾折叠成同一个键，同一文件同时以两种形态打开时（文件编辑器 + 暂存 diff 的
    `?HEAD` 一侧）会互相串用标注；现 null 渲染为独立占位符。hover 的 `isTrusted` 从布尔
    改为命令白名单，并对 entry 的 commit id 做形状校验（不合格则整份响应丢弃 + 记录日志）；
    保存时主动失效对应缓存键（原来只能等 watcher 通知）；两个 "Open Changes" 入口合并为
    `openCommitFileDiff`，`toRelativePath` 单一实现。

- **合并后的跨平台 CI 修复（2026-11-09/10）**：本期是 2026-10-22 之后第一次跑齐三平台矩阵，
  连带暴露 4 个平台相关缺陷，其中 1 个属本期代码：
  - `getBlame` 的路径收敛原先用 `Path::is_absolute` 判定。Windows 上 `/etc/passwd` **不算**绝对
    路径（无盘符前缀），而 `PathBuf::push` 的文档明写"有 root 无 prefix 时替换掉除 prefix 外的
    全部内容"，于是 `join` 落到 `C:\etc\passwd`——逃出工作区。blame 是唯一会用客户端路径直接打开
    工作区文件的读，故这里是唯一能读到仓库外文件的地方。改为按 component 只允许 `Normal`
    （连带拒绝 `RootDir`/`Prefix`/`ParentDir`），三平台一致（`d805250`）。
  - 既有代码：`write_queue.rs` 的 exec-bit 探测未做平台 gating，Windows 直接编译失败（`a03370f`）；
    watcher 只按客户端给的路径形式匹配事件，而 macOS FSEvents 报的是解析后路径（`/var` →
    `/private/var`），导致工作区事件被静默丢弃（`75791af`）；Windows 上删除被监听目录时 notify
    收到 `ERROR_ACCESS_DENIED` 后静默撤监听且不发任何事件，re-arm 因此永不触发，改为比对根目录
    创建时间（`RootStamp`）而非试图撞上"目录不存在"的时间窗（`8a3ae88`）。
  - 另修 stash 日期断言假设了非零时区偏移（git `%cI` 在零偏移打印 `Z`），改用 RFC-3339 解析，
    并在协议文档写明 `Z` 与 `±HH:MM` 两种形态都合法（`d1fd1e6`）。

## Dependencies

无（与 phase-1/2 无代码耦合，可并行；建议排在 rebase 两阶段之后交付以先落大件）。

## Related Documents

- .ai-workflow/ideas/20261025-rebase-and-history-insights.md
- .ai-workflow/learnings/20261029-codelens-hunk-actions.md（CodeLens/decoration 分层依据）
- 同组：phase-1/2（rebase）、phase-4（graph 过滤）
