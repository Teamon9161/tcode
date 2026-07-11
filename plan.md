# tcode — Rust Agent Harness CLI 实施计划

> 注：批准后第一步会把本计划复制为 `C:\code\rust\tcode\plan.md`（用户要求放在项目根目录）。

## Context

从零构建一个类 Claude Code / Codex 的 agent harness CLI，取两者之长：

- **要 Claude Code 的**：精致的终端观感、权限确认 Tab 补充意见、丰富工具集、per-tool hooks、sub-agent、双击 Esc 回退、checkpoints。
- **要 Codex 的**：绝不无故卡死——流式请求全程 watchdog + 状态行永远知情。
- **不要的**：Codex 的沙箱式能力阉割。
- **核心约束**：省 token、缓存命中率最大化。上下文一旦写入绝不回改，用类型系统强制，不靠纪律。

已确认决策：v1 双 Provider（Anthropic + OpenAI 兼容）；inline 渲染为主（Renderer 做成 trait，全屏模式 v2）；含 resume、compact、分层项目指令（详见 M5 memory 2.0）；双 Shell 以 PowerShell 为主；v1 含文件 checkpoints + rewind；分支只记录进事件日志，浏览 UI 放 v2。

## 第一性原则：零猜测原则

**模型不应该花任何 token 去获取 harness 本来就知道的信息。** 模型的注意力应全部在任务上，而不是在推断 harness 的状态上。下面多个特性都是这一条原则的实例，不是各自独立的功能：

| 实例 | 消灭的浪费 |
|---|---|
| 中断契约 | 中断后模型自发重新验证文件状态 |
| 文件新鲜度追踪 | 长会话重复读未变动的文件 |
| **自愈式工具错误** | 工具失败后模型花额外 turn 定位原因：edit 的 old_string 不唯一 → 错误信息直接附候选位置上下文；read 路径不存在 → 附相近路径；命令不存在 → 附建议。**省一个 turn = 省一次完整前缀读取**，是最大宗的 token 节约 |
| **开局项目地图** | 每个会话开头仪式性的 ls / git status / 读 README：启动时采集目录树两层 + git 状态，注入 system prompt 尾部，进缓存前缀，一次成本 |
| **尾部自知一行** | 模型不知道剩余上下文，无法自主决定 compact 或改派 sub-agent：每条最新用户消息附 `ctx 61% · mode: default · since-compact 34k`，附在尾部所以缓存安全 |

## 与 Claude Code / Codex 的差异化设计（本项目的特有想法）

1. **类型强制的 append-only Ledger**——缓存命中不是"尽量"，而是编译期保证：历史只有三个合法操作 `append` / `truncate_tail`（rewind）/ `compact`（显式断点原子重写），全部缓存友好。
2. **中断契约（Interrupt Contract）**——Esc 中断时，harness 注入一条精确状态说明：哪些 tool call 完成、哪些被取消、文件是否被改动。消灭"中断后模型自发去重新验证文件"的 token 浪费。
3. **文件新鲜度追踪（Freshness Tracker）**——harness 记录每个已读文件的 (path, mtime, hash, 读取范围)。模型重复读未变动的文件 → 返回一行 stub"内容未变，见前文"；文件被外部改动 → 才返回新内容并附说明。长会话反复读同一文件的浪费在 harness 层直接消掉。
4. **缓存回归哨兵**——每 turn 状态行显示 cache_read/cache_write/in/out token；连续 turn 的 cache_read 占比异常下跌时显式警告。缓存退化当场可见，而不是月底看账单。
5. **事件溯源 UI**——会话 = JSONL 事件日志，渲染器只是事件流的消费者（Renderer trait）。inline/全屏/transcript 导出/resume 重放是同一机制的四个消费者，不是四套代码。
6. **Blob store 输出分页**——所有工具输出过统一 token 预算门，超限部分存 blob store，上下文只进"预览 + 句柄"，配 `read_output` 工具分页取。

## 三个贯穿全局的机制

### 1. Append-only Context Ledger

```rust
pub struct Ledger {
    entries: Vec<Entry>,          // private
    compaction_base: usize,
}

impl Ledger {
    pub fn append(&mut self, e: Entry);
    /// rewind: 截断尾部。前缀不动, 缓存仍命中
    /// (最多从最近一个缓存断点向后重建一小段)。
    pub fn truncate_tail(&mut self, to: EntryId);
    /// 唯一"改前文"的操作: 原子替换 [0, n) 为 Summary,
    /// 一次性付缓存代价, 之后前缀重新稳定。
    pub fn compact(&mut self, summary: Summary, upto: usize);
    pub fn as_messages(&self) -> Vec<Message>;
}
```

