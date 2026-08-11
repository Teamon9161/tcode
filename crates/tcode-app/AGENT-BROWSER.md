# Agent 驱动浏览器（CDP）— 未实现，本文是执行计划

写给**没参与过这次对话的人**。第一节是"这是不是个真问题"，不同意就别往下做。

前置：浏览器窗格本身已经实现（`AGENTS.md` 规则 9h：`electron/browser.js` + `ui/src/webHost.ts` + `WebPane.tsx`，一个 tab 一个 `WebContentsView`，窗口级单例，不带 session）。本文只加一件事：**让模型也能操作那些 tab**。

## 已定的四件事

2026-08-08 定的，理由各写在对应小节里：

1. **工具面**：一个 `browser` 工具 + `action` 枚举，只由桌面 app 注册。
2. **默认 profile**：`persist:tcode-browser`——**就是屏幕上那个浏览器的登录态**，不是一个隔离的空白 profile。
3. **第一版范围**：Phase 0 → 1 → 2，停在**只读**（`open` / `navigate` / `snapshot` / `close`）。
4. **agent 的 tab 进用户的 strip**，带会话色点。

第 2 与第 3 条是**一起**成立的：只读的第一版里没有 `click` / `type`，所以"用用户的登录态"在 v1 中的全部含义就是"读你已经登录的页面"——而那正是这个功能相对 `web_fetch` 的全部价值。副作用风险要到 Phase 3 才到场，届时的补法写在"权限"那节。

## 决策

值得做 —— 但要做的**不是"给 agent 一个浏览器"**，是"把窗口里已经有的那个浏览器变成 agent 也能碰的"。

这个区分是全文的支点。"给 agent 一个浏览器"tcode 今天就能做：`shell` 工具 + 一个 Playwright 脚本，几行的事，而且 headless 更稳。走 CDP 这条路唯一买到的、脚本买不到的东西只有一样：

> agent 操作的浏览器**就是屏幕上那个**——带着用户已有的登录态，用户看得见它在做什么，随时能接管。

所以凡是"另开一个浏览器也一样成立"的需求（抓一个静态页、批量跑一遍脚本），都不是这个功能要解决的问题，`web_fetch` 与 `shell` 已经在那儿了。判断一个新 action 该不该加，先过这一条。

## 已测事实与待测

`spike/results.json` 里已经有一半答案了——Phase 0 那次 spike（win32，Electron 43.3.0 / Chrome 150.0.7871.212）顺手测了 CDP，2026-08-10 在 Linux（KDE 双屏，Wayland 与 X11 各跑一遍）复测结论一致，细节见下面"Linux 已跑"一节：

| 探针 | 结果 |
|---|---|
| `webContents.debugger.attach("1.3")` | 可用 |
| `Runtime.evaluate` | 拿到 `document.title` |
| `Input.dispatchMouseEvent` | 合成的点击真的到了页面（页面自己改标题回读） |
| `Page.captureScreenshot` | 4744 字节的 PNG |
| `Accessibility.getFullAXTree` | 9 个节点（那是个极小的测试页） |
| 打开 DevTools 后 debugger 是否 detach | **没有**。开着 DevTools 1.5s 与 4.5s 两次采样都还 attached，且 `isDevToolsOpened()` 为 true |

最后一条要专门说：Electron 文档说打开 DevTools 会 detach debugger，**这个版本上没复现**。所以不要照那句文档去设计一个 `Detached / Attached / UserDevTools` 三态机——那是为一个在这里没发生的现象付结构成本。仍然监听 `debugger` 的 `detach` 事件（别的版本、别的平台可能不同），但处理方式是最朴素的那种：检测到就重连，重连不上就报一个说人话的错。

### Phase 0 补测的结果（已跑，win32）

三条问题都有答案了，探针在 `spike/main.js::agentBrowserProbes`。

**下面这些表就是记录本身，不是摘要。** `spike/results.json` 是 gitignore 的（`.gitignore:13`），所以它只存在于跑过 spike 的那台机器上；数字连同它们的含义写在这里，是为了让一个新 clone 不用重跑就能读懂当初为什么这么定。要复现就 `cd spike && npm run spike`。

**1. 隐藏的 view 上 CDP 成立，但截图不成立。**

| 探针（`setVisible(false)` 之后） | 结果 |
|---|---|
| `debugger` 仍 attached | 是 |
| `Runtime.evaluate` | 拿到 `document.title` |
| `Input.dispatchMouseEvent` | **点击到了页面**（页面自己改标题回读） |
| `Page.captureScreenshot` ×4 | 超时、4744B、超时、4744B —— **交替** |
| 同一个 view 可见时同一个调用 ×3 | 4744B、4744B、4744B，全部立刻返回 |

这是**整个设计押的那一注，赢了**：agent 可以在一个用户看不见的后台 tab 上导航、读页面、点击，不动屏幕上是哪个 tab。

截图那一栏要看仔细。前两次跑的结果是**互相矛盾**的——一次是 `plain` 超时 / `captureBeyondViewport` 成功，下一次正好反过来。任何一次单独拿去写进设计都会变成一条假事实（"隐藏 view 上必须用 captureBeyondViewport"）。问四次才看出真相：**与哪个参数无关，是隔一次成功一次**。合理的机制是隐藏的 view 没有 compositor 在产帧，capture 在等一个不会来的帧；等待这件事本身促使 Chromium 产了一帧，于是下一次调用立刻拿到。

