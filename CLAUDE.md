# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目概述

tcode 是一个类 Claude Code / Codex 的 Rust agent harness CLI。**`plan.md` 是权威设计文档**（设计原则、贯穿机制、已实现里程碑与未决项），改动涉及架构决策时先读它。本文件承载改代码时必须遵守的规则与项目结构索引。

三条贯穿全局的设计约束，改代码时不可违背：

1. **零猜测原则**：模型不应花 token 获取 harness 本来就知道的信息。工具错误信息要自愈（附候选/建议）、文件重复读返回 stub、中断后注入精确状态说明。
2. **缓存命中由类型系统保证**：`Ledger` 是 append-only 的，历史只有 `append` / `truncate_tail`（rewind）/ `compact` 三个合法操作。任何"改前文"的新需求都必须经 compact 语义，不得绕过。
3. **能力靠注册表插拔，不靠主逻辑里长分支**：三个同构注册表——工具 `Tool`/`builtin_tools()`、斜杠命令 `SlashCommand`/`CommandRegistry::builtin()`、工具渲染 `ToolRenderer`/`RenderRegistry::from_tools()`。新增一项能力 = 新写一个文件 + 注册表里加一行，主循环与 `app.rs`/`main.rs` 不动。发现自己要在主逻辑里按名字加 `if`/`match` 分支时，先问：这是不是该由 trait 方法表达的能力？

## 改动勿回退的硬规则

除上面两条约束外，以下已固化，改动不得破坏（design 层面的"为什么"见 `plan.md`）：

**上下文与缓存**

- 进缓存前缀的内容都有预算：system prompt、项目地图（80 项/目录、20 子项、16 KiB）、skills 列表（200 字符/6k）——加内容前先看现有上限。
- 工具输出不可无门直灌 ledger：大输出必须过 blob 预算门并落 scratch 文件（曾因 `gates_output=false` 让 grep 单条巨行撑爆 context）。
- system prompt 会话内不变，唯一例外是 `/dogfood` 这类显式开关：切换时一次性重打前缀（与 compact 同量级），**不得**把同类指令塞进每轮 tail 反复付费。
- token 两个量纲不可混：context 表 = 单次请求的完整 prompt（缓存+未缓存）= 当前窗口占用；turn 汇总 = 本轮**未命中的 `input_tokens`** + cache%。勿用 `total_input()` 把缓存前缀按请求次数重复累加；运行时状态行 `↓ ~N tok` 走 `token_count`。
- 真实 API 端到端验证时盯状态行 cache_read 占比：连续 turn 应接近前缀全长，下跌即缓存回归。
- **辅助模型角色必须像"顺手"一样便宜**：`auto`（分类器）与 `suggest`（下一句 prompt 猜测）都不得重放主会话——分类器读过滤后的转录，suggest 只读最后一轮对话（不是 ledger）。诱惑总是"骑在主前缀上蹭缓存"，但那等于每回合为一个便利功能付一次全窗口 cache read（30k context ≈ $0.009/次），大模型上还要等好几秒，ghost 文本迟到就等于没有。二者都是 `AgentRole` 注册表中可钉的角色，就是为了让人把它们钉到小模型上。
- **一个前缀一个缓存作用域**：共用 provider ≠ 共用缓存键。凡是自带独立前缀的会话——Auto Mode 分类器（policy 前缀）、每个 sub-agent（自己的 ledger）——都必须经 `Session::with_cache_scope` / `Request::cache_scope` 声明作用域；`None` 只留给主会话。Codex 的缓存键是 `session_id` **请求头**（后端会用它覆写 body 里的 `prompt_cache_key`，别指望改 body 生效），provider 按作用域派生稳定 uuid。新增任何"复用主 provider 打另一套前缀"的能力时，先给它一个 scope，否则两套前缀在同一个键上互相稀释亲和性。
- 模型能力差异归 provider 消化，不许上浮到调用方：Codex 订阅端点是**严格白名单**（未知字段一律 400），没有 `max_output_tokens`（官方 `ResponsesApiRequest` 里就没有这个字段，config 的 `model_max_output_tokens` 是客户端预算，不上线），所以 `Request::max_tokens` 在这条路径上无效——需要短输出就靠 prompt 或 `text.verbosity`/结构化 schema，别加参数。effort 同理：我们的 `off` 对应 Responses API 的 `"effort":"none"`，原样发 `off` 是 400。