- System prompt + 工具定义会话内定死不变。
- Anthropic：`cache_control` 断点——system+tools 后固定一个，消息尾部滑动一个，控制在 4 断点预算内。
- OpenAI 兼容：隐式前缀缓存，append-only 天然命中。
- Compact 仅显式触发（`/compact` 或 token 逼近上限），子请求生成摘要。

### 2. Stream Watchdog + 永远知情的状态行

- chunk 级 idle 超时（默认 30s 无字节 → 取消 → 指数退避重试，429/5xx/超时可重试）。
- 状态行实时显示：`thinking 12s · ↑3.2k` / `writing · ↑1.8k tok`（流式 delta 实时累计，用户看得到模型在动）/ `retrying (2/3) in 4s` / `running: cargo build 45s`。无任何静默状态。
- Esc 单击取消当前 turn（走中断契约），双击进入 rewind。

### 3. Rewind + Checkpoints（双击 Esc）

- 双击 Esc → 列出最近的用户输入点 → 选择后 `truncate_tail`。缓存分析：截尾不改前缀，安全。
- **文件 checkpoint**：每次 write/edit 执行前，把原文件按 (session, entry_id) 存入 `~/.tcode/projects/<hash>/checkpoints/`。回退时询问"仅对话 / 对话+文件一起回"。选"仅对话"时由 Freshness Tracker 提醒模型磁盘上有未回滚的改动。
- 事件日志不删旧事件，追加一条 `Rewind { from, to }` 记录 fork；分支浏览 UI 放 v2，数据 v1 就齐。

## Workspace 结构

```
tcode/
├── Cargo.toml                 # workspace
├── crates/
│   ├── tcode-core/            # ledger, agent loop, Tool/Provider/Renderer trait,
│   │                          # permissions, hooks, session, freshness tracker, checkpoints
│   ├── tcode-providers/       # AnthropicProvider, OpenAiProvider
│   ├── tcode-tools/           # 内置工具
│   └── tcode-tui/             # ratatui inline 渲染器
└── src/main.rs                # clap CLI, 装配
```

依赖方向单向：core 不知道 UI 存在。

## 核心 trait

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    /// 统一流事件: TextDelta / ThinkingDelta / ToolUseStart / ToolUseDelta /
    /// Usage / Done / Error。两家 API 差异在 provider 内部消化。
    async fn stream(&self, req: Request, cancel: CancellationToken)
        -> Result<BoxStream<'static, StreamEvent>>;
    fn cache_strategy(&self) -> CacheStrategy;  // ExplicitBreakpoints | ImplicitPrefix
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> serde_json::Value;
    fn permission(&self, input: &serde_json::Value) -> PermissionRequest;
    async fn run(&self, input: serde_json::Value, ctx: &ToolCtx) -> ToolOutput;
}

