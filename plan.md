# tcode — Rust Agent Harness CLI 设计文档

> 权威设计文档。改动涉及架构决策时先读它；已实现细节见代码与 `CLAUDE.md`，本文只留仍指导设计的原则、机制与未决项。


## 第一性原则：零猜测原则

**模型不应该花任何 token 去获取 harness 本来就知道的信息。** 模型的注意力应全部在任务上，而非推断 harness 状态。下面多个特性都是这一条原则的实例：

| 实例 | 消灭的浪费 |
|---|---|
| 中断契约 | 中断后模型自发重新验证文件状态 |
| 文件新鲜度追踪 | 长会话重复读未变动的文件 |
| **自愈式工具错误** | 工具失败后模型花额外 turn 定位原因：edit 的 old_string 不唯一 → 直接附候选位置上下文；read 路径不存在 → 附相近路径；命令不存在 → 附建议。**省一个 turn = 省一次完整前缀读取**，最大宗的 token 节约 |
| **开局项目地图** | 每会话开头仪式性的 ls / git status / 读 README：启动采集目录树两层 + git 状态 + scratch 路径，注入 system prompt 尾部，进缓存前缀，一次成本 |
| **尾部自知一行** | 模型不知剩余上下文与当前权限模式：每条最新用户消息附 `<tcode-status>context ~61% of 200k tokens · permission-mode: default</tcode-status>`（另有 `background running` 与未批准 draft 计划提醒），高占用时改派 sub-agent 的依据；附在尾部所以缓存安全。compact 由 harness 按阈值自动触发，不属模型决定 |

## 差异化设计（本项目特有）

1. **类型强制的 append-only Ledger**——缓存命中是编译期保证：历史只有三个合法操作 `append` / `truncate_tail`（rewind）/ `compact`（显式断点原子重写），全部缓存友好。
2. **中断契约**——Esc 中断时注入一条精确状态说明：哪些 tool call 完成、哪些被取消、文件是否被改动。
3. **文件新鲜度追踪**——记录每个已读文件的 (path, mtime, hash, 读取范围)。重复读未变动文件 → 返回一行 stub；被外部改动 → 才返回新内容并附说明。**stub 的前提是"那次读还在上下文里"**，所以 rewind 与 compact 都走同一个 `Session::forget_seen_files` 清空它：条目一旦离开模型的窗口，"你已经有了"就是假话。


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
- Compact 仅显式触发（`/compact` 或 token 逼近上限），子请求生成摘要。**一次压缩要么替换历史、要么什么都没发生，`compact()` 返回的就是这个事实**：没产出摘要时 ledger 一字未动，触发它的阈值仍然成立，所以自动路径必须停手（写一条 Note 说明，`/compact` 手动仍可再试），否则每个安全边界都会重来一次——而那是这个会话最贵的一种请求，且恰好在前缀最大的时刻发出。同理，**队列里的用户消息在压缩之后才投递**：先投再压等于把用户刚打完的那句话压成别人的转述。
- **Progress 文件是外部可变状态，不是历史**：`~/.tcode/projects/<id>/progress/*.md` 可以被用户随手改，ledger 里只记模型发过的 `progress` 工具调用。二者不冲突——"文件可改"不等于"历史可改"，这份文件记的是**现在为真**的东西，ledger 记的是**当时发生过**的事。用户改过之后下一次工具调用返回自愈冲突（附他们的原文），而不是悄悄覆盖。
- **Compact 移出模型上下文的条目进 `archived`，不销毁**：`entries()` / `as_messages()` 语义一字不变（模型只见 Summary），但 transcript 与 `/export` 走 `history()` = archived + entries，resume 后仍看得到压缩前的对话。archived 没有合法 `truncate_tail` 索引（rewind 进不去被压缩的历史）；`truncate_tail(0)`（`/clear`）连它一起清空。

### 2. Stream Watchdog + 永远知情的状态行

