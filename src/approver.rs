use async_trait::async_trait;
use std::io::Write as _;

use serde_json::Value;
use tcode_core::{Approval, ApprovalDecision, Approver};

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

/// Line-based approval prompt. The M2 TUI replaces this with the
/// arrow-key + Tab-annotation dialog; the semantics are already final:
/// yes / yes-always / no, each with an optional free-text note.
pub struct LineApprover;

#[async_trait]
impl Approver for LineApprover {
    async fn ask(&self, tool: &str, summary: &str, descriptor: &str, input: &Value) -> Approval {
        println!("\n{YELLOW}●{RESET} {BOLD}{summary}{RESET}");
        print_change_preview(tool, input);
        print!(
            "{DIM}  allow? y / a (always: {descriptor}) / n — append a note after y or n, e.g. \"y but use --dry-run\"{RESET}\n  > "
        );
        let _ = std::io::stdout().flush();
        loop {
            let line = tokio::task::spawn_blocking(|| {
                let mut s = String::new();
                std::io::stdin().read_line(&mut s).map(|_| s)
            })
            .await
            .unwrap_or_else(|_| Ok(String::new()))
            .unwrap_or_default();
            let line = line.trim();
            let (head, rest) = match line.split_once(char::is_whitespace) {
                Some((h, r)) => (h, Some(r.trim().to_string()).filter(|s| !s.is_empty())),
                None => (line, None),
            };
            let decision = match head.to_lowercase().as_str() {
                "y" | "yes" | "" => ApprovalDecision::Yes,
                "a" | "always" => ApprovalDecision::YesAlways,
                "n" | "no" => ApprovalDecision::No,
                _ => {
                    print!("{DIM}  y / a / n (+ optional note) > {RESET}");
                    let _ = std::io::stdout().flush();
                    continue;
                }
            };
            return Approval {
                decision,
                comment: rest,
            };
        }
    }
}

/// Keep the non-TUI prompt safe too: approval must never be blind just
/// because stdout is not a full-screen terminal.
fn print_change_preview(tool: &str, input: &Value) {
    match tool {
        "edit" => {
            let old = input["old_string"].as_str().unwrap_or("");
            let new = input["new_string"].as_str().unwrap_or("");
            println!("{DIM}  proposed replacement:{RESET}");
            for line in old.lines() {
                println!("{DIM}  - {line}{RESET}");
            }
            for line in new.lines() {
                println!("{DIM}  + {line}{RESET}");
            }
        }
        "write" => {
            if let Some(content) = input["content"].as_str() {
                println!("{DIM}  proposed file content:{RESET}");
                for line in content.lines().take(20) {
                    println!("{DIM}  + {line}{RESET}");
                }
                if content.lines().count() > 20 {
                    println!("{DIM}  … additional lines omitted in line mode{RESET}");
                }
            }
        }
        _ => {}
    }
}