于是第一条硬规则：**任何 CDP 调用都必须带超时。** 不带的那一版会把一个 turn 永久挂住，而这不是推测：写这个探针的头两次运行就是这么挂的，窗口停在屏幕上、`results.json` 一个字没写。`withTimeout` 与它上面那段注释是这次事故的记录。

**1b. 截图根本不需要 CDP：`webContents.capturePage()` 在隐藏 view 上 9/9。**

这一条被推翻了两次，记录两次都留着，因为推翻它的过程本身是这份文档最该传的东西。

*第一次*：原文写的是"`screenshot` 要求 tab 是屏幕上那个"——那是从一次测量里**推**出来的，不是测出来的。补了 `escapeRoutes`，每种配置连拍三张、跑三遍：

| 配置（都用 `Page.captureScreenshot`） | 三次运行的结果 |
|---|---|
| `setVisible(false)` | 超时/成功**交替**，三遍一致 |
| `setVisible(true)`，但 bounds 挪到窗口内容区**外面** | `timeout, ok, timeout`，**三遍完全一致地坏** |
| `setVisible(true)`，在窗口内，但**被别的 view 完全盖住** | **3/3 好，三遍全好** |
| `setVisible(false)` + `setBackgroundThrottling(false)` | 两遍 3/3 好，一遍 `timeout, ok, ok` |
| 加上 Playwright 那三个 Chromium 开关 | **没有任何差别** |

于是结论改成"可见但被盖住"，方案是临时 restack。**那个方案没有实现，因为它也是多余的。**

*第二次*：上面整张表问的都是同一个问题——"怎么让 CDP 的截图命令拿到帧"。而 `Page.captureScreenshot` 只是 Electron 上两条截图路径里的一条。补测另一条：

| 配置 | 三次运行 × 三张 |
|---|---|
| `setVisible(false)` + `webContents.capturePage()` | **9/9 好**，500×375、4744 字节，尺寸和内容都对 |
| `setVisible(true)` + `capturePage()`（对照） | 9/9 好 |

`capturePage` 不走 debugger，隐藏与否它都答。**这条结论后来被 agent tab 的真实生命周期推翻，9/9 只对“先可见渲染过、再隐藏”的 probe 成立。** 旧测量和它当时推导出的方案留在上面，下面是补齐生命周期后的结论。

**第三次修正（2026-08-10）：从出生起隐藏的 view 没有 capture surface。** 在健康的 loopback Vite 页上，同一个 tab 的 AX snapshot 有数百个节点，而 `capturePage()` 回 0×0/0 bytes；点击的 CDP event 会派发，但 portal/dialog 也可能在隐藏前没有提交。最小 Electron probe 把两个生命周期分开后得到：

| 生命周期 | `capturePage()` |
|---|---|
| `setVisible(false)` 后直接 load，从未可见 | **0×0，空**；`stayHidden` / `stayAwake` 不改变 |
| 可见并产过当前文档的 frame，再隐藏 | **稳定非空** |
| 可见但完整放在 app renderer 下方 | **稳定非空，屏幕仍只显示 app** |

所以生产实现回到了第一次修正里那个“在 cover 下短暂 compositing”的方向，但仍保留 `capturePage()` 而不回到会挂住的 CDP screenshot。`browser.js::rendered` 给目标有效 bounds，把当前 browser sibling 短暂脱离 native tree，让目标在 app renderer 下方可见，以有界 `capturePage()` 驱动 frame，再按 `current`/`shown` 恢复；调用串行，失败带 URL、bounds、current 与 pane visibility。navigate 对旧 `about:blank` 不等待 frame，只在新文档 load 后 warm；前台本来可见的 tab 不走这条恢复。snapshot、click/type/scroll/wait 与 screenshot 共用 helper，因此修的不只是图片，也包括 canvas 和 portal/dialog 的视觉提交。真实回归入口是 `npm run test:browser`。

三条附带结论仍然成立、仍然有用：

- **`setBackgroundThrottling(false)` 不是修法**（2/3），`hiddenThrottleRestored` 那个对照探针是唯一能把"这个调用修好了"和"顺序修好了"分开的东西。
- **Playwright 那三个开关够不着这里**：它们的 page 是独立 target，我们的 tab 是 child view，`setVisible(false)` 是从 layer tree 里摘掉，比遮挡强一个量级。
- **`browser.js` 里"文档没法给原生 view 施加 z 序"那句话是对的**，而且现在也不需要动它——`restack()` 保持"只有当前 tab 可见"。

**这段的方法论比结论值钱**：两次都是"从一次测量推出一条限制"，两次都错。第二次尤其——第一轮的整张表把问题问成了"怎么让这条命令工作"，而正确的问题是"是不是只有这条命令"。

**2. 一个真实重页的 a11y 树：snapshot 必须是过滤器，不是序列化器。**

`https://github.com/rust-lang/rust/pull/1`：

| 口径 | 节点 | 字节 |
|---|---|---|
| `getFullAXTree` 原始 JSON | 2054 | **733 KB** |
| 每节点一行 `ref role "name"` | 2054 | 50 KB |
| 去掉 `ignored`(828) 与结构性容器，只留有 name 的或可交互的 | **588** | **20 KB** |
| 其中可交互的（button/link/textbox/…） | 153 | — |

20 KB ≈ 5k token，一次 snapshot 可以接受；50 KB 的朴素写法不行，733 KB 的原始树连谈都不用谈。**所以 `snapshot` 的实现主体是那张过滤规则，不是序列化代码**——探针里那个 `INTERACTIVE` 集合与 `useful` 判据是它的雏形，真正的表跟工具走。同时它必须带预算与截断，截断时的提示要自愈（说清截在哪、怎么拿下一段）。

