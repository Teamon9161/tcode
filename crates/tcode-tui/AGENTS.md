# tcode-tui 硬规则

改动不得破坏（设计层面的"为什么"见 `plan.md`；全局约束见仓库根 `CLAUDE.md`）。

## 渲染性能

- transcript 是唯一事实源、屏幕只是视图；alternate screen 为唯一路径（inline 已删，非 TTY 走 plain）。
- wrap 只算一次：每块缓存当前宽度的 wrap，resize 才失效；流式追加只重排最后一块。
- 只渲染可见切片：前缀和二分定位视口起点，每帧 O(视口高度)，与转录总长无关。
- ratatui 双缓冲 diff 最小化终端写入，帧外包 crossterm synchronized update 防撕裂；重绘按事件驱动 + 250ms tick 合并。唯一例外是 shimmer 的 100ms 动画 tick（select arm 以 `shimmer_active` 门控，只在有 in-flight 调用或运行中 task 时醒来），且它是 paint-only：`set_task_activity_frame`/`set_live_head` 不改内容不重排，每帧仍 O(视口)。
- wrap 必须展开 tab：`edit`/`append` 回显片段的 `行号\t内容`，tab 宽度测 0 却占 buffer cell，滚动残留浮字；`transcript.rs::wrap_lines_flagged` 按 8 列制表位展开成空格，勿改回裸 tab。

## 渲染注册表

- **按工具名 match 只许出现在 `RenderRegistry::from_tools` 一处**，其余渲染行为一律经 `ToolRenderer` 的 trait 方法（`route` / `header` / `body` / `batch_item` / `quiet_output` / …）。`quiet_output` 派生自活的 `Tool::batch_policy()`，不得退回手工同步的名字表。
- **三条渲染路径（live / replay / approval）必须共用同一组入口**：`bake_call_start`、`batch_header_lines` + `batch_item_lines`、`bake_call_result`（内部 `call_lines` / `result_render`）。各写一套必然漂移——历史教训：重放曾丢批次分组、丢调用间空行、与实时对不上。
- 批量渲染 item 紧跟自己的 result：批次 header 后每个 call 的 `├ 摘要`(+diff) 推迟到自己的 `ToolEnd` 再 bake（`PendingCall.header`），live 与 replay 一致。
- 折叠输出默认：read/grep/glob 转录里默认只显示折叠摘要，不铺开首行。
- 空行是记录的分隔：单发调用 bake 时前置一个空行（带 diff/命令块时后置一个），批次 header 同理。删掉它们记录就糊成一坨。
- 批次分组的判定属于 agent loop（`BatchPolicy` + 路径冲突检查），重放要还原批次显示就调 `Agent::batch_display_label` 问 core，**禁止在 TUI 里重新推导规则**（测试 `batch_display_label_matches_the_live_batch_header` 钉住实时与重放同一标题）。

## 前端归属

- `/model` 与 `/agents` 驱动的是前端自己的选择器，故留在前端；两者共用一个 `Picker`（`/agents` 只是多套一层"选哪个 agent"和一行 inherit），别为第二个选择器再写一套网格。
- **provider setup 的决策逻辑归 `setup.rs`，两个渲染器都不许自己判**：首启向导（`wizard.rs`，裸 crossterm）与 `/provider`（`Overlay::Provider`）驱动同一个 `Setup` 状态机——`App::new` 要 `Arc<Agent>`（即已建好的 provider），正是 setup 要产出的东西，所以首启那条路结构上进不了 overlay，独立渲染器必须存在。两者只在怎么画 `View` 上不同：按键语义、写进 config.toml 的内容都由状态机一处决定。它的两个副作用（读用户全局 config、落盘并重建 provider+菜单）经 `ProviderSetup` 注入，与 `SwitchFn`/`PinFn` 同形——前端不碰磁盘路径也不碰具体 provider，测试才能不依赖本机 `~/.tcode/config.toml`。
- 前端只是 effect 解释器；`CommandEffect` 新增变体的准入标准：要么每个前端都有非平凡解释，要么有明确降级语义，否则逻辑该留在命令自己里。

## 语音输入（`voice/`）

- **识别在 sidecar 进程里**（`crates/tcode-voiced`，`exclude` 出 workspace）。sherpa-onnx 预编译库与 cpal 的 libasound 覆盖不到 musl 与 win-aarch64 两个发布目标，模型能懒下载、静态库不能。协议是单行 JSON，不是 JSON-RPC——一次按住就是一次 start/stop，没有要配对的 id。
- **长按的结束怎么判定是运行时发现的，不是静态探测**。`EndDetect` 三级，`voice/mod.rs` 一处降级，**别压成布尔**：
  | 级别 | 观察到的事实 | 结束判据 | 手势 |
  |---|---|---|---|
  | `Release` | 真实 key-up | 抬键事件 | 按住说话 |
  | `Repeat` | 每次按下都配一个同刻 release | 自动重复停止超过 `idle_limit()` | **仍是按住说话** |
  | `Toggle` | 两次都既无可用 release 也无重复 | 再按一次 | 开关式 |

  关键事实：**伪终端（ConPTY / 无 kitty 协议的终端）结构上给不出 key-up**，它把每个按下配一个同刻抬起；但**操作系统自动重复照常穿过 VT 流**，所以"重复停止"才是普遍可用的松手信号。曾经的两级实现（有/无 key-up）在伪终端下必然退化成开关式，而用户要的正是按住说话。判据是物理的：`SYNTHETIC_RELEASE = 60ms`（没有手指能在 60ms 内完成一次按下抬起）。`idle_limit()` 按观察到的重复间隔自适应（`3×` 最近间隔，下限 250ms）；**首个间隔是重复"延迟"不是"速率"，必须排除**，否则算出的等待会长到离谱。
