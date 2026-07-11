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

## 已实现（M0–M5 及计划外，2026-07；仅索引，细节见代码与 CLAUDE.md）

- **M0–M2 基础**：双 Provider（Anthropic + OpenAI，SSE + watchdog + 重试）；Tool trait + agent loop；append-only ledger；权限模式+规则；blob 预算门；Freshness Tracker；中断契约；开局项目地图；尾部自知一行；inline TUI（markdown/高亮/diff、slash、权限 Tab 意见、图片/长文本粘贴、状态行 token+缓存遥测）。
- **M3–M4**：JSONL 事件日志 + `--continue`/`--resume`；双击 Esc rewind + 文件 checkpoint 回滚；`task` sub-agent + `explore`；hooks；`/compact` + 自动 compact；缓存回归哨兵；`/cost`；半截 tool_use 合法化；Windows Terminal/conhost 兼容。
- **计划外**：多模型 profile + chatgpt provider + 首启向导 + `/model`；交互工具 `update_plan`/`ask_user`/`add_note`；外部会话导入（Codex/Claude Code JSONL，只读复制，`Entry::ImportedTool` 只进转录不进 prompt）；OpenAI 限额进度条；Skills 发现（`.tcode`/`.claude` skills，含 200 字符/6k 预算降级）；project_map 预算防御（80 项/目录 20 子项/16 KiB）。
- **M5**：后台任务（`run_in_background` + `background.rs` 注册表 + `read_output`/`kill_task` + 完成 `Entry::Note`）；`web_fetch`/`web_search`（见下 Web 节，已重写升级）；导入体验（相对时间、诚实映射）；MCP 客户端（stdio JSON-RPC 2025-06-18，`mcp__server__tool`）；`/export`（`export.rs` 纯函数）；Memory 2.0（**人维护指令 vs 模型维护自动记忆分离**，项目根→cwd 分层加载，`context_paths` 外部项目按需加载 + 首次外部写阻断重试，`/memory`）。

v2 方向：/branches 分支浏览、WASM 插件式 hooks。

## M6：自绘屏幕渲染器（2026-07 决策，进行中）

### 动机

inline 模式把定型内容交给终端原生 scrollback，代价是**写入即不可撤销**：rewind 后旧转录仍留在屏幕上（同一条输入显示多次）、edit 的 diff 必须在审批前 prebake（被拒后无法撤下）、`/clear` 要靠清 purge 整个终端、退出时留大片空白。这些不是各自独立的 bug，是"渲染目标不受我们控制"这一个根因的多个症状。Codex tui2 的重做结论相同：**内存中的 transcript 作为唯一事实源，屏幕只是它的一个视图**。

### 已确认决策（2026-07-11）

1. **直接替换 inline**：alternate screen 成为唯一 TUI 路径，inline 代码删除；非 TTY 的 plain 模式保留。不留 `--inline` fallback。
2. **鼠标全套自实现**：开启 mouse capture；滚轮滚动 transcript、拖选高亮、松开复制到系统剪贴板（arboard，SSH 下 OSC 52 回退）。Shift+拖选退回终端原生选择。
3. **rewind 就地跳转**：双击 Esc 不再弹 picker——transcript 直接跳到并高亮上一个用户输入点，输入框预填原文；Esc/↑ 继续向前跳，Enter 确认截断（含文件回滚选项）。转录视觉同步截断，天然正确。
4. **审批拒绝只留一行摘要**：diff 在审批对话框内固定高度滚动展示（不再 prebake 进转录）；Yes 才把 diff 落转录，No 只留 `edit(file) — declined` 一行。`previewed_changes` 机制整个删除。

### 数据结构

```rust
// tcode-tui/transcript.rs
struct Block {
    lines: Vec<Line<'static>>,     // 逻辑行（未 wrap）
    collapse: Option<Collapse>,    // 工具输出/diff 的折叠状态 + 内部滚动偏移
    entry: Option<usize>,          // 对应 ledger entry，rewind 截断的映射
    wrapped: Cache<Vec<Line>>,     // 按当前宽度的 wrap 缓存
}
struct Transcript {
    blocks: Vec<Block>,
    heights: Vec<usize>,           // 可视高度前缀和，append 时增量维护
    scroll: Scroll,                // 距底部偏移；0 = 跟随流式输出
    selection: Option<Selection>,  // 视觉行坐标 anchor/head
}
```

