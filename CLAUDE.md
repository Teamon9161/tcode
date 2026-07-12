# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目概述

tcode 是一个类 Claude Code / Codex 的 Rust agent harness CLI。**`plan.md` 是权威设计文档**（设计原则、贯穿机制、已实现里程碑与未决项），改动涉及架构决策时先读它。本文件承载改代码时必须遵守的规则与项目结构索引。

两条贯穿全局的设计约束，改代码时不可违背：

1. **零猜测原则**：模型不应花 token 获取 harness 本来就知道的信息。工具错误信息要自愈（附候选/建议）、文件重复读返回 stub、中断后注入精确状态说明。
2. **缓存命中由类型系统保证**：`Ledger` 是 append-only 的，历史只有 `append` / `truncate_tail`（rewind）/ `compact` 三个合法操作。任何"改前文"的新需求都必须经 compact 语义，不得绕过。

## 改动勿回退的硬规则

除上面两条约束外，以下已固化，改动不得破坏（design 层面的"为什么"见 `plan.md`）：

**上下文与缓存**

- 进缓存前缀的内容都有预算：system prompt、项目地图（80 项/目录、20 子项、16 KiB）、skills 列表（200 字符/6k）——加内容前先看现有上限。
- 工具输出不可无门直灌 ledger：大输出必须过 blob 预算门并落 scratch 文件（曾因 `gates_output=false` 让 grep 单条巨行撑爆 context）。
- token 两个量纲不可混：context 表 = 单次请求的完整 prompt（缓存+未缓存）= 当前窗口占用；turn 汇总 = 本轮**未命中的 `input_tokens`** + cache%。勿用 `total_input()` 把缓存前缀按请求次数重复累加；运行时状态行 `↓ ~N tok` 走 `token_count`。
- 真实 API 端到端验证时盯状态行 cache_read 占比：连续 turn 应接近前缀全长，下跌即缓存回归。

**TUI 渲染（`tcode-tui`）**

- transcript 是唯一事实源、屏幕只是视图；alternate screen 为唯一路径（inline 已删，非 TTY 走 plain）。
- wrap 只算一次：每块缓存当前宽度的 wrap，resize 才失效；流式追加只重排最后一块。
- 只渲染可见切片：前缀和二分定位视口起点，每帧 O(视口高度)，与转录总长无关。
- ratatui 双缓冲 diff 最小化终端写入，帧外包 crossterm synchronized update 防撕裂；重绘按事件驱动 + 250ms tick 合并。
- wrap 必须展开 tab：工具输出 `行号\t内容` 的 tab 宽度测 0 却占 buffer cell，滚动残留浮字；`transcript.rs::wrap_lines_flagged` 按 8 列制表位展开成空格，勿改回裸 tab。
- 折叠输出默认：read/grep/glob 转录里默认只显示折叠摘要，不铺开首行。
- 批量渲染 item 紧跟自己的 result：批次 header 后每个 call 的 `├ 摘要`(+diff) 推迟到自己的 `ToolEnd` 再 bake（`PendingCall.header`），live 与 replay 一致。

**其他**

- update_plan 不套骨架：多数任务不必 plan；要 plan 时步骤按真实结构增量维护，同时只一个 in_progress，做完即标 completed。

## 常用命令

```powershell
cargo build --workspace          # 构建
cargo test --workspace           # 全部测试（不打真 API）
cargo test -p tcode-core         # 单 crate
cargo test -p tcode-tools --test agent_loop          # agent loop 集成测试
cargo test -p tcode-core ledger::tests::某测试名      # 单个测试
cargo run                        # 启动 TUI（非 TTY 自动降级为 plain 模式）
cargo clippy --workspace
cargo fmt
```

测试策略：核心机制（ledger、freshness、blobs、权限、hooks）用内联单元测试；agent loop 用 `MockProvider` 脚本化 `StreamEvent` 序列驱动真实工具跑真实临时目录（`crates/tcode-tools/tests/agent_loop.rs`）；provider 的 SSE/wire 格式在 `crates/tcode-providers/tests/wire.rs`。测试永不调真实 API。

## 架构

Workspace 四个 crate + 根 binary，**依赖方向单向：core 不知道 UI 存在**。

