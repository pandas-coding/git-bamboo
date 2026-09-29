---
title: webview↔host 是双向信任边界，host 侧必须校验入站消息
date: 2026-09-30
category: pattern
plans:
  - .ai-workflow/plans/20260929-vscode-ux-polish-round1-phase-1.md
tags: [vscode-extension, webview, security, trust-boundary, input-validation]
---

# webview↔host 是双向信任边界，host 侧必须校验入站消息

## Context

git-bamboo 的 Commit Graph webview 与 extension host 双向 postMessage：webview 渲染不可信的仓库数据（commit message、ref 名），host 接收 webview 消息并持久化配置、驱动引擎 RPC（createBranch/switchBranch）。

## Insight

VS Code webview 是一个完整的不可信执行环境（可被注入的 HTML/JS），"数据不可信"的原则必须**双向**落实：host→webview 方向做 textContent-only 渲染的同时，webview→host 方向的每条消息也要在 host 侧校验后再进入持久化存储或引擎调用。webview 自己做的 clamp/过滤不算校验——那是不可信方自己的行为。

## Evidence

Phase 1 计划通篇强调 webview 行填充 textContent-only（出站方向），但对入站消息零校验：review 抓出 host 把 webview 传来的 width 原样写入 durable 的 globalState（字符串/对象/NaN/负数都能持久化，每次激活读回）、右键目标 `commit.id` 不校验格式即可作为 `createBranch` 的 base、停靠目标永不过期（`when: false` 只隐藏 palette，其他扩展仍可 `executeCommand` 调用）。修复：width 校验 finite + 范围 [44,240]；`commit.id` 匹配 `/^[0-9a-f]{40,64}$/i`；目标加 30s TTL；payload 删掉未使用字段（少传少信）。

## Recommendation

- host 处理 webview 消息时逐字段校验类型、范围、格式（正则），校验失败静默忽略即可——webview 是自己人，但按敌人对待。
- 写入 durable 存储（globalState/secrets）前的校验要格外严格：脏值会被未来每次激活读回。
- 传递"待执行动作目标"（如右键菜单停靠目标）时加短 TTL，并在消费时视为一次性（take-then-clear）。
- payload 只带消费方实际需要的字段——计划阶段规定的字段集若实现后发现无人使用，删除而不是留着。
