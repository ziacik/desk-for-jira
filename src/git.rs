//! Local Git operations for ticket worktrees. Never changes the current checkout.
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use anyhow::{Context, Result, bail};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
}

pub struct CreateWorktree<'a> {
    pub repository: &'a Path,
    pub destination: &'a Path,
    pub branch: &'a str,
    pub base: &'a str,
    pub fetch: bool,
}

fn command(repo: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git").arg("-C").arg(repo).args(args).output()
        .with_context(|| format!("Unable to execute git in {}", repo.display()))
}

fn output(repo: &Path, args: &[&str]) -> Result<String> {
    let result = command(repo, args)?;
    if !result.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&result.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

pub fn is_repository(path: &Path) -> bool {
    path.join(".git").exists()
        && output(path, &["rev-parse", "--show-toplevel"]).is_ok()
}

pub fn discover(root: &Path, depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![(root.to_path_buf(), 0_usize)];
    let mut seen = HashSet::new();
    while let Some((dir, level)) = pending.pop() {
        if !seen.insert(dir.clone()) { continue; }
        if is_repository(&dir) {
            found.push(dir);
            continue;
        }
        if level >= depth { continue; }
        let Ok(entries) = fs::read_dir(&dir) else { continue; };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue; };
            if !kind.is_dir() || kind.is_symlink() { continue; }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(name.as_ref(), ".git" | ".cache" | ".local" | "node_modules"
                | "target" | "dist" | "build" | ".venv" | "vendor" | ".next"
                | "Worktrees" | ".worktrees")
            { continue; }
            pending.push((entry.path(), level + 1));
        }
    }
    found.sort();
    found
}

pub fn worktrees(repository: &Path) -> Result<Vec<Worktree>> {
    let raw = output(repository, &["worktree", "list", "--porcelain", "-z"])?;
    let mut trees = Vec::new();
    let mut current: Option<Worktree> = None;
    for field in raw.split('\0') {
        if field.is_empty() {
            if let Some(tree) = current.take() { trees.push(tree); }
        } else if let Some(path) = field.strip_prefix("worktree ") {
            if let Some(tree) = current.take() { trees.push(tree); }
            current = Some(Worktree { path: PathBuf::from(path), branch: None });
        } else if let Some(branch) = field.strip_prefix("branch refs/heads/")
            && let Some(tree) = current.as_mut()
        {
            tree.branch = Some(branch.to_owned());
        }
    }
    if let Some(tree) = current { trees.push(tree); }
    Ok(trees)
}

pub fn branch_matches_issue(branch: &str, key: &str) -> bool {
    let branch = branch.rsplit('/').next().unwrap_or(branch);
    branch.eq_ignore_ascii_case(key)
        || branch.get(..key.len()).is_some_and(|prefix| prefix.eq_ignore_ascii_case(key))
            && branch.as_bytes().get(key.len()) == Some(&b'-')
}

pub fn matching_worktrees(repository: &Path, key: &str) -> Result<Vec<Worktree>> {
    Ok(worktrees(repository)?.into_iter()
        .filter(|tree| tree.branch.as_deref().is_some_and(|b| branch_matches_issue(b, key)))
        .collect())
}

pub fn matching_branches(repository: &Path, key: &str) -> Result<Vec<String>> {
    let names = output(repository, &["for-each-ref", "--format=%(refname:short)", "refs/heads"])?;
    Ok(names.lines().filter(|name| branch_matches_issue(name, key))
        .map(str::to_owned).collect())
}

pub fn available_bases(repository: &Path) -> Result<Vec<String>> {
    let refs = output(repository, &["for-each-ref", "--format=%(refname:short)",
        "refs/heads", "refs/remotes"])?;
    let mut result: Vec<String> = refs.lines()
        .filter(|s| !s.ends_with("/HEAD")).map(str::to_owned).collect();
    result.push("HEAD".to_owned());
    result.sort();
    result.dedup();
    Ok(result)
}

pub fn recommended_base(repository: &Path) -> String {
    if let Ok(reference) = output(repository, &["symbolic-ref", "--quiet", "--short",
        "refs/remotes/origin/HEAD"])
        && !reference.is_empty()
    { return reference; }
    for branch in ["origin/main", "origin/master", "origin/develop", "main", "master", "develop"] {
        if available_bases(repository).is_ok_and(|refs| refs.iter().any(|r| r == branch)) {
            return branch.to_owned();
        }
    }
    "HEAD".to_owned()
}

pub fn create(request: &CreateWorktree<'_>) -> Result<()> {
    if !is_repository(request.repository) { bail!("Not a Git repository"); }
    if !request.destination.is_absolute() { bail!("Worktree destination must be absolute"); }
    if request.destination.exists() { bail!("Worktree directory already exists"); }
    if !command(request.repository, &["check-ref-format", "--branch", request.branch])?
        .status.success()
    { bail!("Invalid branch name"); }

    if request.fetch {
        output(request.repository, &["fetch"])?;
    }
    let base_exists = command(request.repository,
        &["rev-parse", "--verify", "--quiet", &format!("{}^{{commit}}", request.base)])?
        .status.success();
    if !base_exists { bail!("Base ref does not resolve to a commit: {}", request.base); }
    let path = request.destination.to_str().context("Worktree path is not valid UTF-8")?;
    if let Some(parent) = request.destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let exists = command(request.repository,
        &["show-ref", "--verify", "--quiet", &format!("refs/heads/{}", request.branch)])?
        .status.success();
    if exists {
        let occupied = worktrees(request.repository)?.iter()
            .any(|tree| tree.branch.as_deref() == Some(request.branch));
        if occupied { bail!("Branch is already checked out in another worktree"); }
        output(request.repository, &["worktree", "add", "--", path, request.branch])?;
    } else {
        output(request.repository, &["worktree", "add", "-b", request.branch, "--",
            path, request.base])?;
    }
    Ok(())
}

