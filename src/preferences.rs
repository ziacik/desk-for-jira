use std::{
    collections::HashMap,
    env,
    fs,
    io::ErrorKind,
    path::PathBuf,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
struct Preferences {
    #[serde(default)]
    pinned_by_site: HashMap<String, Vec<u64>>,
}

fn preferences_path() -> Result<PathBuf> {
    let root = env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    let Some(root) = root else {
        bail!("Could not find the user configuration directory");
    };
    Ok(root.join("desk-for-jira").join("pinned-boards.json"))
}

fn read_preferences(path: &PathBuf) -> Result<Preferences> {
    match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .with_context(|| format!("Invalid settings file: {}", path.display())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Preferences::default()),
        Err(error) => Err(error).with_context(|| format!("Could not read {}", path.display())),
    }
}

pub fn load_pinned(base_url: &str) -> Result<Vec<u64>> {
    let prefs = read_preferences(&preferences_path()?)?;
    Ok(prefs.pinned_by_site.get(base_url).cloned().unwrap_or_default())
}

pub fn save_pinned(base_url: &str, boards: &[u64]) -> Result<()> {
    let path = preferences_path()?;
    let mut prefs = read_preferences(&path)?;
    prefs.pinned_by_site.insert(base_url.to_owned(), boards.to_vec());
    let parent = path.parent().context("Missing settings directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("Could not create {}", parent.display()))?;
    // Use an atomic rename to avoid leaving half-written JSON after a crash.
    let temp = parent.join(format!("pinned-boards-{}.tmp", std::process::id()));
    fs::write(&temp, serde_json::to_vec_pretty(&prefs)?)
        .with_context(|| format!("Could not write {}", temp.display()))?;
    if let Err(error) = fs::rename(&temp, &path) {
        let _ = fs::remove_file(&temp);
        return Err(error).with_context(|| format!("Could not save {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sites_keep_independent_selections() {
        let mut config = Preferences::default();
        config.pinned_by_site.insert("https://first.example".into(), vec![3, 1]);
        config.pinned_by_site.insert("https://second.example".into(), vec![9]);
        let decoded: Preferences = serde_json::from_slice(
            &serde_json::to_vec(&config).unwrap()
        ).unwrap();
        assert_eq!(decoded.pinned_by_site["https://first.example"], vec![3, 1]);
        assert_eq!(decoded.pinned_by_site["https://second.example"], vec![9]);
    }
}
