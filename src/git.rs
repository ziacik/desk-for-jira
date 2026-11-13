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
                || name.ends_with(".worktrees")
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

/// Extracts the Jira key from the same leading branch convention used to
/// match tickets (e.g. feature/ART-2921-description). This is a suggestion,
/// not a persisted association and does not query Jira.
pub fn ticket_key_from_branch(branch: &str) -> Option<String> {
    let name = branch.rsplit('/').next()?;
    let (project, rest) = name.split_once('-')?;
    let number = rest.split('-').next()?;
    if project.is_empty()
        || !project.as_bytes()[0].is_ascii_alphabetic()
        || !project.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        || number.is_empty()
        || !number.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let key = format!("{}-{number}", project.to_ascii_uppercase());
    branch_matches_issue(branch, &key).then_some(key)
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

/// Whether a branch tip is reachable from an explicit integration ref.
/// This checks commit ancestry, not a Jira/PR state; squash/rebase merges
/// cannot be proven this way without consulting additional evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStatus {
    Merged,
    NotConfirmed,
}

pub fn merge_status(repository: &Path, branch: &str, target: &str) -> Result<MergeStatus> {
    if branch.trim().is_empty() || target.trim().is_empty() || target == "HEAD" {
        bail!("Select a valid integration branch to check merge status");
    }
    if branch == target || target == format!("refs/heads/{branch}") {
        bail!("Cannot compare a branch against itself");
    }
    // Disallow a vacuous 'merged' result from an invalid or stale ref.
    let source = format!("refs/heads/{branch}");
    output(repository, &["rev-parse", "--verify", "--quiet", &format!("{source}^{{commit}}")])
        .with_context(|| format!("Branch is unavailable: {branch}"))?;
    output(repository, &["rev-parse", "--verify", "--quiet", &format!("{target}^{{commit}}")])
        .with_context(|| format!("Integration branch is unavailable: {target}"))?;
    let check = command(repository, &["merge-base", "--is-ancestor", &source, target])?;
    match check.status.code() {
        Some(0) => Ok(MergeStatus::Merged),
        Some(1) => Ok(MergeStatus::NotConfirmed),
        _ => bail!("Could not check merge status: {}",
            String::from_utf8_lossy(&check.stderr).trim()),
    }
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

/// Choose an integration branch to verify before deletion. Saved origin
/// takes priority, then a meaningful upstream, then the repository default.
/// Git does not persist the original creation branch for arbitrary worktrees.
pub struct MergeTarget {
    pub name: String,
    pub source: &'static str,
}

pub fn suggested_merge_target(
    repository: &Path, branch: &str, recorded_origin: Option<&str>,
) -> Option<MergeTarget> {
    let choices = available_bases(repository).ok()?;
    let usable = |candidate: &str| {
        candidate != "HEAD" && candidate != branch
            && candidate != format!("refs/heads/{branch}")
            && choices.iter().any(|name| name == candidate)
    };

    if let Some(base) = recorded_origin.filter(|base| usable(base)) {
        return Some(MergeTarget { name: base.to_owned(), source: "Starting point" });
    }
    // The upstream of a ticket branch is often origin/<same ticket branch>.
    // That is NOT an integration target: merely pushing a branch must never
    // mark its commits as merged.
    let local_ref = format!("refs/heads/{branch}");
    if let Ok(upstream) = output(repository, &["for-each-ref",
        "--format=%(upstream:short)", &local_ref])
        && !upstream.is_empty()
        && upstream != branch
        && !upstream.ends_with(&format!("/{branch}"))
        && usable(&upstream)
    {
        return Some(MergeTarget { name: upstream, source: "Git upstream" });
    }
    let fallback = recommended_base(repository);
    usable(&fallback).then_some(MergeTarget {
        name: fallback, source: "Repository default",
    })
}

/// Returns whether a new branch was created. When an existing branch is
/// checked out, request.base did not actually create that branch.
pub fn create(request: &CreateWorktree<'_>) -> Result<bool> {
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
    Ok(!exists)
}

pub fn status(worktree: &Path) -> Result<String> {
    output(worktree, &["status", "--porcelain=v1", "--untracked-files=all"])
}

fn setup_command(worktree: &Path, script: &str) -> Command {
    // Desktop launchers do not run the user's shell startup files. Use the
    // configured shell in interactive login mode so zshrc/bashrc and runtime
    // managers are initialized just as they are in a terminal.
    // Command inherits the desktop process environment by default.
    let shell = std::env::var_os("SHELL")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "/bin/sh".into());
    let mut command = Command::new(shell);
    command.args(["-l", "-i", "-c"]).arg(script).current_dir(worktree);
    command
}

