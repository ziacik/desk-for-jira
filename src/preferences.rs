use std::{collections::{HashMap, HashSet}, env, fs, io::ErrorKind, path::PathBuf};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use crate::jira::{ProjectStatus, StatusCategory};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StatusRef {
    pub project_key: String,
    pub status_id: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    pub title: String,
    pub statuses: Vec<StatusRef>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardSettings {
    #[serde(default)]
    pub projects: Vec<String>,
    #[serde(default)]
    pub columns: Vec<Column>,
    // A migration marker: older versions created a separate column for every project status.
    #[serde(default)]
    pub grouping_version: u8,
    #[serde(default)]
    pub pinned_issues: Vec<String>,
}
/// Visibility of the Git worktree panel for a Jira project and workflow status ID.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorktreeVisibility(pub HashMap<String, HashMap<String, bool>>);

impl WorktreeVisibility {
    pub fn should_show(&self, project: &str, status_id: &str, category: Option<StatusCategory>) -> bool {
        self.0.get(project).and_then(|statuses| statuses.get(status_id)).copied()
            .unwrap_or(!matches!(category, Some(StatusCategory::Done)))
    }

    pub fn set(&mut self, project: &str, status_id: &str, show: bool) {
        self.0.entry(project.to_owned()).or_default().insert(status_id.to_owned(), show);
    }
}

/// User-chosen workflow action, scoped to project, status ID and issue type.
/// The IDs, not status/transition names, are used to resolve Jira transitions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PrimaryTransitions(pub HashMap<String, HashMap<String, HashMap<String, String>>>);

impl PrimaryTransitions {
    pub fn get(&self, project: &str, status_id: &str, issue_type: &str) -> Option<&str> {
        self.0.get(project)?.get(status_id)?.get(issue_type).map(String::as_str)
    }

    pub fn set(&mut self, project: &str, status_id: &str, issue_type: &str, id: Option<&str>) {
        if let Some(id) = id {
            self.0.entry(project.to_owned()).or_default()
                .entry(status_id.to_owned()).or_default()
                .insert(issue_type.to_owned(), id.to_owned());
        } else if let Some(statuses) = self.0.get_mut(project) {
            if let Some(types) = statuses.get_mut(status_id) {
                types.remove(issue_type);
                if types.is_empty() {
                    statuses.remove(status_id);
                }
            }
            if statuses.is_empty() {
                self.0.remove(project);
            }
        }
    }
}

impl ProjectStatus {
    pub fn reference(&self) -> StatusRef {
        StatusRef { project_key: self.project_key.clone(), status_id: self.id.clone() }
    }
}
fn category_position(category: StatusCategory) -> u8 {
    match category {
        StatusCategory::Todo => 0,
        StatusCategory::InProgress => 1,
        StatusCategory::Done => 2,
    }
}

impl BoardSettings {
    /// Toggle the pinned state of one Jira issue, preserving pin order.
    pub fn toggle_pinned(&mut self, key: &str) -> bool {
        if let Some(index) = self.pinned_issues.iter().position(|pinned| pinned == key) {
            self.pinned_issues.remove(index);
            false
        } else {
            self.pinned_issues.push(key.to_owned());
            true
        }
    }

    /// Use actual workflow status IDs, never Jira board columns or matching names.
    pub fn reconcile(&mut self, available: &[ProjectStatus]) {
        let initial_layout = self.columns.is_empty();
        let valid: HashSet<_> = available.iter().map(ProjectStatus::reference).collect();
        let selected: HashSet<_> = self.projects.iter().map(String::as_str).collect();
        let mut seen = HashSet::new();
        for column in &mut self.columns {
            // Keep mappings for temporarily deselected projects.
            column.statuses.retain(|s| {
                (!selected.contains(s.project_key.as_str()) || valid.contains(s))
                    && seen.insert(s.clone())
            });
        }
        self.columns.retain(|c| !c.statuses.is_empty());

        // Migrate the old per-project defaults once. Only join singleton columns
        // with equal names from different projects, keeping custom mappings intact.
        // Explicitly separated columns stay separated on subsequent refreshes.
        if self.grouping_version == 0 {
            self.merge_legacy_columns();
            self.grouping_version = 1;
        }

        for status in available {
            let reference = status.reference();
            if seen.insert(reference.clone()) {
                let candidates: Vec<usize> = self.columns.iter().enumerate()
                    .filter(|(_, column)| column.title.trim().eq_ignore_ascii_case(status.name.trim())
                        && !column.statuses.iter().any(|s| s.project_key == status.project_key))
                    .map(|(index, _)| index)
                    .collect();
                if let [index] = candidates.as_slice() {
                    self.columns[*index].statuses.push(reference);
                } else {
                    // A repeated name within one project, or an ambiguous split,
                    // must never silently merge into an arbitrary column.
                    self.columns.push(Column { title: status.name.clone(), statuses: vec![reference] });
                }
            }
        }
        // Only a new board gets the suggested order. Existing manually moved
        // columns must never be rearranged on a refresh or project switch.
        if initial_layout {
            self.sort_initial_columns(available);
        }
    }

    fn sort_initial_columns(&mut self, available: &[ProjectStatus]) {
        // Jira's workflow transitions determine relative order when accessible.
        // Without workflow permission, keep Jira's returned order WITHIN each
        // status category. Do not guess from English or localized status names.
        let metadata: HashMap<_, _> = available.iter()
            .map(|status| (status.reference(), (status.category, status.workflow_order)))
            .collect();
        self.columns.sort_by_key(|column| {
            let category = column.statuses.iter()
                .filter_map(|reference| metadata.get(reference).and_then(|(category, _)| *category))
                .min_by_key(|category| category_position(*category));
            let workflow_order = column.statuses.iter()
                .filter_map(|reference| metadata.get(reference).and_then(|(_, order)| *order))
                .min();
            (category.map(category_position).unwrap_or(1), workflow_order)
        });
    }

    fn merge_legacy_columns(&mut self) {
        let mut index = 0;
        while index < self.columns.len() {
            if self.columns[index].statuses.len() != 1 {
                index += 1;
                continue;
            }
            let status = &self.columns[index].statuses[0];
            let title = &self.columns[index].title;
            let candidates: Vec<usize> = (0..index)
                .filter(|&candidate| {
                    let column = &self.columns[candidate];
                    column.title.trim().eq_ignore_ascii_case(title.trim())
                        && column.statuses.iter().all(|s| s.project_key != status.project_key)
                })
                .collect();
            if let [destination] = candidates.as_slice() {
                let removed = self.columns.remove(index);
                self.columns[*destination].statuses.extend(removed.statuses);
            } else {
                index += 1;
            }
        }
    }

    pub fn move_status(&mut self, reference: &StatusRef, target: Option<usize>, title: &str) {
        let source = self.columns.iter().position(|c| c.statuses.contains(reference));
        if source == target { return; }
        if let Some(source) = source {
            self.columns[source].statuses.retain(|s| s != reference);
        }
        match target {
            Some(i) if i < self.columns.len() => self.columns[i].statuses.push(reference.clone()),
            _ => self.columns.push(Column { title: title.into(), statuses: vec![reference.clone()] }),
        }
        self.columns.retain(|c| !c.statuses.is_empty());
    }
    pub fn rename_column(&mut self, index: usize, title: &str) -> bool {
        let title = title.trim();
        if title.is_empty() {
            return false;
        }
        if let Some(column) = self.columns.get_mut(index) {
            column.title = title.to_owned();
            return true;
        }
        false
    }

    pub fn reset_columns(&mut self, available: &[ProjectStatus]) {
        self.columns.clear();
        self.grouping_version = 1;
        self.reconcile(available);
    }

    /// Move the column containing this status, skipping inactive project columns.
    /// The reference identifies the column even after previous moves.
    pub fn move_visible_column(&mut self, reference: &StatusRef, left: bool) -> bool {
        let visible: Vec<_> = self.columns.iter().enumerate()
            .filter(|(_, column)| column.statuses.iter()
                .any(|status| self.projects.contains(&status.project_key)))
            .map(|(index, _)| index)
            .collect();
        let Some(position) = visible.iter()
            .position(|&index| self.columns[index].statuses.contains(reference))
        else { return false; };
        let destination_position = if left {
            position.checked_sub(1)
        } else {
            position.checked_add(1).filter(|&next| next < visible.len())
        };
        let Some(destination_position) = destination_position else { return false; };
        let source = visible[position];
        let destination = visible[destination_position];
        if left {
            for index in (destination + 1..=source).rev() {
                self.move_column(index, true);
            }
        } else {
            for index in source..destination {
                self.move_column(index, false);
            }
        }
        true
    }

    pub fn move_column(&mut self, index: usize, up: bool) {
        let other = if up { index.checked_sub(1) } else { index.checked_add(1) };
        if let Some(other) = other.filter(|i| *i < self.columns.len()) {
            self.columns.swap(index, other);
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueScope {
    #[default]
    Mine,
    Unassigned,
    All,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewSettings {
    pub scope: IssueScope,
    pub active_sprint_only: bool,
    pub project_filter: Option<String>,
    pub search: String,
    pub collapsed_columns: HashSet<String>,
    pub open_issue: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiLayout {
    pub window_width: i32,
    pub window_height: i32,
    pub sidebar_visible: bool,
    pub sidebar_width: i32,
    pub detail_width: i32,
    pub worktrees_window_width: i32,
    pub worktrees_window_height: i32,
}

impl Default for UiLayout {
    fn default() -> Self {
        Self {
            window_width: 1460,
            window_height: 860,
            sidebar_visible: true,
            sidebar_width: 224,
            detail_width: 470,
            worktrees_window_width: 980,
            worktrees_window_height: 760,
        }
    }
}

impl UiLayout {
    pub fn sanitized(mut self) -> Self {
        self.window_width = self.window_width.max(960);
        self.window_height = self.window_height.max(580);
        self.sidebar_width = self.sidebar_width.clamp(184, 800);
        self.detail_width = self.detail_width.clamp(320, 2048);
        self.worktrees_window_width = self.worktrees_window_width.clamp(640, 2400);
        self.worktrees_window_height = self.worktrees_window_height.clamp(480, 1600);
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GitRepository {
    pub name: String,
    pub path: String,
    /// User-approved setup script, run only immediately after new worktree creation.
    pub setup: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeOrigin {
    pub repository: String,
    pub branch: String,
    pub base: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct GitSettings {
    pub repositories: Vec<GitRepository>,
    pub scan_roots: Vec<String>,
    pub worktree_root: String,
    pub editor: String,
    /// Optional multi-line instructions included only in Fix with Codex runs.
    pub codex_instructions: String,
    pub fetch_before_create: bool,
    /// site -> Jira project key -> repository path -> number of successful creates.
    pub usage: HashMap<String, HashMap<String, HashMap<String, u64>>>,
    /// Worktree path -> origin of a branch newly created by Desk for Jira.
    /// This is a suggested merge-check target, never proof of a merge.
    pub worktree_origins: HashMap<String, WorktreeOrigin>,
}

impl Default for GitSettings {
    fn default() -> Self {
        Self {
            repositories: Vec::new(),
            scan_roots: Vec::new(),
            worktree_root: "~/Worktrees".to_owned(),
            editor: "code".to_owned(),
            codex_instructions: String::new(),
            fetch_before_create: true,
            usage: HashMap::new(),
            worktree_origins: HashMap::new(),
        }
    }
}

impl GitSettings {
    pub fn register(&mut self, repository: GitRepository) {
        if let Some(previous) = self.repositories.iter_mut().find(|r| r.path == repository.path) {
            // Re-discovery never overwrites an explicitly configured setup.
            if previous.name.is_empty() { previous.name = repository.name; }
        } else {
            self.repositories.push(repository);
        }
    }

    pub fn preferred_index(&self, site: &str, key: &str) -> Option<usize> {
        let project = key.split_once('-')?.0;
        let counts = self.usage.get(site)?.get(project)?;
        self.repositories.iter().enumerate()
            .filter(|(_, repo)| counts.get(&repo.path).copied().unwrap_or(0) > 0)
            .max_by_key(|(_, repo)| counts.get(&repo.path).copied().unwrap_or(0))
            .map(|(index, _)| index)
    }

    pub fn remember_worktree_origin(
        &mut self, repository: &str, worktree: &str, branch: &str, base: &str,
    ) {
        self.worktree_origins.insert(worktree.to_owned(), WorktreeOrigin {
            repository: repository.to_owned(),
            branch: branch.to_owned(),
            base: base.to_owned(),
        });
    }

    pub fn worktree_origin<'a>(
        &'a self, repository: &str, worktree: &str, branch: &str,
    ) -> Option<&'a str> {
        let origin = self.worktree_origins.get(worktree)?;
        (origin.repository == repository && origin.branch == branch)
            .then_some(origin.base.as_str())
    }

    pub fn forget_worktree_origin(&mut self, worktree: &str) {
        self.worktree_origins.remove(worktree);
    }

    pub fn record_usage(&mut self, site: &str, key: &str, path: &str) {
        let Some((project, _)) = key.split_once('-') else { return; };
        let entry = self.usage.entry(site.to_owned()).or_default()
            .entry(project.to_owned()).or_default()
            .entry(path.to_owned()).or_default();
        *entry = entry.saturating_add(1);
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Preferences {
    #[serde(default)]
    board_by_site: HashMap<String, BoardSettings>,
    #[serde(default)]
    view_by_site: HashMap<String, ViewSettings>,
    #[serde(default)]
    worktree_visibility_by_site: HashMap<String, WorktreeVisibility>,
    #[serde(default)]
    primary_transitions_by_site: HashMap<String, PrimaryTransitions>,
    #[serde(default)]
    ui_layout: UiLayout,
    #[serde(default)]
    git: GitSettings,
}
fn preferences_path() -> Result<PathBuf> {
    let root = env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty()).map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    let Some(root) = root else { bail!("Could not find the user configuration directory"); };
    Ok(root.join("desk-for-jira").join("board-settings.json"))
}
fn read_preferences(path: &PathBuf) -> Result<Preferences> {
    match fs::read(path) {
        Ok(data) => serde_json::from_slice(&data).with_context(|| format!("Invalid settings file: {}", path.display())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Preferences::default()),
        Err(error) => Err(error).with_context(|| format!("Could not read {}", path.display())),
    }
}
pub fn load(base_url: &str) -> Result<BoardSettings> {
    Ok(read_preferences(&preferences_path()?)?.board_by_site.get(base_url).cloned().unwrap_or_default())
}
fn write_preferences(path: &PathBuf, prefs: &Preferences) -> Result<()> {
    let parent = path.parent().context("Missing settings directory")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!("board-settings-{}.tmp", std::process::id()));
    fs::write(&temp, serde_json::to_vec_pretty(prefs)?)?;
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error).with_context(|| format!("Could not save {}", path.display()));
    }
    Ok(())
}

pub fn save(base_url: &str, settings: &BoardSettings) -> Result<()> {
    let path = preferences_path()?;
    let mut prefs = read_preferences(&path)?;
    prefs.board_by_site.insert(base_url.into(), settings.clone());
    write_preferences(&path, &prefs)
}

pub fn load_view(base_url: &str) -> Result<ViewSettings> {
    Ok(read_preferences(&preferences_path()?)?
        .view_by_site.get(base_url).cloned().unwrap_or_default())
}

pub fn save_view(base_url: &str, view: &ViewSettings) -> Result<()> {
    let path = preferences_path()?;
    let mut prefs = read_preferences(&path)?;
    prefs.view_by_site.insert(base_url.into(), view.clone());
    write_preferences(&path, &prefs)
}

pub fn load_worktree_visibility(base_url: &str) -> Result<WorktreeVisibility> {
    Ok(read_preferences(&preferences_path()?)?
        .worktree_visibility_by_site.get(base_url).cloned().unwrap_or_default())
}

pub fn save_worktree_visibility(base_url: &str, visibility: &WorktreeVisibility) -> Result<()> {
    let path = preferences_path()?;
    let mut prefs = read_preferences(&path)?;
    prefs.worktree_visibility_by_site.insert(base_url.to_owned(), visibility.clone());
    write_preferences(&path, &prefs)
}

pub fn load_primary_transitions(base_url: &str) -> Result<PrimaryTransitions> {
    Ok(read_preferences(&preferences_path()?)?
        .primary_transitions_by_site.get(base_url).cloned().unwrap_or_default())
}

pub fn save_primary_transitions(base_url: &str, transitions: &PrimaryTransitions) -> Result<()> {
    let path = preferences_path()?;
    let mut prefs = read_preferences(&path)?;
    prefs.primary_transitions_by_site.insert(base_url.to_owned(), transitions.clone());
    write_preferences(&path, &prefs)
}

pub fn load_layout() -> Result<UiLayout> {
    Ok(read_preferences(&preferences_path()?)?.ui_layout.sanitized())
}

pub fn save_layout(layout: &UiLayout) -> Result<()> {
    let path = preferences_path()?;
    let mut prefs = read_preferences(&path)?;
    prefs.ui_layout = layout.clone().sanitized();
    write_preferences(&path, &prefs)
}
pub fn load_git() -> Result<GitSettings> {
    Ok(read_preferences(&preferences_path()?)?.git)
}

pub fn save_git(settings: &GitSettings) -> Result<()> {
    let path = preferences_path()?;
    let mut prefs = read_preferences(&path)?;
    prefs.git = settings.clone();
    write_preferences(&path, &prefs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_transition_overrides_are_scoped_and_removable() {
        let mut rules = PrimaryTransitions::default();
        rules.set("ART", "101", "Bug", Some("31"));
        assert_eq!(rules.get("ART", "101", "Bug"), Some("31"));
        assert_eq!(rules.get("ART", "101", "Task"), None);
        assert_eq!(rules.get("ART", "102", "Bug"), None);
        assert_eq!(rules.get("AFB", "101", "Bug"), None);
        let restored: PrimaryTransitions =
            serde_json::from_str(&serde_json::to_string(&rules).unwrap()).unwrap();
        assert_eq!(rules, restored);
        rules.set("ART", "101", "Bug", None);
        assert_eq!(rules.get("ART", "101", "Bug"), None);
        assert!(rules.0.is_empty());
    }

    #[test]
    fn worktree_visibility_defaults_to_hidden_only_for_done() {
        let rules = WorktreeVisibility::default();
        assert!(rules.should_show("ART", "1", Some(StatusCategory::Todo)));
        assert!(rules.should_show("ART", "2", Some(StatusCategory::InProgress)));
        assert!(rules.should_show("ART", "3", None));
        assert!(!rules.should_show("ART", "4", Some(StatusCategory::Done)));
    }

    #[test]
    fn worktree_status_rules_are_scoped_by_project_and_status_id() {
        let mut rules = WorktreeVisibility::default();
        rules.set("ART", "2", false);
        rules.set("ART", "4", true);
        assert!(!rules.should_show("ART", "2", Some(StatusCategory::InProgress)));
        assert!(rules.should_show("AFB", "2", Some(StatusCategory::InProgress)));
        assert!(rules.should_show("ART", "4", Some(StatusCategory::Done)));
        let restored: WorktreeVisibility = serde_json::from_str(
            &serde_json::to_string(&rules).unwrap()
        ).unwrap();
        assert_eq!(rules, restored);
    }
    fn status(project: &str, id: &str, name: &str) -> ProjectStatus {
        ProjectStatus {
            project_key: project.into(), id: id.into(), name: name.into(),
            category: None, workflow_order: None, workflows: Vec::new(),
        }
    }
    #[test]
    fn worktrees_window_size_defaults_and_clamps() {
        let defaults = UiLayout::default();
        assert_eq!((defaults.worktrees_window_width, defaults.worktrees_window_height), (980, 760));
        let invalid = UiLayout {
            worktrees_window_width: 0,
            worktrees_window_height: 5000,
            ..UiLayout::default()
        };
        let corrected = invalid.sanitized();
        assert_eq!(corrected.worktrees_window_width, 640);
        assert_eq!(corrected.worktrees_window_height, 1600);
    }

    #[test]
    fn first_load_uses_real_workflow_order_independent_of_names() {
        let mut board = BoardSettings {
            projects: vec!["ART".into()],
            ..BoardSettings::default()
        };
        let mut last = status("ART", "3", "Fertig");
        last.category = Some(StatusCategory::Done);
        last.workflow_order = Some(2);
        let mut middle = status("ART", "2", "Spracúva sa");
        middle.category = Some(StatusCategory::InProgress);
        middle.workflow_order = Some(1);
        let mut first = status("ART", "1", "Čaká");
        first.category = Some(StatusCategory::Todo);
        first.workflow_order = Some(0);
        board.reconcile(&[last, middle, first]);
        assert_eq!(
            board.columns.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(),
            vec!["Čaká", "Spracúva sa", "Fertig"]
        );
    }

    #[test]
    fn workflow_order_sorts_within_category_without_name_heuristics() {
        let mut board = BoardSettings {
            projects: vec!["ART".into()],
            ..BoardSettings::default()
        };
        let mut first = status("ART", "1", "Whatever first");
        first.category = Some(StatusCategory::InProgress);
        first.workflow_order = Some(0);
        let mut second = status("ART", "2", "Whatever second");
        second.category = Some(StatusCategory::InProgress);
        second.workflow_order = Some(1);
        board.reconcile(&[second, first]);
        assert_eq!(board.columns[0].title, "Whatever first");
        assert_eq!(board.columns[1].title, "Whatever second");
    }

    #[test]
    fn without_workflow_access_preserves_api_order_within_categories() {
        let mut board = BoardSettings {
            projects: vec!["ART".into()],
            ..BoardSettings::default()
        };
        let mut alpha = status("ART", "1", "Closed");
        alpha.category = Some(StatusCategory::InProgress);
        let mut beta = status("ART", "2", "Backlog");
        beta.category = Some(StatusCategory::InProgress);
        board.reconcile(&[alpha, beta]);
        assert_eq!(
            board.columns.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(),
            vec!["Closed", "Backlog"]
        );
    }

    #[test]
    fn jira_category_takes_precedence_over_workflow_position_across_categories() {
        let mut board = BoardSettings {
            projects: vec!["ART".into()],
            ..BoardSettings::default()
        };
        let mut todo = status("ART", "11", "Indeterminate");
        todo.category = Some(StatusCategory::Todo);
        todo.workflow_order = Some(10);
        let mut progress = status("ART", "12", "Working");
        progress.category = Some(StatusCategory::InProgress);
        progress.workflow_order = Some(0);
        let mut done = status("ART", "13", "Final");
        done.category = Some(StatusCategory::Done);
        done.workflow_order = Some(1);
        board.reconcile(&[done, progress, todo]);
        let names: Vec<_> = board.columns.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(names, vec!["Indeterminate", "Working", "Final"]);
    }

    #[test]
    fn manual_order_survives_refresh_but_reset_suggests_order_again() {
        let mut board = BoardSettings {
            projects: vec!["ART".into()],
            ..BoardSettings::default()
        };
        let mut done = status("ART", "1", "Ukončené");
        done.category = Some(StatusCategory::Done);
        let mut progress = status("ART", "2", "Pracuje sa");
        progress.category = Some(StatusCategory::InProgress);
        let mut todo = status("ART", "3", "Čaká");
        todo.category = Some(StatusCategory::Todo);
        let statuses = vec![done, progress, todo];
        board.reconcile(&statuses);
        assert_eq!(board.columns[0].title, "Čaká");
        board.move_column(2, true);
        board.move_column(1, true);
        assert_eq!(board.columns[0].title, "Ukončené");
        board.reconcile(&statuses);
        assert_eq!(board.columns[0].title, "Ukončené");
        board.reset_columns(&statuses);
        assert_eq!(
            board.columns.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(),
            vec!["Čaká", "Pracuje sa", "Ukončené"]
        );
    }

    #[test]
    fn identical_names_across_projects_merge_by_default() {
        let a = status("ART", "12", "Open");
        let b = status("AFB", "12", "Open");
        let mut board = BoardSettings::default();
        board.reconcile(&[a.clone(), b.clone()]);
        assert_eq!(board.columns.len(), 1);
        assert_eq!(board.columns[0].title, "Open");
        assert_eq!(board.columns[0].statuses, vec![a.reference(), b.reference()]);
        board.reconcile(&[a, b]);
        assert_eq!(board.columns.len(), 1);
    }

    #[test]
    fn explicitly_split_equal_names_stay_separate_after_reload() {
        let art = status("ART", "1", "Open");
        let afb = status("AFB", "2", "Open");
        let mut board = BoardSettings::default();
        board.reconcile(&[art.clone(), afb.clone()]);
        board.move_status(&afb.reference(), None, "Open");
        assert_eq!(board.columns.len(), 2);
        let serialized = serde_json::to_vec(&board).unwrap();
        let mut restored: BoardSettings = serde_json::from_slice(&serialized).unwrap();
        restored.reconcile(&[art, afb]);
        assert_eq!(restored.columns.len(), 2);
    }

    #[test]
    fn legacy_separate_columns_merge_on_first_refresh() {
        let old = serde_json::json!({
            "projects": ["ART", "AFB"],
            "columns": [
                {"title":"Open","statuses":[{"project_key":"ART","status_id":"1"}]},
                {"title":"In Review","statuses":[{"project_key":"ART","status_id":"2"}]},
                {"title":"Open","statuses":[{"project_key":"AFB","status_id":"3"}]}
            ]
        });
        let mut board: BoardSettings = serde_json::from_value(old).unwrap();
        board.reconcile(&[
            status("ART", "1", "Open"),
            status("ART", "2", "In Review"),
            status("AFB", "3", "Open"),
        ]);
        assert_eq!(board.columns.len(), 2);
        assert_eq!(board.columns[0].title, "Open");
        assert_eq!(board.columns[0].statuses.len(), 2);
        assert_eq!(board.grouping_version, 1);
    }

    #[test]
    fn duplicate_names_within_one_project_stay_separate() {
        let mut board = BoardSettings::default();
        board.reconcile(&[status("ART", "1", "Open"), status("ART", "2", "Open")]);
        assert_eq!(board.columns.len(), 2);
    }
    #[test]
    fn deselecting_a_project_does_not_forget_its_mappings() {
        let art = status("ART", "1", "Review");
        let afb = status("AFB", "2", "Review");
        let mut board = BoardSettings {
            projects: vec!["ART".into(), "AFB".into()],
            ..BoardSettings::default()
        };
        board.reconcile(&[art.clone(), afb.clone()]);
        board.projects = vec!["ART".into()];
        board.reconcile(std::slice::from_ref(&art));
        assert_eq!(board.columns[0].statuses.len(), 2);
        board.projects.push("AFB".into());
        board.reconcile(&[art, afb]);
        assert_eq!(board.columns[0].statuses.len(), 2);
    }
    #[test]
    fn split_and_order() {
        let a = status("ART", "1", "Todo");
        let b = status("ART", "2", "Review");
        let mut board = BoardSettings::default();
        board.reconcile(&[a, b.clone()]);
        board.move_status(&b.reference(), Some(0), &b.name);
        board.move_status(&b.reference(), None, &b.name);
        board.move_column(1, true);
        assert_eq!(board.columns[0].title, "Review");
    }
    #[test]
    fn move_eighth_column_to_first_with_repeated_left_actions() {
        let statuses: Vec<_> = (0..8)
            .map(|index| status("ART", &index.to_string(), &format!("Column {index}")))
            .collect();
        let selected = statuses[7].reference();
        let mut board = BoardSettings {
            projects: vec!["ART".into()],
            ..BoardSettings::default()
        };
        board.reconcile(&statuses);

        for expected_index in (0..7).rev() {
            assert!(board.move_visible_column(&selected, true));
            let actual_index = board.columns.iter().position(|column|
                column.statuses.contains(&selected));
            assert_eq!(actual_index, Some(expected_index));
        }
        assert!(!board.move_visible_column(&selected, true));
        assert_eq!(board.columns[0].statuses, vec![selected]);
        assert_eq!(board.columns[1].title, "Column 0");
    }

    #[test]
    fn repeated_moves_follow_the_same_column_and_skip_inactive_projects() {
        let first = status("ART", "1", "First");
        let hidden = status("OTHER", "2", "Hidden");
        let second = status("ART", "3", "Second");
        let third = status("ART", "4", "Third");
        let mut board = BoardSettings {
            projects: vec!["ART".into(), "OTHER".into()],
            ..BoardSettings::default()
        };
        board.reconcile(&[first.clone(), hidden, second.clone(), third.clone()]);
        board.projects = vec!["ART".into()];

        assert!(board.move_visible_column(&third.reference(), true));
        assert!(board.move_visible_column(&third.reference(), true));
        let active: Vec<_> = board.columns.iter()
            .filter(|column| column.statuses.iter()
                .any(|item| board.projects.contains(&item.project_key)))
            .map(|column| column.title.as_str())
            .collect();
        assert_eq!(active, vec!["Third", "First", "Second"]);
        assert!(!board.move_visible_column(&third.reference(), true));
        assert!(board.move_visible_column(&third.reference(), false));
        assert!(board.move_visible_column(&third.reference(), false));
        assert_eq!(board.columns.iter().filter(|column| column.title == "Third").count(), 1);
        let active: Vec<_> = board.columns.iter()
            .filter(|column| column.statuses.iter()
                .any(|item| board.projects.contains(&item.project_key)))
            .map(|column| column.title.as_str())
            .collect();
        assert_eq!(active, vec!["First", "Second", "Third"]);
    }

    #[test]
    fn pinning_keeps_order_and_survives_serialization() {
        let mut board = BoardSettings::default();
        assert!(board.toggle_pinned("ART-42"));
        assert!(board.toggle_pinned("AFB-2"));
        assert_eq!(board.pinned_issues, vec!["ART-42", "AFB-2"]);
        assert!(!board.toggle_pinned("ART-42"));
        assert_eq!(board.pinned_issues, vec!["AFB-2"]);
        let restored: BoardSettings = serde_json::from_slice(
            &serde_json::to_vec(&board).unwrap()
        ).unwrap();
        assert_eq!(restored.pinned_issues, vec!["AFB-2"]);
    }

    #[test]
    fn legacy_board_settings_have_no_pins() {
        let board: BoardSettings = serde_json::from_value(serde_json::json!({
            "projects": ["ART"], "columns": []
        })).unwrap();
        assert!(board.pinned_issues.is_empty());
    }

    #[test]
    fn rename_and_reset_preserve_projects_but_restore_default_grouping() {
        let art = status("ART", "1", "Open");
        let afb = status("AFB", "2", "Open");
        let review = status("ART", "3", "In Review");
        let mut board = BoardSettings {
            projects: vec!["ART".into(), "AFB".into()],
            ..BoardSettings::default()
        };
        board.reconcile(&[art.clone(), afb.clone(), review.clone()]);
        assert!(!board.rename_column(0, "   "));
        assert!(board.rename_column(0, "Ready"));
        assert_eq!(board.columns[0].title, "Ready");
        board.move_status(&afb.reference(), Some(1), &afb.name);
        board.reset_columns(&[art, afb, review]);
        assert_eq!(board.projects, vec!["ART", "AFB"]);
        assert_eq!(board.columns.len(), 2);
        assert_eq!(board.columns[0].title, "Open");
        assert_eq!(board.columns[0].statuses.len(), 2);
    }

    #[test]
    fn per_site_roundtrip() {
        let mut prefs = Preferences::default();
        prefs.board_by_site.insert("first".into(), BoardSettings { projects: vec!["ART".into()], ..BoardSettings::default() });
        prefs.view_by_site.insert("first".into(), ViewSettings {
            scope: IssueScope::Unassigned,
            active_sprint_only: true,
            project_filter: Some("ART".into()),
            search: "regression".into(),
            collapsed_columns: HashSet::from(["In Review".into()]),
            open_issue: Some("ART-42".into()),
        });
        prefs.view_by_site.insert("second".into(), ViewSettings { scope: IssueScope::All, ..ViewSettings::default() });
        prefs.ui_layout.sidebar_width = 300;
        let decoded: Preferences = serde_json::from_slice(&serde_json::to_vec(&prefs).unwrap()).unwrap();
        assert_eq!(decoded.board_by_site["first"].projects, vec!["ART"]);
        assert_eq!(decoded.view_by_site["first"], prefs.view_by_site["first"]);
        assert_eq!(decoded.view_by_site["second"].scope, IssueScope::All);
        assert_eq!(decoded.view_by_site["first"].open_issue.as_deref(), Some("ART-42"));
        assert_eq!(decoded.view_by_site["second"].open_issue, None);
        assert_eq!(decoded.ui_layout.sidebar_width, 300);
    }

    #[test]
    fn remembers_worktree_origin_only_for_matching_repository_and_branch() {
        let mut settings = GitSettings::default();
        settings.remember_worktree_origin("/repo", "/trees/a", "ART-12-fix", "release/2.7");
        let restored: GitSettings = serde_json::from_slice(
            &serde_json::to_vec(&settings).unwrap(),
        ).unwrap();
        assert_eq!(
            restored.worktree_origin("/repo", "/trees/a", "ART-12-fix"),
            Some("release/2.7"),
        );
        assert_eq!(restored.worktree_origin("/other", "/trees/a", "ART-12-fix"), None);
        assert_eq!(restored.worktree_origin("/repo", "/trees/a", "ART-13-fix"), None);
        let mut restored = restored;
        restored.forget_worktree_origin("/trees/a");
        assert!(restored.worktree_origins.is_empty());
    }

    #[test]
    fn codex_instructions_are_optional_and_persisted() {
        let old: GitSettings = serde_json::from_value(serde_json::json!({
            "editor": "code",
        })).unwrap();
        assert!(old.codex_instructions.is_empty());

        let configured = GitSettings {
            codex_instructions: "Use $superpowers:using-superpowers.\nFollow clean code rules.".into(),
            ..GitSettings::default()
        };
        let restored: GitSettings = serde_json::from_slice(
            &serde_json::to_vec(&configured).unwrap(),
        ).unwrap();
        assert_eq!(restored.codex_instructions, configured.codex_instructions);
    }

    #[test]
    fn old_board_settings_without_ui_keys_still_load() {
        let old = serde_json::json!({
            "board_by_site": { "https://jira.example": {"projects": ["ART"]} }
        });
        let prefs: Preferences = serde_json::from_value(old).unwrap();
        assert_eq!(prefs.board_by_site["https://jira.example"].projects, vec!["ART"]);
        assert!(prefs.view_by_site.is_empty());
        let old_view: ViewSettings = serde_json::from_value(serde_json::json!({
            "scope": "unassigned", "search": "bug"
        })).unwrap();
        assert_eq!(old_view.open_issue, None);
        assert_eq!(prefs.ui_layout, UiLayout::default());
    }

    #[test]
    fn saved_layout_keeps_user_resized_detail_width() {
        let mut prefs = Preferences::default();
        prefs.ui_layout.detail_width = 1180;
        let restored: Preferences =
            serde_json::from_slice(&serde_json::to_vec(&prefs).unwrap()).unwrap();
        assert_eq!(restored.ui_layout.sanitized().detail_width, 1180);
    }

    #[test]
    fn layout_limits_invalid_saved_dimensions() {
        let layout = UiLayout {
            window_width: -1, window_height: 100,
            sidebar_width: -20, detail_width: 99,
            ..UiLayout::default()
        };
        assert_eq!(layout.sanitized(), UiLayout {
            window_width: 960, window_height: 580,
            sidebar_width: 184, detail_width: 320,
            ..UiLayout::default()
        });
    }
}
