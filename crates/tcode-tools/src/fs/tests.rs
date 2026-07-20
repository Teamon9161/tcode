use super::edit::{
    candidate_help, near_miss_help, occurrence_help, replacement_plan, MAX_EDIT_CANDIDATES,
};
use super::*;
use serde_json::{json, Value};
use std::sync::Arc;
use tcode_core::images::detect_image_mime;
use tcode_core::{PermissionRequest, Tool, ToolCtx, ToolOutput};
use tokio_util::sync::CancellationToken;

#[cfg(windows)]
use std::path::Path;

struct TextOnlyProvider;

#[async_trait::async_trait]
impl tcode_core::Provider for TextOnlyProvider {
    fn name(&self) -> &str {
        "test"
    }
    fn model(&self) -> &str {
        "text-only"
    }
    fn cache_strategy(&self) -> tcode_core::CacheStrategy {
        tcode_core::CacheStrategy::ImplicitPrefix
    }
    fn supports_vision(&self) -> bool {
        false
    }
    async fn stream(
        &self,
        _request: tcode_core::Request,
        _cancel: CancellationToken,
    ) -> Result<tcode_core::EventStream, tcode_core::ProviderError> {
        unreachable!("read routing test never streams")
    }
}

fn text_only_model() -> tcode_core::ModelCell {
    tcode_core::ModelCell::new(tcode_core::ActiveModel {
        provider: Arc::new(TextOnlyProvider),
        max_tokens: 1024,
        context_window: 16_000,
        effort: None,
    })
}

#[cfg(windows)]
#[test]
fn mapped_file_errors_explain_the_windows_lock() {
    let error = std::io::Error::from_raw_os_error(1224);
    assert!(is_windows_user_mapped_file(&error));

    let message = write_error(Path::new("locked.txt"), &error);
    assert!(message.contains("temporarily mapped or locked"));
    assert!(message.contains("retried once after 50ms"));
}

#[test]
fn edit_match_accepts_lf_old_string_in_crlf_file() {
    let text = "one\r\ntwo\r\nthree\r\n";
    let plan = replacement_plan(text, "two\nthree\n", "deux\ntrois\n")
        .unwrap()
        .unwrap();

    assert_eq!(plan.old, "two\r\nthree\r\n");
    assert_eq!(plan.new, "deux\r\ntrois\r\n");
    assert_eq!(
        text.replacen(&plan.old, &plan.new, 1),
        "one\r\ndeux\r\ntrois\r\n"
    );
}

#[test]
fn edit_matches_through_typographic_punctuation() {
    // File has ASCII; model's old_string uses an en-dash and curly quotes.
    let text = "let x = a - b; // \"note\"\n";
    let old = "a \u{2013} b; // \u{201C}note\u{201D}";
    let plan = replacement_plan(text, old, "a + b; // ok")
        .unwrap()
        .unwrap();
    assert_eq!(plan.count, 1);
    assert_eq!(plan.old, "a - b; // \"note\"");
    assert_eq!(
        text.replacen(&plan.old, &plan.new, 1),
        "let x = a + b; // ok\n"
    );
}

