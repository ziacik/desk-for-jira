//! Persistent, revision-scoped progress for the PR review workspace.
//! AI clearing a file is distinct from a deliberate human checkbox override.
use std::{collections::HashMap, env, fs, io::ErrorKind, path::{Path, PathBuf}};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Verdict {
    Routine,
    Review,
    Risk,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assessment {
    pub verdict: Verdict,
    pub reason: String,
    #[serde(default)]
    pub impact: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileProgress {
    #[serde(default)]
    pub ai: Option<Assessment>,
    /// None follows the AI decision, Some(true/false) is an explicit human override.
    #[serde(default)]
    pub human_checked: Option<bool>,
}

impl FileProgress {
    pub fn checked(&self) -> bool {
        self.human_checked.unwrap_or_else(|| {
            self.ai.as_ref().is_some_and(|ai| ai.verdict == Verdict::Routine)
        })
    }

    pub fn ai_cleared(&self) -> bool {
        self.human_checked.is_none()
            && self.ai.as_ref().is_some_and(|ai| ai.verdict == Verdict::Routine)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewProgress {
    #[serde(default)]
    pub files: HashMap<String, FileProgress>,
}

impl ReviewProgress {
    pub fn entry(&self, file: &str) -> FileProgress {
        self.files.get(file).cloned().unwrap_or_default()
    }

    pub fn set_human_checked(&mut self, file: &str, checked: bool) {
        self.files.entry(file.to_owned()).or_default().human_checked = Some(checked);
    }

    pub fn set_ai(&mut self, file: &str, assessment: Assessment) {
        self.files.entry(file.to_owned()).or_default().ai = Some(assessment);
    }

    pub fn checked_count<'a>(&self, paths: impl IntoIterator<Item = &'a String>) -> usize {
        paths.into_iter().filter(|path| self.entry(path).checked()).count()
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    by_revision: HashMap<String, ReviewProgress>,
}

/// Scope by Git directory, PR number and immutable source commit. New revisions
/// start with new decisions instead of silently reusing approvals for old code.
pub fn revision_key(repository: &Path, number: u32, head_sha: &str) -> String {
    format!("{}\n{}\n{}", repository.display(), number, head_sha)
}

fn storage_path() -> Result<PathBuf> {
    let base = env::var_os("XDG_STATE_HOME").filter(|s| !s.is_empty()).map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .context("No user state directory")?;
    Ok(base.join("desk-for-jira").join("pr-reviews.json"))
}

fn read_store(path: &Path) -> Result<Store> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("Corrupt PR review state: {}", path.display())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Store::default()),
        Err(error) => Err(error).with_context(|| format!("Cannot read {}", path.display())),
    }
}

fn write_store(path: &Path, state: &Store) -> Result<()> {
    let parent = path.parent().context("Missing state directory")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!("pr-reviews-{}.tmp", std::process::id()));
    fs::write(&temp, serde_json::to_vec_pretty(state)?)?;
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error).with_context(|| format!("Cannot persist PR review state at {}", path.display()));
    }
    Ok(())
}

pub fn load(key: &str) -> Result<ReviewProgress> {
    Ok(read_store(&storage_path()?)?.by_revision.remove(key).unwrap_or_default())
}

pub fn save(key: &str, progress: &ReviewProgress) -> Result<()> {
    if key.is_empty() { bail!("Missing PR review revision key"); }
    let path = storage_path()?;
    let mut store = read_store(&path)?;
    store.by_revision.insert(key.to_owned(), progress.clone());
    write_store(&path, &store)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_cleared_and_manual_review_are_separate() {
        let mut state = ReviewProgress::default();
        state.set_ai("model.ts", Assessment {
            verdict: Verdict::Routine, reason: "Generated metadata".into(), impact: String::new(),
        });
        assert!(state.entry("model.ts").checked());
        assert!(state.entry("model.ts").ai_cleared());
        state.set_human_checked("model.ts", false);
        assert!(!state.entry("model.ts").checked());
        assert!(!state.entry("model.ts").ai_cleared());
        state.set_human_checked("model.ts", true);
        assert!(state.entry("model.ts").checked());
        assert!(!state.entry("model.ts").ai_cleared());
        state.set_ai("model.ts", Assessment {
            verdict: Verdict::Risk, reason: "New risk".into(), impact: "UI".into(),
        });
        assert!(state.entry("model.ts").checked()); // The human decision remains explicit.
    }

    #[test]
    fn persists_per_exact_revision_and_preserves_150_files() {
        let root = std::env::temp_dir().join(format!(
            "desk-for-jira-review-progress-{}", std::process::id(),
        ));
        let path = root.join("progress.json");
        let first = revision_key(Path::new("/tmp/repo"), 42, "111");
        let second = revision_key(Path::new("/tmp/repo"), 42, "222");
        assert_ne!(first, second);
        let mut progress = ReviewProgress::default();
        for index in 0..150 {
            progress.set_human_checked(&format!("path/{index}.ts"), index % 3 == 0);
        }
        let mut store = Store::default();
        store.by_revision.insert(first.clone(), progress.clone());
        write_store(&path, &store).unwrap();
        let restored = read_store(&path).unwrap();
        assert_eq!(restored.by_revision[&first], progress);
        assert!(!restored.by_revision.contains_key(&second));
        fs::remove_dir_all(root).unwrap();
    }
}
