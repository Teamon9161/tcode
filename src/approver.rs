use async_trait::async_trait;
use std::io::Write as _;

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
    async fn ask(&self, _tool: &str, summary: &str, descriptor: &str) -> Approval {
        println!("\n{YELLOW}●{RESET} {BOLD}{summary}{RESET}");
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
