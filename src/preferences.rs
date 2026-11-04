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
        for status in available {
            let reference = status.reference();
            if seen.insert(reference.clone()) {
                self.columns.push(Column { title: status.name.clone(), statuses: vec![reference] });
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
    fn identical_names_or_ids_across_projects_do_not_merge_automatically() {
        let a = status("ART", "12", "Review");
        let b = status("AFB", "12", "Review");
        let mut board = BoardSettings::default();
        board.reconcile(&[a.clone(), b.clone()]);
        assert_eq!(board.columns.len(), 2);
        board.move_status(&b.reference(), Some(0), &b.name);
        assert_eq!(board.columns.len(), 1);
        board.reconcile(&[a, b]);
        assert_eq!(board.columns[0].statuses.len(), 2);
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
        board.move_status(&afb.reference(), Some(0), &afb.name);
        board.projects = vec!["ART".into()];
        board.reconcile(&[art.clone()]);
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
    fn per_site_roundtrip() {
        let mut prefs = Preferences::default();
        prefs.board_by_site.insert("first".into(), BoardSettings { projects: vec!["ART".into()], columns: vec![] });
        let decoded: Preferences = serde_json::from_slice(&serde_json::to_vec(&prefs).unwrap()).unwrap();
        assert_eq!(decoded.board_by_site["first"].projects, vec!["ART"]);
    }
}