/// UI 是事件流的消费者。v1: InlineRenderer; v2: FullscreenRenderer;
/// transcript 导出和 resume 重放走同一接口。
pub trait Renderer {
    fn on_event(&mut self, e: &SessionEvent);
}
```

`ToolCtx`：cwd、Freshness Tracker、checkpoint 写入、blob store、cancellation token、事件上报通道。

## v1 工具集

| 工具 | 要点 |
|---|---|
| `read` | offset/limit + 行号；大文件截断提示分页；**经 Freshness Tracker 去重** |
| `write` / `edit` | edit = 精确字符串替换，要求先 read（mtime 校验防写脏）；执行前存 checkpoint；渲染红绿 diff |
| `shell` | Windows: PowerShell 为主 + 检测到 Git Bash 时提供 `bash`；Unix: bash。超时控制 |
| `grep` / `glob` | 内嵌 grep-searcher/ignore/globset，无外部依赖，默认 head_limit |
| `read_output` | blob store 分页读取被截断的大输出 |
| `task` | sub-agent：注册表选类型（v1 内置 `general` + 只读 `explore`），独立 ledger，受限工具集，tokio task 并发，结果作为 tool result 返回 |

## 权限系统

**模式**（Shift+Tab 循环，状态行常显）：

| 模式 | 行为 |
|---|---|
| `plan` | 只读工具放行，写/执行全部拦截 |
| `default` | 按规则匹配，未命中则逐个询问 |
| `accept-edits` | 文件编辑自动放行，shell 等仍询问 |
| `auto` | 全部放行（deny 规则仍生效） |

**规则**：global + project 两级 `config.toml`，`allow` / `deny` / `ask` 列表，匹配 `工具名(参数 pattern)`，如 `shell(git status*)`、`shell(cargo *)`、`edit(src/**)`。交互中选 "Yes, don't ask again" 自动写入 project 规则。

**Tab 补充意见**：确认对话的任何选项上按 Tab 展开内联输入框：

- "Yes + 意见" → 批准执行，意见作为 user message 追加在 tool result 后，模型立刻小幅调整。纯 append，缓存安全。
- "No + 理由" → 拒绝原因进上下文，模型不用猜。

## Hooks

`config.toml` 按事件 + 工具 matcher 触发外部命令，JSON 走 stdin/stdout（语义对齐 Claude Code，迁移心智低）：`pre_tool_use`（可 block/改参）、`post_tool_use`、`turn_end`、`session_start`。例：对 `edit` 单独挂 formatter。

## 配置与项目记忆

```
~/.tcode/config.toml      # provider profiles (model, base_url, api_key_env), 全局权限规则
~/.tcode/AGENTS.md        # 用户级持久指令（所有项目加载）
.tcode/config.toml        # 项目级: hooks, 权限规则
项目指令: 每层目录按 .tcode/AGENTS.md > AGENTS.md > CLAUDE.md 取第一个，祖先到目标逐层叠加
自动记忆: ~/.tcode/projects/<project-id>/memory/{MEMORY.md,topic files}
会话/checkpoint/blob: ~/.tcode/projects/<cwd-hash>/{sessions,checkpoints,blobs}/
```

密钥经 `api_key_env` 指环境变量，不落盘。

## TUI

**调研结论**：Claude Code 默认 inline（内容进原生 scrollback）+ 实验性全屏模式（`NO_FLICKER`，解决闪烁/内存但丢 scrollback）；Codex 用 alternate screen，正因丢 scrollback 被诟病而重做 tui2。→ v1 做 inline（ratatui `Viewport::Inline` + `insert_before`），"未定型内容"（spinner、流式中的块）只在底部固定区渲染，定型后才 `insert_before` 进 scrollback——这是避免 inline 闪烁的关键纪律。全屏渲染器 v2 加，靠 Renderer trait 零重构。

- Markdown 渲染 + syntect 高亮 + similar 红绿 diff。
- 输入框：多行编辑、历史、slash 命令补全弹层（`/compact` `/resume` `/model` `/cost` `/rewind`…）。
- **图片粘贴**：Ctrl+V 时经 arboard 读系统剪贴板——有图 → 存会话临时目录，输入框显示 `[image #1]` 占位，发送时作为 image content block（两家 API 均支持）。Windows/macOS 原生支持；Linux 需 X11/Wayland 后端（arboard 均覆盖，Wayland 下失败时提示装 wl-clipboard）。粘贴的是图片文件路径时同样识别为图片附件。
- 大段文本粘贴折叠为 `[pasted #N lines]`。
- 并发模型：UI 事件循环与 agent loop 是两个 tokio task，仅 `mpsc` 通道通信，无共享可变状态。
- 非 TTY（管道/CI）自动降级为 plain 输出模式。

## Agent Loop

```
loop {
    req = ledger.as_messages() + tools + cache 断点
    stream = provider.stream(req)            // watchdog 包裹
    渲染 deltas (状态行实时累计 token); 收集 tool_use
    if 无 tool_use: break
    for call in tool_uses:                   // 独立调用可并行
        权限 (模式 → 规则 → 交互, 可带 Tab 意见)
        hooks.pre_tool_use
        checkpoint (若为写操作)
        output = tool.run()                  // freshness 去重 + token 预算门
        hooks.post_tool_use
        ledger.append(result + 可选用户意见)
}
// Esc: cancel → 中断契约注入精确状态 → ledger 保持对 API 合法
```

## 主要依赖

tokio, reqwest(rustls, stream), serde/serde_json, ratatui, crossterm, arboard, syntect, similar, grep-searcher, ignore, globset, clap, toml, tracing, uuid, dirs, sha2, anyhow, thiserror, async-trait, tokio-util, futures。

## 里程碑（每个可运行、可验证）

1. ✅ **M0 骨架 + Provider**：workspace、config、双 Provider（SSE 解析、watchdog、重试）、行式 REPL 流式对话。→ 验证：两后端各跑对话；断网/挂代理验证 watchdog。
2. ✅ **M1 工具 + loop**：Tool trait、六个工具（含自愈式错误信息）、ledger、agent loop、权限模式+规则（行式确认）、blob store 预算门、Freshness Tracker、中断契约、开局项目地图、尾部自知一行。→ 验证：真实任务"读→改→跑测试"；故意让 edit 失败观察模型无需额外 read 即修复；中断后观察模型不做无效验证；重复读同文件返回 stub。
3. ✅ **M2 TUI**：inline 渲染器、markdown/高亮/diff、输入框、slash 命令、**权限选项+Tab 意见**、状态行（token 实时计数 + 缓存遥测）、图片/长文本粘贴。→ 验证：approve+意见流程；连续 turn 的 cache_read 占比接近前缀全长；贴图发给模型描述。
4. ✅ **M3 持久化 + rewind + sub-agent + hooks**：JSONL 事件日志、`--continue`/`--resume`、双击 Esc rewind + 文件 checkpoint 回滚、task 工具 + explore agent、hooks、`/compact` + 自动 compact。→ 验证：resume 续任务；rewind 后文件恢复且缓存仍命中（看遥测）；edit 挂 formatter hook。
5. ✅ **M4 打磨**：中断边角（半截 tool_use 合法化）、Windows Terminal/conhost 兼容、缓存回归哨兵、`/cost`、错误信息。

## 计划外已实现（2026-07）

- **多模型 profile + chatgpt provider + 首启向导 + /model**（见下节"模型配置"）。
- **交互工具**：`update_plan`（TUI 常驻 plan 面板）、`ask_user`（结构化选项提问，复用审批对话框）、`add_note`。
- **外部会话导入**：`/resume` 可列出并导入 Codex（`~/.codex/sessions`）与 Claude Code（`~/.claude/projects/<dir>`）的 JSONL 会话。设计决定：
  - 导入是**只读复制**为新 tcode 会话，原文件不动；
  - 工具调用/输出映射为 `Entry::ImportedTool`，**只进终端转录、不进 prompt**（不可回放、不占上下文）；`apply_patch` 按红绿 diff 渲染；
  - 映射保持诚实：Codex `exec` → `shell(原命令)`，不再猜测 grep/read 等 tcode 工具名（假映射已删）；
  - 导入尾部附 harness note 告知模型"历史为二手、工具输出已省略、文件可能已变"（零猜测原则）；
  - 已评估 ACP：ACP 解决的是"把别的 agent 当引擎实时驱动"，不提供历史转录访问，对导入场景无帮助，维持 JSONL 解析方案。
- **OpenAI 限额显示**：状态区 5h/周限额进度条。
- **Skills**：发现 `<项目>/.tcode/skills`、`<项目>/.claude/skills`、`~/.tcode/skills`、`~/.claude/skills`（同名去重，项目 > 全局，.tcode > .claude 兼容位）；解析 SKILL.md YAML front matter（含 `description: |` 块标量折叠）。**context 预算**：每条描述截 200 字符、列表总预算 6k 字符（约 1.5k token，进缓存前缀），超出的 skill 降级为"仅名字"列出但仍可调用；调用时才读 SKILL.md 正文（过统一输出预算门）；名字错误的调用返回全部合法名字（自愈式错误）；无 skill 的项目不注册该工具，零 token 成本。
- **project_map 预算防御**：目录树全局 80 项 + 每目录 20 子项上限（单个爆炸目录不再吃光预算），git status 只列 15 个文件、超出加 `+N more` 标记，项目指令总预算 16 KiB。

## M5（下一步提案）

1. ✅ **后台任务**（2026-07 实现）：`shell`/`bash` 加 `run_in_background` 参数（模型判定，工具描述给判定准则）。实现：`tcode-core/background.rs` 的 `BackgroundTasks` 注册表挂在 `ToolCtx`（与 blobs 同构）；子进程由 supervisor tokio task 持有（`kill_on_drop`，tcode 退出不留孤儿），stdout/stderr 按行流入共享缓冲；返回 `b1` 类 task id。完成时 agent loop 在两个安全边界（下一 turn 开头 / 当前工具批结束）append `Entry::Note` 通知模型（纯 append，缓存安全，`take_completion_notes` 保证恰好一次）；`read_output` 认 `b` 前缀 id 分页读实时输出（带状态头）；`kill_task` 工具停任务（幂等，杀已结束任务不算错）。运行中任务列在尾部 `<tcode-status>` 状态行。
2. ✅ **工具差距补齐**（2026-07 实现）：`web_fetch` + `web_search`（`tcode-tools/web.rs`）。
   - `web_fetch`：reqwest 直连（30s 超时、5MB 上限），HTML 经 `htmd` 转 markdown（跳过 script/style/nav 等），json pretty、text 原样、二进制报错；重定向手动跟（≤5 跳），**跨域重定向不自动跟**——返回目标 URL 让模型显式重调，保证按域审批（descriptor `web_fetch(host)`）诚实；输出过统一 blob 预算门。
   - `web_search`：DuckDuckGo HTML 端点（`html.duckduckgo.com/html/`）scraper 解析（`div.result`/`a.result__a`/`.result__snippet`，解码 `uddg=` 重定向、跳广告），无需 API key、三家 provider 一律可用、不碰 wire 格式；descriptor 无参数（`web_search`），一次 always-allow 覆盖全部搜索。空结果与 bot-check 分开报错（自愈式）。
   - workspace reqwest 加 `system-proxy` feature：Windows 系统代理（如 Clash `127.0.0.1:7890`）下 env 变量为空时请求也走代理，providers 同样受益。
   - 测试：解析/转换纯单元测试；`tests/web_live.rs` 是 `#[ignore]` 的真实网络 smoke（`cargo test -p tcode-tools --test web_live -- --ignored` 手动跑）。
   - `notebook_edit` 暂不做（用户场景少）；`multi_edit` 不做（Claude Code 已弃用，edit 循环即可）。
3. ✅ **导入体验完善**（2026-07 实现）：两个 picker（本会话 + 外部导入）都显示相对修改时间（`SessionInfo`/`ExternalSessionInfo` 加 `modified`，渲染 "3h ago"）；Claude 导入解析 content 数组——tool_use → `Entry::ImportedTool`（保留 Claude 原始工具名与 input，不做假映射），tool_result → `output` 条目（复用 compact_output 折叠测试输出）；`summarize_call` 认识 `file_path`/`url`/`query` 键。
4. ✅ **MCP 客户端**（2026-07 实现）：`tcode-tools/mcp.rs`，stdio 传输（newline-delimited JSON-RPC，协议版 2025-06-18）。`config.toml` 的 `[mcp_servers.名字]`（command/args/env，全局+项目级 overlay）；启动时 initialize → tools/list（含 nextCursor 分页），工具以 `mcp__server__tool` 注册进普通 Tool trait，该名字即权限 descriptor（规则可写 `mcp__server__*`）。Windows 经 `cmd /c` 解析 .cmd shim（npx 等）。server 挂掉/起不来只警告不阻塞启动；请求超时 init 30s / call 120s；进程 `kill_on_drop`，tcode 退出不留孤儿。测试：`tests/mcp_stdio.rs` 用脚本化 python fake server 打真协议（无 python 自动跳过）。
5. ✅ **/export**（2026-07 实现）：`tcode-core/export.rs` 纯函数 `export_markdown(entries)`——ledger 是唯一事实源，导出只是又一个视图。User/Assistant 文本、工具调用摘要行、工具结果（`<details>` 折叠 + 自适应长度 code fence 防逃逸）、Note/Summary/ImportedTool 全覆盖；`<tcode-status>` 等 harness 管道内容不导出。TUI `/export [path]` + REPL 同名命令，默认文件名 `tcode-transcript-<unix>.md`。
6. ✅ **Memory 2.0**（2026-07 实现；替换 `/remember` 盲追加方案）：参照 Claude Code 当前的两类记忆，但保留 tcode 对 `AGENTS.md` 的原生支持。核心区别必须建模清楚：**指令由人维护、可随仓库共享；自动记忆由模型维护、仅存本机**。二者不得写进同一个文件。
   - **用户与项目指令**：`~/.tcode/AGENTS.md` 是用户级指令；项目内每层目录按 `.tcode/AGENTS.md > AGENTS.md > CLAUDE.md` 取第一个命中，多个目录层级不互相覆盖，而是按“项目根 → 目标目录”依次拼接，使更具体的指令最后出现。启动时加载项目根到 cwd；之后访问其他子目录时按需补载尚未出现的层级。
   - **自动记忆**：每个项目使用 `~/.tcode/projects/<project-id>/memory/`，`MEMORY.md` 是精简索引，启动只注入前 200 行或 25 KiB（先到者为准）；详细内容放同目录 topic 文件，由模型按需 `read`。Git 项目的 `<project-id>` 基于 canonical git common dir，使同仓库的子目录和 worktree 共享记忆；非 Git 项目基于显式 `.tcode/config.toml` 项目根。自动记忆默认开启，system prompt 明确何时值得记录、不得保存秘密、优先更新而非重复追加。项目累计 20 个用户回合或距上次整理提醒满 7 天后，在下一次活跃 turn 注入一次维护提醒：整理重复/过期条目、把细节归档到 topic 文件，并记录仍有效的重要决策；模型成功写入当前项目自动记忆后重置周期。状态持久化，因此关闭后不会后台运行，重新使用项目时才继续计数和提醒。
   - **项目边界**：启动项目根取最近的 `.git` 或 `.tcode/config.toml` 祖先；若都不存在，当前 cwd 自身就是临时项目根。访问 cwd 项目以外的路径时，只有目标祖先存在 `.git` 或 `.tcode/config.toml` 才视为另一个项目；单独的 `.tcode/AGENTS.md` 只是目录级指令，不重定义项目根。`home` 和文件系统根永不因附近存在 `AGENTS.md`/`CLAUDE.md` 被猜成项目。无显式标记的外部路径不加载额外项目记忆，只继承用户级指令。
   - **外部项目按需加载**：`Tool` 增加声明式 `context_paths(input)`，由 `read`/`write`/`edit`/`grep`/`glob` 返回 path，`shell` 返回显式 `cwd`（未给则当前 cwd）；agent loop 在工具执行前统一解析目标项目并去重已加载的 canonical 指令路径。禁止解析 shell 命令字符串猜文件路径；MCP/第三方工具没有声明路径时不做隐式发现。
   - **注入与执行顺序**：按需内容通过 append-only `Entry::Note` 进入 ledger，绝不改 system prompt 前缀。模型已经生成 tool call 后才发现新指令，因此首次 `write`/`edit` 或外部 `cwd` 的 `shell` 必须返回“已加载新指令，请据此重试”且不执行；只读工具可正常执行，并把新指令与 tool results 一起交给下一轮。这样不会在未知项目规则时先产生副作用，也不破坏 provider 要求的 tool-use/tool-result 相邻关系。并行批次先统一 preflight、合并并去重所有新指令，再决定执行或整体阻断。
   - **命令语义（已确认）**：删除 `/remember <fact>`，改为 Claude Code 风格的自然语言“记住 X”，由模型维护自动记忆；新增 `/memory` 列出本会话已加载的用户/项目指令、自动记忆目录和开关状态，`/memory on|off` 显式切换，不暗中改记忆正文。
   - **首期 non-goals**：不做 `@import`、`.tcode/rules`/path glob、会话结束自动建议、shell 脚本静态分析、无项目标记的外部目录猜测；这些没有真实需求前不增加解析器和状态。
   - **预算与安全**：启动指令与自动记忆分别计量，项目指令总预算先维持 16 KiB，自动记忆独立 25 KiB/200 行；动态补载每文件和每次 turn 都有硬上限，截断必须列出来源。所有路径 canonicalize 后做去重和边界判断，读取失败只产生可见诊断，不阻断无关工具。
   - **验收**：单测覆盖祖先顺序、同层优先级、home/root 排除、Git worktree identity、无 marker 外部路径、symlink/canonical 去重、预算 UTF-8 边界；agent-loop 集成测试覆盖只读按需加载、首次外部写阻断后重试、并行 read/edit preflight、resume/compact 后不重复注入；真实 smoke 覆盖从项目 A 读取并修改项目 B，确认 B 的规则在副作用前生效。
7. **全屏渲染器**（可选，靠 Renderer trait 零重构）。

v2 方向（未变）：/branches 分支浏览、WASM 插件式 hooks。


## 验证方式（贯穿）

- Ledger / 缓存断点 / 预算门 / Freshness Tracker：纯单元测试。
- Agent loop：MockProvider 脚本化 tool_use 序列做集成测试，不打真 API。
- 每里程碑用真实 API 跑端到端任务，盯状态行缓存命中数字（这本身就是对"省 token"的持续验收）。


## 待解决问题
1. 退出时会留下较大空白，之前好像是因为退出的时候新的命令行会挤在底下的context栏什么的中间才被改成这样的，但这个解决方式是不是有点过于简单了，现在退出的时候中间一大片空白行也有点奇怪。
2. deepseek模型名称那里感觉不需要带[1m]后缀了吧，这个是为了让claude-code知道这是个1m context的模型才使用的，