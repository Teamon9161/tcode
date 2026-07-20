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

## 测试

`app/harness.rs`（`#[cfg(test)]`）构造一个画进内存缓冲区的真 `App`，`App::frame()` 把整帧读回成文本、`App::press()` 喂真按键。**测试必须驱动真的 `on_term_event` / `on_agent_event` / `redraw`**——harness 只负责构造与读回，复述任何 app 行为就等于什么都没测。测试永不调真实 API。