- **听写落点由 `Dialog::text_field` 一处决定，粘贴与语音共用**。四个文本框（plan 行内评论 / plan feedback / 问题页 note / 普通 note）任何时刻只有一个持有光标，所以"文本该去哪"是一个枚举、不是两份分支——`text_target`（读）与 `focus_text_target`（移焦点）都读它。**分开成两个函数是必须的**：判断一次按键算不算听写要先读落点，而读落点绝不能顺手把焦点抢过去。App 侧 `voice_target()` 把这个和主编辑器统一起来；pickers 没有文本光标，返回 `None`，PTT 因此不拦它们的按键。
- **`space` 的准入条件是"光标在词边界"（行首或前一个字符是空白），不是"草稿为空"**。别的位置上空格就是正在打的分隔符，抢走它等于弄坏键盘；而没人会在空格后面再打一个空格，所以那里的长按是手势。这条同时**消掉了一个特例**：暂定期打出的那个空格自己就是光标前的空白，所以证实长按的那些重复天然仍然匹配，不再需要 `|| is_recording()`。`Editor::at_word_boundary` 必须按字符而非字节取前一个字符——中文用户正是这个功能的主要用户。
- **兼作字符的键（`space`，默认）走"暂定录音"**：暂定期间**每一次**按下都照常把字符打出去（`TypeSpace`）并静默录音，长按被证实后一次性全部退格收回（`RetractSpaces(n)`）；没证实就丢弃录音、字符留下。吞掉按键等于剥夺空格键。三条别改：
  - **每次按下都打字，不只第一次**。只放行第一个会让"连打两个空格"少掉一个；而把后续字符延后补插会让它们排到中间已打进去的普通字符**后面**，顺序错乱。
  - **判据是"数量 + 机器速度"**（`PROVISIONAL_REPEATS` + `FAST_REPEAT`），不是单看数量：只看数量的话慢慢连打迟早凑够，只看速度的话一次抖动就误判。终端若给出 `KeyEventKind::Repeat` 则直接采信。**历史教训：第一次重复就确认（等价于阈值 1）会把"连打两个空格"判成录音，实测踩过。**
  - 暂定期间**不显示录音行**——每打一个空格闪半秒红点比不显示更糟。

  够数但太慢时给一条指向功能键的自愈提示：那种绑定根本不需要与打字区分。
- 转写只进编辑器，**永不自动提交**。录音行用 `theme::recording()`（红），这是唯一一处约定压过内部配色一致性的地方。
- `/voice`、`/voice key`、`/voice model` 的选择都落 `state.toml`（同 `/suggest`、shift+tab 的模式），config.toml 保持手写不被程序改写。
- **模型清单只存在于 sidecar 的 `model.rs::PRESETS`，前端一个字都不许抄。** `voice_picker` 的菜单是**问出来的**（`--list-models` 起一个短命进程，打印 JSON 就退出），不是编译进前端的常量——装着的那个 binary 才知道自己支持什么。抄一份到前端，菜单就会提供它装不上的模型，而用户发现的方式是下完 500MB 才报错。同理 `VoiceConfig::model` 默认是**空串**不是某个名字：默认由 `model::find("")` 一处决定。加一个模型 = `PRESETS` 一行 + `asr.rs` 一个 arm（若是新家族），前端、config、picker 都不动。
- **sidecar 拒绝未知参数，前端把这个拒绝翻译成"重新编译"**（`sidecar.rs::explain`）。flag 只增不减，所以"unknown argument"必然意味着 binary 比 tcode 旧——这是有唯一解法的情形，不该让用户猜。反过来让 sidecar 静默忽略未知 flag 更糟：用户选的模型会悄悄不生效。
- **热词的能力差异由 `Layout` 的 arm 表达，不由名字判断。** transducer（`zh-en`）走 sherpa 的 contextual biasing：必须同时给 `modified_beam_search` + `cjkchar+bpe` + `bpe_vocab`，缺一个都是静默无效；qwen3 走 prompt，逗号分隔一个字符串；CTC 家族没有入口，直接忽略。**只在热词非空时才切 beam search**——不用这个功能的人不该付解码代价。`model.rs` 的测试钉住"note 里写了 hotwords 的 preset 必须真的是有能力的那两个 layout"，否则就是对用户撒谎。
- **`bpe.vocab` 是我们自己从 `bpe.model` 解出来的**（`bpe.rs`，五十行最小 protobuf 读取）。上游只发 `bpe.model`，官方转换脚本要 Python + sentencepiece——为了说一句 "tokio" 而要求用户装 Python 工具链不成立。解出来缓存在模型目录旁边，只做一次。那个 `#[ignore]` 的测试是唯一能证明这个解析器对的东西（拿真模型比对它从没读过的 `tokens.txt`），手搓的字节只能证明它自洽，别删。
- **失败必须能被消掉，且行内自己说怎么消。** `Unavailable` 会把警告钉在 hint 行上，所以 `VoiceEvent::Failed` **不再同时发 notice**——notice 会盖住 `rest`，正好盖掉那句 "esc dismisses"。同一行既是原因又是出口。

## 测试

`app/harness.rs`（`#[cfg(test)]`）构造一个画进内存缓冲区的真 `App`，`App::frame()` 把整帧读回成文本、`App::press()` 喂真按键。**测试必须驱动真的 `on_term_event` / `on_agent_event` / `redraw`**——harness 只负责构造与读回，复述任何 app 行为就等于什么都没测。测试永不调真实 API。