**TUI 渲染（`tcode-tui`）**

- transcript 是唯一事实源、屏幕只是视图；alternate screen 为唯一路径（inline 已删，非 TTY 走 plain）。
- wrap 只算一次：每块缓存当前宽度的 wrap，resize 才失效；流式追加只重排最后一块。
- 只渲染可见切片：前缀和二分定位视口起点，每帧 O(视口高度)，与转录总长无关。
- ratatui 双缓冲 diff 最小化终端写入，帧外包 crossterm synchronized update 防撕裂；重绘按事件驱动 + 250ms tick 合并。唯一例外是 shimmer 的 100ms 动画 tick（select arm 以 `shimmer_active` 门控，只在有 in-flight 调用或运行中 task 时醒来），且它是 paint-only：`set_task_activity_frame`/`set_live_head` 不改内容不重排，每帧仍 O(视口)。
- wrap 必须展开 tab：工具输出 `行号\t内容` 的 tab 宽度测 0 却占 buffer cell，滚动残留浮字；`transcript.rs::wrap_lines_flagged` 按 8 列制表位展开成空格，勿改回裸 tab。
- 折叠输出默认：read/grep/glob 转录里默认只显示折叠摘要，不铺开首行。
- 批量渲染 item 紧跟自己的 result：批次 header 后每个 call 的 `├ 摘要`(+diff) 推迟到自己的 `ToolEnd` 再 bake（`PendingCall.header`），live 与 replay 一致。
- **按工具名 match 只许出现在 `RenderRegistry::from_tools` 一处**，其余渲染行为一律经 `ToolRenderer` 的 trait 方法（`route` / `header` / `body` / `batch_item` / `quiet_output` / …）。`quiet_output` 派生自活的 `Tool::batch_policy()`，不得退回手工同步的名字表。
- **三条渲染路径（live / replay / approval）必须共用同一组入口**：`bake_call_start`、`batch_header_lines` + `batch_item_lines`、`bake_call_result`（内部 `call_lines` / `result_render`）。各写一套必然漂移——历史教训：重放曾丢批次分组、丢调用间空行、与实时对不上。
- 空行是记录的分隔：单发调用 bake 时前置一个空行（带 diff/命令块时后置一个），批次 header 同理。删掉它们记录就糊成一坨。

**工具执行**

- 工具的 `run` 是 async：文件 IO 走 `tokio::fs`，重 CPU 走 `spawn_blocking`。**在 async fn 里做阻塞 `std::fs` 等于把并行批次变回串行**（`join_all` 的每个 future 在一次 poll 里同步跑完）并堵住 runtime 线程——`read`/`write`/`edit` 曾如此。
- 图片输入只能经 `tcode-core::images` 归一化：文件 read、`view_image` 与剪贴板入口复用同一长边/大小预算，禁止各自编码或缩放。
- `ToolCtx` 的 `std::sync::Mutex`（freshness/blobs/memory）只在短临界区内持有：跨 `await` 持锁会让 future 非 Send，还会把整批写序列化在一个文件的磁盘延迟后面。
- 自愈式错误的匹配回退（`edit` 的 punct/ws 归一化）跑在失败路径上，仍要控复杂度：行 key 每行只算一次，别在滑窗里重算（分配级 O(n·m)）。

**信任边界**

