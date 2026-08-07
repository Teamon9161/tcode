# tcode-app：从 Tauri 迁到 Electron

> 迁移期的权威文档。做完后本文并入 `AGENTS.md`（规则 9h 重写）并删除。
> 状态：**Phase 0 ✅（Wayland 一项待在 Linux 上跑）/ Phase 1 ✅ / Phase 2 ✅ / Phase 3 ✅ / Phase 4 ✅（Windows 上两个壳都实测跑通，含浏览器窗格）/ 下一步 Phase 5**。每完成一阶段在下面的清单里勾掉并记录实测数字。
>
> **在 Linux 机器上要做的两件事**（都不挡 Phase 4，但挡"这次迁移值不值"的结论）：
> 1. `cd crates/tcode-app/spike && npm install && npm run spike:wayland`，把 `results.json` 的 `geometry` 与 `zOrder` 贴回 Phase 0。
> 2. 在 spike 页面里用真实中文输入法打一段，看预编辑串、候选框位置、提交（风险 1）。

## 已知的既有失败（不是迁移引入的，已在 main 上复现确认）

1. ~~`tsc --noEmit` 报 `Workspace.tsx:39` 的 `Rail`/`rail` 大小写冲突~~ —— **Phase 3 修了，因为它挡路了**。原来记的"`vite build` 本身是成功的"**是错的**：rollup 同样解析到 `rail.ts` 并报 `"Rail" is not exported`，`ui/dist` 根本产不出来，而 Electron 壳只加载 `ui/dist`。修法是把数据模块 `rail.ts` 改名 `railData.ts`（它的首行文档就是"The rail, as data"），组件名一个没动，5 处 import 跟着改。
2. `cargo test --test terminal` 的 `a_shell_echoes_what_it_is_told_and_reports_how_it_ended` 在这台 Windows 上超时（20s）。另外两条 PTY 测试通过。同样在 main 上复现。

## 决策

**值得做** —— 但理由不是"Linux WebView 有坑"，而是：这个 app 想成为的东西（带 session/profile、能被 agent 用 CDP 驱动的内嵌浏览器）**只在 Chromium 上存在**，而 Tauri 在三个平台上给的是三个不同引擎。继续走 Tauri 等于给每一个新浏览器能力写三份实现，其中两份没有对应的协议。

反过来说，**如果只是为了躲 GTK 的坑，这次迁移不划算**：删掉 `browser/` 换来的是同等体量的 Electron 主进程代码，净行数基本持平。真正买到的是"新代码走在受支持的 API 上，而不是伸手进别人库的 widget 树里"。这一条要在后面每次犹豫时拿出来对。

## 现状测量（决定这件事可行的全部依据）

不是估计，是数出来的：

| | 行数 | Tauri 耦合 |
|---|---|---|
| Rust 后端合计 | 9 751 | 只有 **6 个文件**出现 `tauri` |
| `src/commands.rs` | 1 931 | 63 处，**全是 `#[tauri::command]` 属性 + `State<>` 提取器**（50 个命令） |
| `src/browser/` | 1 148 | 整个模块，**迁移后删除** |
| `src/main.rs` | 152 | 9 处，就是那个 shell |
| `src/bridge.rs` | 440 | **2 处**，`impl Emit for AppHandle` |
| `src/state.rs` / `serve.rs` | 1 555 | 各 1 / 3 处，全是 `tauri::async_runtime::spawn` |
| `src/{boot,workspace,projects,picker,openers,paths,terminal}.rs` | 3 400+ | **0 处** |
| 前端合计 | 33 346 | 只有 **18 个文件** import `@tauri-apps/*` |

结论：**接缝已经存在，而且两侧都存在。**

- 后端侧是 `bridge.rs` 的 `Emit` trait（硬规则 2 逼出来的：跑 turn 的路径必须能在没有窗口时被测试驱动），以及 `commands.rs` 那 50 个"`State` + serde 参数 → `Result<T, String>`"的纯函数。
- 前端侧是 `invoke` / `listen` 两个调用，加上 `ui/src/preview/mock-*.ts` —— 设计预览早就把 Tauri API 整个替换掉过一次并且能跑，这是接缝可替换性的**既成证据**，不是推测。

所以这不是重写，是换壳。**8 600 行 Rust 与 33 000 行 TS 一个字都不该动**（除了 18 个 import 行）。

