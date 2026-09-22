---
title: Git Workbench 工程选型研究报告
date: 2026-09-21
status: research
tags: [git, rust, vscode-extension, llm-agent, zed, architecture]
related: ideas/20260921-rust-vscode-git-workbench.md
---

# Git Workbench 工程选型研究报告

> 三个并行研究（git 引擎选型 / VS Code 前端 / 协议 + Zed + LLM），关键事实均经在线验证（GitHub API、crates.io、zed.dev/docs、VS Code 官方文档，2026-09-21 抓取），未验证项已标置信度。

## 结论一：git 引擎选型 → 混合方案（gix 读 + CLI 写 + 自建 undo）

| 维度 | A. CLI 包装 | B. git2-rs (libgit2) | C. gitoxide (gix) | D. 混合 ★ |
|---|---|---|---|---|
| 大仓库读性能 | 中（进程启动 5-20ms + 文本解析） | 中 | 高（并行遍历、scoped status walk） | 取 C 上限 |
| rebase/merge/cherry-pick/stash | 全支持 | 全支持（进程内 API） | **不支持**（占位 crate） | 取 A 上限 |
| undo 事务 | 自建 | git_transaction | 自建 | 自建 |
| 维护状态（已验证） | — | 活跃（rust-lang org，2026-09 仍在合并 PR；"已归档"传闻不成立） | 非常活跃（gix 0.87.1，4700 万下载） | 可控 |
| 分发 | 需检测 git | FFI + vendored C，CI 复杂 | 纯 Rust，三平台极简 | 纯 Rust |
| 新特性（partial clone/LFS/SHA-256） | 天然跟随 | 落后 | 未完成 | CLI 侧兜底 |

**推荐 D**：读路径（log/graph/status/diff/blame）用 gix 进程内计算——scoped status walk + fs 事件刷新直接命中"5 万文件 status < 100ms"；写路径与高层操作（rebase/merge/cherry-pick/push）走 git CLI 子进程——恰好是 gix 的未完成区、CLI 的 100% 覆盖区。undo 快照（复制 refs + HEAD + index + stash 记录）自建，不依赖任何库的事务 API。

关键工程约束：
- gix 未到 1.0，需锁版本 + 抽象层隔离（防止 0.87→0.9 破坏性变更）
- CLI 写路径与 gix 读路径并发访问 index/refs 需**统一的仓库级命令队列**
- 交互式 rebase 用 `GIT_SEQUENCE_EDITOR` 程序化驱动 `git rebase -i`（git4idea 同法）
- PoC 需实测：998k commit 仓库 log、50k 文件 status（用 B/C 各跑 benchmark 锁死选型）

## 结论二：VS Code 前端 → Canvas + 二进制 IPC 是达成性能指标的必要条件

- **渲染**：虚拟化 DOM 行（文本 + hover/选择/a11y）+ 单 Canvas 覆盖层画分支连线；纯 DOM 路线在 100k 级无法稳 60fps（Fork/GitKraken 均为 Canvas 路线）。WebGL 在此场景收益不成比例。
- **IPC**：webview postMessage 官方为 JSON 序列化，但 1.57+ 支持 ArrayBuffer zero-copy transfer。滚动时视口 + 前后 ~500 commit 预取，本地命中缓存，每帧最多一次消息，二进制紧凑格式（lane 路径由 Rust 预计算）。注意两跳：Rust→extension host→webview，Node 中转层只透传 ArrayBuffer、不做 JSON 化。
- **数据驻留**：全量数据驻留 Rust 侧，webview 只持视口数据。
- **Diff**：`vscode.diff` + TextDocumentContentProvider；大文件由 Rust 侧 histogram diff 只传 hunk，超阈值降级为 hunk 分页。
- **TreeView**：数千节点即卡（无虚拟化）；分支/标签 >1k 时用 QuickPick 搜索式访问。
- **分发**：rust-analyzer 模式——按 platform 拆分 VSIX（darwin-x64/arm64、linux-x64/arm64、win32-x64 等）+ 官方支持 platform-specific VSIX；运行时 GitHub Release 下载作 fallback。
- **进程生命周期**：引擎侧监听 stdin 关闭/父进程 PID 自杀（VS Code 崩溃时 deactivate 不可靠）；每窗口一个引擎实例或共享 daemon + 引用计数。
- **与内置 git 共存**：并行而非替代。自建 `scm.createSourceControl` 视图 + 自注册 QuickDiff/Timeline provider；禁用内置会失去 gutter 装饰等并破坏 PR 扩展生态。引导用户关内置的仓库自动发现减负。
- **交互组件**：fuzzy 分支切换 QuickPick 够用（自实现 fzy 评分）；rebase todo 用结构化编辑器生成标准 todo 文件；冲突解决先用逐 hunk QuickPick（ours/theirs/both）+ diff 预览，三栏 UI 后期自研。
- **⚠️ 竞品情报**：VS Code 内置 git 2025 年已加入原生 commit graph 视图（中高置信度）——我们的差异化必须落在**引擎性能 + 大仓库工作流 + LLM agent**上，而不是"有 graph 视图"本身。

