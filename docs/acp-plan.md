# ACP / Zed 集成计划

**状态：Phase 1 MVP 已实现；继续补充 provider 驱动的流/取消覆盖**  
**目标协议：ACP v1**  
**入口：`tcode --acp`**

## 目标与非目标

让 Zed 等 ACP client 能把 tcode 作为外部 agent 启动，并获得可靠的：会话创建、文本 prompt、流式输出、工具调用状态、审批和取消。

第一阶段不把“进程运行在 Zed 启动的工作目录”误称为完整编辑器集成。当前 tcode 的文件工具和 shell 直接读写本地磁盘、创建本地进程；因此它看不到未保存 buffer，也不能让 Zed 托管 terminal。ACP server 必须只声明实际实现的能力。

## 现状与约束

- `tcode_core::Agent::user_turn` 已经是协议无关的主循环，负责 provider 流、工具循环、取消和 ledger 一致性。
- `tcode_core::AgentEvent` 是内部事件模型，必须由 adapter 映射成 ACP `session/update`；不能直接作为 wire format 写入 stdout。
- `Approver` 是前端无关 trait，ACP 适配器应以 `session/request_permission` 实现它；无响应、断连和取消必须 fail closed。
- `tcode_frontend::boot` / `open_session` 是共享装配层。ACP 不依赖 TUI 或桌面 app。
- ACP session 的 `mcpServers` 属于会话而不是进程。现有 `boot()` 只在启动时连接 config MCP server；第一阶段要把 client 提供的 stdio MCP server 进入每个 ACP session 的工具集。
- ACP transport 的 stdout 只能输出 UTF-8 JSON-RPC 帧，任何日志、配置诊断和 provider warning 都必须写 stderr。

## 架构边界

```text
src/main.rs
  --acp -> tcode_acp::serve_stdio(...)

crates/tcode-acp
  ACP JSON-RPC transport and protocol handlers
  connection/session supervisor
  ACP client requests (permission)
  AgentEvent -> session/update mapper
  protocol integration tests

crates/tcode-frontend
  reusable config/model/session assembly
  extend only where ACP needs a session-local tool/MCP input

crates/tcode-core
  remains ACP-agnostic: Agent, Session, ledger, permission and Tool contracts

crates/tcode-tools
  keeps local FS/shell tools and MCP client implementation
```

No ACP schema or JSON-RPC type belongs in `tcode-core`.

## Phase 1 — usable ACP MVP

### Included

1. Add workspace crate `tcode-acp` and the `--acp` CLI flag.
2. Serve newline-delimited JSON-RPC 2.0 over stdio.
3. Implement ACP v1 `initialize`, `session/new`, `session/prompt` and `session/cancel`.
4. Build one isolated `Session` per ACP session ID and retain its cancellation token.
5. Accept text prompt content. Reject unsupported content types with a protocol error instead of silently dropping them.
6. Translate core events:
   - `TextDelta` -> `agent_message_chunk`;
   - `ThinkingDelta` -> `agent_thought_chunk` where supported by the schema;
   - `ToolStart` / `ToolEnd` -> ACP tool-call lifecycle updates;
   - `Interrupted` -> prompt response with `stopReason: cancelled`.
7. Implement `AcpApprover` with `session/request_permission`; map allow-once, allow-always (session scoped initially), and reject-once. Cancellation and missing reply reject.
8. Connect client-provided stdio MCP servers per ACP session, as required by ACP. Merge them into that session's configured MCP tool set; reject duplicate names rather than silently choosing one source.
9. Add a subprocess integration test that verifies handshake, new session and protocol-only stdout; cover event, permission and cancellation mappings with focused unit tests that never call a provider.

### Explicitly deferred

- ACP `session/load` / `session/resume` and transcript replay.
- Images, audio, embedded resources, additional directories, authentication, agent commands and configuration options.
- Editor-backed filesystem and terminal operations.
- Elicitation mapping for `ask_user`.
- Rich plan / task / cohort UI mappings.

### Phase-1 local execution disclaimer

The MVP uses the existing local tools. It must not advertise client FS or terminal support merely because Zed offers it. The adapter may request ACP permission, but file and shell execution remain local until Phase 2.

## Phase 2 — ACP resource backends

Introduce injectable filesystem and terminal execution ports. The local CLI/TUI retains current direct implementations; ACP uses `fs/read_text_file`, `fs/write_text_file` and `terminal/*` only after checking client capabilities.

This phase must define freshness and checkpoint behavior against editor-visible state, then adapt `read`, `write`, `edit`, `append`, shell, background tasks and monitors. `edit` and `append` can begin as client-read / pure transform / client-write operations.

## Phase 3 — session and UX completeness

- Advertise and implement load/resume with complete ACP replay.
- Expose tcode permission modes through ACP session modes.
- Map `ask_user` to elicitation where available.
- Add file locations, plans, commands, richer tool output and documented extension metadata.
- Define the deterministic merge/failure policy for config MCP servers and client-supplied MCP servers.

## Verification gates

- Unit tests for event, permission and cancellation mappings.
- A stdio subprocess test validates every stdout line as JSON-RPC and exercises an ACP client conversation.
- `cargo fmt --check` and targeted crate tests before expanding scope.
- No real provider API call in any test.

## Release criterion for the MVP

Zed can launch `tcode --acp`, establish ACP v1, open multiple independent sessions, receive text/tool progress, approve or reject a change, and cancel a running turn without hanging the client. The documentation must label file/terminal behavior as local until Phase 2 lands.