**3. 后台 tab 照常报告导航**：`did-navigate` 在 10ms 内到达。`wait(idle)` 可以建在事件上，不用轮询。

（原本还有一条"运行时新建 partition 的代价"，随"默认用用户的 profile"这个决定一起作废：第一版只有一个 partition，和今天一模一样。）

**Linux 已跑（2026-08-10，KDE Plasma，双屏）：结论与 win32 一致。** 两个 ozone 路径各跑一遍：`npm run spike:wayland` 与 `npx electron --ozone-platform=x11 .`。Wayland 那条会弹一次 xdg-desktop-portal 的"分享哪个屏幕"对话框（KDE 双屏尤其明显）——那是安全边界，没有编程指定的口子；X11 那条走 X 抓屏、不经过 portal，不想点对话框就加 `--ozone-platform=x11`。真 app 的截图走 `capturePage()`，两条路径都不弹。

复测结果（`results.json` 的 `probes.agentBrowser`）：

- **setBounds 到达页面**（`pageFollowedTheHost: true`）——当初 Tauri 在 Linux 上不成立的那个坑，Electron 成立。
- **隐藏 view 上 CDP 照常工作**（attached / evaluate / 点击都到页面）；`Page.captureScreenshot` 仍交替超时（Wayland 那次 `timeout/9113/timeout/9113`），`capturePage()` 隐藏时 3/3 好——`browser_screenshot` 走 capturePage 的决策跨平台成立。
- **DevTools 打开不 detach debugger**（1.5s 与 4.5s 两条采样都 attached）——"别做三态机"跨平台成立。
- **partition cookie 共享 / 隔离 / 持久**：第二次跑 `sessionPersistedAcrossRuns: true`，persist cookie 从上一轮活了下来。
- **a11y 树**：GitHub PR 页 2058 节点，过滤后 588 useful / ~20KB——snapshot 是过滤器不是序列化器。
- **后台 tab 照常报导航**（10ms 内）。

**唯一没测成的是 zOrder 的像素采样**：spike 假设单屏且窗口可自定位（硬编码 `WINDOW = {x:60, y:60}`），KDE 双屏 + Wayland 不允许应用定位窗口，两次运行的采样点都落在捕获画面外（Wayland 恒为 `neither`，X11 报 `outside the capture`）。这个问题只能由改过的探针回答——窗口显示后读 `win.getBounds()`、遍历 `screen.getAllDisplays()` 找包含窗口的那块屏——留着给真正需要它的那天，别为它猜答案。

- 这些答案现在对 win32 与 linux 都成立；`results.json` 带着 platform 字段，仍是"跑过的那台机器"的记录。
- 第一次在 Linux 上开这个 app 时，spike 已先行跑过（2026-08-10），可以直接用功能。
- 仍然不要为没测过的平台预留抽象：真出问题时改的是壳里那几行，而现在猜出来的接缝八成划在错的地方。

## 数据结构

先定这个，其余都是它的推论。

```
BrowserManager                 ← 窗口级单例，活在 electron 壳里
├── Profile                    ← 存储边界：cookie / localStorage / 登录态
│     partition: string
│     persistent: bool
└── Tab
      id: string               ← 唯一标识，UUID
      profile: Profile         ← 一辈子不变
      view: WebContentsView
```

原本这里还有一行 `refs: Map<string, backendNodeId>`——最近一次 snapshot 的 ref 表。**实现时发现壳里不需要它**：`ref` 直接是 Chromium 的 backend node id，不经壳翻译。Rust 工具另持一个短生命周期的 `tab → {snapshot URL, emitted backend ids}` 准入集合，只有 `computed_style` 用它拒绝模型未见过、或已被页面变更失效的 id；它不是 session 所有权映射，也不要求壳保存 ref 状态。`tabs` 数组仍只有 id 和 view。

三条关系，每条都承重：

- **Profile : Tab = 1 : N。** 登录态属于浏览器、不属于某个 tab——今天已经是这样了（规则 9h：所有 tab 共用 `persist:tcode-browser`），这里只是把"一个"扩成"几个"。
- **一个 Tab 的 Profile 一辈子不变。** Electron 的 session 在 `WebContentsView` 构造时定死，这不是设计选择是平台事实；而它恰好是对的——一个"能换 profile 的 tab"意味着地址栏没变、cookie 换了一套，那是一个没人能推理的对象。换 profile = 开新 tab。
- **`owner`（哪个会话开的）不在这张图里。** 这是下一节。

## Session ↔ Tab：不要建这个映射

问题是"不同 session 的 agent 怎么对应不同的 tab"。答案是**不建立对应关系**，而且这不是偷懒。

### 不该怎么做

给 `ToolCtx` 加一个 session id，让 `browser` 工具持一张 `session → [tab]` 表。三条否掉它：

1. **归属规则**（根 `CLAUDE.md`）：只服务一个前端的一个能力的派生状态，不许上浮成 `ToolCtx` 的通用字段。`ToolCtx` 今天没有 session id 是个**结论**不是个遗漏——`scratch_dir` 的最后一段恰好是 session id，别拿它当 key，那是把一条路径当成一个标识符用。
2. **工具是单例。** 一个 `Arc<Agent>` 配多个 `Session`（`tcode-frontend::build_agent`），工具只有一份。那张表因此要自己处理会话关闭、resume、rewind——三种生命周期事件，每一种都要和 ledger 对齐，而 ledger 那三个合法操作里没有一个知道浏览器存在。
3. **它买到的是隔离，而隔离已经免费拿到了。**