## 目标架构

```
Electron main（只做三件事）
├── 窗口 + 自绘标题栏的窗口命令
├── WebContentsView：app 一个、browser tab 各一个
└── 起 tcode-sidecar 子进程，双向转发 JSON-RPC
        │  stdio, 行分隔 JSON
        ▼
tcode-sidecar（Rust，就是今天的 tcode-app lib）
├── Supervisor / Session / Agent      ← 一字不动
├── workspace / projects / picker     ← 一字不动
├── terminal（portable-pty）          ← 一字不动
├── serve.rs（127.0.0.1 artifact 源） ← 一字不动
└── dispatch.rs                       ← 新写，替掉 #[tauri::command]
```

Electron main 里**不许出现第二份业务逻辑**。判据和硬规则 1 同一条：发现自己在 main.js 里写"决定开哪个会话""校验路径""拼模型菜单"，那段属于 Rust。main 唯一的自有状态是 tab ↔ WebContentsView 的映射，因为那个对象本来就活在 Electron 里。

### 为什么是 sidecar 进程，不是 napi-rs 原生模块

原生模块要为每个平台 × 每个 Node ABI 预编译 —— 那正是这次要逃离的那类痛苦，而且会把 agent loop 挂到 Node 的线程模型上。stdio sidecar 的代价只有一次序列化，而 `tcode-acp` 已经在 `crates/tcode-acp/src/lib.rs` 里跑着同一套传输（行分隔 JSON、stdout 只发帧、诊断走 stderr），照抄即可。

## 我对那份建议的四点修正

**1. Rust 侧不只是 "agent core"。** 那份建议画的是 "Electron 管桌面集成、Rust 管 agent"。按这个分法，`workspace.rs`(947)、`projects.rs`(244)、`picker.rs`(484)、`serve.rs`(512) 会被理解成"桌面集成"而搬进 Node —— 那是把 3 000 行经过审计的路径校验、权限优先级链、link/canonical 检查重写一遍。**正确的分法是：除了窗口和 WebContentsView，一切留在 Rust。**

**2. 终端留在 Rust，不换 node-pty。** 已经写好了，`portable-pty` 已经把 ConPTY 那份麻烦吃掉了，而 node-pty 是原生模块（见上）。代价是 PTY 字节走 stdio 管道 —— 但后端本来就已经合流（`FLUSH_WINDOW` 16ms / `MAX_CHUNK` 64KB，规则 9i），且字节一直是 base64 过桥，行分隔 JSON 天然安全。另有一条**安全理由**：规则 9i "终端是用户的，模型碰不到"，PTY 待在同一个被审计的边界后面比散到两个进程里好守。

**3. `serve.rs` 留着，不换 Electron 的 `protocol.handle()`。** 规则 11b 要的是**一个真正不同的源**，`serve.rs` 已经给了，并且带着 tower-http 的 byte range / 条件请求 / HEAD / MIME —— 一个 python 报告全都会用到，重写必然漏。Electron 侧只要 CSP 里放行 `frame-src http://127.0.0.1:*`。

**4. 最大的陷阱那份建议没提：WebContentsView 一样合成在渲染进程之上。**

规则 9h 里那些"原生子 webview 不在任何 CSS 能触达的层叠上下文里"的后果 —— `seat.ts::yieldToPopover`（popover 打开时浏览器必须让位，否则菜单开在页面**后面**，看起来像按钮没反应）、`WebPane` 里那个**不带依赖数组**的 `useLayoutEffect`（窗格会不改变尺寸地移动，`ResizeObserver` 一次都不响）—— **在 Electron 下一条都不会消失**。WebContentsView 是加进窗口 view 层级的原生视图，不是 DOM 节点。

迁移买到的是**另一半**：`setBounds()` 在三个平台上真的生效，于是 `place.rs` 那 305 行 GtkOverlay/GtkFixed/`size_allocate`/`idle_add_local_once` 的考古全部消失，`browser/mod.rs` 里"最后一个 tab 永不销毁"那套 WebContext 生命周期 workaround 也消失（Electron 的 session 由 partition 持有，不由最后一个 view 持有）。

**别把"几何问题解决了"读成"合成问题解决了"。** `seat.ts` 与 `browserYield.ts` 原样保留，`WebPane.tsx` 的 rect 上报原样保留。

