# tcode-app — 硬规则

Tauri 桌面前端：Rust 后端（本 crate）+ webview 前端（`ui/`，Vite + React + TS）。后端是持有一个 `Arc<Agent>` 与多个隔离 `Session` 的 supervisor，事件经 Tauri emit 推给 webview。

## 构建与运行

**不在 workspace 里**（理由同 `tcode-voiced`：Tauri 链接平台 webview，Linux 上需要 webkit2gtk + libsoup，`cargo build --workspace` 不能开始要求所有人装这些）。所以命令都在本目录跑：

```bash
cd crates/tcode-app
(cd ui && npm install && npm run build)   # 首次 / 改过前端
cargo build && cargo test                 # 后端 + 集成测试
./target/debug/tcode-app                  # 起 app（把 cwd 作为第一个会话）

(cd ui && npm run preview:ui)             # 设计预览：浏览器里看全部界面状态
```

**改界面先开 `npm run preview:ui`。** 它用 `PREVIEW=1` 把 `@tauri-apps/api/*` 与 dialog 插件别名到 `ui/src/preview/` 下的 fixture，然后加载 `preview.html`，把**真实组件**（不是另画一套 mock）按 launchpad / session / approval / empty 四个场景摆出来。没有它，"跑起来的会话正在等审批"这类状态要复现一次得起真 provider 打真 API。别把 mock 引进 `main.tsx` 那条路径——别名只在 `PREVIEW=1` 下生效，发布产物里没有它们。

**`tauri.conf.json` 里刻意不配 `devUrl`。** Tauri 在 debug 构建下只要看见 `devUrl` 就去连它，于是 `cargo run` 会撞上 "Connection refused"——而 `cargo run` 正是这里的主流程。不配它，debug 与 release 一样加载 `frontendDist`（`ui/dist`），代价是改前端要重跑一次 `npm run build`。想要 HMR 就临时加回 `devUrl: "http://localhost:5173"` 并同时起 `npm run dev`，别把它留在提交里。

## 不可违背

1. **装配逻辑不在这里重写**。config 加载、`Arc<Agent>` 组装、开会话全部走 `tcode-frontend`（`boot` / `open_session`）。`src/boot.rs` 只放 app 独有的决定（开哪个文件夹、没配置 provider 时报错而不是画向导——这里没有终端可画）。发现自己在抄 `src/main.rs` 的段落时，那段就该下沉到 `tcode-frontend`。
2. **一切逻辑写在 `Emit` 上，不写在 `AppHandle` 上**。跑 turn 的路径必须能在没有窗口时被测试驱动（`tests/bridge.rs` 用 collector 顶替 webview）。要 `AppHandle` 才能做的事只允许出现在 `main.rs` 与 `impl Emit for AppHandle` 里。
3. **webview 传来的一切是数据，不是指令**，`decision` 字符串尤其如此：认不出的决定一律当拒绝（`ApprovalAnswer::into_approval` 的 `_ =>` 分支），有测试钉住。永远不要为了"宽容"给它加 fallback 到放行的分支。
4. **一个会话同时只跑一个 turn，靠所有权保证**：`SessionHandle` 里的 `Session` 被跑 turn 的一方 `take` 走，结束再放回。不许改成"用一个 bool 标记忙"——那会漂移。
5. **事件名是契约**：`bridge.rs` 的 `AGENT_EVENT`/`APPROVAL_REQUEST`/`TURN_FINISHED` 常量与 `ui/src/types.ts` 里的同名常量必须同时改。`AgentEvent` 的 JSON 信封形状（adjacently tagged，`{type, data}`）由 `tcode-core` 的 `event_wire_tests` 钉住，改它就要同时改 `ui/src/types.ts`。
6. **用到新的 Tauri 内建能力，先改 `capabilities/default.json`**。自定义 `#[tauri::command]` 默认放行，但 core 插件的命令（event 的 `listen`/`emit`、window、fs、dialog…）必须显式授权，**未授权时前端那侧只是 promise reject，没有任何报错会自己冒出来**。这条是踩出来的：漏了 `core:default` 时，turn 正常跑完、事件正常 emit，界面却全空，看起来和"卡死"一模一样。
7. **前端不许有静默 reject 的 promise**。`listen()` / `invoke()` 一律接 `catch`，把原因显示成致命错误屏。第 6 条那个 bug 之所以难查，就是因为它当时是个 unhandled rejection。
8. **组件里不许出现字面量颜色/圆角/字号/字体栈，一律 `var(--token)`**。`ui/src/theme/base.css` 是 token 契约（含由 `--bg`/`--ink`/`--brand` 推导的兜底值，本身不含任何字面色），`themes/porcelain.css` 是默认主题包，两者的加载顺序就是覆盖顺序。**换主题 = 换 `main.tsx` 里的一行 import**，包括排版、密度、圆角、阴影，不只是配色。token 的**名字**是契约，主题可以改值不能改名。为什么这么严：写死一个 `#1d201b` 不会报错，只会在换主题那天变成一个找不着的污点。设计依据见 `DESIGN.md`，产品判断见 `PRODUCT.md`。
9. **路径不许用 `direction: rtl` 做前截断**。bidi 重排会把开头的 `/` 挪到结尾——`/home/me/code` 渲染成 `home/me/code/`。这不是外观问题：审批弹窗里给人看的是一条错的路径。用 `components/Path.tsx`，它按整段省略，一个字符都不改写。