### 性能纪律（渲染效率的保证，改动不得破坏）

- **wrap 只算一次**：每块缓存当前宽度的 wrap 结果，只有 resize 使其失效；流式追加只重排最后一块。
- **只渲染可见切片**：前缀和二分定位视口起点，每帧成本 O(视口高度)，与转录总长无关。
- **ratatui 双缓冲 diff** 负责最小化终端写入；帧外再包 crossterm synchronized update 防撕裂。
- 重绘按事件驱动 + 250ms tick 合并，不做无变化重绘。


### 调研备注

- 现成 crate 无法直接套：`tui-textarea`/`ratatui-code-editor` 的选择只针对编辑器组件；`tui-scrollview` 按内容总尺寸分配整块 buffer，长转录不可接受。自实现是正解。
- Codex tui2 踩过的坑值得预防：滚轮/触控板事件在不同终端节奏差异大（iTerm2 连发），滚动步长需按事件序列自适应；alternate screen 下终端原生搜索不可用，后续可考虑 `/` 转录内搜索（暂列 v2，不进 M6）。


### 澄清（排除误判）

- `read` 默认 `limit = 2000` 行未变（`DEFAULT_READ_LIMIT`）；会话里的"200"是 `grep` 的 `head_limit` 默认值，两者无关。
- tcode 的 `grep` **本就全量输出 `file:line: 内容`**（`search.rs`），无需新增"返回 file:line"的工具；P0 让该输出不被切即可等价于"Claude Code 式 Grep 全量返回"。


## 验证方式（贯穿）

- Ledger / 缓存断点 / 预算门 / Freshness Tracker：纯单元测试。
- Agent loop：MockProvider 脚本化 tool_use 序列做集成测试，不打真 API。
- 每里程碑用真实 API 跑端到端任务，盯状态行缓存命中数字（这本身就是对"省 token"的持续验收）。


## 待解决问题

> 已修复且无长期价值的细节（diff 行号、批次分组标题、Ctrl+O 移除、resume 工具输出/彩色 diff 恢复、鼠标 hitbox 赋值、freshness 用内容 hash 判断——后者已并入"三个贯穿全局的机制 #3"）已从本节删除；下面只留仍需守住的设计要点与未决项。

### 设计要点（已固化，改动勿回退）
- **折叠输出默认**：read/grep/glob/read_output 转录里默认只显示折叠摘要，不把输出首行铺开。
- **wrap 必须展开 tab**：工具输出的 `行号\t内容` 里 tab 宽度测 0 却占 buffer cell，滚动会残留浮字；`transcript.rs::wrap_lines_flagged` 按 8 列制表位把 tab 展开成空格，每个显示 cell 都写到。勿改回保留裸 tab。
- **token 两个量纲不可混**：context 表 = 单次请求的完整 prompt（system+tools+全部历史，缓存+未缓存一起）= 当前窗口占用；turn 汇总 = 本轮**未命中的 `input_tokens`**（新付全价量）+ cache%。曾用 `total_input()` 把缓存前缀按请求次数重复累加，更离谱，已回退。运行时状态行 `↓ ~N tok` 走 `token_count`。
- **update_plan 不套骨架**：多数任务（局部改动、单文件编辑）不必 plan；要 plan 时步骤按真实结构、增量维护状态（同时只一个 in_progress，做完即标 completed）。
- **工具加固（2026-07-11，对照 claude-code / codex 反编译源）**：起因是 `grep C:\Users\Teamon\.tcode` 命中 jsonl 单条巨行（710KB）经 `gates_output=false` 直灌 ledger 撑爆 context。已修（`search.rs`/`fs_tools.rs`）：grep 每行截 512B（对标 rg `--max-columns`）、`max_filesize=256KB`、`build_parallel()`+按 (path,line) 排序、`SEARCH_DEADLINE=10s` 兜底（超时给明确 partial 标记而非静默"无匹配"）、`PRUNE_DIRS` 剪 VCS/node_modules/target/各类缓存（补"离开 git 仓库就无 gitignore 剪枝"的洞）；grep/glob 改 `hidden(false)` 搜 dotfiles + `offset` 分页；read 先 `metadata` stat >10MB 拒读、输出 128KB 字节预算（`numbered_capped`）；edit `replacement_plan` 加末层标点归一模糊匹配（对标 codex `seek_sequence`）。