**Phase 0 已实测确认这一条**（抓屏读像素，见下）。唯一的好消息是让位变便宜了：`setVisible(false)` 与 `removeChildView` 都能让 DOM 露出来，且都不销毁页面。

## 分阶段

每一阶段结束时 **Tauri 版本必须仍然能编能跑**，直到 Phase 6。这不是保守，是因为这个 app 有 43 000 行、`tests/bridge.rs` 有 1 743 行断言 —— 唯一能分辨"迁移搞坏了"和"本来就这样"的手段，就是随时能切回去对一次。

### Phase 0 — 一次性 spike（半天，代码全丢）

**执行顺序上它排在 Phase 1 之后、Phase 3 之前**，尽管编号在前。理由：它要回答的四个问题全部只影响浏览器窗格（Phase 4），而 Phase 1 的后端脱壳**无论 spike 结论如何都是对的** —— 它把 50 个命令从属性宏挪到一张注册表上，Tauri 版本自己也因此变得可测。先做无条件正确的那部分，不要为了对齐编号让一个必然要做的重构等在一次实验后面。

另有一条现实约束：**四个问题里第 2 个在这台机器上答不了**（开发机是 Windows，而 Wayland 的坑在 Linux 上）。1/3/4 可以在 Windows 上答，第 2 个必须在目标 Linux 机器上跑一次 —— 它同时也是这次迁移的头号动机，不许靠"Chromium 应该没问题"跳过。

在 `crates/tcode-app/spike/` 起一个最小 Electron 工程，只回答四个**会推翻计划**的问题，答完删掉。**实测于 Electron 43.3.0 / Chromium 150 / Windows 11**，原始数据在 `spike/results.json`：

- [x] **WebContentsView 合成在 renderer 之上 —— 确认。** 让 app view 画一个 `z-index: 2147483647` 的红块，browser view 盖在它正中间画蓝色，然后**抓屏读像素**（抓整屏而不是抓窗口：Windows 上窗口抓取可能走 `PrintWindow`，那条路恰好会漏掉独立的子 HWND，而子视图正是被测对象）。结果：`viewAdded → view`。
  - **所以 `seat.ts::yieldToPopover` 与 `browserYield.ts` 原样保留**，我在"四点修正"里的第 4 条成立。
  - 让位有两个杆子，都验过：`setVisible(false)` → `dom`（Electron 43 有这个方法），`removeChildView` → `dom`。而且**两者都不销毁页面**（`pageSurvivedReAdd: true`），所以让位比今天的 hide 更便宜也更安全。
- [ ] **`setBounds()` 与页面宽度 —— Windows 上一次到位，Linux 未测。** Windows：请求 520×240，页面自报 `innerWidth=520 / innerHeight=240`，`pageFollowedTheHost: true`，**不需要**任何补一次 allocation 的动作。**这条必须在目标 Linux/Wayland 机器上再跑一次**（`npm run spike:wayland`，带 `--ozone-platform-hint=auto`），它是整次迁移的头号动机。测法照抄 `place.rs`：只信页面自报的 `innerWidth`，不信宿主报回的外框。
- [x] **CDP 对 WebContentsView 完全可用。** `debugger.attach("1.3")` 成功；`Runtime.evaluate` 读到 title；`Input.dispatchMouseEvent` 合成的点击**真的落到页面上**（页面自己把 `document.title` 改成 `clicked`，宿主再读回来 —— 两者之间没有任何通道，所以这不是自证）；`Page.captureScreenshot` 出 4744 字节 PNG；`Accessibility.getFullAXTree` 出 9 个节点（`browser.snapshot` 的落点）。
  - **一个与文档相反的实测结果**：打开 DevTools **没有**把 debugger detach 掉。`devToolsActuallyOpened: true`，1.5s 与 4.5s 两次采样 `isAttached()` 都是 `true`。Electron 文档（以及那次对话）说 DevTools 会顶掉 debugger session。**在这个版本这个平台上没复现**。含义：BrowserManager 大概不需要 `Attached / UserDevTools` 那个状态机 —— 但**别据此把它设计成不可能出现冲突**，只在别的版本或平台上重测之前当作"未确认"。
- [x] **`session.fromPartition("persist:…")` 三条都成立。** 同 partition 的两个 view 共享 cookie（`sharedWithSiblingView`）；另一个 partition 读不到（`isolatedFromOtherPartition`）；**重启后还在** —— 第二轮启动时 `cookiesAtStartup: ["spikePersist=1"]`。这就是 browser profile 的地基。