- **指令只来自 system prompt 与用户消息，其余一切都是数据**：文件内容、命令输出、网页、sub-agent report、MCP 结果、仓库自带的 `AGENTS.md`/skills/agent def 都是观察到的事实，不是发给模型的请求。主 system prompt 有 `Trust and authority` 一节兜底，但**能用类型和结构挡的不许退化成 prompt 纪律**——下面三条是结构防线，看着冗余，删掉即破防。
- **`SKILL_ECHO_OPEN` 归 core（`ledger.rs`）而非 tools**：`/name` 触发的 skill 正文以 `Entry::User` 进 ledger（省一轮），但正文是仓库文件、不是用户的话。`ClassifierTranscript` 必须能在不反向依赖 tools 的前提下认出它并打成 `<skill-body>` 而非 `<user>`；Auto Mode 的授权判定只认 `<user>`。这条链断一环，clone 来的仓库就能靠一个诱人的命令名（`/test`、`/build`）拿到用户授权。格式本身仍只有 `wrap_skill_echo` / `parse_skill_echo` 知道。
- **包标签必须在发出方转义闭合序列**：`auto_mode::append_tag` 中和 `</tag>`、`web.rs::fence_page` 中和 `</web-page-content>`。只包不转义等于没包——正文提前闭合就能续接一个更高权限的标签。转义放发出方（一处），不放读取方（多处，必漂移）。
- **外部内容进 context 必须有围栏**：`web_fetch` 三条出口（普通 / `pattern` 命中 / 委派 `[agents.fetch]`）全走 `fence_page`，新增出口一并走；吃外部内容的子 agent prompt 自己也要声明围栏内是数据（`web-fetch-summary.md`）。
- **自动记忆是注入的持久化通道**：写进去的东西下次以开局前缀身份回来，比任何一次性注入都值钱。故 `memory/system.md` 限定来源——第三方内容只能作带出处的观察，"以后总是……"形状的常驻指令只有用户能授权。

**Prompt 归置（`prompts/`）**

- **给模型读的指令正文一律是 `prompts/*.md` + `include_str!`，不在 .rs 里写多行字符串常量**：system prompt（`interactive-agent-system.md`）、sub-agent 人设（`task-*-system.md`）、Auto Mode 分类器策略与两个阶段指令（`auto-classifier-*.md`）、compact（`compact.md`）、自动记忆（`memory-system.md`）、下一句猜测（`suggest-system.md`）、`/dogfood`（`dogfood.md`）。理由：prompt 是要反复读、逐字调、跨人 review 的**内容**，不是代码；埋在 `r#"…"#` 里既没高亮也没 diff 可读性，还会被顺手改坏缓存前缀。新增一个吃 prompt 的功能 = 新写一个 md + 一行 `include_str!`。
- **例外只有 tool 的 description/参数 schema**：它们与 `Tool` 实现同生共死（改签名必改描述），拆开只会漂移，留在各自的 `.rs` 里。
- 拼装逻辑（按配置追加规则、插 focus 段、按开关拼后缀）留代码里；md 只放不变的正文。

**跨层职责**

