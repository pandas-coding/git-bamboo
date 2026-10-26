---
title: 网络操作增强 Phase 5：hunk 级暂存 + stash 管理 UI
date: 2026-10-26
status: ready
ideas:
  - .ai-workflow/ideas/20261025-network-ops-enhancements.md
group: network-ops-enhancements
phase: 5
tags: [git, rust, vscode-extension, ui]
---

# 网络操作增强 Phase 5：hunk 级暂存 + stash 管理 UI

## Goal

日常提交流程死角消除：在 diff 视图中按 hunk（阶段一不含逐行）暂存/取消暂存；提交视图新增 stash 区（列表/create/pop/apply/drop），stash pop 冲突走 Phase 3 冲突栏。此后用户无需回退 VS Code 内置 git 视图完成日常操作。

## Background

见 idea 文件。用户决策：hunk 先行、逐行暂存后置（后续轮次）；stash pop 冲突与 pull 冲突共用冲突流。引擎 Phase 1 已有 stash RPC 与 `STASH_CONFLICT` 错误码；暂存现状为整文件级（`git add --` / `git reset HEAD --`，`write_queue.rs` L285-320），无任何 hunk 基础设施（greenfield）。

## Research Summary

- 现有暂存链路：webview `commit.js`（`{type:'stage'|'unstage', paths}` 消息）→ host `commitView.ts` `changePaths` L187 → engine Stage/Unstage（`write_queue.rs` L285-320，`:(literal)` pathspec L388-392）。
- diff 打开链路：提交视图文件点击 → ENGINE_SCHEME diff URI（`headContent.ts` 内容提供者，getBlob 按 revision 取内容）。
- **外部研究（关键取舍）**：VS Code git 的 hunk 暂存**不用** `git apply --cached`（"patch does not apply" 上下文不匹配陷阱），而是计算选中行变化应用后的完整文件内容，写入 index blob（`git hash-object -w` + `git update-index --cacheinfo <mode> <hash> <path>`）—— 对任意行区间都成立。本计划采用此方案且**由引擎侧计算**（客户端只传行区间，不传内容 —— 内容来自客户端会扩大信任面）。
- stash 交互研究：create 支持 message/untracked/staged-only；pop/apply `--index` 恢复暂存态；**冲突的 pop 不丢弃 stash**（引擎 -32008 已暴露）；GitStudio/GitKraken 的 stash 区形态 = 列表 + 每条目 pop/apply/drop。
- 学习教训：host 侧每条新消息验证（路径合法、区间数值范围钳制）；消费时新鲜校验（diff 可能已过期）；"第二个消费者出现时抽取共享模块"。

## Steps

1. **hunk diff 读 RPC** — `crates/protocol/src/lib.rs` 新增 `getDiff { path, staged: bool }` 读 RPC：`staged=false` → 工作区 vs HEAD；`staged=true` → index vs HEAD。响应为 hunk 列表（各含新旧行区间 + 变更行内容）。实现落 `gix_read.rs`（gix blob diff 或 shell `git diff -U3 -- <path>` 解析，禁止 host 侧 diff）。path 过 `validate_path_arg`（write_queue.rs L90）。

2. **stageRanges/unstageRanges 引擎 RPC** — `WriteCommand` 新增 `StageRanges { path, ranges }`（ranges = 新文件侧行区间，引擎自行与 getDiff 的 hunk 求交）：
   - **StageRanges**：读 HEAD blob → 应用选中变更（引擎侧计算得到"HEAD + 选中 hunk"的完整内容）→ `git hash-object -w --stdin --path` → `git update-index --add --cacheinfo <mode>,<hash>,<path>`。**不用 `git apply --cached`**。
   - **UnstageRanges**：对称（index 内容回退选中变更后写回 index blob）。
   - 执行前新鲜校验：重新计算当前 diff，若选中区间与最新 hunk 不再相交则返回 `INVALID_ARGUMENT`（diff 过期，客户端重新拉取）—— 与 discard 的新鲜分类语义一致。
   - **undo 决策：bypass journal**（index blob 重构无对应快照语义，理由写入代码注释，模式同 discard）。