pub fn run_setup(worktree: &Path, script: &str) -> Result<String> {
    if script.trim().is_empty() { return Ok(String::new()); }
    let result = setup_command(worktree, script)
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
    // "Open worktree" must never become an implicit creation path.
    // A stale UI target or a deleted folder must fail, not open an empty
    // VS Code workspace at a non-existent destination.
    if !is_repository(worktree) {
        bail!("Worktree directory is missing or is not a Git repository: {}", worktree.display());
    }
    Command::new(editor).arg(worktree).spawn()
        .with_context(|| format!("Unable to open {editor}"))?;
    Ok(())
}

// Prefer GNOME Console over the older GNOME Terminal. Keep this list shared
// between plain terminal opening and the interactive Codex launcher.
pub const TERMINAL_PROGRAMS: [&str; 4] =
    ["kgx", "xdg-terminal-exec", "cosmic-term", "gnome-terminal"];

pub fn open_terminal(worktree: &Path) -> Result<()> {
    // Never launch a terminal in a removed or otherwise invalid worktree.
    if !is_repository(worktree) {
        bail!("Worktree directory is missing or is not a Git repository: {}", worktree.display());
    }
    for program in TERMINAL_PROGRAMS {
        let mut command = Command::new(program);
        if program == "xdg-terminal-exec" {
            command.arg(format!("--dir={}", worktree.display()));
        } else {
            command.arg(format!("--working-directory={}", worktree.display()));
        }
        if command.spawn().is_ok() { return Ok(()); }
    }
    bail!("No supported terminal found (kgx, xdg-terminal-exec, cosmic-term, gnome-terminal)")
}

/// True for the primary checkout. Git lists it first, even when called
/// from a secondary worktree. It must never be removed by this application.
pub fn is_primary_worktree(repository: &Path, tree: &Worktree) -> Result<bool> {
    let trees = worktrees(repository)?;
    let primary = trees.first().context("Git returned no worktrees")?;
    Ok(primary.path == tree.path)
}

/// An ancestry check cannot establish squash/rebase PR completion. The UI
/// may offer a separate, explicitly confirmed local-only deletion on this
/// error, but must never silently interpret it as a successful merge.
pub const MERGE_NOT_CONFIRMED: &str = "Git cannot confirm merge:";

/// Returns the remote branch corresponding to a locally selected target.
/// A named remote target takes precedence over the generic "origin" fallback.
fn remote_merge_target(repository: &Path, target: &str) -> Result<Option<(String, String)>> {
    let remote_output = output(repository, &["remote"])?;
    let mut remotes: Vec<&str> = remote_output.lines().collect();
    remotes.sort();
    if remotes.is_empty() { return Ok(None); }
    if let Some(target) = target.strip_prefix("refs/remotes/")
        && let Some((remote, name)) = remotes.iter().find_map(|remote| {
            target.strip_prefix(&format!("{remote}/")).map(|name| (*remote, name))
        })
    {
        return Ok(Some((remote.to_owned(), name.to_owned())));
    }
    for remote in &remotes {
        if let Some(name) = target.strip_prefix(&format!("{remote}/")) {
            return Ok(Some(((*remote).to_owned(), name.to_owned())));
        }
    }
    let local_target = target.strip_prefix("refs/heads/").unwrap_or(target);
    if let Ok(upstream) = output(repository, &["for-each-ref", "--format=%(upstream:short)",
        &format!("refs/heads/{local_target}")])
    {
        for remote in &remotes {
            if let Some(name) = upstream.strip_prefix(&format!("{remote}/"))
                && !name.is_empty()
            {
                return Ok(Some(((*remote).to_owned(), name.to_owned())));
            }
        }
    }
    let remote = remotes.iter().find(|remote| **remote == "origin")
        .or_else(|| (remotes.len() == 1).then_some(&remotes[0]));
    Ok(remote.map(|remote| ((*remote).to_owned(), local_target.to_owned())))
}

