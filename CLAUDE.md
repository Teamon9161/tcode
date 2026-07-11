# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目概述

tcode 是一个类 Claude Code / Codex 的 Rust agent harness CLI。**`plan.md` 是权威设计文档**（含设计原则、已实现里程碑 M0–M4、下一步 M5 提案），改动涉及架构决策时先读它。

两条贯穿全局的设计约束，改代码时不可违背：

1. **零猜测原则**：模型不应花 token 获取 harness 本来就知道的信息。工具错误信息要自愈（附候选/建议）、文件重复读返回 stub、中断后注入精确状态说明。
2. **缓存命中由类型系统保证**：`Ledger` 是 append-only 的，历史只有 `append` / `truncate_tail`（rewind）/ `compact` 三个合法操作。任何"改前文"的新需求都必须经 compact 语义，不得绕过。

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
- **`tcode-providers`** — `anthropic.rs` / `openai.rs` / `chatgpt.rs`（Codex Responses API）+ `retry.rs`（watchdog：chunk 级 idle 超时 + 指数退避）。入口 `build_active(profile, selection, watchdog)`。
- **`tcode-tools`** — 内置工具，`builtin_tools()` 组装；Windows 上 PowerShell 为主、检测到 Git Bash 才加 `bash` 工具。`task.rs` 是 sub-agent（独立 ledger、受限工具集），`skills.rs` 发现 `.tcode/skills` / `.claude/skills`，`grounding.rs` 生成开局项目地图（有严格 token 预算，改动需守住上限），`web.rs` 是 `web_fetch`（htmd 转 markdown，跨域重定向不自动跟）+ `web_search`（DDG HTML 解析，无 key）。后台任务：`shell` 的 `run_in_background` 进 `tcode-core/background.rs` 注册表，`read_output` 认 `b1` 类 id，`kill_task` 停任务，完成通知由 agent loop 在安全边界 append `Entry::Note`。
- **`tcode-tui`** — ratatui inline 渲染：定型内容经 `insert_before` 进原生 scrollback，只有未定型内容（流式块、状态行、输入框、对话框）在底部 viewport 渲染——这是避免闪烁的关键纪律，勿破坏。
- **`src/main.rs`** — clap CLI 装配 + system prompt（`IDENTITY`）+ 非 TTY 的 REPL/plain 路径（`approver.rs` / `printer.rs`）。

## 配置与运行时路径

- `~/.tcode/config.toml`：profile/模型/权限规则，永远手写（首启向导生成初版）。
- `~/.tcode/state.toml`：当前 profile/model/effort 选择，程序只写这个文件。优先级 CLI flag > state > config。
- `.tcode/config.toml`：项目级 hooks、权限规则与 MCP server（`[mcp_servers.名字]` command/args/env，工具注册为 `mcp__名字__工具`，权限规则按该名字匹配）。
- 持久上下文分两类：用户/项目指令由人维护，自动记忆由模型维护，二者禁止混写。项目指令从项目根到目标目录逐层加载，每层按 `.tcode/AGENTS.md` > `AGENTS.md` > `CLAUDE.md` 取第一个；访问其他子目录或显式标记的外部项目时按工具目标路径懒加载。自动记忆位于 `~/.tcode/projects/<project-id>/memory/`，`MEMORY.md` 只做精简索引。
- 会话/checkpoint/blob：`~/.tcode/projects/<cwd-hash>/{sessions,checkpoints,blobs}/`。
- API key 经 `api_key_env` 指环境变量，不落盘。

## 改动时的验收习惯

真实 API 端到端验证时盯状态行的 cache_read 占比——连续 turn 应接近前缀全长，下跌即缓存回归。任何进缓存前缀的内容（system prompt、项目地图、skills 列表）都有字符/条目预算，加内容前先看现有上限。