- **turn 运行中用户提交的 prompt 只能在工具批次边界投递**：`Entry::Assistant`(tool_use) 与其 `Entry::ToolResults` 之间不许插任何东西（否则请求非法），所以队列（`Session::pending` / `PendingInput`，前端持克隆句柄）由 agent loop 在"批次结果已提交"那一点 drain，append 成真正的 `Entry::User`——`as_messages` 会把它与同位置的 tool_result 合并进同一条 user message，仍是纯 append，模型下一步就读到。它是 `Entry::User` 而非 `Entry::Note`，因为 Auto Mode 的授权判定只认用户消息。循环没走到边界就结束的（收尾发言期间入队、或 ctrl+c 打断），由前端在 turn 结束时立刻起新 turn 发出去。
- **可逆的 harness 状态不许由控制操作直接写进 `Entry::Note`**：mode、`/memory on|off`、`/cd` 等先立即更新本地运行时状态；连续改动在 `Session` 的 pending 槽内覆盖合并。只有真实用户交互到达合法投递点（新 prompt、批次边界投递的排队 prompt、审批完成）才把**最终**状态 append 给模型；纯 UI 键、命令和 monitor wake 不得产生该类 Note。环境另分“已观察”与“已投递”两个 JSONL snapshot，resume 用后者作为模型已知基线；旧日志的 `EnvironmentChanged` 兼容地视作已投递。工具结果、hooks、monitor、中断、compact 等已发生事实仍立即 append，不能错误延迟。
- **monitor 事件是 `Entry::Note`，不是 `Entry::User`**：Auto Mode 授权判定只认用户消息，所以监控事件在结构上永远不可能被当成用户授权（claude-code 靠 prompt 纪律解决的问题，这里靠类型解决）——不得把事件改成 User 注入。事件注入分两种价位：turn 进行中搭批次边界的车（纯 append，免费）；空闲时每次唤醒 = 一次完整前缀 cache read，所以必须合流——前端等 quiet 窗口（deadline = 首个未投递事件 + quiet_ms，锚定首个事件保证有界延迟）后调 `Agent::monitor_turn`，无事件待投递时它不发任何请求。
- 批次分组的判定属于 agent loop（`BatchPolicy` + 路径冲突检查），重放要还原批次显示就调 `Agent::batch_display_label` 问 core，**禁止在 TUI 里重新推导规则**（测试 `batch_display_label_matches_the_live_batch_header` 钉住实时与重放同一标题）。
- **状态与策略按最窄使用范围归属**：只服务一个工具、provider、命令或前端能力的静态配置、派生状态与机械判定，应由该能力的实现持有并输出已有的通用结果；composition root 只负责一次性规范化与注入。不得为图方便把它们上浮为 `Agent` / `Session` / `ToolCtx` 的通用字段、公共 enum/trait 的平行变体，或主逻辑的按名称分支。只有多个独立消费者确实需要同一安全不变量或统一审计/配置语义，或结论依赖 session 级动态事实时，才提升到共享层。
- 斜杠命令归属：语义作用于 `Session`/`Ledger`/文件系统 → core `commands/`（TUI 与 REPL 共享，自动获得 /help 与补全）；语义是操纵前端专属对象（model picker、provider wizard）→ 留在前端。开发者用的命令实现 `SlashCommand::hidden()`：照常 dispatch，但不进 /help 与补全（如 `/dogfood`）。`/model` 与 `/agents` 驱动的是前端自己的选择器，故留在前端；两者共用一个 `Picker`（`/agents` 只是多套一层"选哪个 agent"和一行 inherit），别为第二个选择器再写一套网格。前端只是 effect 解释器；`CommandEffect` 新增变体的准入标准：要么每个前端都有非平凡解释，要么有明确降级语义，否则逻辑该留在命令自己里。
- update_progress 不套骨架：多数任务不必追踪进度；需要时按真实结构分阶段维护，同时只一个 in_progress，做完即标 completed。它不同于只读的 plan permission mode。
- **agent 定义统一走 `AgentDef` 注册表，不在 task.rs 里按 kind 长分支**：builtin explore/plan/general 与 custom `.tcode/agents/*.md` 都是 `AgentDef`，system prompt / 工具过滤（`keeps_tool`）/ permission / batch policy 全从 def 字段读。新增一种内建 kind = `AgentRegistry::builtin()` 里加一个 def，不是在 `run_with_call` 里加 `match` 臂。**保留字**：explore/plan/general 不许被文件覆盖（其 read-only 永不询问的权限语义绑定在 `read_only` 上，覆盖会静默放宽——这与 skills 的"文件覆盖 builtin"刻意相反）。**嵌套授权只认 def 的 `agents` 字段**：有该字段才发受限子 `TaskTool`（`allowed` 限定 spawn 集），`depth < MAX_TASK_DEPTH`（=3）封死递归，不做环检测。**追问（resume）走同一 session 同一 cache scope 纯 append**：`max_exchanges > 0` 才进程内保活 Agent+Session（`live` map，cap 8 最旧逐出），别把它做成持久化或另起前缀——追问的全部价值就是命中已有前缀缓存、只付增量。`--agent` 顶层 run 用 `scoped_to(def)` 把进程本身当作深度 1 的该 agent。

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
  - `agent/`：`mod.rs` 是 agent loop（`Agent` / `AgentEvent`，loop 内顺序为 权限 → pre_tool_use hook → checkpoint → tool.run → post_tool_use hook → append，含批处理三种策略与 `batch_display_label`），核心 loop 刻意保持内聚不再细拆；周边切出 `session.rs`（`Session` / cwd 切换）、`compact.rs`、`summarize.rs`（`summarize_call` 等纯函数）。
  - `commands/`：斜杠命令插件（`SlashCommand` trait + `CommandCtx` / `CommandOutcome` / `CommandEffect` + `CommandRegistry::builtin()`），一命令一文件；TUI 与 REPL 共用同一注册表。
  - `provider.rs`：`Provider` trait，统一流事件 `StreamEvent`（TextDelta/ToolUseStart/Usage/…），两家 API 差异在 provider 内部消化；`CacheStrategy` 区分 Anthropic 显式断点与 OpenAI 隐式前缀。`ModelCell` 是 Agent 与 TaskTool 共享的 RwLock 模型句柄，支撑 `/model` 热切换。
  - `tool.rs`：`Tool` trait + `ToolCtx`（cwd、freshness tracker、checkpoint、blob store、cancellation、事件通道）。
  - 支撑机制：`freshness.rs`（文件重复读去重）、`blobs.rs`（大输出分页，预算门）、`checkpoint.rs`（写前文件快照，供 rewind 回滚）、`store.rs`（JSONL 事件日志 = 会话持久化，resume 是重放）、`import.rs`（将已标准化的 `Entry` 复制为新会话，`Entry::ImportedTool` 只进转录不进 prompt）。