3. **diff 视图 hunk 动作** — 扩展侧：提交视图打开文件 diff（现有链路）时，注册 CodeLens/装饰 provider 在每个 hunk 首行显示动作：未暂存文件 → [Stage hunk]；已暂存文件（index vs HEAD diff）→ [Unstage hunk]。点击 → host 组装 `stageRanges { path, ranges }`（区间来自该 hunk 的新侧行号）→ RPC → `commitView.refresh()`。区间数值 host 侧钳制（非负、start ≤ end、上限内）。
   - CodeLens 方案若在 diff 编辑器（只读侧）不可用，备选：hunk 行 gutter 装饰 + 命令 URI；实现时二选一并记录取舍。

4. **stash 区（webview）** — `commit.html`/`commit.js`：提交视图新增 stash 区（列表：message + 相对时间；空态提示 + [Create Stash] 按钮）。动作：create（message 输入 + include-untracked/include-staged-only 选项）、每条目 [Pop][Apply][Drop]、pop/apply 附 `--index` 恢复暂存态开关（默认开）。数据源 `stashList` 读 RPC。

5. **stash 区（host 镜像）** — `commitView.ts`：新增消息 `stashCreate`/`stashPop`/`stashApply`/`stashDrop`/`stashRefresh` 处理 — host 验证（stash ref 匹配 `stash@{N}`、message 长度上限、枚举开关为布尔）→ 调 Phase 1 RPC → refresh。右键/点击目标消费时新鲜校验（stash 列表可能已变，复用 takeContextTarget 模式：ref 仍在最新列表才执行）。

6. **stash 冲突接入冲突栏** — stash pop/apply 返回 -32008 → 错误通知 "Stash applied with conflicts — the stash entry is kept" + [Open Conflict Files] → Phase 3 冲突栏（冲突文件在工作区，冲突栏由 status 刷新自然出现）。用户解决后手动 drop（列表中条目仍可见，Drop 有确认 modal —— 破坏性操作）。

7. **逐行暂存后置声明** — 本期不实现行级区间选择 UI；StageRanges 的引擎接口按"任意行区间"设计（blob 构造方案天然支持），行级只差前端选区传递 —— 为后续轮次留好接口，不在本期做 UI。

8. **测试** — 引擎：getDiff hunk 解析（新增/修改/删除/重命名/二进制）、StageRanges 后 `git status` 与 `git diff --cached` 内容断言（含部分 hunk 暂存、hunk 交叉过期拒绝）、UnstageRanges 对称、bypass undo 注释；扩展：tsc + 手工冒烟（改两个 hunk 只暂存一个 → commit 只含该 hunk；stash 全流程含冲突 pop）。

## Acceptance Criteria

- [ ] diff 视图中每个 hunk 有 Stage（或 Unstage）动作；暂存后该 hunk 从工作区 diff 消失、出现在 staged。
- [ ] 只暂存部分 hunk 后提交，提交内容恰好只含选中 hunk（`git show` 验证）。
- [ ] diff 过期（暂存后文件又被修改）时区间请求被引擎拒绝而非错误应用。
- [ ] 提交视图 stash 区：create（含 untracked 选项）→ 列表出现 → pop 恢复改动；apply 保留条目；drop 有确认。
- [ ] stash pop 冲突 → 冲突栏出现、stash 条目仍在、通知明确"stash retained"。
- [ ] 每条新增 webview→host 消息有 host 验证；stash 目标消费时新鲜校验。
- [ ] `cargo test` + tsc 通过。

## Dependencies

- Phase 1（stash RPC、错误码 -32008、WriteCommand 模式）。
- Phase 3（冲突栏，stash pop 冲突的解决路径）。
- 提交视图结构（20260930 phase-3/4）与 diff 打开链路（headContent.ts）。

## Related Documents

- .ai-workflow/ideas/20261025-network-ops-enhancements.md
- .ai-workflow/plans/20261025-network-ops-enhancements-phase-1.md
- .ai-workflow/plans/20261025-network-ops-enhancements-phase-3.md