### 隔离是免费的

**tab id 本身就是 capability，而 context 天然隔离。** 会话 A 的模型只知道 A 自己 `browser.open` 拿回来的那些 id;B 的 id 从来没进过 A 的 context，而 UUID 猜不出来。要越界只有一条路——用户自己把 id 从一边粘到另一边，那是用户的决定，不是漏洞。

于是后端没有 session→tab 归属表，也没有需要随 ledger 对齐的 ref 翻译表；rewind、resume 与 `closeSession` 都不用管理 tab 所有权。`BrowserTool` 仍保留两个窄范围、按 tab 的观测缓存：同步审批所需的 host 备忘，以及仅供 `computed_style` 的最新 snapshot ref/URL 准入集；二者在页面可能变化后宁可清除而非猜测。

### 归属活在前端，而它已经在了

事件流按会话打标（`bridge.rs::pump_events` 把每个 `AgentEvent` 裹进 `SessionEvent{session, event}`）。`browser.open` 的返回值到达前端时，它在一个带 session 的 `ToolEnd` 里。所以 `webHost.ts` 那个 module store 给 `Tab` 加一个字段就够了：

```ts
type Tab = { …; agent: boolean; owner: string | null };
//              ↑ 出生时就知道      ↑ 事后从带 session 标签的工具调用里学
```

- strip 上给 agent 开的 tab 画一个会话色点（实现细节见下面 Phase 2 收尾那节）。
- **会话关闭时 tab 不关，只 disown**（`owner` 置 `null`）。理由与"浏览器窗格不带 session"是同一条：你正在读的页面不该因为关掉一个对话而消失。这条和规则 9h 的 `closeSession` 过滤是同一句话的两半。

### 反过来：agent 能不能操作用户自己开的 tab

能，但不靠 owner 判定，靠**用户把 id 交出去**。

这条同时解释了**为什么没有 `browser.tabs` 列表 action**：一个能列举全部 tab 的 action 会把用户正在浏览的 URL 送进模型的 context，那是隐私泄露披着能力的皮。用户交出去的 tab 是**用户消息里的一条事实**，不是模型能枚举的东西。模型自己开的那些，它自己的转录就是列表。

**入口是工具栏的一个按钮（"Mention this page in the message"），不是原设计里的 `@tab` 补全。** 换掉的理由有三条，第一条是硬的：

1. **`@` 已经是文件命名空间**。`completion.ts` 的 `tokenAt` 只有 `command` / `mention` 两种 token，而 `mentions()` / `segments()` / `useKnownMentions()` 全都把 mention 的正文当路径用（后端还会去展开它）。往同一个 sigil 底下塞第二个命名空间，正是根 `CLAUDE.md` 让人设计掉的那类特例；另起一个 sigil 则是为**一种**只有两三项的条目新造一整套 token 机制——补全的价值在列表长的时候，这个列表长不了。
2. **决定"给你看这个页面"是在读它的时候发生的**，所以动作该在你正看着的那个页面上，而不是在另一个窗格的输入框里回想 tab 叫什么。
3. 每个 tab 加一个按钮会挤掉 strip——关闭那个叉已经占着 hover 出现的那个位置了。

写进 draft 而不是发送，也不是 ledger 里的 note：和文件树的 "Mention in the message" 同一个机制、同一个理由——**交出去一个页面却不带问题对谁都没用**，"看看这个，然后……"是一句还没写完的话。落进 composer 的是 `browser tab <id> (<url>)`。

**只带 URL，不带页面标题。** 标题是网站写的散文，而这行字最终会落在**用户**消息里——那是指令来源。用户按回车前会看见它，所以这里安全；但把网页写的字放进用户的那一轮，本来就没有理由，地址已经说明了一切。

哪个会话？**聚焦的那个窗格**（`focused(tiling)` + `paneSession`），和 `terminalCwd` 回答"哪个文件夹"用的是同一句话（规则 9c）。浏览器窗格不属于任何会话，所以这个问题只能这么答。

### 后台 tab：agent 的命令绝不改变屏幕上是哪个 tab

`restack()` 今天按 `current` 决定谁可见，`current` 只由前端的 `browser_select` 设。**agent 的 navigate / click / snapshot 一律不碰 `current`。** 用户想看就自己点过去。

这条已经测过并成立（见上面 Phase 0 第 1 条）：隐藏的 view 上 `Runtime.evaluate` 与 `Input.dispatchMouseEvent` 都照常工作。

**`screenshot` 不改变 `current`，但后台 tab 需要一次被 app 完整覆盖的 render recovery**（见 Phase 0 第 1b 条的第三次修正）。`rendered` 短暂调整 native child tree，目标像素从未露出，完成后当前 tab 与 pane visibility 原样恢复；click/type 等视觉交互也走同一条，保证 portal/canvas 在隐藏前提交。

顺带一句：**a11y 树的读取不依赖像素内容，但隐藏出生的当前文档仍先走同一条 bounded recovery。** 这样“AX 有内容、capture 为空”不再被当成一个可接受的页面状态。snapshot 仍是主要观测面。

## 传输：反向 RPC

今天管道是单向请求的：壳 → sidecar `{id, method, args}`，sidecar → 壳 `{id, ok|error}` 与 `{event, payload}`（`src/sidecar.rs`）。浏览器工具跑在 sidecar 里，要驱动壳里的 view，需要的是**反向的请求-应答**——事件是发出去就不管的，拿不回 snapshot。

加两种帧，各一个新键：