### 未决
- ⚠️ **输入框快捷键（待你实测）**：Ctrl/Alt+V（含 +Shift）已统一走 `paste_from_clipboard`；Ctrl+C 只做中断阶梯（取消→清空→退出），复制走 Ctrl+Shift+C / Alt+C / 鼠标松开。若仍"粘贴即发送"，属终端把粘贴换行当 Enter（bracketed paste 被吃），只能在终端层解决，应用层无法从单个 Enter 区分粘贴还是手敲。
- ⬜ **UNC 路径未防护**（Windows）：read/write/grep 对 `\\server\share`、`//` 开头路径无拦截，`std::fs` 访问触发 SMB 认证、可能泄漏 NTLM 凭证。claude-code 在各工具 `validateInput` 显式跳过交权限层。tcode 应在 `ctx.resolve` 或工具入口加守卫。**与下面 web SSRF 同源**——都是"未校验目标地址就发起访问"，宜一并设计一个"出站目标白/黑名单"守卫。

## Web 工具：现状、对照与改进（2026-07-11 调研）

### 四方现状（谁都不自己爬 SERP）
- **claude-code**：`web_search` = Anthropic 服务端 `web_search_20250305`（`server_tool_use`）。**无任何本地/客户端搜索实现，也无 DDG 兜底**——`WebSearchTool.isEnabled()` 只按 `getAPIProvider()`（firstParty/vertex/foundry）判定。`web_fetch` = 客户端抓取 → turndown markdown → **Haiku 子模型按 `prompt` 摘要**，只回摘要；15min URL LRU 缓存；二进制存盘；`validateURL` 做 SSRF 校验；www 增删/同源视为安全重定向自动跟。
- **codex**：(a) **OpenAI Responses 服务端 hosted `web_search`**（`hosted_spec.rs`，模式 Cached/Indexed/Live，带 `user_location`/`filters`/`search_context_size`）；(b) 客户端 `web` 命名空间工具，命令 `search`/`open_page`/`find_in_page`，仍走 provider 的 `SearchClient`（非独立抓取），喂入近期对话上下文。
- **opencode**：**本地工具调第三方托管搜索 API**——Exa（`mcp.exa.ai`）/ Parallel（`search.parallel.ai`），走 MCP over HTTP，按 `EXA_API_KEY`/`PARALLEL_API_KEY`，返回**"为 LLM 优化的 context 字符串"**（一次调用 = 搜索+抓取+抽取，`contextMaxCharacters` 控量）。源码注释明确区分"provider-independent 本地搜索"与"provider-hosted 搜索"。webfetch：turndown markdown / htmlparser2 纯文本 / **图片转 base64 attachment**、Cloudflare 403 challenge 换 UA 重试；`http-body.ts::collectBoundedResponseBody` 是**流式按字节截断**的参考实现（正是下面 P0-2 要抄的）。**opencode 也没做 SSRF 校验**——说明该防护是 claude-code 特有加固，非普遍共识。
- **结论**：四家里 **tcode 是唯一自己解析 SERP HTML（DDG）的**。其余三家都不爬——要么委托给模型 provider 的服务端工具（claude-code/codex），要么调专门的搜索 API（opencode 的 Exa/Parallel）。

### 为什么都不自己爬（服务端/托管搜索的优势）
1. **搜索质量与排序**：真搜索要爬取+索引+排序的语料库。DDG HTML 抓取拿到的是被降级的 SERP（易 bot-block，你已处理 anomaly/challenge 即证其脆），Exa/provider 搜索是为程序化调用建的，相关性/时效性都强。
2. **一次调用 = 搜索+抓取+抽取+压缩**：Exa/hosted 直接回"为 LLM 优化的干净文本"，省掉客户端 fetch→markdown→摘要 的整条流水线，token 与延迟双省（tcode 现在是 search 出 URL、再 web_fetch 整页、模型自己读，多轮往返）。
3. **鲁棒**：抓 SERP 依赖 HTML 结构不变，markup 一改就崩；托管 API 有 SLA。
4. **provider-hosted 特有**：搜索在模型 turn 内执行（`server_tool_use`），模型一轮内能多次搜、带 citation，无客户端往返，provider 维护。代价：只在你真连该 provider 一方端点时可用。