**顺带验掉了安全一节的两条假设**（同一份 `results.json`）：不带 preload 的 browser view 里 `['process','require','tcode','__TAURI__']` **一个都不存在**；`setWindowOpenHandler` 确实截住了页面发起的 `window.open` 并能 deny。

**四个问题都是"我现在不知道答案"**，尤其第一个 —— 它决定要不要保留 `seat.ts` 那 125 行以及 `WebPane` 里那条反直觉的 `useLayoutEffect`。不许靠推断跳过。

### Phase 1 ✅ 后端脱壳（不动 Electron）

- [x] `src/dispatch.rs`（~300 行）：`&'static str -> Handler` 一张表。**参数类型一处都没重述** —— 注册行只写 `[ctx 字段](参数名)`，类型经调用推断，写错就是编译错误而不是运行时错解析。camelCase 转换复刻 Tauri v2 的默认行为（`entry_index` → `entryIndex`）。
- [x] `Ctx { supervisor, serve, terminals, emit }` 取代 `State<>`。命令函数体**一行未改**，只改签名。`browser` 不在 `Ctx` 里 —— 它是壳的东西。
- [x] `tauri::async_runtime::spawn` → `tokio::spawn`（commands 4 处 + state 1 + serve 2）。
- [x] `commands.rs` 现在 `tauri` 引用数为 **0**；9 个 browser 命令搬进新的 `src/browser/commands.rs`（含 `register()`），那个文件就是 Phase 4 要整体删掉的东西。
- [x] 测试：`every_command_the_frontend_calls_is_registered` 扫 `ui/src` 的所有 `invoke("…")` 去比**真表**，并断言扫到 >40 个调用点（空扫也会绿，这条防的就是那个）。
- [x] `impl Emit for StdioEmitter` + 新 bin `src/bin/sidecar.rs`：读 stdin 的 JSON-RPC，查表，回结果。**在 Phase 3 做的**，因为在 Electron 主进程存在之前它没有对端可测。

**与原计划的一处偏离，结果更好**：本来打算给 Tauri 留 52 个薄 wrapper 好让两个壳并存。实际做法是 `main.rs` 只留**一个** `rpc` 命令转发给注册表，`invoke_handler!` 从 52 行缩到 1 行。连带一个真实收益：所有命令现在都在 async 上下文里执行，`commands::deliver` 里那条"sync command 跑在主线程、`tokio::spawn` 会 panic"的老坑**结构上消失了**（原注释保留并说明了为什么）。

### Phase 2 ✅ 前端收口（不动 Electron）

- [x] `ui/src/ipc.ts`：导出 `invoke` / `listen`，签名与 `@tauri-apps/api` 完全一致。Tauri 下 `invoke(name, args)` 包成 `tauriInvoke("rpc", {method, args})`，调用点全都不知道有这层。
- [x] 19 个文件改 import。**用 `@ipc` 这个裸 specifier 而不是相对路径 `./ipc`**，这是承重的：相对导入在 alias 看到它之前就解析完了，而"fixture 绝不进发布产物"这条规则靠的正是别名是唯一开关。
- [x] `vite.config.ts` 的 preview alias 从四条收成三条（`@ipc` → `preview/mock-ipc.ts`，外加尚未收口的 window / dialog 两条）；`vitest.config.ts` 与 `tsconfig.json` 同步加 `@ipc` 映射。
  - **`tsconfig` 里只能加 `paths`，不能加 `baseUrl`** —— 踩出来的：加了 `baseUrl` 会改掉整个模块解析，`@xterm/*` 直接找不到，还会翻出别的既有冲突。TS 5 的 `paths` 不需要它。
- [x] `npm test` 33 files / 347 tests 全绿。`WebPane.test.tsx` 原本有两条 `vi.mock`（core 与 event），收成一个模块后必须合并成一条 —— **同一 specifier 的第二条 `vi.mock` 会静默顶掉第一条**，那会让 `invoke` 变成 undefined。
- [x] `WindowControls.tsx`（`getCurrentWindow`）与 `FolderPicker.tsx`（`plugin-dialog`）仍直连 Tauri。它们要的是**主进程能力**而不是后端命令，所以留到 Phase 3 跟 Electron main 一起写，不为了"收口干净"先造一层没有对端的抽象。