- **`tcode-providers`** — `anthropic.rs` / `openai.rs` / `codex.rs`（Codex Responses API；`codex_cli.rs` 负责复用 `~/.codex/auth.json`、刷新 token 和补全本地模型缓存）+ `retry.rs`（watchdog：chunk 级 idle 超时 + 指数退避）。入口 `build_active(profile, selection, watchdog)`。
- **`tcode-importers`** — 外部 transcript adapter：扫描 Codex / Claude Code 会话目录、解析各自 JSONL 格式并标准化为 core `Entry`；只读复制，绝不修改来源文件。
- **`tcode-tools`** — 内置工具，`builtin_tools()` 组装；Windows 上 PowerShell 为主、检测到 Git Bash 才加 `bash` 工具。`task.rs` 是 sub-agent（独立 ledger、受限工具集），`agent_defs.rs` 是 agent 定义注册表（builtin explore/plan/general 与文件发现的 `.tcode/agents/*.md` custom def 统一为 `AgentDef`/`AgentRegistry`，供 `task` 工具与 `--agent` 共用；`frontmatter.rs` 是与 skills 共享的 YAML 解析），`skills.rs` 发现 `.tcode/skills` / `.claude/skills`，`grounding.rs` 生成开局项目地图（有严格 token 预算，改动需守住上限），`web.rs` 是 `web_fetch`（readability 正文抽取 + htmd 转 markdown，`raw=true` 跳过抽取；`pattern` 回命中行 ±2 行上下文；`prompt` 委派 `[agents.fetch]` 一次性总结、全文落 scratch，未钉 fetch 模型则附 note 返回原文；跨域重定向不自动跟）+ `web_search`（Exa/Parallel hosted 链 + DDG 兜底）。后台任务：`shell` 的 `run_in_background` 进 `tcode-core/background.rs` 注册表，`read_output` 认 `b1` 类 id，`kill_task` 停任务，完成通知由 agent loop 在安全边界 append `Entry::Note`。`monitor.rs` 是同一注册表的另一种通知语义（不是第二套机制）：脚本 stdout 每行即事件，`take_notes` 在安全边界注入并以 `AgentEvent::Note` 同步前端；事件行 512B 截断、pending 上限、洪水自动停（120 事件/60s）都在注册表层，权限走 `run(...)` 描述符复用 shell 规则。
- **`tcode-tui`** — 自绘全屏（alternate screen）：内存 `transcript.rs` 单一事实源，事件溯源消费者模型（渲染/`/export`/resume 重放同一接口）。`render.rs` 是工具渲染注册表（`ToolRenderer` + `RenderRegistry`），`diff.rs` 提供无名字的渲染原语（`edit_diff` / `write_preview` / `command_block`），`app.rs` 只做事件循环与 bake。markdown + syntect 高亮、similar 红绿 diff、鼠标全套；UI 事件循环与 agent loop 是两个 tokio task，仅 mpsc 通道通信。渲染性能纪律见上方硬规则。
- **`src/main.rs`** — clap CLI 装配 + system prompt（`IDENTITY`）+ 非 TTY 的 REPL/plain 路径（`approver.rs` / `printer.rs`）。