```text
sidecar → 壳   {"call": 3, "method": "browser_snapshot", "args": {...}}
壳 → sidecar   {"call": 3, "ok": {...}} | {"call": 3, "error": "..."}
```

- **两个方向的 id 空间独立**（`id` vs `call`），两边的 pending map 不会撞。
- sidecar 的读循环今天按 `id` + `method` 判定，加一条：有 `call` 键的是回复，进 pending map，不是新请求。
- 壳的 line handler 今天按 `frame.event !== undefined` 分流，加一条 `frame.call !== undefined` → 查 `verbs` 表 → 把答复写回 stdin。
- **壳的 verb 表一张不变。** `ipcMain.handle("tcode:invoke")` 和这条新路径查的是同一张表——这不是新机制，是同一张表多了一个调用方。`browser_open` 被前端调就返回 id 给 strip，被 sidecar 调就返回 id 给工具。

Rust 侧与 `Emit` 同形（同一个 `Outbound`，同一种 late-bind）：

```rust
#[async_trait]
pub trait Shell: Send + Sync {
    async fn call(&self, method: &str, args: Value) -> Result<Value, String>;
}
```

工具持 `Arc<dyn Shell>`;`tests/bridge.rs` 用 fake 顶替，于是**整个浏览器工具可以在没有窗口的情况下被测试驱动**（硬规则 2 的原话）。

**这条通道是通用的，但只许有一个消费者。** 发现第二个功能想用它时先问它是不是该在 Rust 里做完——"壳里不许出现第二份业务逻辑"不因为多了一条通道而失效。

## 工具面

### 一个工具，`action` 枚举

不是十个工具。理由：

1. 它属于 `display_tools` 那一类——**只有桌面 app 注册**（`BootSpec::display_tools`，`ShowTool` 是先例）。十个工具进 tool list 会让两个前端的工具面差出一大截，而 `agent` 工具的 `ToolPolicy`、system prompt、权限规则都要跟着列十行。
2. 权限描述符是一族：`browser(navigate github.com)`,一条规则一族行为。
3. 它确实是**一个对象的一组方法**，不是一组独立能力。

代价诚实地写在这儿：**多 action 单工具的 schema 质量比不上一个 action 一个工具**，模型会在参数上出错。缓解只有两条——description 按 action 分段列参数，以及错误信息自愈（"action=click 需要 ref;先 snapshot 取"，零猜测原则）。

### 第一版的 action

| action | 参数 | 阶段 | 说明 |
|---|---|---|---|
| `open` | — | 2 | 开 tab,返回 id,停在 `about:blank`。**没有 `profile` 参数**:第一版只有一个 partition |
| `navigate` | `tab, url` | 2 | url 经 `crate::address::to_url`,与地址栏同一份判定 |
| `snapshot` | `tab` | 2 | 过滤后的 a11y 树 + `ref`。**默认的"看"**;实现主体是过滤规则(见 Phase 0 第 2 条)。每个保留下来的具名元素都带自己的 backend-node ref，除了交互还供 `computed_style` 精确定位；Rust 只准后者使用最近一次 snapshot 实际输出、且绑定该 snapshot URL 的 ref。**没有 `cursor` 参数**——输出过大时走 `Tool::gates_output` 那条既有的溢出到文件的路,模型已经知道怎么 read/grep 它,再发明一套分页只是第二种要学的机制 |
| `close` | `tab` | 2 | |
| `back` / `forward` / `reload` | `tab` | 3 | 只读。之后**清掉 host 备忘与 snapshot ref 准入集**——页面去哪了这工具没看见,下一次 `click` 会要求先 snapshot，下一次 `computed_style` 也只能查新快照 |
| `click` | `tab, ref` | 3 | 真鼠标事件(`Input.dispatchMouseEvent`),不是 `element.click()`——后者跳过 hover/focus/遮挡层,页面看着能用其实没动 |
| `type` | `tab, ref, text, submit?` | 3 | **替换**字段原有内容(用元素自己的 `select()`),`Input.insertText` 一次写完;`submit` 是真 Enter |
| `scroll` | `tab, ref?, direction, amount?` | 3 | 带 `ref` 就在那个元素里滚 |
| `wait` | `tab, text?` | 3 | 有 `text` 等这句话出现,没有就等页面不再加载(`isLoading()` 连续两次为假)。**没有 `for` 枚举**:一个可选参数就分得开,少一个概念 |
| `computed_style` | `tab, ref, properties` | 3 | 只读一个**最近 snapshot 实际输出且 URL 仍相同**的元素的 1–12 个白名单 CSS computed values；Rust 与壳双重校验，固定函数 + value arguments，**没有 selector / JS eval** |
| `screenshot` | `tab, prompt?` | 3 | `capturePage()`；后台 tab 先在 app cover 下做 bounded render recovery（见 Phase 0 第 1b 条第三次修正），不改变 current；主模型能看图就回 image block，纯文本主模型委派 live vision role 后只回围栏内文字，两边都不能看才在 capture 前拒绝并指向 `snapshot` |

**`ref` 三种写法都收**(`ref_44` / `"44"` / `44`)。零猜测原则最小的一次应用:替代方案是模型花一个 turn 试出这工具要哪一种。

多 action 单 schema 的代价这下真的到场了(13 个 action)。缓解还是那两条——description 按"看"和"做"分段列参数,错误信息自愈("先 snapshot,那也是 ref 的来源")。拆不拆等真看到模型在参数上出错再说,现在拆是猜。