### ANTHROPIC_BASE_URL 指向非 Anthropic 端点（如 DeepSeek 代理）时 claude-code 搜索调什么？
- **web_search**：`getAPIProvider()` 只认 bedrock/vertex/foundry 三个 env flag，设 `ANTHROPIC_BASE_URL` 仍返回 `firstParty`，故 `isEnabled()` 仍 true——工具**名义上开着**。但它把 `web_search_20250305` 这个**服务端工具规格发给你配的那个端点**；DeepSeek 代理若没实现 Anthropic 服务端搜索，该 tool_use 要么报错、要么模型根本搜不了。**claude-code 没有任何本地兜底**，所以结论是：**指向 DeepSeek → web_search 实质不可用，且无退路**。（另有 `isFirstPartyAnthropicBaseUrl()` 能正确识别自定义 base_url，但只用于 model-capabilities/policy 等一方特性，未 gate web_search。）
- **web_fetch**：抓取是客户端做的，能工作；但摘要步骤 `queryHaiku` 走**同一个配置端点**的小模型，摘要质量取决于该端点；且默认还有个打向 `api.anthropic.com/api/web/domain_info` 的域名预检（非一方 setup 需 `skipWebFetchPreflight` 关掉，否则每次 fetch 前的预检会失败）。
- **对 tcode 的启示**：tcode 的 DDG 方案恰恰在"任意后端都能用"这点上胜过 claude-code——claude-code 一旦离开一方端点就没搜索。理想是 **hosted 优先 + 独立兜底**：能用原生 hosted（Anthropic/OpenAI）就用，否则回落到独立后端（DDG 或 Exa/Parallel 这类 API）。这是 tcode 相对 claude-code 的差异化机会，不是短板。

### web_fetch 的两个真实洞（安全 / 正确性，P0）
1. **SSRF**：`parse_url` 只校验 scheme，之后照单全收内网/环回/云元数据地址、URL 内嵌凭证、单段主机名（详见下节"SSRF 风险"）。修法：解析后拦截环回/私有/链路本地 IP、内嵌凭证、单段主机名。与 UNC 守卫同源，合并成一个出站目标守卫。
2. **body 上限只在有 Content-Length 时生效**（`web.rs:201`）：chunked/流式响应无该头 → `resp.text()` 无界读入内存，5MB 上限形同虚设。修法：改 `bytes_stream()` 边读边累积，超 `MAX_BODY_BYTES` 立即中止，不信任 Content-Length。

### 改造方案（2026-07-11 拍板，✅ 已实现）
SSRF **不做**（四家里只有 claude-code 做，opencode/codex 都没做，非刚需；保持简单）。落地如下（`web.rs` 重写，8 单测 + 1 个 `#[ignore]` Exa live smoke 实测通过、clippy 0 警告）：

**web_search — 可插拔后端（对标 opencode）**
- **已实测**（2026-07-11，`curl` 裸 `tools/call` 无 key）：`https://mcp.exa.ai/mcp` **匿名可用**，直接返回真实"LLM 优化文本"（标题+URL+正文摘要）。opencode 正是靠这个匿名端点做到"不用注册 key"。**保留**：匿名端点限流未文档化（官方 20k/月是带账号的）、无 SLA、Exa 可随时改成要 key——best-effort。
- 后端选择是 **harness 状态、不给模型选**（零猜测原则：模型无从判断 Exa/Parallel/DDG 优劣，暴露 `backend` 参数只会让它在无法推理的选择上浪费 token + 给缓存前缀加噪）。模型只调 `web_search(query)`；选哪个由 `search_chain()` 决定、失败自动向后兜底：
  - 默认无 key：**Exa(匿名) → Parallel(匿名) → DDG**（两家托管都实测 keyless 可用、免费；DDG 最终兜底）。
  - `EXA_API_KEY` / `PARALLEL_API_KEY` 存在：honor 该家（带 key 提限额）→ DDG，不静默乱撒到另一家。
  - `TCODE_WEBSEARCH_BACKEND=ddg|exa|parallel` 人工显式覆盖（仍带 DDG 兜底，除非选 ddg）。
  - 对比 opencode：它默认按 sessionID FNV-1a 哈希做 **50/50 A/B**（给自己收集对比数据用）、**无任何 fallback、无 DDG**，选中一家挂了就报错。tcode 的链式兜底更稳。Parallel 也实测 keyless（普通 UA 即可，非 opencode UA 鉴权）。