## 现有结构

- `src/bridge.rs`：出向事件（`SessionEvent`/`TurnFinished`/`ApprovalRequest`）、入向审批（`ApprovalAnswer`/`Pending`）、`WebviewApprover`、`pump_events`。`Emit` trait 在这里。
- `src/state.rs`：`Supervisor`（agent + `SessionFactory` + 会话表 + 顺序）、`SessionHandle`（会话私有的 session/cancel/pending）、`run_turn`。
- `src/commands.rs`：Tauri command，薄封装，只做参数校验后转 `state`/`projects`。
- `src/boot.rs`：app 的 composition root，外加 `SessionFactory`（开第二个文件夹时**按该文件夹重新加载 config**，因为 `.tcode/config.toml` 是项目级的）。
- `src/projects.rs`：启动台的数据源。`~/.tcode/projects/<id>/` 的目录名是路径的**有损**变换（`store::project_id` 把非字母数字全折成 `-`），反推不回文件夹，所以真实路径只从每条 session log 首行的 `Meta{cwd}` 读——每个项目一行，够便宜；带 preview 的完整重放留给用户真打开的那个项目（`project_sessions`）。
- `tests/bridge.rs`：scripted provider 驱动真实 agent loop，断言事件流、审批往返、fail-closed、双会话隔离、忙会话拒绝第二个 turn。**测试不打真 API。**
- `ui/src/`：`types.ts`（wire 契约）、`transcript.ts` 与 `files.ts`（事件→块 / 事件→文件清单，都是纯函数 reducer）、`Launchpad.tsx`（第一屏）、`Workspace.tsx`（会话栏 + 对话 + 文件侧栏）、`theme/`（token 契约与主题包）、`preview/`（只在 `PREVIEW=1` 下加载的 fixture）。
- `capabilities/default.json`：webview 的权限授予（见硬规则 6）。现有 `core:default` + `dialog:allow-open`（"打开文件夹"要它）。
- `icons/`：由 `icons/mark.svg` 用 `rsvg-convert` 生成，改标记要重新导出全部尺寸。

## 已知限制

**多文件夹会话共用一条 `ShellFilters` 链。** 它是 boot 时建的、被 agent 的工具集持有，`open_folder` 只能把同一个 `Arc` 再注册一次。后果：A 项目 `.tcode/filters.toml` 里的 shell 输出过滤规则会作用到 B 项目的 shell 输出。影响面是输出裁剪，不涉及权限或安全边界，所以没为它改 core；但这条**不是**"每会话隔离"，别在它上面叠新假设。真要修，得让 shell 工具按 `ToolCtx` 取 filter 链而不是在构造时捕获。

## 排查手册

界面没反应时，**先看 stderr**，它把"没跑起来 / 跑完了但前端没收到"分得很清楚：

- 无 `turn started` → command 里的 spawn 没起来。
- 有 `turn started` 无 `turn finished/failed` → 卡在 provider 请求。
- 两行都有但界面空 → 前端监听侧。九成是 capabilities 或某个没接 catch 的 promise。
- `could not emit '…'` → 事件名非法（Tauri 只收 `[a-zA-Z0-9-/:_]`）或窗口已关。