**不给原始 `cdp` 逃生口。** ChatGPT 那份建议留一个,这里第一版不留:它会立刻变成模型的默认工具(什么都能干),然后语义层永远等不到反馈——真实用例会全部藏在 `Runtime.evaluate` 里,而我们看不到。真需要时再加,加的时候要审批,并且挂在 `/dogfood` 后面(`SlashCommand::hidden()` 的同一条理由)。

### ref 从哪来：不建翻译表，但绑定 snapshot

原来这里写的是“snapshot 时建一张 `ref_N → backendNodeId` 表存在壳里”。实现时发现那张**翻译**表不需要存在：**`ref` 直接就是 `backendDOMNodeId`**（`Accessibility.getFullAXTree` 每个节点自带），壳不保存它，也没有将 A tab 的编号转换给 B tab 的路径。

但 `computed_style` 不能把 Chromium 内部 id 当作可猜的 API。Rust 在每次 snapshot 后按 tab 记录**实际写入模型输出的** backend ids 与该快照 URL；输出预算截掉的节点不进入集合。查询先要求命中这份最新准入集，再把 URL 一起交给壳；壳在 serialized `rendered` work 内、`DOM.resolveNode` 前复核当前 URL。navigate、history、reload、close 以及可能改页的交互清掉集合；页面自行跳转则被壳的 URL 复核挡住。

**过期仍是双保险**：即使节点在同一页面重渲染后消失，`DOM.resolveNode` 也会报错，理由仍是“重新 snapshot”。不注入 `data-tcode-ref` 属性：那会让观测手段污染被观测对象，而且 SPA 一次重渲染就把属性冲掉，症状是“点了个不存在的东西”。

## 权限与信任边界

四件事，一件都不能省。

### 1. 页面内容是数据，不是指令

`snapshot` 的输出必须走 `web.rs::fence_page` **同一套**围栏与转义（`<web-page-content url="…">` + 对结束标签的转义），不许新发明一种。

这是整个功能里唯一可能变成安全事故的地方，而且 ChatGPT 那份完全没提：一个能点按钮、能填表单的 agent，它读到的字节来自开放网页。`web_fetch` 的输出还只是"模型读到了一段话"，这里是"模型读到一段话之后会去点东西"。围栏不是洁癖，它是这条链上唯一的结构防线（根 `CLAUDE.md` 的信任边界那节：能用结构挡的不许退化成 prompt 纪律）。

### 2. 导航复用 `web_fetch` 的按 host 词汇，但不共用 descriptor

"要不要连这个 host"是同一个问题，所以判定的**词汇**同源（bare host，`www.` 折叠，跨 host 重定向回到模型手上）。

但 **descriptor 分开**：`browser(navigate github.com)` 不等于 `web_fetch(github.com)`。一个"永久允许读 github.com"的规则不该悄悄扩散成"允许在 github.com 上点按钮"——前者是读，后者带副作用。

**Auto Mode 的同构结论**：`navigate` 与 `web_fetch` 共享**同一份判定**（`tcode_tools::trusted_public_read`，host 列表是 `[auto_mode] trusted_read_hosts` 那同一份），trusted 的匿名 HTTPS 默认端口 host 直接 `Allow`、跳过安全分类器；loopback（`localhost` / `127.0.0.1` / `::1`）与 RFC 1918 私网段（10/8、172.16/12、192.168/16，含 IPv6 ULA fc00::/7）也直接 `Allow`——前者是 dev server，后者是本机浏览器在自家局域网里读东西。但 `click`/`type` 不在这条快路上：读一个可信站点和以你的身份在上面点按钮是两件事，interact 永远回分类器（或审批），trusted 列表不扩散到它。

### 3. 登录态由"能不能动"守，不由"用哪个 profile"守

**已定：agent 的 tab 就在 `persist:tcode-browser` 上**，与用户屏幕上那个浏览器共用登录态。没有第二个 partition，`Profile` 那一层在第一版里退化成一个常量。

这条曾经的另一个写法是"默认给一个空白的 ephemeral profile，要用登录态就审批一次"。放弃它的理由不是省事，是**它守错了东西**：一个只能 `navigate` 和 `snapshot` 的 agent，拿到登录态之后能做的全部事情就是**读**你已经登录的页面——而那正是这个功能相对 `web_fetch` 的唯一价值。为"读"设一道审批墙，换来的是每次都要点一下头才能看见自己早就打开着的那个 PR，而墙后面并没有危险。

**危险在 Phase 3**，所以边界画在那儿（已实现）：

- 在持久 profile 上的**变更类**交互（`click` / `type`）要审批，descriptor 带 host。
- **descriptor 是 `browser(interact <host>)`，click 和 type 共用一条**（原文写的是 `browser(click …)`）。理由就是上面那条判据本身：人在回答的问题是"这个 agent 能不能以我的身份在这个站点上动手"，那有**一个站点一个答案**，不是一个输入设备一个答案。具体点了哪个 ref、填了什么字，进 `summary`——那是给人读的；descriptor 是给**规则**写的。
- 其余全部免审：`open` 造空 tab、`close` 销毁、`snapshot`/`computed_style`/`screenshot` 读已经加载好的页、`scroll`/`wait` 什么都不动、`back`/`forward`/`reload` 重访这个浏览器已经请求过的页。判据不是"这个 action 危不危险"，是"它会不会在**别人的**服务器上留下痕迹"。

**host 从哪来，这是这一节唯一有结构成本的地方。** `Tool::permission` 是同步的，问不到窗口。所以工具持一张 `tab → host` 的备忘（`BrowserTool::seen`），只由工具自己观测到的 URL 写入：它解析过的 `navigate`，以及页面答回来的 `snapshot`/`computed_style`/`screenshot`。**绝不采信模型说的 host**——那等于让模型自己写自己的权限描述符。

