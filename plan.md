# tcode — Rust Agent Harness CLI 实施计划

> 注：批准后第一步会把本计划复制为 `C:\code\rust\tcode\plan.md`（用户要求放在项目根目录）。

## Context

从零构建一个类 Claude Code / Codex 的 agent harness CLI，取两者之长：

- **要 Claude Code 的**：精致的终端观感、权限确认 Tab 补充意见、丰富工具集、per-tool hooks、sub-agent、双击 Esc 回退、checkpoints。
- **要 Codex 的**：绝不无故卡死——流式请求全程 watchdog + 状态行永远知情。
- **不要的**：Codex 的沙箱式能力阉割。
- **核心约束**：省 token、缓存命中率最大化。上下文一旦写入绝不回改，用类型系统强制，不靠纪律。

已确认决策：v1 双 Provider（Anthropic + OpenAI 兼容）；inline 渲染为主（Renderer 做成 trait，全屏模式 v2）；含 resume、compact、项目记忆（`.tcode/AGENTS.md` > `AGENTS.md` > `CLAUDE.md`）；双 Shell 以 PowerShell 为主；v1 含文件 checkpoints + rewind；分支只记录进事件日志，浏览 UI 放 v2。

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
~/.tcode/AGENTS.md        # 全局记忆
.tcode/config.toml        # 项目级: hooks, 权限规则
项目记忆: .tcode/AGENTS.md > AGENTS.md > CLAUDE.md (第一个命中注入 system prompt)
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

1. **M0 骨架 + Provider**：workspace、config、双 Provider（SSE 解析、watchdog、重试）、行式 REPL 流式对话。→ 验证：两后端各跑对话；断网/挂代理验证 watchdog。
2. **M1 工具 + loop**：Tool trait、六个工具（含自愈式错误信息）、ledger、agent loop、权限模式+规则（行式确认）、blob store 预算门、Freshness Tracker、中断契约、开局项目地图、尾部自知一行。→ 验证：真实任务"读→改→跑测试"；故意让 edit 失败观察模型无需额外 read 即修复；中断后观察模型不做无效验证；重复读同文件返回 stub。
3. **M2 TUI**：inline 渲染器、markdown/高亮/diff、输入框、slash 命令、**权限选项+Tab 意见**、状态行（token 实时计数 + 缓存遥测）、图片/长文本粘贴。→ 验证：approve+意见流程；连续 turn 的 cache_read 占比接近前缀全长；贴图发给模型描述。
4. **M3 持久化 + rewind + sub-agent + hooks**：JSONL 事件日志、`--continue`/`--resume`、双击 Esc rewind + 文件 checkpoint 回滚、task 工具 + explore agent、hooks、`/compact` + 自动 compact。→ 验证：resume 续任务；rewind 后文件恢复且缓存仍命中（看遥测）；edit 挂 formatter hook。
5. **M4 打磨**：中断边角（半截 tool_use 合法化）、Windows Terminal/conhost 兼容、缓存回归哨兵、`/cost`、错误信息。

v2 方向：MCP 客户端（Tool trait 天然容纳）、全屏渲染器、/branches 分支浏览、memory 深化、WASM 插件式 hooks。

## 验证方式（贯穿）

- Ledger / 缓存断点 / 预算门 / Freshness Tracker：纯单元测试。
- Agent loop：MockProvider 脚本化 tool_use 序列做集成测试，不打真 API。
- 每里程碑用真实 API 跑端到端任务，盯状态行缓存命中数字（这本身就是对"省 token"的持续验收）。