- chunk 级 idle 超时（默认无字节 → 取消 → 指数退避重试，429/5xx/超时可重试）。
- 状态行实时显示：`thinking 12s · ↑3.2k` / `writing · ↑1.8k tok`（流式 delta 实时累计）/ `↻ retrying (2/3) in 4s` / `running: cargo build 45s`。无任何静默状态。
- **所有重试统一在 agent 层**（`agent.rs::stream_step` 的 `'retry` 循环）：连接失败与流中途 stall 同一处理，每次发 `AgentEvent::Retrying`，因此都可见。provider 只做单次尝试并分类返回错误。

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
| `default` | 按规则匹配，未命中则逐个询问 |
| `accept-edits` | 文件编辑自动放行，shell 等仍询问 |
| `auto` | 全部放行（deny 规则仍生效） |

**规则**：global + project 两级 `config.toml`，`allow`/`deny` 列表，匹配 `工具名(参数 pattern)`（`*` 是唯一通配），如 `shell(cargo *)`、`edit(src/**)`。交互中选 "Yes, don't ask again" 自动写入 project 规则。

**交互选中的模式一律记进 `[tcode_state]` 当新会话默认，`unsafe` 也不例外**（TUI `persist_mode` 与 app `picker::remember_mode`，两处必须一致）。`unsafe` 曾是唯一例外，理由是"一次性放行不得静默武装以后每个会话"；实际代价全落在真按那个模式工作的人身上——每开一个会话重选一次自己早就做过的决定，而重选不是决定。当前模式一直显示在状态行/chip 上、一个键就能改，提醒该由它承担。

**Tab 补充意见**：确认对话任意选项上按 Tab 展开内联输入框。"Yes + 意见" → 批准并把意见作为 user message 追加在 tool result 后；"No + 理由" → 拒绝原因进上下文。纯 append，缓存安全。

**模式切换即时生效于下一次权限判定**：shift+tab 切的模式只改闸门、不入 ledger。闸门在每个权限判定点提交（回合始、`run_tools` 入口、串行批每个待批调用前、批边界、回合末），所以切到 unsafe/auto 后，模型这一步吐出的调用、乃至同批后续调用立刻按新模式判定，不必等一整个来回到下一批边界。**模式从不作为 note 写进 ledger**：模型看到的当前模式由 `status_block` 每回合重新导出到 user turn 的 tail，所以裸切键既不用追加什么、也没有什么需要撤回。曾经的 plan enter/exit note 是唯一的例外，随 plan mode 一起删除。

**读工具永不询问**：read/grep/glob 的 `permission()` 返回 `None`，任何模式直接放行；外部路径门控只拦 mutating 工具。

**plan mode 已删除，规划改由 Progress 文件承载**。它曾占着 `PermissionMode` 的一格，但"我还在决定做什么"和"我愿意承担多大风险"是正交的两件事，合并的代价是规划期选不了 auto/unsafe——而探索恰恰是最该放开的阶段。它在 `decide()` 里也从来没有自己的臂（与 default 同一段代码），即**在权限表里本来就没有语义，却霸占着权限表的一格**。现在"先别动手"这个约束来自**文件处于 `draft`** 这一事实：`status_block` 每回合从文件重新导出，不是一次性注入后靠模型自觉记住的指令，批准后自动消失。唯一保留的权限语义是 `PermissionRequest::PlanReview` **在任何模式下都 Ask**（含 unsafe）：用户要求看计划，与他执行时愿担多大风险是两个问题、两个答案，正好在同一个对话框里分别回答。

**`/plan` 回合本身也有结构保障，不再纯靠 prompt 纪律**。`/plan`（桌面 composer 的 plan 开关同）提交的是类型化的一等概念：`CommandEffect::PlanTurn` / `PendingMessage.expects_plan` 把"这是规划回合"从命令效果一路通到 `Session::planning_expected`（内存态，不持久化；resume 后由 ledger 里的 plan 指令兜底）。**任务文本是用户自己的消息，不是指令的一部分**：`PlanTurn` 只携带 harness 指导（`planning_instruction`），任务描述作为用户消息（blocks）提交——这样 `@path` 引用照常展开、任务留在转录/rewind 里，与桌面 composer 一致；把任务揉进 instruction 字符串会让它绕过 `expand_references`，这是曾修掉的一个 bug。draft 之前的窗口——连文件都不存在、`status_block` 无从导出的阶段——由两道防线覆盖，都是"排序"而非"风险"语义，不动任何审批准入：