#[test]
fn edit_matches_through_drifted_internal_space() {
    // The real loop: model reproduced the block verbatim but added one
    // space (`["primary"] )` vs `["primary"])`); everything else matches.
    let text = "\
fn rate_limits_from() {
    Some(RateLimits {
        primary: parse(&value[\"primary\"])?,
        secondary: parse(&value[\"secondary\"]),
    })
}
";
    let old = "\
fn rate_limits_from() {
    Some(RateLimits {
        primary: parse(&value[\"primary\"] )?,
        secondary: parse(&value[\"secondary\"]),
    })
}
";
    let new = "fn rate_limits_from() { None }\n";
    let plan = replacement_plan(text, old, new).unwrap().unwrap();
    assert_eq!(plan.count, 1);
    // The spliced `old` is the file's real bytes (no drifted space).
    assert!(plan.old.contains("[\"primary\"])?"));
    assert!(!plan.old.contains("[\"primary\"] )?"));
    assert_eq!(text.replacen(&plan.old, &plan.new, 1), new);
}

#[test]
fn edit_matches_through_indentation_diff() {
    // File is tab-indented; model guessed spaces. Real bytes are restored.
    let text = "fn f() {\n\treturn 1;\n}\n";
    let old = "fn f() {\n    return 1;\n}\n";
    let plan = replacement_plan(text, old, "fn f() {\n\treturn 2;\n}\n")
        .unwrap()
        .unwrap();
    assert_eq!(plan.count, 1);
    assert_eq!(plan.old, "fn f() {\n\treturn 1;\n}\n");
}

#[test]
fn edit_ws_fallback_rejects_content_mismatch() {
    // Same shape, different token — must NOT match on whitespace alone.
    let text = "fn f() {\n    return 1;\n}\n";
    assert!(matches!(
        replacement_plan(text, "fn f() {\n    return 2;\n}\n", "x"),
        Ok(None)
    ));
}

#[test]
fn edit_fallback_rejects_ambiguous_whitespace_matches() {
    let text = "fn f() {\n\treturn 1;\n}\n\nfn f() {\n    return 1;\n}\n";
    let old = "fn f() {\n  return 1;\n}\n";
    let Err(candidates) = replacement_plan(text, old, "x") else {
        panic!("expected ambiguous match");
    };
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].at, text.find("fn f").unwrap());
    assert!(candidate_help(text, &candidates)[0].contains("candidate 1"));
}

#[test]
fn edit_recovers_single_line_old_string_against_reflowed_block() {
    // cargo fmt split the call across lines; model still has the pre-fmt
    // single line. Match anyway and splice the file's real (multi-line) bytes.
    let text = "\
fn t() {
    assert_eq!(
        left,
        right
    );
}
";
    let old = "assert_eq!(left, right);";
    let new = "assert_eq!(left, expected);";
    let plan = replacement_plan(text, old, new).unwrap().unwrap();
    assert_eq!(plan.count, 1);
    assert!(
        plan.old.contains('\n'),
        "spliced real bytes stay multi-line"
    );
    assert!(plan.old.starts_with("assert_eq!("));
    assert_eq!(
        text.replacen(&plan.old, &plan.new, 1),
        "fn t() {\n    assert_eq!(left, expected);\n}\n"
    );
}

#[test]
fn edit_recovers_multi_line_old_string_against_collapsed_line() {
    // Reverse reflow: file is one line, model has the pre-fmt multi-line form.
    let text = "    assert_eq!(left, right);\n";
    let old = "assert_eq!(\n    left,\n    right\n);";
    let plan = replacement_plan(text, old, "assert_eq!(left, expected);")
        .unwrap()
        .unwrap();
    assert_eq!(plan.count, 1);
    assert_eq!(plan.old, "assert_eq!(left, right);");
}

#[test]
fn edit_reflow_match_reports_ambiguous_when_block_repeats() {
    // Two differently-wrapped copies, neither an exact match — the reflow
    // rung must refuse to guess rather than silently pick the first.
    let text = "\
fn a() {
    assert_eq!(
        a,
        b
    );
}
fn c() {
    assert_eq!(a,
        b);
}
";
    let old = "assert_eq!(a, b);";
    assert!(replacement_plan(text, old, "x").is_err());
}

#[test]
fn edit_exact_match_short_circuits_before_reflow() {
    // An exact single-line hit must win outright; the reflowed decoy below
    // would otherwise make the loose reflow rung report ambiguity.
    let text = "\
assert_eq!(a, b);
assert_eq!(
    a,
    b
);
";
    let old = "assert_eq!(a, b);";
    let plan = replacement_plan(text, old, "assert_eq!(a, c);")
        .unwrap()
        .unwrap();
    assert_eq!(plan.count, 1);
    assert_eq!(plan.old, old);
}

#[tokio::test]
async fn edit_rejects_non_utf8_without_mutating_the_file() {
    let dir = std::env::temp_dir().join(format!("tcode-edit-binary-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("data.bin");
    let original = b"before\xffafter";
    std::fs::write(&file, original).unwrap();
    let ctx = ToolCtx::new(dir.clone(), 10_000);

    let out = EditTool
        .run(
            json!({
                "path": "data.bin",
                "old_string": "before",
                "new_string": "changed",
            }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;

    assert!(out.is_error);
    assert!(out.content.contains("not valid UTF-8"));
    assert_eq!(std::fs::read(&file).unwrap(), original);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn edit_rejects_an_empty_old_string() {
    let dir = std::env::temp_dir().join(format!("tcode-edit-empty-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("text.txt");
    std::fs::write(&file, "unchanged").unwrap();
    let ctx = ToolCtx::new(dir.clone(), 10_000);

    let out = EditTool
        .run(
            json!({
                "path": "text.txt",
                "old_string": "",
                "new_string": "insert everywhere",
                "replace_all": true,
            }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;

    assert!(out.is_error);
    assert_eq!(out.content, "old_string must not be empty");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "unchanged");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn edit_occurrences_show_bounded_line_numbered_context() {
    let text = "header\nfirst section\nalpha\nbeta\nsecond section\nalpha\nbeta\nfooter\n";
    let candidates = occurrence_help(text, "alpha\nbeta\n", 8);

    assert_eq!(candidates.len(), 2);
    assert!(candidates[0].contains("candidate 1 (lines 3–4):"));
    assert!(candidates[0].contains("     2\tfirst section"));
    assert!(candidates[1].contains("candidate 2 (lines 6–7):"));
    assert!(candidates[1].contains("     5\tsecond section"));
}

#[test]
fn edit_occurrence_help_marks_additional_candidates_as_omitted() {
    let text = "match\n".repeat(MAX_EDIT_CANDIDATES + 1);
    let candidates = occurrence_help(&text, "match\n", MAX_EDIT_CANDIDATES + 1);

    assert_eq!(candidates.len(), MAX_EDIT_CANDIDATES + 1);
    assert!(candidates
        .last()
        .unwrap()
        .contains("1 additional candidate matches omitted"));
}

#[test]
fn edit_not_found_shows_production_and_test_similar_locations() {
    let text = "\
pub fn helper() {
    actual();
}

#[cfg(test)]
mod tests {
    fn helper() {
        actual();
    }
}
";
    let help = near_miss_help(
        text,
        "fn helper() {
    expected();
}",
    );

    assert!(help.contains("diagnostic hints, not replacement targets"));
    assert!(help.contains("candidate 1 (line 1):"));
    assert!(help.contains("candidate 2 (line 7):"));
    assert!(help.contains("#[cfg(test)]"));
    assert!(!help.contains("Exact current text"));
}

#[test]
fn edit_not_found_bounds_similar_location_hints() {
    let text = "target marker\n".repeat(MAX_EDIT_CANDIDATES + 1);
    let help = near_miss_help(&text, "target marker\nmiss");

    assert_eq!(help.matches("candidate ").count(), MAX_EDIT_CANDIDATES);
    assert!(help.contains("1 additional similar location omitted"));
}

#[test]
fn edit_not_found_without_a_similar_line_keeps_reread_guidance() {
    let help = near_miss_help("actual content\n", "expected content\n");

    assert!(help.contains("No similar line found"));
    assert!(help.contains("re-read the relevant range"));
}

#[tokio::test]
async fn edit_target_line_selects_one_exact_occurrence() {
    let dir = std::env::temp_dir().join(format!(
        "tcode-edit-target-line-exact-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("text.txt");
    std::fs::write(&file, "header\nneedle\nseparator\nneedle\n").unwrap();
    let ctx = ToolCtx::new(dir.clone(), 10_000);

    let out = EditTool
        .run(
            json!({
                "path": "text.txt",
                "old_string": "needle",
                "new_string": "changed",
                "target_line": 4,
            }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;

    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "header\nneedle\nseparator\nchanged\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn edit_target_line_selects_one_normalized_occurrence() {
    let dir = std::env::temp_dir().join(format!(
        "tcode-edit-target-line-normalized-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("text.txt");
    std::fs::write(&file, "call - x;\nseparator\ncall - x;\n").unwrap();
    let ctx = ToolCtx::new(dir.clone(), 10_000);

    let out = EditTool
        .run(
            json!({
                "path": "text.txt",
                "old_string": "call – x;",
                "new_string": "changed();",
                "target_line": 3,
            }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;

    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "call - x;\nseparator\nchanged();\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn edit_target_line_rejects_a_nonmatching_line_without_mutating() {
    let dir = std::env::temp_dir().join(format!(
        "tcode-edit-target-line-miss-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("text.txt");
    let original = "needle\nseparator\nneedle\n";
    std::fs::write(&file, original).unwrap();
    let ctx = ToolCtx::new(dir.clone(), 10_000);

    let out = EditTool
        .run(
            json!({
                "path": "text.txt",
                "old_string": "needle",
                "new_string": "changed",
                "target_line": 2,
            }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;

    assert!(out.is_error);
    assert!(out.content.contains("does not contain an exact old_string"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn edit_target_line_rejects_replace_all_without_mutating() {
    let dir = std::env::temp_dir().join(format!(
        "tcode-edit-target-line-replace-all-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("text.txt");
    let original = "needle\nneedle\n";
    std::fs::write(&file, original).unwrap();
    let ctx = ToolCtx::new(dir.clone(), 10_000);

    let out = EditTool
        .run(
            json!({
                "path": "text.txt",
                "old_string": "needle",
                "new_string": "changed",
                "target_line": 1,
                "replace_all": true,
            }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;

    assert!(out.is_error);
    assert_eq!(
        out.content,
        "target_line cannot be combined with replace_all=true"
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn edit_result_snippet_is_anchored_at_the_replacement() {
    let dir = std::env::temp_dir().join(format!("tcode-edit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("many.rs");
    // The needle sits deep in the file, and an identical-looking `new`
    // string also occurs earlier — a naive `find(new)` would report the
    // wrong region.
    let mut body = String::from("target\n");
    for i in 1..=200 {
        body.push_str(&format!("line {i}\n"));
    }
    std::fs::write(&file, &body).unwrap();

    let ctx = ToolCtx::new(dir.clone(), 10_000);
    let out = EditTool
        .run(
            json!({
                "path": file.to_str().unwrap(),
                "old_string": "line 150",
                "new_string": "target",
            }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;

    assert!(!out.is_error, "{}", out.content);
    // Anchored at line 151 (the file's line 1 is "target"), not line 1.
    assert!(out.content.contains("   151\ttarget"), "{}", out.content);
    assert!(out.content.contains("   148\tline 147"), "{}", out.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn edit_does_not_mark_unshown_lines_as_read() {
    let dir = std::env::temp_dir().join(format!("tcode-edit-freshness-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("many.txt");
    let body = (1..=300)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    std::fs::write(&file, body).unwrap();
    let ctx = ToolCtx::new(dir.clone(), 10_000);

    let edited = EditTool
        .run(
            json!({
                "path": "many.txt",
                "old_string": "line 1\n",
                "new_string": "changed 1\n",
            }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;
    assert!(!edited.is_error, "{}", edited.content);

    let unseen = ReadTool
        .run(
            json!({ "path": "many.txt", "offset": 200, "limit": 120 }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;
    assert!(!unseen.is_error, "{}", unseen.content);
    assert!(unseen.content.contains("line 200"), "{}", unseen.content);
    assert!(
        !unseen.content.starts_with("unchanged:"),
        "an edit snippet must not make distant lines fresh: {}",
        unseen.content
    );

    let repeated = ReadTool
        .run(
            json!({ "path": "many.txt", "offset": 200, "limit": 120 }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;
    assert!(repeated.content.starts_with("unchanged:"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn capped_read_does_not_mark_unemitted_tail_as_seen() {
    let dir = std::env::temp_dir().join(format!("tcode-read-cap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("long.txt");
    let body = (1..=600)
        .map(|line| format!("line {line}: {}\n", "x".repeat(600)))
        .collect::<String>();
    std::fs::write(&file, body).unwrap();
    let ctx = ToolCtx::new(dir.clone(), 10_000);

    let first = ReadTool
        .run(
            json!({ "path": "long.txt" }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;
    assert!(!first.is_error, "{}", first.content);
    assert!(
        first.content.contains("[showing lines"),
        "{}",
        first.content
    );

    let tail = ReadTool
        .run(
            json!({ "path": "long.txt", "offset": 500, "limit": 120 }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;
    assert!(!tail.is_error, "{}", tail.content);
    assert!(tail.content.contains("line 500:"), "{}", tail.content);
    assert!(
        !tail.content.starts_with("unchanged:"),
        "the capped tail was never emitted: {}",
        tail.content
    );

    let repeated_tail = ReadTool
        .run(
            json!({ "path": "long.txt", "offset": 500, "limit": 120 }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;
    assert!(repeated_tail.content.starts_with("unchanged:"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn long_lines_clip_on_char_boundaries() {
    // Multi-byte chars: clipping by byte index would panic or corrupt.
    let line = "の".repeat(MAX_LINE_CHARS + 10);
    let clipped = clip(&line);
    assert_eq!(clipped.chars().take(MAX_LINE_CHARS).count(), MAX_LINE_CHARS);
    assert!(clipped.starts_with(&"の".repeat(MAX_LINE_CHARS)));
    // The marker declares itself and says how much is missing.
    assert!(
        clipped.ends_with(&format!("\u{2026}[+{} chars]", 10)),
        "{clipped}"
    );
    // A line at the limit is passed through untouched, without allocating.
    let short = "の".repeat(4);
    assert!(matches!(clip(&short), std::borrow::Cow::Borrowed(_)));
}

/// The reported friction: a 6-line file with one 2000-char line came back
/// truncated, and the model had to shell out for the real text. The byte
/// budget, not a per-line rule an order of magnitude below it, is the
/// constraint that matters.
#[tokio::test]
async fn ordinary_long_lines_are_returned_whole() {
    let dir = std::env::temp_dir().join(format!("tcode-wide-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let wide = "x".repeat(2000);
    std::fs::write(dir.join("notes.md"), format!("a\nb\n{wide}\nd\ne\nf\n")).unwrap();
    let ctx = ToolCtx::new(dir.clone(), 10_000);

    let out = ReadTool
        .run(
            json!({ "path": "notes.md" }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains(&wide), "the long line was clipped");
    assert!(!out.content.contains("clipped at"), "{}", out.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn oversized_lines_are_clipped_with_a_self_healing_note() {
    let dir = std::env::temp_dir().join(format!("tcode-minified-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let huge = "y".repeat(MAX_LINE_CHARS + 500);
    std::fs::write(dir.join("bundle.js"), format!("ok\n{huge}\nend\n")).unwrap();
    let ctx = ToolCtx::new(dir.clone(), 10_000);

    let out = ReadTool
        .run(
            json!({ "path": "bundle.js" }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains(&format!("\u{2026}[+{} chars]", 500)),
        "{}",
        out.content
    );
    assert!(
        out.content.contains(&format!(
            "note: line 2 was clipped at {MAX_LINE_CHARS} of {} chars",
            MAX_LINE_CHARS + 500
        )),
        "{}",
        out.content
    );
    assert!(out.content.contains("grep"), "{}", out.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn read_redacts_credentials_and_write_refuses_the_placeholder() {
    let dir = std::env::temp_dir().join(format!("tcode-redact-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let secret = "sk-ant-api03-zzzzzzzzzzzzzzzz";
    let original = format!(
        "[profile.deepseek]\nmodel = \"deepseek-chat\"\napi_key = \"{secret}\"\napi_key_env = \"DEEPSEEK_API_KEY\"\n"
    );
    std::fs::write(dir.join("config.toml"), &original).unwrap();
    let ctx = ToolCtx::new(dir.clone(), 10_000);

    let out = ReadTool
        .run(
            json!({ "path": "config.toml" }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        !out.content.contains(secret),
        "the key leaked: {}",
        out.content
    );
    assert!(out.content.contains("redacted"), "{}", out.content);
    // The env-var pointer is information, not a secret: it must survive.
    assert!(out.content.contains("DEEPSEEK_API_KEY"), "{}", out.content);

    // Round-tripping what read returned would write the placeholder into the
    // file. The freshness gate is satisfied here (full file seen), so this
    // check is the only thing standing between the model and a broken config.
    // `read` returns content verbatim, so this is a literal round-trip.
    let rewritten = out.content.clone();
    let write = WriteTool
        .run(
            json!({ "path": "config.toml", "content": rewritten }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;
    assert!(write.is_error, "{}", write.content);
    assert!(
        write.content.contains("redaction marker"),
        "{}",
        write.content
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("config.toml")).unwrap(),
        original,
        "the file must be untouched"
    );

    // Same for a targeted edit whose old_string carries the placeholder.
    let edit = EditTool
        .run(
            json!({
                "path": "config.toml",
                "old_string": format!("api_key = \"[redacted: {} chars, starts \"sk-a\"]\"", 29),
                "new_string": "api_key = \"replaced\"",
            }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;
    assert!(edit.is_error, "{}", edit.content);
    assert!(
        edit.content.contains("redaction marker"),
        "{}",
        edit.content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn detect_image_mime_by_magic_bytes() {
    assert_eq!(
        detect_image_mime(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
        Some("image/png")
    );
    assert_eq!(
        detect_image_mime(&[0xFF, 0xD8, 0xFF, 0x00]),
        Some("image/jpeg")
    );
    assert_eq!(detect_image_mime(b"GIF89a....."), Some("image/gif"));
    let mut webp = b"RIFF".to_vec();
    webp.extend_from_slice(&[0, 0, 0, 0]);
    webp.extend_from_slice(b"WEBP");
    assert_eq!(detect_image_mime(&webp), Some("image/webp"));
    // Plain text is not an image even though it starts with printable bytes.
    assert_eq!(detect_image_mime(b"#!/bin/sh\n"), None);
}

#[tokio::test]
async fn text_only_models_are_routed_to_view_image_without_freshness() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("shot.png");
    std::fs::write(
        &image,
        tcode_core::images::normalize_rgba(1, 1, vec![0; 4])
            .unwrap()
            .bytes,
    )
    .unwrap();
    let ctx = ToolCtx::new(dir.path().to_path_buf(), 10_000).with_model(text_only_model());

    for _ in 0..2 {
        let result = ReadTool
            .run(
                json!({ "path": "shot.png" }),
                &ctx,
                &CancellationToken::new(),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("view_image"));
    }
}

#[tokio::test]
async fn read_inlines_a_png_as_an_image_block() {
    let dir = std::env::temp_dir().join(format!("tcode-img-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("shot.png");
    // A complete 1×1 PNG: normalization decodes images before inlining.
    let bytes = tcode_core::images::normalize_rgba(1, 1, vec![0; 4])
        .unwrap()
        .bytes;
    std::fs::write(&png, &bytes).unwrap();

    let ctx = ToolCtx::new(dir.clone(), 10_000);
    let out = ReadTool
        .run(
            json!({ "path": png.to_str().unwrap() }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;

    assert!(!out.is_error);
    assert!(out.content.contains("Read image"));
    assert_eq!(out.images.len(), 1);
    assert!(matches!(
        &out.images[0],
        tcode_core::ContentBlock::Image { media_type, .. } if media_type == "image/png"
    ));

    // A second read of the unchanged image dedupes: no image re-sent.
    let again = ReadTool
        .run(
            json!({ "path": png.to_str().unwrap() }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;
    assert!(again.images.is_empty());
    assert!(again.content.contains("unchanged"));

    let _ = std::fs::remove_dir_all(&dir);
}

fn append_test_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tcode-append-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn run_append(ctx: &ToolCtx, path: &str, content: &str) -> ToolOutput {
    AppendTool
        .run(
            json!({ "path": path, "content": content }),
            ctx,
            &CancellationToken::new(),
        )
        .await
}

async fn run_read(ctx: &ToolCtx, input: Value) -> ToolOutput {
    ReadTool.run(input, ctx, &CancellationToken::new()).await
}

/// `read` emits file content verbatim: no line-number gutter, so the bytes it
/// returns are the bytes on disk. An `edit` built from what the model saw must
/// match, which is exactly what a reintroduced gutter would silently break.
#[tokio::test]
async fn read_returns_content_verbatim_without_a_line_number_gutter() {
    let dir = std::env::temp_dir().join(format!("tcode-verbatim-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let body = "alpha\n    indented\nbeta\n";
    std::fs::write(dir.join("f.txt"), body).unwrap();
    let ctx = ToolCtx::new(dir.clone(), 10_000);

    let out = run_read(&ctx, json!({ "path": "f.txt" })).await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content, body, "read must not reshape file content");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Without a gutter, an offset read's footer is the only thing that says which
/// lines arrived — so it must appear even when the window reaches EOF.
#[tokio::test]
async fn offset_read_reports_its_line_range_even_when_it_reaches_the_end() {
    let dir = std::env::temp_dir().join(format!("tcode-range-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let body: String = (1..=200).map(|n| format!("line {n}\n")).collect();
    std::fs::write(dir.join("f.txt"), &body).unwrap();
    let ctx = ToolCtx::new(dir.clone(), 10_000);

    let out = run_read(&ctx, json!({ "path": "f.txt", "offset": 150 })).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("[showing lines 150-200 of 200]"),
        "offset read must state its range: {}",
        out.content
    );
    // A whole-file read starts at line 1; saying so would be noise.
    let ctx2 = ToolCtx::new(dir.clone(), 10_000);
    let whole = run_read(&ctx2, json!({ "path": "f.txt" })).await;
    assert!(
        !whole.content.contains("[showing lines"),
        "whole-file read needs no range footer: {}",
        whole.content
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn append_creates_missing_file_with_parents_and_notes_it() {
    let dir = append_test_dir("create");
    let ctx = ToolCtx::new(dir.clone(), 10_000);

    let out = run_append(&ctx, "sub/new.txt", "one\ntwo\n").await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("created new file"), "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(dir.join("sub/new.txt")).unwrap(),
        "one\ntwo\n"
    );

    // Fully model-authored: a follow-up read dedupes to a stub.
    let read = run_read(&ctx, json!({ "path": "sub/new.txt" })).await;
    assert!(read.content.starts_with("unchanged:"), "{}", read.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn append_adds_exact_bytes_without_auto_newline() {
    let dir = append_test_dir("bytes");
    let ctx = ToolCtx::new(dir.clone(), 10_000);

    let file = dir.join("log.txt");
    std::fs::write(&file, "line\n").unwrap();
    run_read(&ctx, json!({ "path": "log.txt" })).await;
    let out = run_append(&ctx, "log.txt", "tail").await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "line\ntail");
    assert!(!out.content.contains("did not end with a newline"));

    // No trailing newline: the appendix continues the last line.
    let out = run_append(&ctx, "log.txt", "-more\n").await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "line\ntail-more\n");
    assert!(
        out.content.contains("did not end with a newline"),
        "{}",
        out.content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn append_after_partial_read_succeeds_but_write_stays_gated() {
    let dir = append_test_dir("partial");
    let ctx = ToolCtx::new(dir.clone(), 10_000);
    let file = dir.join("big.txt");
    let body = (1..=300)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    std::fs::write(&file, &body).unwrap();

    let read = run_read(&ctx, json!({ "path": "big.txt", "offset": 1, "limit": 10 })).await;
    assert!(!read.is_error, "{}", read.content);

    let out = run_append(&ctx, "big.txt", "line 301\n").await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .ends_with("line 301\n"),
        "appendix must land at the end"
    );
    assert!(out.content.contains("line 301"), "{}", out.content);

    // The partial view must not let a whole-file overwrite through.
    let write = WriteTool
        .run(
            json!({ "path": "big.txt", "content": "gone" }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;
    assert!(write.is_error, "{}", write.content);
    assert!(
        write.content.contains("only seen lines"),
        "{}",
        write.content
    );
    assert!(std::fs::read_to_string(&file)
        .unwrap()
        .ends_with("line 301\n"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn append_refuses_without_prior_read() {
    let dir = append_test_dir("unread");
    let ctx = ToolCtx::new(dir.clone(), 10_000);
    let file = dir.join("seen.txt");
    std::fs::write(&file, "original\n").unwrap();

    let out = run_append(&ctx, "seen.txt", "extra\n").await;
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("have not read its current version"),
        "{}",
        out.content
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "original\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn append_refuses_when_file_changed_on_disk() {
    let dir = append_test_dir("stale");
    let ctx = ToolCtx::new(dir.clone(), 10_000);
    let file = dir.join("watched.txt");
    std::fs::write(&file, "original\n").unwrap();
    run_read(&ctx, json!({ "path": "watched.txt" })).await;

    std::fs::write(&file, "external change\n").unwrap();
    let out = run_append(&ctx, "watched.txt", "extra\n").await;
    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("changed on disk"), "{}", out.content);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "external change\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn append_rejects_empty_content_and_non_utf8_files() {
    let dir = append_test_dir("reject");
    let ctx = ToolCtx::new(dir.clone(), 10_000);

    let out = run_append(&ctx, "any.txt", "").await;
    assert!(out.is_error);
    assert!(out.content.contains("content must not be empty"));

    let bin = dir.join("data.bin");
    let original = b"before\xffafter";
    std::fs::write(&bin, original).unwrap();
    let out = run_append(&ctx, "data.bin", "tail").await;
    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("not valid UTF-8"), "{}", out.content);
    assert_eq!(std::fs::read(&bin).unwrap(), original);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn append_after_full_read_lets_write_overwrite() {
    let dir = append_test_dir("full");
    let ctx = ToolCtx::new(dir.clone(), 10_000);
    let file = dir.join("small.txt");
    std::fs::write(&file, "a\nb\n").unwrap();
    run_read(&ctx, json!({ "path": "small.txt" })).await;

    let out = run_append(&ctx, "small.txt", "c\n").await;
    assert!(!out.is_error, "{}", out.content);

    // Full sight carried forward: overwrite is allowed.
    let write = WriteTool
        .run(
            json!({ "path": "small.txt", "content": "rewritten\n" }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;
    assert!(!write.is_error, "{}", write.content);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "rewritten\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn read_then_edit_then_append_flow_works() {
    let dir = append_test_dir("flow");
    let ctx = ToolCtx::new(dir.clone(), 10_000);
    let file = dir.join("flow.txt");
    let body = (1..=100)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    std::fs::write(&file, &body).unwrap();

    let read = run_read(
        &ctx,
        json!({ "path": "flow.txt", "offset": 1, "limit": 20 }),
    )
    .await;
    assert!(!read.is_error, "{}", read.content);

    let edited = EditTool
        .run(
            json!({
                "path": "flow.txt",
                "old_string": "line 5\n",
                "new_string": "changed 5\n",
            }),
            &ctx,
            &CancellationToken::new(),
        )
        .await;
    assert!(!edited.is_error, "{}", edited.content);

    // Our own edit must not read as an external change.
    let out = run_append(&ctx, "flow.txt", "line 101\n").await;
    assert!(!out.is_error, "{}", out.content);
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains("changed 5\n"));
    assert!(text.ends_with("line 101\n"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn append_does_not_mark_unseen_middle_as_read() {
    let dir = append_test_dir("middle");
    let ctx = ToolCtx::new(dir.clone(), 10_000);
    let file = dir.join("many.txt");
    let body = (1..=300)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    std::fs::write(&file, &body).unwrap();

    run_read(
        &ctx,
        json!({ "path": "many.txt", "offset": 1, "limit": 10 }),
    )
    .await;
    let out = run_append(&ctx, "many.txt", "line 301\n").await;
    assert!(!out.is_error, "{}", out.content);

    // The never-seen middle still returns content, not a stub.
    let middle = run_read(
        &ctx,
        json!({ "path": "many.txt", "offset": 150, "limit": 10 }),
    )
    .await;
    assert!(middle.content.contains("line 150"), "{}", middle.content);
    assert!(
        !middle.content.starts_with("unchanged:"),
        "{}",
        middle.content
    );

    // The appended tail was echoed already: re-reading it dedupes.
    let tail = run_read(
        &ctx,
        json!({ "path": "many.txt", "offset": 299, "limit": 3 }),
    )
    .await;
    assert!(tail.content.starts_with("unchanged:"), "{}", tail.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn append_snippet_is_anchored_at_the_append_point() {
    let dir = append_test_dir("snippet");
    let ctx = ToolCtx::new(dir.clone(), 10_000);
    let file = dir.join("anchor.txt");
    let body = (1..=50)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    std::fs::write(&file, &body).unwrap();
    run_read(&ctx, json!({ "path": "anchor.txt" })).await;

    let out = run_append(&ctx, "anchor.txt", "line 51\n").await;
    assert!(!out.is_error, "{}", out.content);
    // Snippet starts at most 3 lines before the appendix (line 48)...
    assert!(out.content.contains("    48\tline 48"), "{}", out.content);
    // ...and does not replay the whole file.
    assert!(!out.content.contains("line 1\n"), "{}", out.content);
    assert!(out.content.contains("    51\tline 51"), "{}", out.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn append_permission_is_an_edit() {
    let request = AppendTool.permission(&json!({ "path": "a.txt", "content": "x" }));
    let PermissionRequest::Ask {
        descriptor,
        is_edit,
        ..
    } = request
    else {
        panic!("append must ask for permission");
    };
    assert_eq!(descriptor, "append(a.txt)");
    assert!(is_edit, "append must auto-allow in accept-edits mode");
}