- Exa/Parallel 走 **MCP over HTTP**（单发 JSON-RPC `tools/call`，不做 initialize 握手；`Accept: application/json, text/event-stream`；响应是 SSE，取 `data:` 行里 `result.content[].text`），照抄 opencode `mcp-websearch.ts` 的请求/响应形状（源码 + 实测双验证）。Exa 工具 `web_search_exa`（args `query/type/numResults/livecrawl/contextMaxCharacters`）、Parallel 工具 `web_search`（args `objective/search_queries`，Bearer 头）。
- 关键收益：Exa 回的是**为 LLM 优化的 context 文本**（一次调用 = 搜索+抓取+抽取），省掉"search 出 URL → web_fetch 整页 → 模型自读"的多轮往返；DDG 仍只回标题/URL/snippet。
- 响应流式按字节截断（`MAX_RESPONSE_BYTES`）；25s 超时。descriptor 仍无参数（一次 always-allow 覆盖）。
- hosted 委托（Anthropic/OpenAI 服务端搜索）**暂不做**——需碰 wire 格式、且只在一方端点可用；Exa 免费层已能显著提质，性价比更高。留作后续。

**web_fetch — 抄 opencode/claude-code 的成熟点**
- **流式 body 截断**（P0，真实 bug）：弃用 `resp.content_length()` 信任，改 `bytes_stream()` 边读边累积、超 `MAX_BODY_BYTES` 立即中止。参考 opencode `http-body.ts::collectBoundedResponseBody`。
- **find_in_page**（对标 codex）：web_fetch 加可选 `pattern`（正则）参数——给了就只回 markdown 里命中行 + 上下文（复用 grep 的 `cap_line`/截断），不 dump 整页。比 claude-code 的 Haiku 摘要更轻、零额外模型调用，契合省 context 原则。工具描述引导"找特定内容时带 pattern"。
- **安全重定向放宽**：`example.com ↔ www.example.com`（去掉前导 `www.` 后同 host）视为安全、自动跟；仅真正异 host 才弹回模型。
- **15min URL 缓存**（对标 claude-code）：`Mutex<HashMap<url, (Instant, rendered_text)>>`，TTL 15min、条目上限（超限逐旧）；缓存渲染后文本，`pattern` 过滤在缓存命中后再跑（同 URL 不同 pattern 复用抓取）。
- **http→https 升级** + prompt 加一句"认证/私有 URL 会失败"。（便宜，顺手）

**重构方式**：web.rs 拆出 `fetch_capped()`（流式截断，web_fetch/搜索后端共用）、`search`.rs 式的后端 trait 或 enum 分派；DDG 解析保留。测试：MCP 响应解析 + find_in_page 过滤 + 缓存 TTL 用纯单元测试；Exa/Parallel 真实网络走 `#[ignore]`。

### SSRF 风险（Server-Side Request Forgery，暂不实现，仅存档）
一句话：**工具让"服务器"（跑 tcode 的这台机器）去请求一个由模型/外部输入决定的 URL，攻击者借此把请求打向本不该被外部触达的内网目标。** 具体风险点：
- **云厂商元数据端点**：`http://169.254.169.254/latest/meta-data/iam/security-credentials/`（AWS）、GCP/Azure 类似 → 直接吐出**临时 IAM 凭证**，最危险。
- **环回 / 内网服务**：`http://127.0.0.1:*`、`http://localhost`、`http://[::1]`、`http://10.x/172.16.x/192.168.x` → 打到本机或内网未鉴权的管理端口（数据库、admin、调试接口）。
- **URL 内嵌凭证**：`http://user:pass@host/` → 凭证进日志/转录。
- **单段主机名 / 内网 DNS**：`http://internal-service/` → 解析到公司内网。
- **链路本地**：`169.254.0.0/16`、`::ffff:` 映射绕过。
触发面：web_fetch 每 host 要人确认看似有闸，但 `169.254.169.254` 这种裸 IP 用户很难看出是元数据端点就点了同意；prompt 注入（让模型读到"请 fetch 这个 URL"）可诱导。**防御**：DNS 解析后判定目标 IP 是否落在环回/私有/链路本地段，落入即拒（不能只按主机名字符串判断，要防 DNS rebinding —— 理想是解析到 IP 再校验、并对该 IP 发起连接）。