## 结论三：协议与多前端 → 控制面 JSON-RPC + 数据面可选二进制；Zed 路线 = MCP，不是"前端"

**协议**：LSP 风格 JSON-RPC 2.0 over stdio（请求/响应 + 通知 + 能力协商 + 分页游标）。serde-json 吞吐数百 MB/s，100k commit 场景瓶颈在前端渲染而非序列化——**单 JSON 通道起步够用**，schema 预留二进制帧扩展（length-prefixed bincode + zstd，仅 VS Code 侧大流量启用）。

**订阅模型**：客户端在 initialize 声明订阅，服务端发细粒度通知：`refsChanged`（附变化 ref 名单）、`worktreeChanged`（附路径）、`indexChanged`、`headChanged`。内部：fs watch → 去抖合并 → 事件化。引擎为 daemon，每前端一条连接（会话 id），内部单写队列 + 多读快照，引用计数 + idle 超时退出。

**Zed（已验证 zed.dev/docs 2026-09）**：Zed 扩展 = Rust → wasm32-wasip2，**没有自定义 UI/面板能力**（高置信）。但 MCP 扩展模式是：扩展声明 `context_server_command`，**由 Zed 主进程拉起原生二进制并经 stdio 以 MCP 通信**。→ Zed 兼容的正确路线不是"给 Zed 写 UI 前端"，而是**引擎直接实现 MCP server**，复用 Zed 原生 agent 面板。约束：协议保持纯 JSON 文本、自有 schema、零 VS Code 类型耦合；二进制数据面在 Zed/MCP 通道不可用，必须有纯 JSON 分页降级。

**LLM agent**：MCP 是 2026 主流（spec 2025-11-25 + 官方 registry + Zed/VS Code 原生集成）。引擎直接实现 MCP——同一份实现同时服务 Zed 和任意 agent 客户端。安全架构：
- 工具三级权限：只读 / mutating / destructive（默认关闭，显式开启）
- destructive 两段式：`execute(planId)`——计划 → 用户确认 → 执行
- 每次 mutating 操作自动挂 undo 事务点，undo 亦暴露为工具
- Zed 不支持 Elicitation，确认流在对话内完成（工具返回待确认计划）
- BYO key：provider trait + 系统 keyring；Ollama 兼容 OpenAI API，本地模型适配成本低
- commit message 生成是无状态单次调用（独立轻量工具），不进完整 agent 会话

## 修订后的总体架构

```
┌─ VS Code 扩展（TS）─────────────────────────────┐
│  webview: 虚拟化 DOM + Canvas graph            │
│  extension host: SCM/QuickPick/diff 编排        │
└──────────────┬ JSON-RPC + ArrayBuffer 数据面 ───┘
┌──────────────▼ Rust 引擎（daemon，常驻）────────┐
│  ┌─ 协议适配层 ──────────────────────────────┐  │
│  │ ① JSON-RPC (编辑器 UI，订阅推送)          │  │
│  │ ② MCP Tools+Prompts (LLM/Zed，纯 JSON)    │  │
│  │ ③ 可选二进制数据面 (仅 VS Code)           │  │
│  └───────────────────────────────────────────┘  │
│  core: 状态机 + 事件总线 + undo 事务管理        │
│  ┌─ 读路径: gix (graph/status/diff/blame) ──┐  │
│  └─ 写路径: git CLI 队列 (rebase/merge/push) ┘  │
│  watcher: fs 事件 → 去抖 → 细粒度通知           │
│  会话管理: 多前端连接 + 引用计数                 │
└────────────────────────────────────────────────┘
```

与原架构图的差异：① 增加协议适配层（三种面共享 core）；② Zed 不是"第二个 webview 前端"而是 MCP 客户端；③ 读写路径明确分离（gix / CLI 队列）；④ undo 事务管理提升为 core 级组件（LLM 安全地基）。

## 主要风险清单（按优先级）

1. gix 与真实大 monorepo 的兼容性长尾（多 worktree/submodule/特殊 ref 布局）——PoC 实测
2. 读写并发锁竞争——统一命令队列的设计验证
3. Canvas 自绘的交互成本（滚动/命中测试/a11y 全自管）
4. MCP 规范半年一版，演进快——适配层隔离
5. 内置 git 的原生 commit graph 持续进化——差异化锚定引擎性能 + LLM
6. Windows fs 事件延迟实测

## 建议的 PoC（进入 pan-plan 前的验证清单）

- [ ] benchmark：linux 仓库（~100 万 commit）log/graph 遍历，gix vs CLI（首屏 <1s 目标）
- [ ] benchmark：5 万文件仓库 scoped status + fs 事件增量（<100ms 目标）
- [ ] 最小 daemon + JSON-RPC + 订阅通知跑通（单仓库会话）
- [ ] webview Canvas graph 虚拟滚动 spike（1 万 commit，60fps）
- [ ] ArrayBuffer 两跳透传性能验证
- [ ] undo 事务 spike：rebase 前快照 + 一键回滚
- [ ] MCP 最小工具集（log 查询、plan/execute 两段式）在 Zed 里实测

