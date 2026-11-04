use std::{collections::{HashMap, HashSet}, env, fs, io::ErrorKind, path::PathBuf};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use crate::jira::ProjectStatus;

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
}
impl ProjectStatus {
    pub fn reference(&self) -> StatusRef {
        StatusRef { project_key: self.project_key.clone(), status_id: self.id.clone() }
    }
}
impl BoardSettings {
    /// Use actual workflow status IDs, never Jira board columns or matching names.
    pub fn reconcile(&mut self, available: &[ProjectStatus]) {
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

    pub fn move_column(&mut self, index: usize, up: bool) {
        let other = if up { index.checked_sub(1) } else { index.checked_add(1) };
        if let Some(other) = other.filter(|i| *i < self.columns.len()) {
            self.columns.swap(index, other);
        }
    }
}
#[derive(Default, Serialize, Deserialize)]
struct Preferences {
    #[serde(default)]
    board_by_site: HashMap<String, BoardSettings>,
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
pub fn save(base_url: &str, settings: &BoardSettings) -> Result<()> {
    let path = preferences_path()?;
    let mut prefs = read_preferences(&path)?;
    prefs.board_by_site.insert(base_url.into(), settings.clone());
    let parent = path.parent().context("Missing settings directory")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!("board-settings-{}.tmp", std::process::id()));
    fs::write(&temp, serde_json::to_vec_pretty(&prefs)?)?;
    if let Err(error) = fs::rename(&temp, &path) {
        let _ = fs::remove_file(&temp);
        return Err(error).with_context(|| format!("Could not save {}", path.display()));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn status(project: &str, id: &str, name: &str) -> ProjectStatus {
        ProjectStatus { project_key: project.into(), id: id.into(), name: name.into() }
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
        let decoded: Preferences = serde_json::from_slice(&serde_json::to_vec(&prefs).unwrap()).unwrap();
        assert_eq!(decoded.board_by_site["first"].projects, vec!["ART"]);
    }
}