fn validate_removal(repository: &Path, tree: &Worktree) -> Result<String> {
    if !is_repository(repository) { bail!("Repository is missing"); }
    let branch = tree.branch.as_deref().context("Detached worktree cannot be removed here")?;
    if is_primary_worktree(repository, tree)? {
        bail!("The primary checkout cannot be removed");
    }
    let registered = worktrees(repository)?;
    if !registered.iter().any(|t| t.path == tree.path && t.branch == tree.branch) {
        bail!("Worktree is no longer registered in this repository");
    }
    if !tree.path.exists() { bail!("Worktree path is missing"); }
    if !status(&tree.path)?.is_empty() { bail!("Worktree has uncommitted changes"); }
    Ok(branch.to_owned())
}

fn remove_validated_worktree(repository: &Path, tree: &Worktree, branch: &str) -> Result<()> {
    let path = tree.path.to_str().context("Worktree path is not valid UTF-8")?;
    output(repository, &["worktree", "remove", "--", path])?;
    // The branch was checked as clean and safe for local deletion. Do not
    // delete any remote branches or force-remove dirty or locked worktrees.
    output(repository, &["branch", "-D", "--", branch])?;
    Ok(())
}

/// Removes only after proving commit ancestry into the selected target.
/// A stale local release branch is checked against freshly fetched
/// remote-tracking state before returning an unconfirmed-merge error.
pub fn remove_merged_worktree(repository: &Path, tree: &Worktree, target: &str) -> Result<()> {
    if target.trim().is_empty() { bail!("Select the branch it was merged into"); }
    let branch = validate_removal(repository, tree)?;
    if target == branch || target == format!("refs/heads/{branch}")
        || target.ends_with(&format!("/{branch}"))
    {
        bail!("The merge target cannot be the worktree branch itself");
    }

    if merge_status(repository, &branch, target)? == MergeStatus::Merged {
        return remove_validated_worktree(repository, tree, &branch);
    }

    let mut checked = target.to_owned();
    let mut refresh_error = None;
    if let Some((remote, remote_branch)) = remote_merge_target(repository, target)? {
        let remote_ref = format!("{remote}/{remote_branch}");
        // An explicit deletion request can update remote-tracking refs; never
        // switch or modify the user's current checkout.
        let destination = format!(
            "+refs/heads/{remote_branch}:refs/remotes/{remote}/{remote_branch}"
        );
        match output(repository, &["fetch", "--no-tags", "--quiet", &remote, &destination]) {
            Ok(_) => {
                checked = remote_ref;
                if merge_status(repository, &branch, &checked)? == MergeStatus::Merged {
                    return remove_validated_worktree(repository, tree, &branch);
                }
            }
            Err(error) => refresh_error = Some(error.to_string()),
        }
    }

    if let Some(error) = refresh_error {
        bail!("{MERGE_NOT_CONFIRMED} branch {branch} into {target}. Could not refresh remote target: {error}. A completed squash/rebase PR requires manual confirmation.");
    }
    bail!("{MERGE_NOT_CONFIRMED} branch {branch} into {checked}, even after refreshing remote Git history. A completed squash/rebase PR requires manual confirmation.");
}

/// Only the dedicated UI's SECOND explicit destructive confirmation may call
/// this. It does not claim Git merge ancestry or PR completion. All worktree
/// cleanliness, registration and primary-checkout safeguards still apply.
pub fn remove_manually_confirmed_worktree(repository: &Path, tree: &Worktree) -> Result<()> {
    let branch = validate_removal(repository, tree)?;
    remove_validated_worktree(repository, tree, &branch)
}