- **`tcode-core`** — 所有核心抽象与机制：
  - `ledger.rs`：append-only 上下文账本（缓存命中的根基）。
  - `agent.rs`：agent loop（`Agent` / `Session` / `AgentEvent`）；loop 内顺序为 权限 → pre_tool_use hook → checkpoint → tool.run → post_tool_use hook → append。
  - `provider.rs`：`Provider` trait，统一流事件 `StreamEvent`（TextDelta/ToolUseStart/Usage/…），两家 API 差异在 provider 内部消化；`CacheStrategy` 区分 Anthropic 显式断点与 OpenAI 隐式前缀。`ModelCell` 是 Agent 与 TaskTool 共享的 RwLock 模型句柄，支撑 `/model` 热切换。
  - `tool.rs`：`Tool` trait + `ToolCtx`（cwd、freshness tracker、checkpoint、blob store、cancellation、事件通道）。
  - 支撑机制：`freshness.rs`（文件重复读去重）、`blobs.rs`（大输出分页，预算门）、`checkpoint.rs`（写前文件快照，供 rewind 回滚）、`store.rs`（JSONL 事件日志 = 会话持久化，resume 是重放）、`external.rs`（导入 Codex / Claude Code 会话，只读复制，`Entry::ImportedTool` 只进转录不进 prompt）、`codex.rs`（ChatGPT 凭证复用 `~/.codex/auth.json`）。
- **`tcode-providers`** — `anthropic.rs` / `openai.rs` / `codex.rs`（Codex Responses API，凭证复用 `tcode-core/codex.rs` 读 `~/.codex/auth.json`）+ `retry.rs`（watchdog：chunk 级 idle 超时 + 指数退避）。入口 `build_active(profile, selection, watchdog)`。
- **`tcode-tools`** — 内置工具，`builtin_tools()` 组装；Windows 上 PowerShell 为主、检测到 Git Bash 才加 `bash` 工具。`task.rs` 是 sub-agent（独立 ledger、受限工具集），`skills.rs` 发现 `.tcode/skills` / `.claude/skills`，`grounding.rs` 生成开局项目地图（有严格 token 预算，改动需守住上限），`web.rs` 是 `web_fetch`（htmd 转 markdown，跨域重定向不自动跟）+ `web_search`（DDG HTML 解析，无 key）。后台任务：`shell` 的 `run_in_background` 进 `tcode-core/background.rs` 注册表，`read_output` 认 `b1` 类 id，`kill_task` 停任务，完成通知由 agent loop 在安全边界 append `Entry::Note`。
- **`tcode-tui`** — 自绘全屏（alternate screen）：内存 `transcript.rs` 单一事实源，事件溯源消费者模型（渲染/`/export`/resume 重放同一接口）。markdown + syntect 高亮、similar 红绿 diff、鼠标全套；UI 事件循环与 agent loop 是两个 tokio task，仅 mpsc 通道通信。渲染性能纪律见上方硬规则。
- **`src/main.rs`** — clap CLI 装配 + system prompt（`IDENTITY`）+ 非 TTY 的 REPL/plain 路径（`approver.rs` / `printer.rs`）。

## 配置与运行时路径

- `~/.tcode/config.toml`：profile/模型/权限规则，永远手写（首启向导生成初版）。
- `~/.tcode/state.toml`：当前 profile/model/effort 选择，程序只写这个文件。优先级 CLI flag > state > config。
- `.tcode/config.toml`：项目级 hooks、权限规则与 MCP server（`[mcp_servers.名字]` command/args/env，工具注册为 `mcp__名字__工具`，权限规则按该名字匹配）。
- 持久上下文分两类：用户/项目指令由人维护，自动记忆由模型维护，二者禁止混写。项目指令从项目根到目标目录逐层加载，每层按 `.tcode/AGENTS.md` > `AGENTS.md` > `CLAUDE.md` 取第一个；访问其他子目录或显式标记的外部项目时按工具目标路径懒加载。自动记忆位于 `~/.tcode/projects/<project-id>/memory/`，`MEMORY.md` 只做精简索引。
- 会话/checkpoint/blob/scratch：`~/.tcode/projects/<cwd-hash>/{sessions,checkpoints,blobs,scratchpad}/`。溢出输出与后台日志落 `scratchpad/tool-output/`。
- API key 经 `api_key_env` 指环境变量，不落盘。