- **回合末护栏**（`run_steps` 的 `planning_turn_end`）：规划回合结束而没有任何 plan 文件时，第一次注入一条 model-only 的 nudge（`prompts/commands/plan-nudge.md`）让模型补救并继续循环；第二次仍无 plan 则上用户可见的 notice（`Entry::Note` + `AgentEvent::Note`："No plan was produced…"）并清标记。max_tokens 放弃与 step-limit 路径只 notice 不 nudge；中断直接清标记，不污染下一次 monitor 唤醒回合。护栏看"文件存在"（draft 也算做出了 plan），不看是否 active——提交与审批是 PlanReview 对话框自己的保障。
- **文件变更门**（`run_tools` 的 `planning_gate_blocks`）：plan 文件存在前，`touches` 指向**项目内文件**的 write/edit/append 在 `permission_decision` 之前被拒（不弹审批框），返回自愈错误并说明 scratch 例外。**scratch 与 auto memory 永远放行**——规划要 clone 参考仓库、写探针脚本，正是 plan def 摘掉 `readonly` 时承诺的能力；shell/monitor 放行保探索（真正执行由回合末护栏兜底）；`progress`/`ask_user` 永不被拦（创建 plan 不死锁、规划中可问人）。draft 一旦创建门即失效，回到文件状态的软约束。
- **用户输入是唯一人为释放手段**：`user_turn`/`monitor_turn` 起始清标记，`deliver_pending_input` 按每条消息自身意图设置（普通用户消息即释放）。模型无法伪造用户输入，所以"无 plan 静默结束"在结构上只剩两条出路：模型两次拒绝（伴随 notice 上屏），或用户开口接管。

**`scratch` 没有任何模式例外**：scratch/memory 的本地放行只发生在 auto mode，别的模式一律照常审批。

**子 agent 继承父会话的模式与规则，能力天花板由 def 自己声明**。两个正交旋钮，别混：

- **继承（`ToolCtx::delegated_permissions`，由 `forward_delegates` 在每次委派调用期间安装并在结束时清除）**：委派出去的活仍是本会话的活，用户为它选的模式、写的 allow/ask/deny 一并适用。读的是**调用时**的状态，所以 turn 中途切模式对下一次委派立刻生效；resume 追问同样重新取，不重放当初 park 时的旧姿态。修掉的是一个真 bug：子 session 曾用 `PermissionRules::default()`，**用户的 deny 规则对子 agent 完全不生效**（`deny=["run(*)"]` 实测拦不住），而权限表明写着 deny 连 unsafe 都能穿透——委派曾是它唯一的静默缺口。同理，父在 default 时子 agent 不再靠分类器自我批准。
- **天花板（def 的 `readonly`）**：mutating 工具在 `sub_tools` 里就被摘掉。它比模式更强，因为模式可以被用户点 yes 抬高，而这里连请求都不存在——所以 `explore` 在父是 `unsafe` 时依然动不了项目（有测试钉住）。

`plan` 据此**去掉了 `readonly`**：它的职责本来就包含"先 clone 参考仓库再出计划"，而 def 正文一直在教它这么做、工具集里却没有 shell——承诺了一个结构上不存在的能力。现在它拿到 write/append/edit/shell，全部经继承来的模式把关。**注意 `readonly` 仍重载着另外两件事**，所以摘掉它连带两个可观察变化（各有测试钉住）：`agent(agent='plan')` 的派生本身现在要审批一次（`permission()` 只对 read-only def 返回 `None`），且不再走 `ParallelReadOnly` 批次而是 `Isolated`（对一个能写的 agent 反而更合适）。真嫌派生那次审批多余，就得把 `readonly` 拆成"能力天花板 / 派生免审 / 可并行"三个字段——继承落地后派生审批已是双重把关，但那是独立的一次改动。

**提交计划结构性地不发给任何子 agent**：`progress` 根本不在 `builtin_tools()` 里，它由三个前端在主会话装配点单独 push（`tcode-frontend/src/agent.rs`、TUI harness）。提交计划意味着**父会话**的权限模式迁移，这不是被委派方能做的决定。`sub_tools` 里那条按 `PermissionRequest::PlanReview` 过滤的 retain 留作兜底，防的是以后有扩展工具带上同一请求类型。

**审批桥对所有委派运行安装**，不再由 `questionPolicy` 把关。`questionPolicy` 管的是 `ask_user` 这个**工具**（能不能问开放式问题），与"它要动手时人有没有权决定"是两件事；绑在一个字段上，继承一个会询问的模式就会静默变成一个会拒绝的模式（`NeverAsk`）。