## M7：待做工具（新 session 交接，2026-07-11）

> **参考仓库不在项目内，需自行 clone**（调研时的 clone 在临时 scratchpad，路径每 session 不同）。仓库内**相对路径稳定**，按下面的相对路径定位即可。三家实现风格：claude-code = 反编译 TS（最全）、codex = Rust、opencode = TS/Effect（最接近"本地工具"心智）。

### 参考仓库
- **claude-code**（反编译 TS）：`git clone https://github.com/Teamon9161/claude-code.git`（源码在 `src/`）
- **codex**（Rust）：`git clone --depth 1 https://github.com/openai/codex.git`（源码在 `codex-rs/`）
- **opencode**（TS/Effect）：`git clone --depth 1 https://github.com/anomalyco/opencode.git`（源码在 `packages/`）

### 1. `read` 支持图片 / PDF（P0，最划算）
**动机**：三家的 read 都能把图片读进 context，tcode 的 `read` 直接拒二进制（`fs_tools.rs` 的 null-byte 检测）。有截图/图表/设计稿、以及浏览器自动化产物时很实用。
**参考**：
- opencode `packages/opencode/src/tool/read.ts`（`SUPPORTED_IMAGE_MIMES` = png/jpg/gif/webp，PDF，转 `data:<mime>;base64` attachment，约 300–321 行）——**最贴近，直接照抄形状**。
- claude-code `src/tools/FileReadTool/{FileReadTool.ts,imageProcessor.ts,limits.ts}`（图片压缩到 token 预算：`detectImageFormatFromBuffer`、`compressImageBufferWithTokenLimit`）。
- codex 也有 `view_image`（`codex-rs` 内搜 `view_image`）。
**要做**：
- 前置改动：tcode `ToolOutput` 现在**只承载文本**，需扩展成能带 **image content block**（tool_result 图片）。**先看 tcode 现有"剪贴板 Ctrl+V 粘贴图片 → image content block"那条路**（TUI 输入 + provider 消息构建处），复用它已有的 content-block 类型，别重造。
- `read` 按 magic bytes / 扩展名识别：图片 + PDF → 返回 image block（大图先压到尺寸/token 上限，参考 claude-code `limits.ts`）；否则维持现有文本/二进制逻辑。
- 两家 provider：Anthropic 原生支持 image block；OpenAI 若不支持 tool_result 图片则降级为"已保存路径 X、无法内联"并诚实标注。
**验收**：read 一张本地 png，模型能描述内容；大图不超 token 预算；非图片二进制仍走原拒绝路径。

### 2. LSP 插件系统（高价值，分两步做）
**动机**：LSP 的**导航**（goToDefinition/findReferences/hover/symbols）远胜 grep（grep 命中同名字符串/注释/字面量），**诊断**（编译/lint 错误）在 edit 后自动注入 = 纯零猜测（模型不必跑 `cargo check` 再解析）。
**架构决策（已确认方向）**：**做成插件，不全量附带**。claude-code 实证：**LSP server 只能经插件提供**（`src/services/lsp/config.ts:11` 原话 "LSP servers are only supported via plugins, not user/project settings"）。opencode 则是**自动探测**（`packages/opencode/src/lsp/server.ts:923` `which("rust-analyzer")`，按扩展名映射）——更省事但把服务器清单写死在内置。tcode 取 claude-code 路线：`/plugins` 里安装 rust-analyzer 这类 LSP 插件，不预装。

**关于"做成 Tool trait 之类的"——架构建议**：**不要试图运行时加载 Rust `Tool` 实现**（需 dylib/ABI，unsafe 且跨版本脆）。tcode 的 `Tool` trait 是编译期的。运行时插件能暴露的实际单元都是**数据 / 外部进程声明**：LSP server（command+args，tcode 驱动协议）、MCP server（**tcode 已支持**，command+args，tcode 当 client）、hooks（外部命令）、skills（markdown）、slash commands。所以 **plugin = 一个 manifest，打包这些外部声明**，而非编译进来的 Rust 代码。claude-code 的插件正是这样：可暴露 `skills/hooks/mcpServers/commands/agents/lspServers`（见 `src/types/plugin.ts` 第 68 行 `lspServers?: Record<string, LspServerConfig>`；`LspServerConfig` = `command`(不含空格) + `args`，见 `src/utils/plugins/schemas.ts` 的 `LspServerConfigSchema`）。