### Phase 3 ✅ Electron 壳跑起来（浏览器窗格先用不上）

**实测于 Windows 11 / Electron 43.3.0**：两个壳都起得来、都渲染完整界面、`tcode://window-state` 在两边都走通。

- [x] `src/sidecar.rs` + `src/bin/sidecar.rs`：行分隔 JSON，`{id,method,args}` → `{id,ok}` / `{id,error}`，事件是 `{event,payload}`。**一个请求一个 task**（实测可见乱序返回：id 3 先于 id 2 回来），stdin 关闭 = 关终端然后退出。
  - **不是 JSON-RPC 2.0，这是决定不是遗漏**：`tcode-acp` 说 JSON-RPC 是因为管道另一头是 Zed，协议是别人的；这里两头同仓库、错误类型本来就是 `String`，每帧写一遍 `"jsonrpc": "2.0"` 只是在兼容一个不存在的对端。照抄 acp 的是真正要紧的那部分：**stdout 只发帧**，诊断走 stderr。
  - 出站 channel 是 unbounded，因为 `Emit::emit` 是同步的：`blocking_send` 会停住 runtime 线程，`try_send` 会在满时**丢事件**——丢掉的 `AgentEvent` 是看不见的，transcript 少一块而没人说。
- [x] `electron/main.js`：`BaseWindow` + 一个 `WebContentsView`（不是 `BrowserWindow`——Phase 4 的 tab 是同一个容器里的兄弟 view，窗口里只该有一种东西）、`frame: false`、起 sidecar、双向转发、窗口命令、`dialog.showOpenDialog`。
  - **`.js` 不是 `.ts`**（与原计划的偏离）：主进程刻意不放业务逻辑，为十来个 Electron API 调用加一条编译流水线不划算，而 `ui/scripts/*.mjs` 已经是这个仓库里 build 相邻代码的写法。preload 无论如何得是 CJS——sandbox 化的 preload 不是 ES module，而 `sandbox: true` 对这个文档不可谈判。
  - 应用从 `app://tcode` 加载（`registerSchemesAsPrivileged` + `protocol.handle` 服务 `ui/dist`），**不是 `file:`**。规则 11b 要的是两个真正不同的源，`file:` 下没有源可言，`Framed.tsx` 那段长注释也就不成立了。CSP 直接由 handler 打在响应头上——那不是 HTTP 请求，治理文档的那个头值得跟产出文档的函数写在一起。
- [x] `electron/preload.js`：`contextBridge.exposeInMainWorld("tcode", { invoke, listen })`。**只有这两个**，不暴露 `ipcRenderer`。回复走信封而不是抛异常，好让失败命令 reject 出**后端自己那句话**——`ipcMain.handle` 否则会包成 "Error invoking remote method 'tcode:invoke'"，而那串是给人看的（规则 7）。
- [x] app renderer 的 `webPreferences`：`contextIsolation: true` / `nodeIntegration: false` / `sandbox: true`。顺手加了两条 Tauri 本来免费给的：`will-navigate` 一律 `preventDefault`、`setWindowOpenHandler` 一律 deny。
- [x] CSP 逐字保留。**但发现并修了一个既有缺陷**：vite 把 4 kB 以下的资源内联成 `data:` URI，变体字体的小 subset 正好在线下，于是 `@font-face` 里出现 `data:font/woff2;…`，被 `default-src 'self'` 挡掉——**Tauri 下同样挡，只是没人看得见 renderer 的 console**。修法是 `assetsInlineLimit` 让字体永远保持文件，不是给 CSP 加 `font-src data:`：`data:` 在这份策略里只出现一次（`img-src`，为了粘贴的截图），改构建设置比改那句话便宜。
- [x] 窗口能力**没有做成第二个 import，而是同一张表里的命令**：`window_minimize` / `window_toggle_maximize` / `window_close` / `window_is_maximized` / `dialog_open_folder`，Tauri 侧由 `main.rs::register_shell` 答，Electron 侧由 `main.js` 答。前端只有一个 `invoke`，"谁来答"是壳的事——和 `browser_*` 同一个安排。
  - 连带删掉了 `preview` 的两条 alias（`mock-window.ts` / `mock-dialog.ts` 已删）：vite 的 alias 表现在**只剩 `@ipc` 一条**，这就是它该有的最终形状，再多一条就说明有东西绕过了 seam。
  - 最大化状态改由事件驱动（`tcode://window-state`），取代 `onResized` 轮询式重读。**两个壳都实测过**：从窗口外部 `ShowWindow(SW_MAXIMIZE)`，标题栏图标都换成了 restore。