**scratch 目录必须在第一个工具跑起来前就存在**（`ToolCtx::with_scratch_dir` / `rebind_scratch_dir` 里 best-effort `create_dir_all`）：它以前只是个算出来的路径，靠 `write` 建父目录顺带成形，于是 `shell(cwd=scratch)` 撞 `cwd does not exist`——我们把一个路径塞进项目地图承诺给模型，就得保证它真的能用。

**auto mode 本地放行 memory 写入**：`~/.tcode/projects/<id>/memory/` 既不在项目根也不在 scratch 内，但 policy.md 本来就声明该目录写入合法——每次记忆维护都付一次分类器请求只是让模型给自己盖章。`AutoModePolicy::with_memory_root` 把它变成本地快速路径（仅文件编辑工具；shell 在该目录跑命令、或经重定向间接写，仍走分类器，故 policy.md 的 `${TCODE_MEMORY_DIR}` 条款保留）。

**教训——保护路径检查曾误伤 scratch 自己**：`is_protected_path` 把任何含 `.tcode` 组件的路径算作保护路径，而生产环境 scratch 就在 `~/.tcode/projects/…/scratchpad/` 下，于是 auto mode 写 scratch 的快速路径**实际从不生效**（临时目录做的单元测试测不出来）。修法是分层判定：scratch 与 memory 先判、且不做保护检查（`AllowInScratch` 与 plan-mode 例外本来就不查，三者立场一致），项目路径才走 `!is_protected_path`。回归测试直接用 `.tcode` 下的 scratch 路径钉住。

### Hooks

`config.toml` 按事件 + 工具 matcher 触发外部命令，JSON 走 stdin/stdout（对齐 Claude Code）：`pre_tool_use`（可 block/改参）、`post_tool_use`、`turn_end`、`session_start`。

## 工具集

| 工具 | 要点 |
|---|---|
| `read` | offset/limit；**无行号 gutter**（内容逐字返回，引用行时从窗口首行数起——省掉每行的行号 token）；经 Freshness Tracker 去重；识别图片按 magic bytes 归一化后返回 image block（文本模型自愈指向 `view_image`）；大输出/后台日志落 scratch 文件用 read 分页 |
| `view_image` | 以独立 cache scope 调用 `[agents.vision]`（或主模型）按需理解最多 8 张图片，文本结论回流主会话，图片不驻留 ledger |
| `write` / `edit` / `append` | edit = 精确字符串替换；write 覆盖已有文件要求**完整**读过当前版本（partial 视图得到列出已见行段的自愈错误）；append = 末尾原样追加（部分读过即可、缺失文件直接创建、不自动补换行）；三者执行前存 checkpoint；渲染红绿 diff |
| `shell` | Windows: PowerShell 为主 + 检测到 Git Bash 时提供 `bash`；`run_in_background` 进后台注册表，日志流到文件，`kill_task` 停 |
| `monitor` | 后台监视（对齐 claude-code 的 Monitor）：跑平台主 shell 脚本，stdout 每行即一个事件（512B 截断），安全边界作为 `Entry::Note` 注入、空闲时前端按 quiet 合流窗口唤醒 `monitor_turn`（每次空闲唤醒 = 一次完整前缀 cache read，合流即省钱）；事件是 Note 不是 User，Auto Mode 授权判定天然不把事件当用户授权（claude-code 靠 prompt 纪律，这里靠类型）；洪水自动停（120 事件/60s，附"收紧过滤器"自愈提示）；与 shell 共用注册表、日志管道、`kill_task` 与权限规则域（`run(...)`）；默认 5min 超时，`persistent` 免超时；resume 时未终结的任务/监视注入一条"未恢复"Note（零猜测） |
| `grep` / `glob` | 内嵌 grep-searcher/ignore/globset；每行截 512B、`max_filesize` 上限、并行 + 按 (path,line) 排序、deadline 兜底给 partial 标记、剪 VCS/缓存目录、搜 dotfiles + offset 分页 |
| `task` | sub-agent：注册表选类型（`general` + 只读 `explore`），独立 ledger，受限工具集；`background: true`（仅主 agent）不阻塞派发，完成时 report 作为 fenced `Entry::Instruction` 唤醒主 agent——**模型收得到、转录里不出现**（人这边对应的是那次 run 本身），非交互 |
| `web_fetch` / `web_search` | 见下 Web 节 |
| `progress` | 一个任务一份耐久的计划文件（`~/.tcode/projects/<id>/progress/`，`draft`/`active`/`done`），也是前端进度面板的数据源。**它是该文件唯一的写入者**——模型不得用 `edit` 改它。**一份计划是包含阶段表的文档**：`description`（一行，inventory 用）+ `background`（不属于任何阶段的正文：决策、勘查确立的事实、数据结构、贯穿约束、被否掉的方案）+ `phases`。返回值只回显刚翻成 `[>]` 的那一阶段的 `detail`，所以十二阶段的计划任何时刻只有一个阶段的正文在上下文里；`background` 只在会话接手文件时随摘要下发一次，超预算退化成小节标题。`state: "active"` 就是"提交给用户审批"这一动作本身，因此 `permission()` 是输入的纯函数；用户手改文件后下次更新返回自愈冲突（附他们的原文）。不可代替方案、结论或交接记录。 |
| `ask_user` | 必须由用户选择才能继续的阻塞分歧；支持多问题分页。不可用于可由代码、项目上下文或现有用户要求确定的细节。 |
| `add_note` | 当前 Ledger 的一条高价值交接记录：仅记录用户决策、已验证约束或未完成工作的边界，供后续步骤延续。不是进度跟踪，不写入跨会话自动记忆；compact 后是否保留由摘要决定。 |

