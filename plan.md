# tcode — Rust Agent Harness CLI 设计文档

> 权威设计文档。改动涉及架构决策时先读它；已实现细节见代码与 `CLAUDE.md`，本文只留仍指导设计的原则、机制与未决项。

## 目标与取舍

类 Claude Code / Codex 的 agent harness CLI，取两者之长：

- **要 Claude Code 的**：精致终端观感、权限确认 Tab 补充意见、丰富工具集、per-tool hooks、sub-agent、双击 Esc 回退、checkpoints。
- **要 Codex 的**：绝不无故卡死——流式请求全程 watchdog + 状态行永远知情。
- **不要**：Codex 的沙箱式能力阉割。
- **核心约束**：省 token、缓存命中率最大化。上下文一旦写入绝不回改，用类型系统强制，不靠纪律。

v1 双 Provider（Anthropic + OpenAI 兼容）；inline 起步、M6 已换自绘全屏；双 Shell 以 PowerShell 为主；含 resume、compact、分层项目指令、文件 checkpoints + rewind。

## 第一性原则：零猜测原则

**模型不应该花任何 token 去获取 harness 本来就知道的信息。** 模型的注意力应全部在任务上，而非推断 harness 状态。下面多个特性都是这一条原则的实例：

| 实例 | 消灭的浪费 |
|---|---|
| 中断契约 | 中断后模型自发重新验证文件状态 |
| 文件新鲜度追踪 | 长会话重复读未变动的文件 |
| **自愈式工具错误** | 工具失败后模型花额外 turn 定位原因：edit 的 old_string 不唯一 → 直接附候选位置上下文；read 路径不存在 → 附相近路径；命令不存在 → 附建议。**省一个 turn = 省一次完整前缀读取**，最大宗的 token 节约 |
| **开局项目地图** | 每会话开头仪式性的 ls / git status / 读 README：启动采集目录树两层 + git 状态 + scratch 路径，注入 system prompt 尾部，进缓存前缀，一次成本 |
| **尾部自知一行** | 模型不知剩余上下文，无法自主决定 compact 或改派 sub-agent：每条最新用户消息附 `ctx 61% · mode: default · since-compact 34k`，附在尾部所以缓存安全 |

## 差异化设计（本项目特有）

1. **类型强制的 append-only Ledger**——缓存命中是编译期保证：历史只有三个合法操作 `append` / `truncate_tail`（rewind）/ `compact`（显式断点原子重写），全部缓存友好。
2. **中断契约**——Esc 中断时注入一条精确状态说明：哪些 tool call 完成、哪些被取消、文件是否被改动。
3. **文件新鲜度追踪**——记录每个已读文件的 (path, mtime, hash, 读取范围)。重复读未变动文件 → 返回一行 stub；被外部改动 → 才返回新内容并附说明。
4. **缓存回归哨兵**——每 turn 状态行显示 cache_read/cache_write/in/out token；连续 turn cache_read 占比异常下跌时显式警告。
5. **事件溯源 UI**——会话 = JSONL 事件日志，渲染器只是事件流消费者；inline/全屏/transcript 导出/resume 重放是同一机制的消费者。
6. **输出分页**——所有工具输出过统一 token 预算门，超限部分落 scratch 文件，上下文只进预览 + 路径，用 read/grep 分页取。

## 三个贯穿全局的机制

### 1. Append-only Context Ledger

```rust
pub struct Ledger { entries: Vec<Entry>, compaction_base: usize }

impl Ledger {
    pub fn append(&mut self, e: Entry);
    /// rewind: 截断尾部。前缀不动, 缓存仍命中。
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

- chunk 级 idle 超时（默认无字节 → 取消 → 指数退避重试，429/5xx/超时可重试）。
- 状态行实时显示：`thinking 12s · ↑3.2k` / `writing · ↑1.8k tok`（流式 delta 实时累计）/ `↻ retrying (2/3) in 4s` / `running: cargo build 45s`。无任何静默状态。
- **所有重试统一在 agent 层**（`agent.rs::stream_step` 的 `'retry` 循环）：连接失败与流中途 stall 同一处理，每次发 `AgentEvent::Retrying`，因此都可见。provider 只做单次尝试并分类返回错误。

### 3. Rewind + Checkpoints（双击 Esc）