备忘会过期（页面自己会跳），而过期**不会**造成错误的点击：`browser_click` / `browser_type` 把这个 host 一起传给壳，壳在 serialized `rendered` work 内、派发事件**之前**拿实时 URL 比一次，不等就报错。放在壳里而不是多一次往返，是因为排队恢复 compositor 期间页面也可能跳转，只有检查与动作在同一队列工作内才没有窗口；壳在这儿不做判断，只比一个后端算好的值（`dispatch.rs::acting_on_a_tab_checks_the_page_has_not_moved` 钉住）。

这也是为什么"没看过的 tab 不能点"：没有备忘 = 没有 host 可问 = 模型手里那个 `ref` 也不可能是真的。`permission()` 此时返回 `None`（无主题的审批面板问不出问题），`run` 直接拒并说"先 snapshot，那也是 ref 的来源"。

ChatGPT 那份把"agent 直接用你的 work session"当卖点，方向是对的——人机共驾确实是全部价值所在。它漏掉的是**共驾的风险不在读的那一侧**。

### 4. 不许碰自己家

一律拒绝，判定在 Rust（`src/address.rs` 旁边，纯函数 + 测试），不在壳里：

- `app://tcode` —— 这个 app 自己的 origin，preload 在那儿。
- `http://127.0.0.1:<serve.rs 的端口>` —— 规则 11b 那个第三方 origin。**只挡这一个端口，不挡整个 loopback**：看 dev server 是这个窗格的头号用途（规则 9h）。
- `file:` —— 那是把浏览器变成一个没有审批的文件读取器，而读文件有 `read` 工具，那条路上有审批面板。

外加：`evaluate`（如果以后加）每次都要审批，不给 always-allow。

## 分阶段

**第一版 = Phase 0 + 1 + 2。** 三步都做完才有东西可用，做完就停下来看。

**Phase 0 — 补测。✅ 已完成**（win32 初测，2026-08-10 Linux/KDE 复测一致）。`spike/main.js::agentBrowserProbes`，结果在 `results.json` 的 `probes.agentBrowser`，结论在上面"Phase 0 补测的结果"。zOrder 像素探针在 Linux 双屏下未测成（采样点落在捕获外），见上。

**Phase 1 — 通道。✅ 已完成。** `sidecar.rs` 的 `Shell` trait + `ShellClient`（`{"call": n, …}` 帧、按 `call` id 路由回复、`CALL_TIMEOUT` 30s、shell 退出时清空 pending），`electron/main.js` 的 `serveCall`（同一张 `verbs` 表，多一个调用方）。六条单测在 `sidecar::tests`。

**这一步不写任何浏览器代码**，通道在 Phase 2 之前是惰性的。诚实地说清验证到哪一步：Rust 那半有单测跑真往返（帧形状、错误传播、shell 死掉、迟到的回复、请求不被误认成回复）；**JS 那半只有一条读文本的断言**（`the_shell_routes_call_frames`），因为在有调用方之前没有东西能真正驱动它——第一次真跑通是 Phase 2 的事，别把这条文本断言当成"测过了"。

**Phase 2 — 只读的浏览器工具。✅ 已完成。**

- `src/browser.rs`：`BrowserTool`（`open` / `navigate` / `snapshot` / `close`），经 `BootSpec::display_tools` 注册，与 `ShowTool` 同一个钩子。没有 session→tab 表或 ref 翻译表；后续 `computed_style` 另用短生命周期 snapshot 准入集，不参与 session 生命周期。
- `navigable()`：叠在 `address::to_url` 上的准入检查（只放 http/https、拒掉 viewer 端口、loopback 其余照常）。纯函数，5 条单测。
- snapshot 的过滤 + 缩进 + 围栏（与 `web_fetch::fence_page` 同一个标签同一套转义）。
- **`ref` 就是 Chromium 的 `backendDOMNodeId`**,所以壳不存 ref 翻译表。`computed_style` 仍需 Rust 按 tab 记录最近 snapshot 实际输出的 ids 与 URL，页面变化后清空；这让内部 id 只有被观察到后才成为读取 capability。
- 壳侧 `browser_snapshot`（`Accessibility.getFullAXTree`），**返回原始节点不做过滤**——过滤是要反复调、要测的判断，属于工具不属于壳。
- tab 进 strip：`BROWSER_TAB_OPENED` 事件 + `knownTab` 去重 + `addTabBehind`（不抢焦点）。这**推翻了 `browser.js` 里那句"主进程自己造一个 tab 会让它出现在屏幕上却不在 strip 里"**，`AGENTS.md` 规则 9h 已同步。

**验证到哪一步**：Rust 侧 10 条单测 + 7 条集成测试（fake shell 驱动整个工具，无窗口）；前端 3 条新测试（agent 的 tab 进 strip、不抢当前 tab、不重复添加）。**壳里的 `browser_snapshot` 与整条反向 RPC 的真实往返仍未被自动化测试覆盖**——第一次真跑通要在 app 里让模型调一次。别把 `dispatch.rs` 那几条读文本的断言当成"测过了"。

**Phase 2 收尾 — 会话色点。✅ 已完成。** 两个事实，不是一个：