## 配置与运行时路径

- `~/.tcode/config.toml`：provider profiles、全局权限规则（手写，首启向导生成初版）。
- `~/.tcode/state.toml`：当前 profile/model/effort 选择（程序只写这个）。优先级 CLI flag > state > config。
- `.tcode/config.toml`：项目级 hooks、权限规则、MCP server（`[mcp_servers.名字]`，工具注册为 `mcp__名字__工具`）。
- `[agents.<kind>]`（`explore`/`plan`/`general`/`auto`/`suggest`/`vision`/`fetch`）：给 sub-agent 与辅助角色钉模型，`profile`/`model`/`effort` 三个可选字段，未写的继承父模型选择；也可写成一个字符串（`"off"` / `"inherit"` / 模型名）。`fetch` 是唯一"未钉即关"的角色（web_fetch 返回原文而非回退主模型）。Codex CLI 凭证与动态模型缓存由 `tcode-providers` 在加载配置后补全，core 只解析已规范化的 profile 模型。
- **`[presets.<name>]` 是"整套模型编排"，与 `[profiles.*]`（怎么连到 provider）正交**：主模型 + 每个角色跑什么，整体切换。三层解析次序 `[agents.*]` → 活跃 preset → `[tcode_state]` 的临时 pick，`Config::apply_active_preset` 是唯一汇合点。**切 preset 会清空 `[tcode_state]` 里的临时 pick 与主模型**——不清就永远没有一个 preset 能完整描述"现在跑的是什么"；想留住微调就 `/model save <name>` 存成新 preset（这也是程序唯一一处写 `[tcode_state]` 之外的表，只增/替换那一张）。
- 持久上下文两类禁止混写：**人维护指令**（项目根→cwd 分层，每层 `.tcode/AGENTS.md` > `AGENTS.md` > `CLAUDE.md` 取第一个）；**模型维护自动记忆**（`~/.tcode/projects/<id>/memory/`，`MEMORY.md` 只做精简索引）。
- 会话/checkpoint/blob/scratch：`~/.tcode/projects/<project-id>/{sessions,checkpoints,blobs,scratchpad}/`。scratch 暴露给模型（project_map 的 `scratch:` 行 + 系统 prompt 引导），溢出输出与后台日志落 `scratchpad/tool-output/`，7 天清理。
- **`<project-id>` 用路径本身而非 hash**（`c:\code\rust\tcode` → `c--code-rust-tcode`）：这个目录名是人去翻会话日志、存档 plan、项目记忆时唯一的线索，不可读的 hash 让每个目录都无法辨认。折叠分隔符不是单射（`C:\code\rust-tcode` 会撞上），代价是两个项目共用一份状态，接受。旧 hash 目录在 `store::project_dir_in` 里懒迁移（rename，目标已存在则并入），迁完即可删那段代码。
- **voice sidecar has an independent release version**: `tcode-voice-protocol` is the shared source of `SIDECAR_VERSION` and the `voice-v<version>` release tag. Normal tcode releases reuse `~/.tcode/voice/tcode-voiced-<sidecar-version>`; bump and publish this version only when the sidecar itself changes. The sidecar’s `Cargo.toml` version is tested against the shared version, while the `voice-v*` workflow validates both manifests before building platform assets.
- API key 经 `api_key_env` 指环境变量，不落盘。