- [x] 拖拽区：`[data-shell="electron"] [data-tauri-drag-region] { -webkit-app-region: drag }`。**选择器故意用那个属性**——Tauri 是"指针下的元素自己带着属性才起拖"，用同一组元素才是一根标题栏而不是两根。按壳限定而不是无条件下发，是因为"WebView2 认不认 `-webkit-app-region`"不是这份 CSS 该赌的事。规则 17 那条 `drag.js` 的成因确实消失了，但按计划留到 Phase 6 一起清。
- [x] **修了一条 Phase 1 引入的真 regression**：`state.rs` 的 `tokio::spawn` 是对的（后端不该知道壳用哪个 runtime），但 `attach_emitter` 是唯一一处在 async command **之外**被调用的地方——Tauri 的 `setup` 跑在主线程且没进 runtime，于是 `there is no reactor running` 直接 panic 在启动路径上。Phase 1 之后没人真的跑过 Tauri app，所以一直没暴露。现在 `main.rs` 用 `tauri::async_runtime::block_on` 包住它。
- [x] `the_title_bar_is_granted_the_commands_it_calls` **反了过来**：它原本扫组件里的 `.minimize()` 去要 capability 授权，而组件已经不这么写了——于是它照样绿、且什么也没钉住，这比失败更糟。现在钉的是取代授权的那条不变量：`WindowControls.tsx` 里不许出现 `@tauri-apps` 的 import 或任何直接窗口调用。
- [x] 两个壳并行，除浏览器窗格外一切可用（Electron 下 `browser_*` 回 `unknown command`，那是准确且可见的答案）。

### Phase 4 ✅ 浏览器窗格重建（这是这次迁移的正题）

`electron/browser.js`，230 行，对上 `src/browser/` 的 1 148 行。**wire 契约一个字节没改** —— 同样九个动词、同样的参数、同样的 `tcode://browser-navigated` 负载，所以 `webHost.ts` / `web.ts` / `WebPane.tsx` 一行未动（只改了两处已经变得不准确的注释）。

- [x] 每个 tab 一个 `WebContentsView`，`session.fromPartition("persist:tcode-browser")`。**不给 preload**，这就是"指着任意 URL 也合理"的全部依据。
- [x] `setBounds()` 接今天 `browser_bounds` 那条路。**实测**：点真正的浏览器按钮开窗格，页面自报视口 `1042x748`，窗格 body 的 `getBoundingClientRect()` 也是 `1042x748` —— 一次到位，没有 `place.rs` 要解决的那个问题。另一次用固定 rect (300,150,600×400) 抓屏读像素：矩形内是页面的蓝、外面是 app 的纸白，落点分毫不差。
- [x] `browser_step` → `navigationHistory.goToOffset(delta)`，是**真的导航历史**而不是页面里的 `history.go()`。实测 p1→p2→后退→前进都对。**前进后退按钮仍然不许置灰**：Electron 这次答得出 `canGoBack()`，但前端刻意不问 —— 一个会置灰的按钮是在做一个必须跟上"页面自己会导航"的承诺（规则 9h）。
- [x] 导航回报 → `did-navigate` / `did-navigate-in-page` / `page-title-updated`，**都带 tab id**。实测一次导航出两个事件（先 url 后 title），正是 `navigatedTab` 写来接的形状。
- [x] **"最后一个 tab 永不销毁"删掉了。** 实测关掉两个 tab 都返回 `true`、view 真的没了、再开一个照常。`browser_close` 的布尔返回值**留着**，因为 Tauri 那边还得答 `false` —— 前端画哪一种由壳说了算，这正是当初让它返回布尔而不是 void 的理由。
- [x] `browser::to_url` 挪进 `src/address.rs`，并作为**后端命令** `resolve_url` 暴露。它是浏览器窗格里唯一与窗口无关的部分，而"`localhost:5173` 是主机还是搜索"这个判断带着五条测试 —— 在 Electron 主进程里再抄一份 JS 才是这一步真正的风险。代价是每次地址栏导航多一个管道往返，那本来就是一次网络请求前的事。
- [ ] ~~删 `src/browser/place.rs` 等~~ —— **推迟到 Phase 6，与 Tauri 一起删**。计划原本把它排在这里，但这与本文开头那条"每一阶段结束时 Tauri 版本必须仍然能编能跑"直接冲突：`place.rs` 是 Tauri 版在 Linux 上的几何，删了就是在迁移中途给还要用的那个壳引入一个行为变更。删除是机械的，晚删不多花钱，早删要冒险。

