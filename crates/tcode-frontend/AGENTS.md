# tcode-frontend — 硬规则

跨前端共享的**UI 无关**装配逻辑：core 只给驱动契约（`Agent::user_turn` / `AgentEvent` / `Approver`），本 crate 收拢每个前端否则都要手搓的 composition-root 接线——建 agent、开会话（带持久化）、菜单/preset/provider-setup 数据、（后续）turn driver。

## 不可违背

1. **绝不依赖任何 UI crate**（`tcode-tui`、ratatui、crossterm 等一律禁止）。这是拆这个 crate 的全部意义：让桌面 app / 未来前端不必链接 tui。依赖只能是 core（及后续必要的 tools/providers 用于组装）。发现要用 tui 里的类型 → 那类型本身就该下沉到这里或 core。
2. **只放"每个前端都一样"的逻辑**。渲染、键盘、终端、webview 桥这些各前端专属的东西不进来；它们消费本 crate 的输出。判据：TUI、plain REPL、桌面 app 三者是否都需要同一份且行为一致——是才下沉。
3. **行为等价迁移**：从 `src/main.rs` / `tcode-tui` 搬进来的逻辑不得顺手改语义。搬迁 = 纯提取，先保证三个前端行为不变，语义调整另开改动。
4. **不做终端 I/O**：库函数不 `println!`/`eprintln!`，警告一律作为返回值（如 `agent_models -> (AgentModels, Vec<String>)`）交给 caller 呈现。新加一条"这里要提醒用户"时，沿返回签名把它抛出去，别就地打印。
5. **输入用语义按键，不用某个 stack 的事件类型**：`setup::Key` 只有 setup 分辨得出的那几个动作。前端各自把 crossterm / 浏览器事件映射过来（TUI 的映射在 `tcode-tui/src/setup.rs`）。要往 `Key` 里加 modifier 位时先停：那说明想表达的是终端细节，该留在映射层。

## 现有内容

- `agent.rs`：`build_agent(AgentBuild) -> Arc<Agent>`——工具集 + `agent` 工具 + 安全分类器的装配。
- `session.rs`：`open_session(SessionSpec) -> Session`——建 `Session`、按 `[tcode_state]` 播种运行态开关、挂 JSONL 持久化（create/resume）。桌面 app 的 supervisor 每开一个项目文件夹调一次。
- `menu.rs` / `build.rs`：model / preset / agents / provider 菜单的数据类型与构建。渲染留在各前端；这里只产出数据与"应用某个选择"的闭包。
- `setup.rs`：provider setup 状态机（`Setup` + `View`/`Row`——一个什么都不画的状态机）、`Key`、以及 `/login` 的 `CodexLogin`/`LoginUpdate` 契约。三个渲染器共用：首启 wizard、`/provider` overlay、桌面 app 的表单。