## 验证方式

- Ledger / 缓存断点 / 预算门 / Freshness Tracker：纯单元测试。
- Agent loop：MockProvider 脚本化 tool_use 序列做集成测试，不打真 API。
- provider SSE/wire 格式：`tcode-providers/tests/wire.rs`。
- 每里程碑用真实 API 跑端到端，盯状态行缓存命中数字（对"省 token"的持续验收）。


## 改进
1. 图表数据绑定（`show` 第二阶段，`{"$file": "pnl.csv"}`）——**计划已写，未实现**，执行细节与"可能不做"的前提检查见 `crates/tcode-app/DATA-BINDING.md`。
2. gpt订阅有图片生成模型吗
3. acp支持
12. app plan模式execute in new session应该支持切换模型,现在直接就是原模型执行
22. 图片查看器为啥用白色背景，一般不都是淡黑色还什么的吗，而且图片至少也要居中吧， 然后还支持放大和缩小，类似放大镜的那种icon，方向键左右可以换图片，这种有现成的库吗。
24. Tool friction — shell 自托管构建隔离：当前 harness 正在使用仓库的 target/debug/tcode.exe，导致 cargo test --workspace 无法替换该文件；为保留会话和构建产物，只能切换目标目录并重新编译整个 workspace，额外耗时约 69 秒。若 harness 从仓库 target 外的副本启动，或自动保留独立的 harness target，正常 workspace 验证即可复用现有缓存
26. Tool friction — read: crates/tcode-app/ui/src/Rail.tsx 第 88 行在字符串里用了 NUL 作分隔符（sessions.map(s => s.cwd).join("\0")），read 因此把整个文件判为 binary 拒读，我被迫用 tr '\0' '|' 走 bash 看内容，丢了行号与分页能力，还多花了几次探测。JS 源码里用 NUL 当分隔符是合法常见模式（"\0" 是转义字符，文件本身是 UTF-8 文本）；最小改进是让 read 对"含 NUL 但其余为合法 UTF-8"的文件降级读取，把 NUL 显示为转义（如 \0），而不是整文件判 binary。
28. app prompt框上方的plan栏，展开后，再展开background，没法滚动，在长background的时候展开后看不全，也没法看phase了。
30. Tool friction — browser.snapshot：为验证一个搜索筛选结果，快照把页面中 180 行事件明细和所有图表可访问文本都返回，产生了 3,500 余行输出。若支持按元素 ref 截取快照或提供最大文本量参数，可避免长表页面的无关上下文开销。
31. Tool friction — browser.screenshot：真实截图成功后，API 返回了“image(s) omitted: this API cannot carry images returned from a tool”，导致当前会话无法把像素交给 view_image 做二次检查。最小改进是将返回图片保留为可检查的 artifact/path，或允许工具返回的 image block 直接作为 view_image 输入；否则截图成功也只能验证尺寸和调用状态，不能检查实际像素。
32. app Append不显示diff
33. Tool friction — browser.screenshot: 使用具体视觉检查 prompt 截图后，只返回了“image omitted: this API cannot carry images”，没有返回描述中预期的视觉模型结论。为完成检查，不得不通过临时 Electron 脚本保存截图，再调用 view_image。在图片不能进入主上下文时直接返回视觉子模型的文字结论即可消除这组额外调用。

Capability gap — browser viewport sizing: 本次必须验证 900px 以下的 responsive rail，但 browser 工具没有调整 viewport 的动作。最终通过临时 BrowserWindow 设置 800px 宽度并截图。增加受限的 resize 动作，或给 screenshot 增加 viewport 宽高参数，就能直接完成响应式页面验证。