**顺手修了 `to_url` 一个真实短板**（不是迁移引入的）：`127.0.0.1: 12587` 这种带空格的输入会被套上 scheme 变成 `http://127.0.0.1: 12587` —— 没有任何 URL 解析器接受它，于是屏幕上出现的是 Chromium 的 `ERR_INVALID_URL`，一个**关于这个函数自己产出的字符串**的报错。现在过了显式 scheme 那几支之后遇到空白就拒绝，说的是输入哪里不对。是我用合成键击测地址栏时误打误撞发现的。

### Phase 5 — 安全边界重新推导

见下节。**这一阶段不许和 Phase 4 合并**：把"能用了"和"能安全地用"放进同一个 commit，就没有任何一次 review 是只看边界的。

Phase 3 留下的一件具体事：`capabilities/default.json` 里的 `core:window:allow-minimize` / `allow-toggle-maximize` / `allow-close` / `dialog:allow-open` **现在没有消费者了**（窗口与对话框都从 Rust 侧调，不过 IPC）。没有跟着删，因为删之前要确认 `blocking_pick_folder` 这条 Rust 路径确实不过 capability——**这是推断，没实测**。`core:default` 与 `core:window:allow-start-dragging` 仍然必须留着。

### Phase 6 — 删 Tauri

- [ ] 删 `src/browser/`（含 `place.rs` 那 305 行 GTK 考古）、`Cargo.toml` 的 `gtk` 依赖、`build.rs` 的 `gtk_cfg()`。**`src/address.rs` 留着** —— `to_url` 已经在 Phase 4 搬出去了。
- [ ] 删 `tauri.conf.json` / `capabilities/` / `gen/` / `tauri-build` / `@tauri-apps/*`。
- [ ] `ipc.ts` 里的 Tauri 分支删掉。
- [ ] 重写 `AGENTS.md` 规则 6 / 9c / 9h，清掉规则 17 里那条 `drag.js` 的成因。
- [ ] `Cargo.toml` 的注释（"不在 workspace 里，因为 Tauri 链接平台 webview"）要重写 —— **理由变了但结论没变**：sidecar 只依赖 `portable-pty` 与 hyper，本可以进 workspace；但它的构建产物要被 Electron 打包引用，独立留着仍然更清楚。这条决定留给做到这里的人重新判断，不要照抄旧注释。

## 安全规则的重新推导

**这一节是整次迁移里最容易静默破掉的部分。** 下面每一条右列都要有一个机械测试钉住，和今天 `browser.rs` 里那两条读 `capabilities/default.json` 的测试同规格。