pub fn status(worktree: &Path) -> Result<String> {
    output(worktree, &["status", "--porcelain=v1", "--untracked-files=all"])
}

pub fn run_setup(worktree: &Path, script: &str) -> Result<String> {
    if script.trim().is_empty() { return Ok(String::new()); }
    let result = Command::new("sh").arg("-c").arg(script).current_dir(worktree)
        .output().with_context(|| format!("Unable to run setup in {}", worktree.display()))?;
    let mut log = String::from_utf8_lossy(&result.stdout).into_owned();
    log.push_str(&String::from_utf8_lossy(&result.stderr));
    if !result.status.success() {
        let end = log.chars().rev().take(8000).collect::<String>();
        bail!("Setup failed ({}):\n{}", result.status, end.chars().rev().collect::<String>());
    }
    Ok(log.chars().rev().take(8000).collect::<String>()
        .chars().rev().collect())
}

pub fn open_editor(worktree: &Path, editor: &str) -> Result<()> {
    if !matches!(editor, "code" | "codium") { bail!("Unsupported editor"); }
    Command::new(editor).arg(worktree).spawn()
        .with_context(|| format!("Unable to open {editor}"))?;
    Ok(())
}

/// Never deletes a branch merely because Jira says Done.
/// Requires its tip to be an ancestor of the explicitly selected integration ref.
pub fn remove_merged(repository: &Path, tree: &Worktree, target: &str, key: &str) -> Result<()> {
    if !is_repository(repository) { bail!("Repository is missing"); }
    let branch = tree.branch.as_deref().context("Detached worktree cannot be removed here")?;
    if !branch_matches_issue(branch, key) { bail!("Branch does not belong to this ticket"); }
    // A branch is trivially an ancestor of itself. Never treat that as a merge.
    if target == branch || target == format!("refs/heads/{branch}") {
        bail!("The merge target cannot be the ticket branch itself");
    }
    if !tree.path.exists() { bail!("Worktree path is missing"); }
    if !worktrees(repository)?.iter().any(|t| t.path == tree.path && t.branch == tree.branch) {
        bail!("Worktree is no longer registered in this repository");
    }
    if !status(&tree.path)?.is_empty() { bail!("Worktree has uncommitted changes"); }
    let target_exists = command(repository,
        &["rev-parse", "--verify", "--quiet", &format!("{}^{{commit}}", target)])?
        .status.success();
    if !target_exists { bail!("Merge target is unavailable: {target}"); }
    let result = command(repository, &["merge-base", "--is-ancestor", branch, target])?;
    if !result.status.success() {
        bail!("Branch is not provably merged into {target}. Squash/rebase merges need manual review.");
    }
    let path = tree.path.to_str().context("Worktree path is not valid UTF-8")?;
    output(repository, &["worktree", "remove", "--", path])?;
    // Deleting with -D is safe here because ancestry was explicitly verified above.
    // -d would instead inspect the branch's *upstream*, which may be its old PR branch.
    output(repository, &["branch", "-D", "--", branch])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn matches_exact_ticket_not_neighbours() {
        assert!(branch_matches_issue("feature/ART-2272-sync", "ART-2272"));
        assert!(branch_matches_issue("ART-2272", "ART-2272"));
        assert!(!branch_matches_issue("ART-22720-sync", "ART-2272"));
        assert!(!branch_matches_issue("feature/ART-2273-sync", "ART-2272"));
    }

    fn temporary_repo() -> PathBuf {
        let suffix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("desk-for-jira-git-{}-{suffix}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        assert!(Command::new("git").args(["init", "-b", "main"])
            .current_dir(&root).output().unwrap().status.success());
        fs::write(root.join("file.txt"), "initial").unwrap();
        assert!(Command::new("git").args(["add", "."])
            .current_dir(&root).output().unwrap().status.success());
        assert!(Command::new("git").args(["-c", "user.name=Test", "-c", "user.email=test@example.org",
            "commit", "-m", "init"]).current_dir(&root).output().unwrap().status.success());
        root
    }

    #[test]
    fn create_and_remove_merged_worktree() {
        let root = temporary_repo();
        let target = root.parent().unwrap().join(format!("{}-worktree", root.file_name().unwrap().to_string_lossy()));
        create(&CreateWorktree { repository: &root, destination: &target,
            branch: "ART-2272-sync", base: "main", fetch: false }).unwrap();
        assert!(matching_worktrees(&root, "ART-2272").unwrap().iter().any(|t| t.path == target));
        let tree = matching_worktrees(&root, "ART-2272").unwrap().remove(0);
        remove_merged(&root, &tree, "main", "ART-2272").unwrap();
        assert!(!target.exists());
        assert!(matching_branches(&root, "ART-2272").unwrap().is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn refuses_to_delete_dirty_worktree() {
        let root = temporary_repo();
        let target = root.parent().unwrap().join(format!("{}-worktree", root.file_name().unwrap().to_string_lossy()));
        create(&CreateWorktree { repository: &root, destination: &target,
            branch: "ART-2272-change", base: "main", fetch: false }).unwrap();
        fs::write(target.join("local.txt"), "untracked").unwrap();
        let tree = matching_worktrees(&root, "ART-2272").unwrap().remove(0);
        assert!(remove_merged(&root, &tree, "main", "ART-2272").is_err());
        assert!(target.exists());
        output(&root, &["worktree", "remove", "--force", "--", target.to_str().unwrap()]).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }
}
