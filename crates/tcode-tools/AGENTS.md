# tcode-tools 硬规则

改动不得破坏（设计层面的"为什么"见 `plan.md`；全局约束见仓库根 `CLAUDE.md`）。

## 工具执行

- 工具的 `run` 是 async：文件 IO 走 `tokio::fs`，重 CPU 走 `spawn_blocking`。**在 async fn 里做阻塞 `std::fs` 等于把并行批次变回串行**（`join_all` 的每个 future 在一次 poll 里同步跑完）并堵住 runtime 线程——`read`/`write`/`edit` 曾如此。
- `ToolCtx` 的 `std::sync::Mutex`（freshness/blobs/memory）只在短临界区内持有：跨 `await` 持锁会让 future 非 Send，还会把整批写序列化在一个文件的磁盘延迟后面。
- 图片输入只能经 `tcode-core::images` 归一化：文件 read、`view_image` 与剪贴板入口复用同一长边/大小预算，禁止各自编码或缩放。
- 自愈式错误的匹配回退（`edit` 的 punct/ws 归一化）跑在失败路径上，仍要控复杂度：行 key 每行只算一次，别在滑窗里重算（分配级 O(n·m)）。
- **shell/bash 的静默非零退出要自报名字解析**（`shell.rs::resolution_hint`）：两条管道都空 + 非零退出时，模型分不出"解释器是个假货"和"命令真失败"，只能换 shell 盲试——而 harness 知道答案。三条不可省：① 解析必须**在同一 interpreter 里**跑（Git Bash 先搜 `/usr/bin`，Rust 侧走 Windows PATH 会自信地报错误的 binary）；② 名字经**环境变量**传给探针，不拼进脚本——它们出自模型写的命令，插值等于给诊断路径开注入；③ **只在有异常项（解析不到 / 落在 `Microsoft/WindowsApps` 这种 app-execution alias 假货目录）时才吐整张表**，因为 `grep -q`、`test`、`git diff --quiet` 天生静默非零且在批次里极常见，无条件标注就是纯噪音。

## read / grep 的返回内容

- **改动了返回内容就必须三条齐全**（`redact.rs` + `fs/mod.rs::clip`）：① 标记自释（`…[+N chars]` / `[redacted: N chars, starts "…"]`，不许退回裸 `…`）；② `write`/`edit` 用同一个 `read_marker` 拒绝带标记的写回——freshness 只认行范围、认不出行内截断，模型照 context 重写就把标记写进文件；③ 错误与尾注说清怎么拿原文。
- **`read` 返回逐字原文，不带行号栏**：行号每行约 7 B，是长会话里最大的一笔可省开销，而 harness 没有任何机制需要它——`edit` 是精确字符串匹配、freshness 记的是行**范围**、`grep` 自带行号、`read` 的 footer 报告窗口边界（故 offset 读即使读到 EOF 也必须打 `[showing lines A-B of N]`，那是模型唯一的定位信息）。它只换模型凭记忆引用 `file.rs:42` 的能力，不值这个价。`numbered_capped` 的 `number` 参数保留是因为 `edit`/`append` 回显的几行片段确实需要定位，别把它当成开关重新接到 `read` 上。
- **逐行上限是防单行 minified 巨行，不是第二道预算门**：真正的约束是 `MAX_READ_OUTPUT_BYTES`，所以 `MAX_LINE_CHARS` 必须远高于 prose/config/markdown 的自然行长（曾设 500，把 6 行小文件的正常长行也截了，模型得额外付一轮 shell 取回原文）。
- **脱敏对 read 与 grep 同时生效，且必须保持行数**：这两个是全系统唯一免审通道，密钥进 context 就同时进了 provider、session JSONL 与带 `web_fetch` 的上下文；脱敏不是安全边界（`shell` 随时能读），价值是把泄漏从免审通道赶到受审通道。规则按**内容**判定不按路径（不开白名单），键名命中后值还要过形态检查（≥16 字符、无空白、非 `$VAR`/全大写环境变量名——指针不是秘密）。`redact_lines` 一行进一行出，PEM 私钥保留 BEGIN/END 行、只换块内正文，否则 read 的行号与文件错位。`shell` 与 `web_fetch` 有意不脱敏。

## 信任边界的结构防线