- **`agent`（不是你开的）随 tab 出生**，走 `BROWSER_TAB_OPENED` 的 payload。必须从第一帧就对：这个 tab 里马上要开始加载一个页面，strip 要是把它画得和别的 tab 一样，就等于在回答"这是我开的吗"时耸肩。
- **`owner`（是谁开的）事后学**，从 `ToolStart` 的 **input**（`{action, tab}`）里读——它同时是带 session 标签的、又是指名 tab 的，全窗口只有这一条流两样都占。**读 input 不读结果文本**：input 是模型发的结构化数据，结果是写给模型读的一句英文，回头解析自己的散文会让一句措辞变成承重墙。
- 代价诚实写着：`open` 自己认领不了（它的 input 还没有 `tab`），所以刚开的 tab 有一两秒没主。那是**诚实的**——确实还没人认领它，而 `agent` 全程为真，strip 不会假装它是用户的。
- 颜色是 session id 的 hash → OKLCH 色相（亮度/彩度固定）。**颜色不是唯一信号**：`.tab-exit` 那条既有规则（"Said in words, never in colour alone"）在这儿照办，dot 的 `aria-label` 和 tab 的 tooltip 都写着是哪个对话开的；看不见色相的人只损失"两个对话的 tab 分得开"这一点。不建调色板表——那是一张要跟着开着的对话手工维护的表。

**顺带修了一个真 bug**：`webHost` 的事件订阅原来从窗格 mount 才开始。模型可以在窗格从没打开过的窗口里开 tab——那条通告发给了空气，之后打开窗格看到空 strip，又开了**第二个** tab，第一个页面存在但谁也够不着。现在窗口启动时就订阅（`App.tsx` 调 `browser.watch()`），并且 mount 时若没有 current 就选第一个。同一类问题的另一半：壳里 `rect` 的初值从 `0×0` 改成 `1280×800`——`place()` 会把 0 夹到 1px，而 1×1 viewport 不是"小页面"，是**停止排版**的页面，snapshot 出来的东西没人见过。

**Phase 3 — 交互与像素/样式观测。✅ 已完成。** `click` / `type` / `scroll` / `wait` / `back` / `forward` / `reload` / `screenshot` / `computed_style`，加变更类交互的按 host 审批（见权限第 3 条）与壳侧的实时 host 复核。

- **没有 ref 翻译表**：`ref` 是 `backendDOMNodeId`，具名静态元素也打印 ref，供 `computed_style` 定位；但 Rust 只允许查询最近 snapshot 实际打印、且 URL 仍绑定的 ref，并在页面可能改变时清空 admission set。`type` 在壳内再次确认目标是 input / textarea / contenteditable，避免静态 ref 把文本送进旧焦点。
- **没有任意 JS 逃生口**：`computed_style` 的 CSS 属性名在 Rust 与 Electron 两层都过静态白名单，再作为 `Runtime.callFunctionOn` 的 value argument 交给固定函数；Rust 传入 snapshot URL，壳在同一条 serialized `rendered` work 内于 `DOM.resolveNode` 前复核它，结果与 snapshot 一样进页面内容围栏。
- 坐标用页面里的 `getBoundingClientRect()`，不用 `DOM.getBoxModel`。两个都能答，但只有前者是**规范定义**的（相对 viewport，正是 `Input.dispatchMouseEvent` 要的）；后者的坐标系是 Chromium 的一个约定，这边验证不了，而搞错的症状是点偏几百像素——看起来像页面坏了，不像坐标系错了。
- `wait` 的 needle 经 `JSON.stringify` 插值进表达式：模型从敌意页面上抄一句话来等，不能让那句话在页面里被执行。

**验证到哪一步**：Rust 侧 15 条集成测试（fake shell 驱动整个工具，无窗口）+ 单测；壳侧靠 `dispatch.rs` 读文本钉住两条**结构**不变量（host 复核在动作之前、截图不走 CDP）。**click/type/scroll/wait 的真实往返仍未被自动化测试覆盖，坐标与 `Input.insertText` 的行为是按 Chromium 的既有做法写的、没在真页面上跑过。** 第一次真跑通要在 app 里让模型点一次。

**Phase 4 — 交接。✅ 已完成。**

- **disown**：会话关闭时它的 tab **不关，只把 `owner` 置 `null`**（`App.tsx::closeSession` → `webHost::disown`）。`agent` 不动——那是"这个 tab 打哪来"的事实，不会因为对话没了而变成假的。这和 `closeSession` 按 `paneSession` 过滤掉浏览器窗格是同一句话的两半。
- **交接**：工具栏一个按钮，把当前页写进聚焦会话的 draft。**原计划的 `@tab` 补全没做**，理由写在上面"反过来"那节——一句话是 `@` 已经是文件命名空间，为一个只有两三项的列表另造一套 token 机制不值。
- **模型这边零猜测**：tool description 里加了一句——用户消息里出现 `browser tab <id> (<url>)` 就是用户在把自己的 tab 交过来，照常用那个 id、从 `snapshot` 开始。少了这句，模型看到一串 UUID 未必知道那是能用的。

自然接上的一点：交过来的 tab 这工具**从没看过**，所以 `seen` 里没有它的 host，`click`/`type` 会先要求 snapshot——正是权限那节要的行为，不用额外写一行。

**Phase 5（多半不做）** — 多 profile。做之前先问有没有人真的想要第二个登录态。

分期的判据不是"把工作切成五份"，是**Phase 2 结束时它已经是一个可以停下来的产品**："读一个需要登录、需要跑 JS 才有内容的页面"——`web_fetch` 干不了的那一整类，到 Phase 2 就解决了。如果 Phase 3 之后发现模型点不准，Phase 2 也不白做。