**Step 2a — 最小插件系统**：
- 插件目录 `~/.tcode/plugins/<name>/`，manifest `plugin.toml`。v1 只实现 `[lsp_servers.<id>]`（`command`、`args`、`extensions`/`languages`、`root_markers`），但 manifest 设计成可扩展（未来加 `[[skills]]`/`[hooks]`/`[mcp_servers]`——这些 tcode 已有机制，插件只是打包层）。
- `/plugins` 命令：列已装、从 git URL / 本地路径 / marketplace 索引安装。参考 claude-code `src/services/plugins/pluginOperations.ts`、`src/utils/plugins/lspPluginIntegration.ts`、`src/plugins/builtinPlugins.ts`。
- 先只跑通 rust-analyzer 一个插件证明管路，再泛化。

**Step 2b — LSP 客户端 + 两个模型面**：
- LSP client：spawn 语言服务器（stdio JSON-RPC）——**复用/泛化 tcode 已有的 `tcode-tools/mcp.rs` JSON-RPC 传输**，别重写。`initialize` 握手 → `textDocument/didOpen|didChange|didSave` → 收 `publishDiagnostics`。按语言/根目录建实例。参考 claude-code `src/services/lsp/{LSPClient.ts,LSPServerManager.ts,LSPServerInstance.ts,LSPDiagnosticRegistry.ts,manager.ts}`；opencode `packages/opencode/src/lsp/server.ts` + `packages/core/src/lsp/`。
- **面 a：edit 后诊断自动注入（最高价值，零猜测）**：edit/write 源文件后，推 didChange/didSave、收新诊断、以 `Entry::Note` append（缓存安全）。参考 claude-code `FileWriteTool.ts` 调 `lspManager.changeFile()/saveFile()` + `clearDeliveredDiagnosticsForFile`，及 `src/services/lsp/passiveFeedback.ts`。
- **面 b：`lsp` 导航工具（按需）**：operations = goToDefinition / findReferences / hover / documentSymbol / workspaceSymbol / goToImplementation / callHierarchy（参数 1-based line/character）。参考 opencode `packages/opencode/src/tool/lsp.ts`（operation 清单 + 参数形状最清晰）、claude-code `src/tools/LSPTool/{LSPTool.ts,symbolContext.ts}`。
**验收**：装 rust-analyzer 插件后，edit 一个引入编译错误的改动 → 下一 turn 模型直接看到诊断 Note（无需跑 cargo）；`lsp findReferences` 能跨文件列引用。

### 3. code-mode / `execute`（v2 前瞻，先不做）
**动机**：模型写一段受限脚本，在沙箱解释器里**编排多个工具调用**（循环/条件/工具间传数据）一次跑完，砍掉大量 round-trip。
**参考**：opencode `packages/opencode/src/tool/code-mode.ts`（`execute` 工具，"Run a confined orchestration script with access to connected MCP tools"）；codex `codex-rs/{code-mode,code-mode-host,code-mode-protocol}/`。
**为何缓**：需要沙箱解释器（rhai/lua/wasm 选型 + 工具桥接 + 安全边界），成本高。列 v2，待前两项落地且有真实需求再评估。

### 附：read_output 折叠观察（可选简化，非新工具）
opencode 没有独立 read_output——超限输出**写进文件、回预览 + "read 这个文件"提示**（`packages/opencode/src/tool/truncate.ts` + `truncation-dir.ts`，写 `<data>/tool-output/<id>`，MAX 2000 行/50KB，保留 7 天）。可考虑把 tcode **前台**截断折进 `read`（溢出写 `~/.tcode/projects/<hash>/tool-output/<id>`、回路径而非 `b1` 句柄）：净减一个工具概念、溢出还能被 grep/offset。**但 blob store 仍需保留**给后台任务的**实时流式**输出（文件快照抓不到还在增长的输出）。不紧急。

## 疑问