仓库根 `CLAUDE.md` 有原则（指令只来自 system prompt 与用户消息），这里是本 crate 里落实它的具体机制。**能用类型和结构挡的不许退化成 prompt 纪律**——下面几条看着冗余，删掉即破防。

- **包标签必须在发出方转义闭合序列**：`web.rs::fence_page` 中和 `</web-page-content>`、`agent` 工具的 `attach_reports` 中和 `</attached-report>`（sub-agent 报告拼进下一个委派的 prompt 时同样是数据）。只包不转义等于没包——正文提前闭合就能续接一个更高权限的标签。转义放发出方（一处），不放读取方（多处，必漂移）。
- **外部内容进 context 必须有围栏**：`web_fetch` 三条出口（普通 / `pattern` 命中 / 委派 `[agents.fetch]`）全走 `fence_page`，新增出口一并走；吃外部内容的子 agent prompt 自己也要声明围栏内是数据（`web-fetch-summary.md`）。
- `SKILL_ECHO_OPEN` 的**格式**只有 `wrap_skill_echo` / `parse_skill_echo` 知道，但常量归 core（`ledger.rs`）——原因见根 `CLAUDE.md`，别为图方便搬回 tools。

## agent 定义

- **统一走 `AgentDef` 注册表，不在 task.rs 里按 kind 长分支**：builtin explore/plan/general/orchestrator 与 custom `.tcode/agents/*.md`/`*.toml` 都是 `AgentDef`，system prompt / 工具过滤（`keeps_tool`）/ permission / batch policy 全从 def 字段读。新增一种内建 kind = `AgentRegistry::builtin()` 里加一个 def，不是在 `run_with_call` 里加 `match` 臂。
- **两种文件格式归一到同一个 `build_def`，别各写一套字段语义**：`.md` 走 YAML frontmatter + 正文，`.toml`（Codex/Impeccable 风）整份是字段表、正文是 `developer_instructions`/`instructions` 字段。`parse_def_toml` 只做两件事——把 TOML 转成同一个 `serde_yaml::Mapping`、抽出正文、把 `model_reasoning_effort` 别名补进 `effort`（native `effort` 优先）——之后与 markdown 共用 `build_def` 做全部校验。新增别名或字段先问：能不能归一到 Mapping 让 `build_def` 统一处理，而不是在 toml 路径里另判一次。
- **discover 递归下钻并跟随目录符号链接**（`collect_agent_files`）：skill 常把自带 agents 放在子目录并 symlink 进 `~/.tcode/agents/<skill>/`，只扫顶层就等于装了 skill 也用不上它的 agent。`MAX_AGENT_DIR_DEPTH` 只为防 symlink 成环，不承担任何布局语义；扫描 best-effort，读不动的条目静默跳过，绝不因一个坏目录让 CLI 起不来。
- **保留字**：explore/plan/general/orchestrator 不许被文件覆盖（其 read-only 语义绑定在 `read_only` 上，覆盖会静默放宽——这与 skills 的"文件覆盖 builtin"刻意相反）。
- **权限分两个正交旋钮，别合并**：模式与 allow/ask/deny **继承自父会话**（经 `ToolCtx::delegated_permissions`，`forward_delegates` 按调用装卸），因为委派出去的活仍是本会话的活——子 session 曾自建 `PermissionMode::Auto` + 空规则，等于用户的 deny 规则对子 agent 静默失效、plan mode 可被委派绕过；而 `readonly` 是 def 自己的**能力天花板**，在 `sub_tools` 里就摘掉 mutating 工具，比模式更强（模式能被用户点 yes 抬高，天花板连请求都不存在），所以 explore 在父是 unsafe 时仍动不了项目。审批桥对**所有**委派运行安装，`questionPolicy` 只管 `ask_user` 工具——把两者绑一起会让"继承一个会询问的模式"静默变成"会拒绝"（`NeverAsk`）。
- **嵌套授权只认 def 的 spawn 策略**（`agents` allowlist / `disallowedAgents` denylist，二选一，镜像 `tools`/`disallowedTools`）：`spawn_list` 解析非空才发受限子 `TaskTool`（`allowed` 限定 spawn 集）；deny 形式对注册表全集实时求差（减名单减自身），自动覆盖后来新增的 custom def——orchestrator 用 `disallowedAgents: []` 编排所有人。`depth < MAX_TASK_DEPTH`（=3）封死递归，不做环检测。
- **追问（resume）走同一 session 同一 cache scope 纯 append**：`max_exchanges > 0` 才进程内保活 Agent+Session（`live` map，`MAX_LIVE_TASKS` 最旧逐出），别把它做成持久化或另起前缀——追问的全部价值就是命中已有前缀缓存、只付增量。
- **失败与中断的 run 一律保活，不许丢**（`park_failed`）：API 报错、watchdog 耗尽重试、用户 ESC，session 都还是**合法**会话（请求没落地则什么都没 append；流中断只 append `IncompleteAssistant`；中断有 `commit_interrupt` 补齐已发起调用的结果），丢掉它等于让父 agent 把整轮工具调用与思考重付一遍。所以失败路径 park 到同一 id、给 `max_exchanges.max(SALVAGE_EXCHANGES)` 次机会（一次性 def 也给），错误文案按零猜测原则直接给出 `agent=…, resume=…` 的续跑调用。resume 的失败同样**回**park 原 id——一次 API 抖动不该毁掉攒了几轮的会话。
- **run id 只在签发它的那次对话里有意义**（`LiveTask::scope` / `StoredReport::scope` = 当时的 `ctx.scratch_dir`）：`AgentTool` 实例活得比对话长（`/resume`、`/clear` 是**同进程**换会话，`bind_scratch_session` 就地重绑），而 trace id 每个会话都从 `t1` 重新数——不带 scope 校验，换会话后模型照着自己旧 transcript 里的 `resume="t1"` 调过来，接上的是**上一个对话**的 sub-agent，那个会话的上下文就这么漏进来了（`attach` 的 reports 同理）。park/remember 时顺手 `retain` 掉非当前 scope 的条目：它们已不可达，留着只占 cap 和内存。
- **内存里没有就回落到 trace 重建**（`restore_run`）：`live` 是进程内的，但 `tasks/<session>/tN.jsonl` 不是——重启或 `/resume` 之后同一对话的 run 仍应可续，换回来的不是缓存（前缀早没了）而是**那轮已经付过钱的工具调用与结论**。三条不可省：① 重建走 `build_run`，与新起一个 run 完全同一条构造路径，别另写一份会漂移的；② 每轮追问各写一份**只含自己增量**的 trace，所以 `TaskTraces::restore` 必须按 `resume_of` 把整条链**重放进同一个 ledger**（`TruncateTail`/`Compact` 记的是会话下标，分别 load 再拼会错位），否则第二次重启恢复出来的是个失忆的 run 却装作完整；③ 被 kill 的 trace 可能停在没有结果的 tool_use 上——`Ledger::close_dangling_tool_calls` 补齐并如实说明"是否生效未知"，这是活路径永远产生不了的形状。
- **`resume` 的 id 是模型给的数据，不是路径**：`TaskTraces::trace_path` 只接受 `t<digits>`，因为它要去 join 文件名——直接拼就等于给 `agent` 工具开了一个免审批的任意文件读。
- `--agent` 顶层 run 用 `scoped_to(def)` 把进程本身当作深度 1 的该 agent。
- sub-agent 的 system prompt 就是 `def.system`（定义文件正文）原样，不拼工具清单也不拼项目地图——工具信息走 API 的 `tools` 参数，prompt 里再列一遍是重复计费。

## 合并审批

- **只能覆盖"本来就会逐个弹窗"的调用**：`combined_change_review` 的候选筛选只认 `session.rules.decide` 判出的 `Decision::Ask`——这一步是纯函数，不发分类器请求、不改 mode，所以"组装这次审批"本身不可能授权任何东西；Auto Mode 走 `Decision::Auto`→Classify 的调用一律保留各自的独立提示。答复只回填给 `CombinedReview::covered` 里记下的下标，屏幕上没出现过的调用永远拿不到这个答复。前端答不出（通道断、不支持）时返回 `Individually` 退回逐个流程，**不得**代用户判 No。

## 测试

agent loop 用 `MockProvider` 脚本化 `StreamEvent` 序列驱动真实工具跑真实临时目录（`tests/agent_loop.rs`）。测试永不调真实 API。