| 今天（Tauri） | 迁移后（Electron） |
|---|---|
| **规则 9h**：capability 恒为空。`webviews: ["main"]` 而非 `windows`，否则等于把 `window.__TAURI__`（=本机任意命令）发给任意网页 | ✅ 已实现：browser 的 WebContentsView **不给 preload**，`nodeIntegration: false` / `contextIsolation: true` / `sandbox: true`，`session.fromPartition("persist:tcode-browser")` 与 app 的 session 不同。结构上比 Tauri 强：没有一个"按 label 匹配"的全局可以填错 —— 但**也正因为没有那个文件，破掉时更没有痕迹**。**测试还没写**：Phase 5 要有一条断言 `electron/browser.js` 里创建 view 的那处不含 `preload`，与今天读 `capabilities/default.json` 的两条同规格 |
| **规则 10**：模型输出永不变 markup，因为这个 webview 里的脚本能拿到 `window.__TAURI__` | 一字不变，只是逃逸目标从 `window.__TAURI__` 换成 preload 暴露的 `window.tcode`。`rich.tsx` 与 `boundary.test.ts` 不动 |
| **规则 11**：`script-src` 永不出现 `unsafe-inline` / `unsafe-eval`（`tauri.conf.json`） | ✅ 同一串 CSP 由 `app://` 的 `protocol.handle` 打在响应头上（不是 `onHeadersReceived`：那不是 HTTP 请求）。`both_shells_serve_the_same_content_security_policy` 从两个文件里各抽一份出来逐字比，并断言**两边都没有 `script-src`** —— 规则 11 真正成立的方式是脚本回落到 `default-src 'self'`，所以"这条指令根本不存在"比"它的值是什么"更该被钉住 |
| **规则 11**：sandbox iframe 只有 `allow-scripts`，绝不加 `allow-same-origin` | 完全不变（这是 HTML 的属性，与壳无关） |
| **规则 11b**：两个 frame，两种相反的隔离机制 | 完全不变（`serve.rs` 留着） |
| *（Tauri 不需要）* | **新增**：每个 browser view 装 `setWindowOpenHandler`，默认 deny。Phase 4 做成 deny + **在同一个 tab 里加载那个 URL** —— "交回我们自己的开 tab 路径"要求前端先知道有这个 tab（`browser_open` 的 id 是前端记的），主进程自己造一个会让 tab 出现在屏幕上却不在 strip 里。那是个新功能不是迁移，同 tab 是诚实的过渡：它绝不会静默地什么都不做，而页面本来就能自己 `location.href` 过去 |
| *（Tauri 不需要）* | **新增**：app renderer 上 `will-navigate` 一律 `preventDefault`，且 app renderer 自己也装 `setWindowOpenHandler` deny。规则 10 里 `target="_blank"` 那条兜底（"万一没被 handler 接住时 app 自己不会被导航走"）在 Electron 下需要这一条才成立。**Phase 3 已做**（`createWindow`），但**还没有测试钉住** —— Phase 5 补 |
| *（Tauri 不需要）* | **新增**：`session.setPermissionRequestHandler` 对 browser partition 默认拒绝（摄像头、麦克风、地理位置、通知）。今天 WebKitGTK 是默认拒的，Chromium 会弹窗。**Phase 4 没做** —— partition 已经建好了，这是挂在它上面的一行，属于 Phase 5 的边界推导 |

## 风险

1. **Wayland 下的 IME 与合成输入 —— 会改善，不会消失，而且 spike 没测它。** Electron 需要 `--ozone-platform-hint=auto` 才走原生 Wayland（否则落到 XWayland，中文输入法的表现又是另一套）。Phase 0 答的是几何与合成，**不是输入法**：那需要一个人坐在 Linux 机器前，在 spike 的页面里用真实输入法打一段中文，看预编辑串、候选框位置和最终提交。跟 `spike:wayland` 那一趟一起做，别等到 Phase 3 —— 这是三条痛点里唯一一条 Chromium 不自动解决的。
2. **包体积从几十 MB 到 150 MB+。** 已知代价，接受。`bundle.active` 今天是 `false`（这个项目还不发安装包），所以打包不在本次范围内 —— 但 Phase 6 之后 `cargo build` 不再产出可运行的 app，**开发流程变了**，AGENTS.md 的"构建与运行"一节必须同时改。
3. **stdio 管道成为单点。** sidecar 挂掉今天的表现是"整个 app 空白"。要有：sidecar 非零退出时在 app 里画致命错误屏（对上规则 7 "前端不许有静默 reject 的 promise"），stderr 转发到 Electron 的日志。
4. **`tests/bridge.rs` 那 1 743 行是这次迁移唯一的安全网。** 它本来就用 collector 顶替 webview（硬规则 2），所以**换壳对它应该完全无感** —— 如果发现要改它，那说明改动跑到了不该跑的地方，停下来看。

## 明确不做

- **不动 Rust agent core / tools / providers / frontend 四个 crate。** 一行都不动。
- **不引 Playwright。** Electron 自带 Chromium + CDP，agent browser 第一版直接用 CDP 足够；Playwright 真正的价值是 locator 的 auto-wait 语义，等到发现自己在重新实现那套时再说。
- **不在这次迁移里做 CDP 与 browser session/profile。** 那是迁移的**目的**，不是迁移的内容。迁移必须行为中性，否则出问题时分不清是壳换坏了还是新功能写坏了。它们的落点已经在 Phase 4 里预留（partition 就是 profile 的雏形）。
- **不改任何 UI。** `DESIGN.md` / `PRODUCT.md` 里的判断与这次无关。屏幕上应该一个像素都不变。
