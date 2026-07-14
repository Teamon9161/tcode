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
pub struct LineApprover {
    /// Whether anyone is on the other end of stdin. A one-shot `-p` run is a
    /// conversation of one turn: nobody is waiting to type `y`, so prompting
    /// there blocks on stdin forever (a TTY still exists, so EOF never comes).
    /// Denying is the only honest answer, and it fails closed.
    interactive: bool,
}

impl LineApprover {
    pub fn new(interactive: bool) -> Self {
        Self { interactive }
    }
}

#[async_trait]
impl Approver for LineApprover {
    async fn ask(&self, tool: &str, summary: &str, descriptor: &str, input: &Value) -> Approval {
        if !self.interactive {
            println!("\n{YELLOW}●{RESET} {BOLD}{summary}{RESET}");
            println!("{DIM}  denied — a one-shot run (-p) has nobody to approve it{RESET}");
            return Approval {
                decision: ApprovalDecision::No,
                comment: Some(format!(
                    "Denied: this action needs approval, and a one-shot run has no one to give it. \
                     Re-run interactively, start with a permission mode that covers it \
                     (--mode auto or --mode accept-edits), or add an allow rule for {descriptor}."
                )),
            };
        }
        if tool == "ask_user" {
            return ask_user_plain(summary, input).await;
        }
        println!("\n{YELLOW}●{RESET} {BOLD}{summary}{RESET}");
        print_change_preview(tool, input);
        print!(
            "{DIM}  allow? y / a (always: {descriptor}) / n — append a note after y or n, e.g. \"y but use --dry-run\"{RESET}\n  > "
        );
        let _ = std::io::stdout().flush();
        loop {
            let (n, line) = tokio::task::spawn_blocking(|| {
                let mut s = String::new();
                std::io::stdin().read_line(&mut s).map(|n| (n, s))
            })
            .await
            .unwrap_or(Ok((0, String::new())))
            .unwrap_or((0, String::new()));
            // EOF (n == 0): no human on stdin to approve. Deny rather than
            // approve blindly — a blank Enter (n >= 1) still defaults to Yes.
            if n == 0 {
                println!("{DIM}  no input (EOF) — denied{RESET}");
                return Approval {
                    decision: ApprovalDecision::No,
                    comment: None,
                };
            }
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

struct PlainQuestion {
    question: String,
    options: Vec<String>,
    multi: bool,
}

async fn ask_user_plain(summary: &str, input: &Value) -> Approval {
    let questions = plain_questions(summary, input);
    let mut answers: Vec<(String, String)> = Vec::new();
    for (index, q) in questions.iter().enumerate() {
        println!("\n{YELLOW}?{RESET} {BOLD}{}{RESET}", q.question);
        for (i, option) in q.options.iter().enumerate() {
            println!("{DIM}  {}) {option}{RESET}", i + 1);
        }
        if questions.len() > 1 {
            println!("{DIM}  question {} / {}{RESET}", index + 1, questions.len());
        }
        let answer = loop {
            let prompt = if q.multi {
                "answer (numbers/text; comma-separated for multiple)"
            } else if q.options.is_empty() {
                "answer"
            } else {
                "answer (number or text)"
            };
            print!("{DIM}  {prompt} > {RESET}");
            let _ = std::io::stdout().flush();
            let (n, line) = read_line_blocking().await;
            if n == 0 {
                println!("{DIM}  no input (EOF) — cancelled{RESET}");
                return Approval {
                    decision: ApprovalDecision::No,
                    comment: None,
                };
            }
            let raw = line.trim();
            if raw.is_empty() {
                println!("{DIM}  please enter an answer{RESET}");
                continue;
            }
            break resolve_plain_answer(raw, &q.options, q.multi);
        };
        answers.push((q.question.clone(), answer));
    }
    let comment = if answers.len() == 1 {
        answers.pop().map(|(_, answer)| answer).unwrap_or_default()
    } else {
        answers
            .into_iter()
            .enumerate()
            .map(|(i, (question, answer))| format!("{}. {question} → {answer}", i + 1))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Approval {
        decision: ApprovalDecision::Yes,
        comment: Some(comment),
    }
}

fn plain_questions(summary: &str, input: &Value) -> Vec<PlainQuestion> {
    let raw = input["questions"].as_array().cloned().unwrap_or_else(|| {
        input
            .get("question")
            .map(|_| vec![input.clone()])
            .unwrap_or_default()
    });
    let mut questions: Vec<PlainQuestion> = raw
        .iter()
        .map(|q| PlainQuestion {
            question: q["question"].as_str().unwrap_or(summary).to_string(),
            options: q["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            multi: q["multiSelect"].as_bool().unwrap_or(false),
        })
        .collect();
    if questions.is_empty() {
        questions.push(PlainQuestion {
            question: summary.to_string(),
            options: Vec::new(),
            multi: false,
        });
    }
    questions
}

fn resolve_plain_answer(raw: &str, options: &[String], multi: bool) -> String {
    if options.is_empty() {
        return raw.to_string();
    }
    if multi {
        let parts = raw
            .split(|c: char| c == ',' || c.is_ascii_whitespace())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        if !parts.is_empty() {
            let mut selected = Vec::new();
            for part in parts {
                let Ok(index) = part.parse::<usize>() else {
                    return raw.to_string();
                };
                let Some(option) = index.checked_sub(1).and_then(|i| options.get(i)) else {
                    return raw.to_string();
                };
                selected.push(option.clone());
            }
            return selected.join(", ");
        }
    } else if let Ok(index) = raw.parse::<usize>() {
        if let Some(option) = index.checked_sub(1).and_then(|i| options.get(i)) {
            return option.clone();
        }
    }
    raw.to_string()
}

async fn read_line_blocking() -> (usize, String) {
    tokio::task::spawn_blocking(|| {
        let mut s = String::new();
        std::io::stdin().read_line(&mut s).map(|n| (n, s))
    })
    .await
    .unwrap_or(Ok((0, String::new())))
    .unwrap_or((0, String::new()))
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