/// Ticket-specific wrapper preserves the issue-key protection in ticket UI.
pub fn remove_merged(repository: &Path, tree: &Worktree, target: &str, key: &str) -> Result<()> {
    let branch = tree.branch.as_deref().context("Detached worktree cannot be removed here")?;
    if !branch_matches_issue(branch, key) { bail!("Branch does not belong to this ticket"); }
    remove_merged_worktree(repository, tree, target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn setup_uses_user_shell_in_interactive_login_mode() {
        let project = Path::new("/tmp/example-worktree");
        let command = setup_command(project, "npm install");
        let shell = std::env::var_os("SHELL")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        assert_eq!(command.get_program(), shell);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["-l", "-i", "-c", "npm install"].map(std::ffi::OsStr::new)
        );
        assert_eq!(command.get_current_dir(), Some(project));
    }

    #[test]
    fn opening_editor_cannot_create_a_missing_worktree() {
        let suffix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("desk-for-jira-missing-{suffix}"));
        assert!(!path.exists());
        assert!(open_editor(&path, "code").is_err());
        assert!(!path.exists());
    }

    #[test]
    fn extracts_issue_keys_from_existing_branch_naming_convention() {
        assert_eq!(ticket_key_from_branch("feature/ART-2921-copy-path"),
            Some("ART-2921".to_owned()));
        assert_eq!(ticket_key_from_branch("art-2921"),
            Some("ART-2921".to_owned()));
        assert_eq!(ticket_key_from_branch("AFB-2272-nqp"),
            Some("AFB-2272".to_owned()));
        assert_eq!(ticket_key_from_branch("bugfix/AFB_2-27-text"),
            Some("AFB_2-27".to_owned()));
        assert_eq!(ticket_key_from_branch("main"), None);
        assert_eq!(ticket_key_from_branch("feature/fix-ART-2921"), None);
        assert_eq!(ticket_key_from_branch("ART-2921other"), None);
        assert_eq!(ticket_key_from_branch("ART-"), None);
        assert_eq!(ticket_key_from_branch("123-2921"), None);
    }

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
    fn merge_status_tracks_commit_reachability_without_false_claims() {
        let root = temporary_repo();
        let path = root.parent().unwrap().join(
            format!("{}-linked", root.file_name().unwrap().to_string_lossy()));
        create(&CreateWorktree { repository: &root, destination: &path,
            branch: "ART-2272-feature", base: "main", fetch: false }).unwrap();

        // Identical tips are already reachable, even before a new commit.
        assert_eq!(merge_status(&root, "ART-2272-feature", "main").unwrap(), MergeStatus::Merged);

        fs::write(path.join("feature.txt"), "new feature").unwrap();
        output(&path, &["add", "feature.txt"]).unwrap();
        output(&path, &["-c", "user.name=Test", "-c", "user.email=test@example.org",
            "commit", "-m", "feature"]).unwrap();
        assert_eq!(merge_status(&root, "ART-2272-feature", "main").unwrap(),
            MergeStatus::NotConfirmed);

        // Merge the commit in the original checkout.
        output(&root, &["-c", "user.name=Test", "-c", "user.email=test@example.org",
            "merge", "--no-ff", "-m", "merge feature", "ART-2272-feature"]).unwrap();
        assert_eq!(merge_status(&root, "ART-2272-feature", "main").unwrap(), MergeStatus::Merged);

        // Any new commit on the worktree makes the tip not reachable again.
        output(&path, &["-c", "user.name=Test", "-c", "user.email=test@example.org",
            "commit", "--allow-empty", "-m", "more work"]).unwrap();
        assert_eq!(merge_status(&root, "ART-2272-feature", "main").unwrap(),
            MergeStatus::NotConfirmed);

        // A missing ref and self-comparison must never report merged.
        assert!(merge_status(&root, "ART-2272-feature", "missing").is_err());
        assert!(merge_status(&root, "ART-2272-feature", "ART-2272-feature").is_err());

        output(&root, &["worktree", "remove", "--", path.to_str().unwrap()]).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn refuses_to_remove_primary_checkout() {
        let root = temporary_repo();
        let primary = worktrees(&root).unwrap().remove(0);
        assert!(is_primary_worktree(&root, &primary).unwrap());
        let error = remove_merged_worktree(&root, &primary, "main").unwrap_err();
        assert!(error.to_string().contains("primary checkout"));
        assert!(root.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn origin_has_priority_over_default_and_same_branch_upstream() {
        let root = temporary_repo();
        output(&root, &["branch", "release/2.7"]).unwrap();
        let target = root.parent().unwrap().join(format!(
            "{}-release-worktree", root.file_name().unwrap().to_string_lossy()
        ));
        assert!(create(&CreateWorktree {
            repository: &root, destination: &target,
            branch: "ART-33-work", base: "release/2.7", fetch: false,
        }).unwrap());

        let choice = suggested_merge_target(&root, "ART-33-work", Some("release/2.7")).unwrap();
        assert_eq!(choice.name, "release/2.7");
        assert_eq!(choice.source, "Starting point");

        // A remote tracking ref for the *same* branch must never be treated
        // as proof that the feature was integrated.
        output(&root, &["remote", "add", "origin", "."]).unwrap();
        output(&root, &["update-ref", "refs/remotes/origin/ART-33-work",
            &output(&root, &["rev-parse", "ART-33-work"]).unwrap()]).unwrap();
        output(&root, &["branch", "--set-upstream-to=origin/ART-33-work", "ART-33-work"]).unwrap();
        let choice = suggested_merge_target(&root, "ART-33-work", None).unwrap();
        assert_eq!(choice.name, "main");
        assert_eq!(choice.source, "Repository default");

        output(&root, &["worktree", "remove", "--", target.to_str().unwrap()]).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn existing_branch_checkout_does_not_claim_base_as_origin() {
        let root = temporary_repo();
        output(&root, &["branch", "ART-44-existing"]).unwrap();
        let target = root.parent().unwrap().join(format!(
            "{}-existing-worktree", root.file_name().unwrap().to_string_lossy()
        ));
        assert!(!create(&CreateWorktree {
            repository: &root, destination: &target,
            branch: "ART-44-existing", base: "main", fetch: false,
        }).unwrap());
        output(&root, &["worktree", "remove", "--", target.to_str().unwrap()]).unwrap();
        fs::remove_dir_all(&root).unwrap();
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
    fn refreshes_stale_local_release_tracking_before_removing_merged_worktree() {
        let root = temporary_repo();
        let remote = root.parent().unwrap().join(format!(
            "{}-remote.git", root.file_name().unwrap().to_string_lossy()));
        output(&root, &["init", "--bare", "--initial-branch=main",
            "--", remote.to_str().unwrap()]).unwrap();
        output(&root, &["remote", "add", "origin", remote.to_str().unwrap()]).unwrap();
        output(&root, &["branch", "release/19.2"]).unwrap();
        output(&root, &["push", "origin", "release/19.2"]).unwrap();
        let path = root.parent().unwrap().join(format!(
            "{}-linked-release", root.file_name().unwrap().to_string_lossy()));
        create(&CreateWorktree {
            repository: &root, destination: &path,
            branch: "ART-2861-work", base: "release/19.2", fetch: false,
        }).unwrap();
        fs::write(path.join("new.txt"), "feature").unwrap();
        output(&path, &["add", "."]).unwrap();
        output(&path, &["-c", "user.name=Test", "-c", "user.email=test@example.org",
            "commit", "-m", "feature"]).unwrap();
        // Another contributor merges to the remote target, leaving our local
        // release/19.2 stale. The deletion check must fetch and re-check.
        output(&root, &["push", "origin", "ART-2861-work:release/19.2"]).unwrap();
        let tree = matching_worktrees(&root, "ART-2861").unwrap().remove(0);
        assert_eq!(merge_status(&root, "ART-2861-work", "release/19.2").unwrap(),
            MergeStatus::NotConfirmed);
        remove_merged_worktree(&root, &tree, "release/19.2").unwrap();
        assert!(!path.exists());
        assert!(matching_branches(&root, "ART-2861").unwrap().is_empty());
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&remote).unwrap();
    }

    #[test]
    fn manual_confirmation_still_refuses_primary_or_dirty_worktrees() {
        let root = temporary_repo();
        let primary = worktrees(&root).unwrap().remove(0);
        assert!(remove_manually_confirmed_worktree(&root, &primary).is_err());
        let path = root.parent().unwrap().join(format!(
            "{}-manual-release", root.file_name().unwrap().to_string_lossy()));
        create(&CreateWorktree {
            repository: &root, destination: &path,
            branch: "ART-2861-work", base: "main", fetch: false,
        }).unwrap();
        fs::write(path.join("untracked.txt"), "important").unwrap();
        let tree = matching_worktrees(&root, "ART-2861").unwrap().remove(0);
        assert!(remove_manually_confirmed_worktree(&root, &tree).is_err());
        assert!(path.exists());
        fs::remove_file(path.join("untracked.txt")).unwrap();
        remove_manually_confirmed_worktree(&root, &tree).unwrap();
        assert!(!path.exists());
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