- 双击 Esc → transcript 就地跳转并高亮上一个用户输入点，输入框预填原文；Esc/↑ 继续前跳，Enter 确认截断（含文件回滚选项）。转录视觉同步截断。
- **文件 checkpoint**：每次 write/edit 执行前，把原文件按 (session, entry_id) 存盘。回退时询问"仅对话 / 对话+文件一起回"；选"仅对话"时由 Freshness Tracker 提醒模型磁盘上有未回滚改动。
- 事件日志不删旧事件，追加一条 `Rewind { from, to }` 记录 fork。

## 核心抽象

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

/// UI 是事件流的消费者。inline/全屏/transcript 导出/resume 重放走同一接口。
pub trait Renderer { fn on_event(&mut self, e: &SessionEvent); }
```

`ToolCtx`：cwd、Freshness Tracker、checkpoint 写入、blob store、cancellation token、事件上报通道。

### Agent Loop

```
loop {
    req = ledger.as_messages() + tools + cache 断点
    stream = provider.stream(req)            // watchdog 包裹
    渲染 deltas (状态行实时累计 token); 收集 tool_use
    if 无 tool_use: break
    for call in tool_uses:                   // 独立只读调用可并行
        权限 (模式 → 规则 → 交互, 可带 Tab 意见)
        hooks.pre_tool_use
        checkpoint (若为写操作)
        output = tool.run()                  // freshness 去重 + token 预算门
        hooks.post_tool_use
        ledger.append(result + 可选用户意见)
}
// Esc: cancel → 中断契约注入精确状态 → ledger 保持对 API 合法
```

### 权限系统

**模式**（Shift+Tab 循环，状态行常显）：

| 模式 | 行为 |
|---|---|
| `plan` | 读工具照常；写/执行一律询问用户（规则匹配与 default 完全一致） |
| `default` | 按规则匹配，未命中则逐个询问 |
| `accept-edits` | 文件编辑自动放行，shell 等仍询问 |
| `auto` | 全部放行（deny 规则仍生效） |

**规则**：global + project 两级 `config.toml`，`allow`/`deny` 列表，匹配 `工具名(参数 pattern)`（`*` 是唯一通配），如 `shell(cargo *)`、`edit(src/**)`。交互中选 "Yes, don't ask again" 自动写入 project 规则。

**Tab 补充意见**：确认对话任意选项上按 Tab 展开内联输入框。"Yes + 意见" → 批准并把意见作为 user message 追加在 tool result 后；"No + 理由" → 拒绝原因进上下文。纯 append，缓存安全。

**读工具永不询问**：read/grep/glob 的 `permission()` 返回 `None`，任何模式（含 plan）直接放行；外部路径门控只拦 mutating 工具。

**plan mode 是协调信号，不是能力边界**：它在 `decide()` 里**没有自己的臂**，和 default 走同一段（deny → ask 规则 → allow 规则 → 否则 Ask）。理由是原来的硬 Deny 制造了一个站不住的不对称——系统里每条边界最终都通向人，唯独 plan mode 剥夺了**在场用户说 yes 的权利**。而 plan mode 的威胁模型自己就写着"防的是过于积极的合作模型，不是对抗性沙箱"，对这个威胁模型来说，替用户拒绝是过度设计："把计划写进 plan.md" 这种用户明确要求的事，不该逼他先切模式。**代价是 allow 规则在 plan mode 下同样生效**，理论上手写一条 `write(src/**)` 就能让 plan mode 静默失效；实际风险低是因为 `YesProject` 持久化的是 `request.descriptor()` **逐字原样**、从不生成通配，攒出来的都是精确 `run(...)` 串，要出现 write 通配必须是人刻意手写。**因此约束力转移到了 prompt 上**（`plan-mode-enter.md`）：它现在是唯一的实际控制，改它等于改 plan mode 的语义，别当成措辞润色。UI 描述同理不能再说 "read-only tools only"。

**plan mode 不再有自己的 scratch 例外**（曾经有，已删）。它是针对硬 Deny 设计的：那时 scratch 免审是规划期唯一能做事的口子。plan mode 改成询问式之后这个例外反而制造了**倒挂**——同一条 `shell(cwd=scratch)` 在 default 下要审批，在 plan 下反而不用，规划期比正常干活更宽松，说不通。删掉后不变量干净了：**scratch/memory 的本地放行只发生在 auto mode**，别的模式一律照常审批。

**fast path 的准入标准是"声明的 target 等于调用的全部效果"**。`write(path)` 只能影响 `path`，所以路径边界是真的、可检查的；`shell` 声明的 cwd 只是它**起步**的地方，命令内部可以走绝对路径、联网、spawn 任意东西，cwd 什么都框不住。auto mode 里分类器是真正的安全机制，让任意 shell 靠一个不设防的 cwd 声明跳过它是实打实的洞（子 agent 也全跑在 Auto 模式下，`general` 同样受益于这个洞）。故 `shell`/`monitor` 回到 `Classify`，`AutoSafety::AllowInScratch` 变体连同 `targets_scratch` 一并删除——**留着一个"看 cwd 就放行"的语义槽，迟早会有人往里塞第二个工具**。代价是 scratch 内的重复命令每次都付一次分类器请求；真觉得吵再考虑按 `(tool, input)` 缓存 Allow 裁决，但那要等观察到实际流量再做，别预先优化。

**子 agent 继承父会话的模式与规则，能力天花板由 def 自己声明**。两个正交旋钮，别混：

- **继承（`ToolCtx::delegated_permissions`，由 `forward_delegates` 在每次委派调用期间安装并在结束时清除）**：委派出去的活仍是本会话的活，用户为它选的模式、写的 allow/ask/deny 一并适用。读的是**调用时**的状态，所以 turn 中途切模式对下一次委派立刻生效；resume 追问同样重新取，不重放当初 park 时的旧姿态。修掉的是一个真 bug：子 session 曾用 `PermissionRules::default()`，**用户的 deny 规则对子 agent 完全不生效**（`deny=["run(*)"]` 实测拦不住），而权限表明写着 deny 连 unsafe 都能穿透——委派曾是它唯一的静默缺口。同理，父在 default/plan 时子 agent 不再靠分类器自我批准，plan mode 也不再能靠委派绕过。
- **天花板（def 的 `readonly`）**：mutating 工具在 `sub_tools` 里就被摘掉。它比模式更强，因为模式可以被用户点 yes 抬高，而这里连请求都不存在——所以 `explore` 在父是 `unsafe` 时依然动不了项目（有测试钉住）。

`plan` 据此**去掉了 `readonly`**：它的职责本来就包含"先 clone 参考仓库再出计划"，而 def 正文一直在教它这么做、工具集里却没有 shell——承诺了一个结构上不存在的能力。现在它拿到 write/append/edit/shell，全部经继承来的模式把关。**注意 `readonly` 仍重载着另外两件事**，所以摘掉它连带两个可观察变化（各有测试钉住）：`agent(agent='plan')` 的派生本身现在要审批一次（`permission()` 只对 read-only def 返回 `None`），且不再走 `ParallelReadOnly` 批次而是 `Isolated`（对一个能写的 agent 反而更合适）。真嫌派生那次审批多余，就得把 `readonly` 拆成"能力天花板 / 派生免审 / 可并行"三个字段——继承落地后派生审批已是双重把关，但那是独立的一次改动。

**`exit_plan` 结构性地不发给任何子 agent**（`sub_tools` 里按 `PermissionRequest::PlanReview` 过滤，不是按工具名）：提交计划意味着**父会话**的权限模式迁移，这不是被委派方能做的决定。判别用请求类型而非名字——"会请求 plan review 的工具"恰好就是"不能被委派的工具"，新增同类工具自动继承该语义。此前 `explore`/`general` 都带着 `exit_plan`，唯独 `plan` 靠 `disallowedTools` 挡掉，正好是反的；结构化之后那行 `disallowedTools` 已删（留着会让人以为约束住在 def 里）。注意**主 agent 任何模式下都保留它**：工具集属于缓存前缀，跟着模式增删会每次切换都废掉前缀，所以它常驻、由 `PermissionRules::decide` 对非 plan 模式的调用自愈。

**审批桥对所有委派运行安装**，不再由 `questionPolicy` 把关。`questionPolicy` 管的是 `ask_user` 这个**工具**（能不能问开放式问题），与"它要动手时人有没有权决定"是两件事；绑在一个字段上，继承一个会询问的模式就会静默变成一个会拒绝的模式（`NeverAsk`）。

**scratch 目录必须在第一个工具跑起来前就存在**（`ToolCtx::with_scratch_dir` / `rebind_scratch_dir` 里 best-effort `create_dir_all`）：它以前只是个算出来的路径，靠 `write` 建父目录顺带成形，于是 `shell(cwd=scratch)` 撞 `cwd does not exist`——我们把一个路径塞进项目地图承诺给模型，就得保证它真的能用。

**auto mode 本地放行 memory 写入**：`~/.tcode/projects/<id>/memory/` 既不在项目根也不在 scratch 内，但 policy.md 本来就声明该目录写入合法——每次记忆维护都付一次分类器请求只是让模型给自己盖章。`AutoModePolicy::with_memory_root` 把它变成本地快速路径（仅文件编辑工具；shell 在该目录跑命令、或经重定向间接写，仍走分类器，故 policy.md 的 `${TCODE_MEMORY_DIR}` 条款保留）。

**教训——保护路径检查曾误伤 scratch 自己**：`is_protected_path` 把任何含 `.tcode` 组件的路径算作保护路径，而生产环境 scratch 就在 `~/.tcode/projects/…/scratchpad/` 下，于是 auto mode 写 scratch 的快速路径**实际从不生效**（临时目录做的单元测试测不出来）。修法是分层判定：scratch 与 memory 先判、且不做保护检查（`AllowInScratch` 与 plan-mode 例外本来就不查，三者立场一致），项目路径才走 `!is_protected_path`。回归测试直接用 `.tcode` 下的 scratch 路径钉住。

### Hooks

`config.toml` 按事件 + 工具 matcher 触发外部命令，JSON 走 stdin/stdout（对齐 Claude Code）：`pre_tool_use`（可 block/改参）、`post_tool_use`、`turn_end`、`session_start`。

## 工具集

| 工具 | 要点 |
|---|---|
| `read` | offset/limit + 行号；经 Freshness Tracker 去重；识别图片按 magic bytes 归一化后返回 image block（文本模型自愈指向 `view_image`）；大输出/后台日志落 scratch 文件用 read 分页 |
| `view_image` | 以独立 cache scope 调用 `[agents.vision]`（或主模型）按需理解最多 8 张图片，文本结论回流主会话，图片不驻留 ledger |
| `write` / `edit` / `append` | edit = 精确字符串替换；write 覆盖已有文件要求**完整**读过当前版本（partial 视图得到列出已见行段的自愈错误）；append = 末尾原样追加（部分读过即可、缺失文件直接创建、不自动补换行）；三者执行前存 checkpoint；渲染红绿 diff |
| `shell` | Windows: PowerShell 为主 + 检测到 Git Bash 时提供 `bash`；`run_in_background` 进后台注册表，日志流到文件，`kill_task` 停 |
| `monitor` | 后台监视（对齐 claude-code 的 Monitor）：跑平台主 shell 脚本，stdout 每行即一个事件（512B 截断），安全边界作为 `Entry::Note` 注入、空闲时前端按 quiet 合流窗口唤醒 `monitor_turn`（每次空闲唤醒 = 一次完整前缀 cache read，合流即省钱）；事件是 Note 不是 User，Auto Mode 授权判定天然不把事件当用户授权（claude-code 靠 prompt 纪律，这里靠类型）；洪水自动停（120 事件/60s，附"收紧过滤器"自愈提示）；与 shell 共用注册表、日志管道、`kill_task` 与权限规则域（`run(...)`）；默认 5min 超时，`persistent` 免超时；resume 时未终结的任务/监视注入一条"未恢复"Note（零猜测） |
| `grep` / `glob` | 内嵌 grep-searcher/ignore/globset；每行截 512B、`max_filesize` 上限、并行 + 按 (path,line) 排序、deadline 兜底给 partial 标记、剪 VCS/缓存目录、搜 dotfiles + offset 分页 |
| `task` | sub-agent：注册表选类型（`general` + 只读 `explore`），独立 ledger，受限工具集 |
| `web_fetch` / `web_search` | 见下 Web 节 |
| `update_progress` | 前端可见的多阶段执行状态；按真实依赖与里程碑更新，避免与只读 `plan` 权限模式混淆。不可代替方案、结论或交接记录。 |
| `ask_user` | 必须由用户选择才能继续的阻塞分歧；支持多问题分页。不可用于可由代码、项目上下文或现有用户要求确定的细节。 |
| `add_note` | 当前 Ledger 的一条高价值交接记录：仅记录用户决策、已验证约束或未完成工作的边界，供后续步骤延续。不是进度跟踪，不写入跨会话自动记忆；compact 后是否保留由摘要决定。 |

## 配置与运行时路径

- `~/.tcode/config.toml`：provider profiles、全局权限规则（手写，首启向导生成初版）。
- `~/.tcode/state.toml`：当前 profile/model/effort 选择（程序只写这个）。优先级 CLI flag > state > config。
- `.tcode/config.toml`：项目级 hooks、权限规则、MCP server（`[mcp_servers.名字]`，工具注册为 `mcp__名字__工具`）。
- `[agents.<kind>]`（`explore`/`plan`/`general`/`auto`/`suggest`/`vision`/`fetch`）：给 sub-agent 与辅助角色钉模型，`profile`/`model`/`effort` 三个可选字段，未写的继承父模型选择。`fetch` 是唯一"未钉即关"的角色（web_fetch 返回原文而非回退主模型）。Codex CLI 凭证与动态模型缓存由 `tcode-providers` 在加载配置后补全，core 只解析已规范化的 profile 模型。
- 持久上下文两类禁止混写：**人维护指令**（项目根→cwd 分层，每层 `.tcode/AGENTS.md` > `AGENTS.md` > `CLAUDE.md` 取第一个）；**模型维护自动记忆**（`~/.tcode/projects/<id>/memory/`，`MEMORY.md` 只做精简索引）。
- 会话/checkpoint/blob/scratch：`~/.tcode/projects/<cwd-hash>/{sessions,checkpoints,blobs,scratchpad}/`。scratch 暴露给模型（project_map 的 `scratch:` 行 + 系统 prompt 引导），溢出输出与后台日志落 `scratchpad/tool-output/`，7 天清理。
- API key 经 `api_key_env` 指环境变量，不落盘。

## TUI（M6 自绘全屏）

内存中的 transcript 是唯一事实源，屏幕只是它的视图。alternate screen 为唯一 TUI 路径（inline 已删，非 TTY 仍走 plain）。数据结构见 `tcode-tui/transcript.rs`。

渲染的性能纪律与"改动勿回退"细节（wrap 只算一次、只渲染可见切片、双缓冲 + synchronized update、tab 展开、批量 item/result 交错等）已固化，见 `CLAUDE.md` 的「改动勿回退的硬规则」。

其余：markdown + syntect 高亮 + similar 红绿 diff；输入框多行/历史/slash 补全/`@` 项目引用；`@` 候选在后台按项目忽略规则索引，文件按大小完整或首尾摘录附入上下文，目录只附受限树；Ctrl+V 经 arboard 粘贴图片为 image content block、大段文本折叠为 `[pasted #N lines]`；鼠标滚动/拖选/松开复制（SSH 下 OSC 52 回退）。UI 事件循环与 agent loop 是两个 tokio task，仅 mpsc 通道通信。

## 已实现里程碑（索引，细节见代码）

- **M0–M2 基础**：双 Provider（SSE + watchdog + 重试）；Tool trait + agent loop；append-only ledger；权限模式+规则；blob 预算门；Freshness Tracker；中断契约；开局项目地图；尾部自知一行；inline TUI。
- **M3–M4**：JSONL 事件日志 + `--continue`/`--resume`；双击 Esc rewind + 文件 checkpoint；`task` sub-agent + `explore`；hooks；`/compact` + 自动 compact；缓存回归哨兵；`/cost`；半截 tool_use 合法化。
- **计划外**：多模型 profile + chatgpt provider + 首启向导 + `/model`；交互工具；外部会话导入（Codex/Claude Code JSONL adapter，只读复制）；Skills 发现；project_map 预算防御。
- **M5**：后台任务；`web_fetch`/`web_search`（见下）；MCP 客户端（stdio JSON-RPC）；`/export`；Memory 2.0（人维护指令 vs 自动记忆分离、分层加载、外部项目按需）。
- **M6**：自绘全屏 TUI（transcript 单一事实源、就地 rewind、审批拒绝只留一行、鼠标全套）。
- **M7 已完成**：`read` 图片；API 重试可视化（统一到 agent 层）；`read_output` 折进 `read`（溢出/后台日志落 scratch 文件）；`ask_user` 多问题分页，plain 路径显示选项并逐题收集答案；scratch 目录接线进 prompt；命令显示优化（长/多行 shell 命令折成 header + 缩进块）；批量工具 item 与 result 交错渲染（每个 call 的 `├ 摘要` 紧跟自己的结果，live 与 replay 一致）；assistant 流式输出直接在 transcript 中 live Markdown 渲染。UI 展示名下沉到工具自身（`Tool::display_name`，如 `shell`→"Run"、`grep`→"Search"、`glob`→"Find"），批量与单次 header 一致着色。
- **M8 已完成**：sub-agent 模型可配（`[agents.<kind>]`）；`✻ thought` 可点击展开（live 与 replay 同一入口）；连接池 keepalive + header 预算放宽（`no response headers` 误报）；文件工具 IO 异步化 + `edit` 匹配/回显提速；`SlashCommand::hidden()` + `/dogfood`。设计取舍见文末 M8 节。
- **图片能力**：统一图片归一化（1568px 长边和编码预算）、`Provider::supports_vision()`、`[agents.vision]` 与一次性 `view_image` 委派；纯文本模型的 read/粘贴路径自愈为委派与 scratch 文件引用。
- **Monitor**：`monitor` 工具——后台事件流监视 + 空闲唤醒（`Agent::monitor_turn`）+ resume 未恢复提示；机制细节见上方工具集表，防回退规则见 `CLAUDE.md`。WebSocket source 暂不做（shell 脚本可兜底，真需要时再议）。
- **自定义 agent（`.tcode/agents/*.md`）**：只发现项目 `.tcode/agents` 与用户 `~/.tcode/agents`（项目优先）；builtin 定义存于 `crates/tcode-tools/src/agent/builtin/*.md`，由 `tcode-tools/build.rs` 自动扫描、编译期嵌入；它们与用户文件共用同一 YAML/Markdown parser。`explore`/`plan`/`general` 是当前 builtin 保留名，用户文件不得覆盖。frontmatter 走真实 YAML：`name`/`description`/`tools`（allowlist）或 `disallowedTools`（denylist，二者互斥）/`agents`（可 spawn 的 kind，即嵌套授权）/`model`|`profile`|`effort`（仅对通过能力校验的定义合并为默认 pin，手写 config 与 `/agents` 优先）/`readonly`/`maxTurns`/`max_exchanges`/`gatesOutput`（默认 `true`；设为 `false` 时仅跳过 parent-facing 最终 sub-agent report 的 blob 预算门，sub-agent 内部工具输出仍正常门控），正文 = system prompt。selector 支持精确工具、`mcp__*`、`mcp__<server>__*`；运行时按实际 sub-agent 工具集（含已连接 MCP，排除前端交互工具）校验。`readonly` 是不可绕过的 mutating-tool 上限：剥掉后 allowlist 为空就跳过定义并警告，`tools: []` 有效，未匹配 denylist 只警告以保留可移植性。**统一表示**：builtin 与 custom 都是 `AgentDef`，进同一 `AgentRegistry`（`crates/tcode-tools/src/agent_defs.rs`）；registry 同时生成唯一的动态模型 catalogue 与 `agent` 输入 schema，避免硬编码 agent 名单。新请求只暴露模型工具 `agent`（`AgentTool`）；TUI renderer 保留 `task` 别名以回放旧 JSONL。

## Web 工具（现状与设计）

四家里 tcode 曾是唯一自己解析 SERP HTML 的；其余委托 provider 服务端搜索或调 Exa/Parallel。tcode 的差异化机会是 **hosted 优先 + 独立兜底**——任意后端都能用（claude-code 一旦离开一方端点就没搜索）。已落地方案：

- **web_search 可插拔后端，harness 状态不给模型选**（零猜测：模型无从判断后端优劣，暴露 `backend` 只会浪费 token + 给缓存前缀加噪）。模型只调 `web_search(query)`，`search_chain()` 失败自动向后兜底：
  - 默认无 key：Exa(匿名) → Parallel(匿名) → DDG（两家托管实测 keyless 可用，DDG 最终兜底）。
  - `EXA_API_KEY`/`PARALLEL_API_KEY` 存在 → honor 该家 → DDG；`TCODE_WEBSEARCH_BACKEND` 人工覆盖。
  - Exa/Parallel 走 MCP over HTTP（单发 `tools/call`，SSE 取 `result.content[].text`）。Exa 回**为 LLM 优化的 context 文本**（一次调用 = 搜索+抓取+抽取），省掉多轮往返。
- **web_fetch**：流式 body 截断（`bytes_stream()` 边读边累积超 `MAX_BODY_BYTES` 立即中止，不信任 Content-Length）；HTML 默认走 readability 正文抽取（dom_smoothie，页面多为导航/侧栏 chrome，htmd 的 tag skip-list 拦不住 div 搭的；不像文章或抽出 <500 字符则回退整页转换，`raw=true` 显式跳过，header 标注 `(main content; raw=true …)` 保住回路）；可选 `pattern`（正则）回命中行 ±2 行上下文（grep -C 语义，`:`/`-` 区分命中与上下文），零额外模型调用；可选 `prompt` 委派 `[agents.fetch]` 钉住的模型一次性作答（`view_image` 同款骨架：独立 cache scope `fetch-N`、`prompts/web-fetch-summary.md`、DelegateEvent 报用量、输入截 96k 字符），全文落 scratch 附路径作安全阀——摘要可以有损因为原文永远在；**未钉 fetch 模型时 `prompt` 降级为附一行 note 返回原文**（文本进得了主 context，raw 就是自然降级；不回退主模型——那等于花一次整页请求换有损摘要，比直接读还差。这与 vision 未钉回退主模型刻意不同：图片进不了文本模型，必须有人接）；`pattern` 与 `prompt` 互斥（自愈错误）；`www.` 增删同 host 视为安全重定向自动跟；15min URL 缓存（raw 与抽取两个 key）；http→https 升级。角色查找经 `ToolCtx::agent_models`（主会话与 sub-agent 装配时注入同一个 `AgentModels` 句柄），工具自身零构造穿线。

## 未决项

- **出站目标守卫（UNC + SSRF，暂不做）**：read/write/grep 对 `\\server\share`、`//` 开头路径无拦截（触发 SMB 认证、可能泄漏 NTLM）；web_fetch 对内网/环回/云元数据地址（`169.254.169.254`）、URL 内嵌凭证、单段主机名照单全收。两者同源——"未校验目标地址就发起访问"。理想是一个统一的"出站目标白/黑名单"守卫，DNS 解析后按 IP 段判定（防 rebinding）。四家里只有 claude-code 做 SSRF，非刚需，保持简单先搁置。

## 后置 / 暂不做（存档决策）

- **LSP 插件系统（⏸ 暂不做）**：收益要"装语言服务器 + 打通诊断自动注入"才体现，管路成本高。若做：走 claude-code 路线**做成插件不预装**（`~/.tcode/plugins/<name>/plugin.toml`，`[lsp_servers.<id>]`）；plugin = 打包外部声明的 manifest（LSP/MCP/hooks/skills），**不运行时加载 Rust `Tool`**（tcode 的 Tool trait 是编译期的）。LSP client 复用 `mcp.rs` 的 JSON-RPC 传输。两个价值面：edit 后诊断以 `Entry::Note` 自动注入（零猜测）、`lsp` 导航工具（goToDefinition/findReferences 等）。
- **code-mode / `execute`（后置）**：模型写受限脚本在解释器里编排多工具一次跑完。两家的"沙箱"都是脚本引擎本身（非 OS 沙箱）。tcode 的 shell 已能跑任意脚本，增量收益在 MCP 工具多时才划算。真做时引擎选 rhai（Rust 原生、天然受限），先给完整设计（含"脚本一次批准、内部自由跑"的权限模型）再动手。
- **web hosted 搜索委托（暂不做）**：Anthropic/OpenAI 服务端搜索需碰 wire 格式、只在一方端点可用；Exa 免费层已显著提质，性价比更高。

v2 方向：/branches 分支浏览、WASM 插件式 hooks、transcript 内 `/` 搜索。

## 验证方式

- Ledger / 缓存断点 / 预算门 / Freshness Tracker：纯单元测试。
- Agent loop：MockProvider 脚本化 tool_use 序列做集成测试，不打真 API。
- provider SSE/wire 格式：`tcode-providers/tests/wire.rs`。
- 每里程碑用真实 API 跑端到端，盯状态行缓存命中数字（对"省 token"的持续验收）。

## 改进
1. 看codex项目如何用reset次数,
2. pdf支持？skill还是原生？需要识别图片吗？
3. claude-code rules?
4. 前端开发需要截图浏览器页面来做验证,技术路线?
5. batch edit某些情形下行号显示有问题。一行里的替换，减一行加一行，但是左边行号全是1，实际也不是第一行。这是6changes across 1file
