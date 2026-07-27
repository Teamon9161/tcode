//! Durable draft plans submitted for approval.
//!
//! A plan is written before its review reaches the user. Keeping the active
//! path in the session lets a rejected plan's next version replace the same
//! human-readable file instead of creating an unhelpful trail of near-duplicates.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::store;

/// Internal field attached only to the approval/execution copy of an
/// `exit_plan` input. It never changes the model-issued tool call in the
/// ledger, but lets frontends show and edit the durable draft.
pub const PLAN_PATH_FIELD: &str = "_tcode_plan_path";

#[derive(Default)]
pub struct PlanDraft {
    path: Option<PathBuf>,
}

impl PlanDraft {
    /// Save this session's submitted plan. The first submission gets a
    /// timestamped, title-derived filename; later submissions while planning
    /// replace that file.
    pub async fn save(&mut self, cwd: &Path, title: &str, plan: &str) -> Result<PathBuf, String> {
        let path = match &self.path {
            Some(path) => path.clone(),
            None => {
                let dir = store::plans_dir(cwd);
                tokio::fs::create_dir_all(&dir).await.map_err(|e| {
                    format!("could not create plan directory {}: {e}", dir.display())
                })?;
                let path = next_plan_path(&dir, title).await?;
                self.path = Some(path.clone());
                path
            }
        };
        tokio::fs::write(&path, plan)
            .await
            .map_err(|e| format!("could not save plan draft {}: {e}", path.display()))?;
        Ok(path)
    }

    /// The file assigned to the currently reviewed plan, if it has one.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// An approved plan completes this planning cycle. A later `/plan` cycle
    /// therefore starts a new titled file.
    pub fn clear(&mut self) {
        self.path = None;
    }
}

/// Add the durable path to an approval-only input copy.
pub fn with_plan_path(input: &Value, path: &Path) -> Value {
    let mut input = input.clone();
    input[PLAN_PATH_FIELD] = Value::String(path.display().to_string());
    input
}

/// Extract a plan path only if it names a direct child of this project's plan
/// directory. Tool input is model-authored, so it must not turn this internal
/// field into an arbitrary-file write capability.
pub fn plan_path_in(input: &Value, cwd: &Path) -> Option<PathBuf> {
    let raw = input[PLAN_PATH_FIELD].as_str()?;
    let dir = store::plans_dir(cwd);
    let relative = Path::new(raw).strip_prefix(&dir).ok()?;
    if relative.components().count() != 1 || relative.extension().is_none_or(|ext| ext != "md") {
        return None;
    }
    Some(dir.join(relative))
}

async fn next_plan_path(dir: &Path, title: &str) -> Result<PathBuf, String> {
    let stem = format!("{}-{}", timestamp(), slug(title));
    for suffix in 0..10_000 {
        let name = if suffix == 0 {
            format!("{stem}.md")
        } else {
            format!("{stem}-{suffix}.md")
        };
        let path = dir.join(name);
        if tokio::fs::try_exists(&path)
            .await
            .map_err(|e| format!("could not inspect plan path {}: {e}", path.display()))?
        {
            continue;
        }
        return Ok(path);
    }
    Err(format!(
        "could not allocate a plan filename in {}",
        dir.display()
    ))
}

/// `yyyymmdd-HHMMSS` in UTC, without pulling in a date crate (civil-from-days).
fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86400;
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}{mo:02}{d:02}-{h:02}{m:02}{s:02}")
}

/// Filesystem-safe short slug from the plan's title.
fn slug(title: &str) -> String {
    let mut slug: String = title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug = slug.trim_matches('-');
    let slug: String = slug.chars().take(40).collect();
    if slug.is_empty() {
        "plan".to_string()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_accepts_direct_plan_files() {
        let root = tempfile::tempdir().unwrap();
        crate::home::testing::temp_home();
        let cwd = root.path();
        let dir = store::plans_dir(cwd);
        let valid = dir.join("draft.md");
        assert_eq!(
            plan_path_in(&with_plan_path(&Value::Null, &valid), cwd),
            Some(valid)
        );
        assert!(plan_path_in(
            &serde_json::json!({ PLAN_PATH_FIELD: "/tmp/outside.md" }),
            cwd
        )
        .is_none());
        assert!(plan_path_in(
            &serde_json::json!({ PLAN_PATH_FIELD: dir.join("nested/draft.md") }),
            cwd
        )
        .is_none());
    }

    #[tokio::test]
    async fn replaces_a_rejected_plan_in_its_original_file() {
        crate::home::testing::temp_home();
        let cwd = tempfile::tempdir().unwrap();
        let mut draft = PlanDraft::default();
        let first = draft
            .save(cwd.path(), "Refactor ledger", "first")
            .await
            .unwrap();
        let second = draft
            .save(cwd.path(), "A different title", "second")
            .await
            .unwrap();
        assert_eq!(first, second);
        assert!(first
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("refactor-ledger"));
        assert_eq!(tokio::fs::read_to_string(first).await.unwrap(), "second");
    }
}
