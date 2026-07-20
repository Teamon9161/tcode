# tcode-providers 硬规则

改动不得破坏（设计层面的"为什么"见 `plan.md`；全局约束见仓库根 `CLAUDE.md`）。

## 模型能力差异归 provider 消化，不许上浮到调用方

- **Codex 订阅端点是严格白名单**：未知字段一律 400。它没有 `max_output_tokens`（官方 `ResponsesApiRequest` 里就没有这个字段，config 的 `model_max_output_tokens` 是客户端预算，不上线），所以 `Request::max_tokens` 在这条路径上无效——需要短输出就靠 prompt 或 `text.verbosity`/结构化 schema，别加参数。
- effort 同理：我们的 `off` 对应 Responses API 的 `"effort":"none"`，原样发 `off` 是 400。

## 缓存键

Codex 的缓存键是 `session_id` **请求头**（后端会用它覆写 body 里的 `prompt_cache_key`，别指望改 body 生效），provider 按 `Request::cache_scope` 派生稳定 uuid。作用域语义本身见 `crates/tcode-core/AGENTS.md`。

## 测试

SSE/wire 格式在 `tests/wire.rs`。测试永不调真实 API。