## 配置与运行时路径

- **配置说明必须同步**：凡是用户可配置的 `Config` / profile / model / agent frontmatter / permission / limit / watchdog / Auto Mode / UI / hook / MCP 字段发生新增、删除、重命名、默认值、优先级或安全语义变化，必须在同一改动更新 `crates/tcode-tools/src/skills/builtin/tcode-config/SKILL.md`；其中要说明字段含义、有效值或类型、默认/继承规则、作用域与一个安全的最小示例。不得让已生效的配置项在该 skill 中无说明。

- `~/.tcode/config.toml`：profile/模型/权限规则，永远手写（首启向导生成初版）。
- `~/.tcode/state.toml`：程序自己决定并要记住的东西——当前 profile/model/effort、`/agents` 的 sub-agent 钉选、`/dogfood` 开关。程序只写这个文件。优先级 CLI flag > state > config。多处写入必须走 `ModelState::update`（读-改-写），整struct `save()` 会把兄弟字段悄悄清掉。
- `.tcode/config.toml`：项目级 hooks、权限规则与 MCP server（`[mcp_servers.名字]` command/args/env，工具注册为 `mcp__名字__工具`，权限规则按该名字匹配）。
- `[ui] suggest_next`（默认 true）：回合结束时猜下一句 prompt，输入框里显示灰字，→ 采纳。它是每回合一次额外请求，所以必须能关。
- `[agents.explore]` / `[agents.general]` / `[agents.auto]` / `[agents.suggest]` / `[agents.vision]` / `[agents.fetch]`：给 sub-agent 与辅助角色钉模型（`profile`/`model`/`effort` 可选，未写的继承父选择）。钉住的 kind 不跟随 `/model`；未配置的 kind 共享父 `ModelCell`。`vision = false` 可标记纯文本模型，使图片路径自动指向 `view_image` 而不是触发 API 400。**`fetch` 是唯一"未钉即关"的角色**：web_fetch 的 `prompt` 在未钉时降级为返回原文（不回退主模型——原文本来就能进主 context，花一次整页请求换有损摘要比直接读还差）；工具经 `ToolCtx::agent_models` 查角色，别再为它加构造穿线。运行时经 `AgentModels`（可换手柄）共享给 `task` 工具与 `/agents` 选择器，改动即刻对下一个 sub-agent 生效。
- 持久上下文分两类：用户/项目指令由人维护，自动记忆由模型维护，二者禁止混写。项目指令从项目根到目标目录逐层加载，每层按 `.tcode/AGENTS.md` > `AGENTS.md` > `CLAUDE.md` 取第一个；访问其他子目录或显式标记的外部项目时按工具目标路径懒加载。自动记忆位于 `~/.tcode/projects/<project-id>/memory/`，`MEMORY.md` 只做精简索引。
- 会话/checkpoint/blob/scratch：`~/.tcode/projects/<cwd-hash>/{sessions,checkpoints,blobs,scratchpad}/`。溢出输出与后台日志落 `scratchpad/tool-output/`。
- 磁盘回收（都是启动时 best-effort 扫一遍，失败即忽略）：`sweep_old_sessions` 保留最近 100 个**有内容的**会话且不超过 30 天，**会话日志与它的 checkpoint 目录同生共死**（还能 resume 的会话必须还能 rewind；反之 checkpoint 没了日志就是无名垃圾），顺带收掉孤儿 checkpoint 目录；空日志（启动了没说话）不占名额、直接删，但有 1 小时宽限期以免删掉另一个正在启动的实例的日志。`sweep_scratchpad` 对**整个 scratchpad** 一条规则：7 天没被碰过的文件删掉，空掉的目录随之删掉——harness 的溢出输出和模型自己建的构建目录/探针脚本同一把尺子，**不要再给某个子目录开豁免**（曾经只扫 `tool-output/`，结果模型建的 3 GB cargo target 在缝里躺了下来）。checkpoint 的 pre-image **按内容 hash 命名**（128-bit FNV-1a，冲突会还原错文件，故不用 64-bit），同一文件反复编辑只存不同状态各一份。
- API key 经 `api_key_env` 指环境变量，不落盘。
