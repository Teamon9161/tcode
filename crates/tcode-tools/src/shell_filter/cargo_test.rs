//! The one filter that cannot be written as regexes.
//!
//! A successful `cargo test` run prints one block per target, and most of
//! those blocks are the same three lines about zero tests. Folding them needs
//! to pair each `test result: ok.` with the `running N tests` line that
//! preceded it — a state machine over the output, not a line predicate. It
//! lives in the same chain as the declarative filters, as a plain
//! `OutputFilter`.

use super::OutputFilter;

/// Where flags that make the output itself the point live. Asking for
/// per-test output and then folding it away would be worse than not filtering.
const VERBOSE_FLAGS: [&str; 2] = ["--nocapture", "--show-output"];

#[derive(Debug)]
pub struct CargoTestFilter;

impl OutputFilter for CargoTestFilter {
    fn name(&self) -> &str {
        "cargo-test"
    }

    /// Gated on the shape of the output, not on the command matching
    /// `^cargo test`: real commands are compound (`cd crates/x && cargo test
    /// -p y`, `just test`), and an anchored pattern would quietly stop
    /// applying. Output that says "test result: ok." came from a test runner
    /// whatever the command was.
    fn apply(&self, command: &str, output: &str) -> Option<String> {
        if VERBOSE_FLAGS.iter().any(|flag| command.contains(flag)) {
            return None;
        }
        compact_successful_test_output(output)
    }
}

/// Successful test runs often contain several nearly-identical target blocks
/// (especially doctests and crates with zero tests). Keep one result for every
/// target that actually ran tests, while avoiding needless context use. Any
/// error-like marker leaves the original output untouched for diagnosis.
fn compact_successful_test_output(output: &str) -> Option<String> {
    if !(output.contains("test result: ok.")
        && output.contains("running ")
        && !output.contains("test result: FAILED")
        && !output.contains("error:")
        && !output.contains("failures:"))
    {
        return None;
    }

    let lines: Vec<&str> = output.lines().collect();
    let mut passed = Vec::new();
    let mut last_result = None;
    for (index, result) in lines.iter().enumerate() {
        if !result.trim_start().starts_with("test result: ok.") || result.contains("0 passed") {
            continue;
        }
        let running = lines[..index]
            .iter()
            .rev()
            .find(|line| {
                let trimmed = line.trim_start();
                trimmed.starts_with("running ")
                    && trimmed.contains(" tests")
                    && !trimmed.contains("running 0 tests")
            })
            .copied();
        if let Some(running) = running {
            passed.push(format!("{running}\n{result}"));
            last_result = Some(index);
        }
    }
    let last_result = last_result?;

    // A compound verification command often prints its workspace summary only
    // after Cargo's final target. Keep that non-empty tail alongside the test
    // evidence instead of making the model reopen the full spill file.
    let tail = lines[last_result + 1..].to_vec().join("\n");
    if !tail.trim().is_empty() {
        passed.push(tail);
    }

    Some(format!(
        "{}\n… successful test output folded …",
        passed.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_TARGETS: &str = "\
   Compiling tcode-core v0.1.8
running 2 tests
..
test result: ok. 2 passed; 0 failed; 0 ignored

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored

running 3 tests
...
test result: ok. 3 passed; 0 failed; 0 ignored";

    #[test]
    fn every_target_that_ran_tests_keeps_its_own_result() {
        let folded = CargoTestFilter
            .apply("cargo test --workspace", TWO_TARGETS)
            .unwrap();
        assert_eq!(
            folded,
            "running 2 tests\ntest result: ok. 2 passed; 0 failed; 0 ignored\n\
             running 3 tests\ntest result: ok. 3 passed; 0 failed; 0 ignored\n\
             … successful test output folded …"
        );
    }

    #[test]
    fn a_compound_command_still_folds() {
        assert!(CargoTestFilter
            .apply("cd crates/tcode-core && cargo test", TWO_TARGETS)
            .is_some());
    }

    #[test]
    fn compound_verification_keeps_the_workspace_summary_after_tests() {
        let output = format!(
            "{TWO_TARGETS}\n\n M crates/tcode-tools/src/shell.rs\n crates/tcode-tools/src/shell.rs | 2 +-\n 1 file changed, 1 insertion(+), 1 deletion(-)"
        );
        let folded = CargoTestFilter
            .apply(
                "cargo test && git status --short && git diff --stat",
                &output,
            )
            .unwrap();
        assert!(
            folded.contains(" M crates/tcode-tools/src/shell.rs"),
            "{folded}"
        );
        assert!(folded.contains("1 file changed"), "{folded}");
    }

    #[test]
    fn asking_for_test_output_turns_folding_off() {
        assert!(CargoTestFilter
            .apply("cargo test -- --nocapture", TWO_TARGETS)
            .is_none());
    }

    #[test]
    fn any_failure_marker_leaves_the_output_alone() {
        for output in [
            "running 2 tests\ntest result: FAILED. 1 passed; 1 failed",
            "error: could not compile\nrunning 2 tests\ntest result: ok. 2 passed",
            "running 2 tests\nfailures:\n    some::test\ntest result: ok. 2 passed",
        ] {
            assert!(
                CargoTestFilter.apply("cargo test", output).is_none(),
                "{output}"
            );
        }
    }

    #[test]
    fn a_run_where_no_target_had_tests_is_not_worth_folding() {
        assert!(CargoTestFilter
            .apply(
                "cargo test",
                "running 0 tests\n\ntest result: ok. 0 passed; 0 failed"
            )
            .is_none());
    }
}