## 深度审查补充（第二轮，2026-09-21）

对修订架构的对抗性审查结论：4 个遗漏的重大问题 + 5 个可优化点。

### 遗漏的重大问题

**A1. 冷启动首屏 <1s 依赖持久化缓存**：gix 并行遍历只保证热路径；冷启动遍历+解析 100k commit 是秒级。引擎需要跨重启的持久化缓存（commit 元数据 + lane 预计算，SQLite/redb），失效条件 = ref tips + 对象存在性校验，不做盲目过期。原架构图的 "Cache" 模块由此具体化。

**A2. gix 缓存与 mmap 的失效（混合方案真正的难点）**：gix 进程内持有 index 快照与 pack 文件 mmap，CLI 写路径（rebase/gc）完成后它们全部过期——轻则读到旧状态，重则 pack 被并发 gc 重写导致访问失效 mmap。修正：仓库状态纪元（epoch）机制——任何 CLI 写操作、任何 fs 事件（.git/index、refs/）bump epoch；gix 读缓存绑定 epoch，变了强制重载。附带收益：用户在终端手跑 git 命令（外部变更）天然被捕获，"引擎是唯一写入口"的理想化假设不再需要成立。

**A3. push/fetch 认证是被低估的硬骨头**：真实用户痛点集中区。范围决策（已定）：
- **T1：继承用户 credential helper + SSH**（零成本，CLI 路径天然继承用户机器上已配好的凭证；需处理首次连接的 known_hosts 主机指纹确认转发到前端）
- **T2：GitHub 设备码授权流**（对标内置 git 的体验，单 provider 先行），PAT 按需补

**A4. LLM 与人的并发冲突**：原安全模型只考虑"LLM 出错→undo"，没考虑 agent 执行 rebase 时用户正在编辑/stage 的交错变更。修正：操作租约（operation lease）——agent 执行 mutating 操作时对仓库加租约，前端显示"agent 正在操作"状态横幅，人的操作被引导排队或明确警告。操作队列需优先级：用户 UI 请求 > agent 批量操作。

**A5. macOS 公证 / Windows 签名**：运行时 GitHub Release 下载路线会被 Gatekeeper/SmartScreen 拦截。推论：优先 platform-specific VSIX 打平二进制（走 Marketplace 信任链），运行时下载只作离线兜底，且兜底路线需提前核算 Apple Developer ID 公证的年费成本。

### 可优化点

**B1. 分页游标的快照语义**：滚动中 git pull 导致列表位移，分页 token 若不绑定状态会渲染错乱。token 绑定 epoch；epoch 变化时 token 失效，发 listInvalidated 通知，前端保留视口锚点 commit 重对齐（不回顶部）。

**B2. Undo 的持久化与分级实现**：
- undo journal 落盘到 `.git/git-workbench/undo/`（引擎私有状态目录：缓存索引、undo、审计日志）——跨引擎重启存活
- 两级实现：refs/HEAD 级用逆操作脚本（reset 到记录 SHA，秒级）；工作区级用 git stash 机制本身。不复制文件
- undo 前校验 epoch：外部变更发生过后相关事务自动标记不可回滚，明确告知而非硬回滚造成更大破坏

**B3. LLM token 经济学**：引擎专门提供 repo_digest 工具——为 LLM 优化的紧凑状态摘要（分支拓扑 + ahead/behind + 未提交改动统计），不让 agent 自己拼 git log 原始输出。进 T3 设计核心。

**B4. fs 监听选型**：直接采用 watchexec 库（成熟处理 Linux inotify 上限 / macOS FSEvents 延迟去重 / Windows ReadDirectoryChangesW 三平台长尾）+ .git 内部目录定向监听（refs/、index、sequencer 状态——后者顺便解决"检测用户终端里正在进行 rebase"）。

**B5. 审计日志**：LLM agent 全部操作落 append-only 审计日志（谁、何时、操作、结果），作为 T1 undo 引擎的伴生组件（已定，成本极低价值高）。

### 范围决策（已定）

| 决策项 | 结论 |
|---|---|
| submodule | T1 只做检测 + 作为普通条目显示（不递归）；完整管理放 T2 之后 |
| 认证 | T1 继承 credential helper + SSH；T2 GitHub 设备码授权流；PAT 按需 |
| 审计日志 | T1（undo 引擎伴生组件） |

### 修订风险优先级

A1（持久化缓存一致性）、A2（gix 失效/mmap）、A4（agent 与人并发）为一等公民——影响核心数据结构设计，pan-plan 必须正面处理。

### 追加 PoC 清单

- [ ] epoch 机制 spike：CLI 写 → gix 缓存强制重载的正确性验证
- [ ] 持久化缓存 spike：100k commit 冷启动首屏实测（有/无缓存对比）
- [ ] mmap 与并发 gc 的安全行为实测（必要时收敛为"写操作期间降级读 CLI"）
- [ ] 操作租约 + 队列优先级的并发场景验证
- [ ] known_hosts / GIT_ASKPASS 交互转发到前端 UI 的验证
