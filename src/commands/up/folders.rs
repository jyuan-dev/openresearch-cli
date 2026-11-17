//! Lists a folder's subfolders, so a browser on another device (or a dashboard
//! served by a background service, where no native picker can open) can still
//! choose an existing project folder on this machine.

use std::path::Path;

use axum::{extract::Query, routing::get, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{bad_request, ApiError, ApiResult, AppState};
use crate::error::anyhow;

#[derive(Deserialize)]
struct BrowseQ {
    path: Option<String>,
    #[serde(default)]
    hidden: bool,
}

pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/api/project-path/browse", get(browse))
}

async fn browse(Query(q): Query<BrowseQ>) -> ApiResult {
    tokio::task::spawn_blocking(move || listing(q.path.as_deref(), q.hidden))
        .await
        .map_err(|error| ApiError::from(anyhow!("folder listing task failed: {error}")))?
        .map(Json)
}

/// The folder `path` names (home when none) with its subfolders, sorted by name.
/// Hidden folders are left out unless `hidden`.
fn listing(path: Option<&str>, hidden: bool) -> Result<Value, ApiError> {
    let home = crate::config::home_dir();
    let asked = match path.map(str::trim).filter(|path| !path.is_empty()) {
        Some(path) => crate::local::projects::expand_path(path).map_err(bad_request)?,
        None => home
            .clone()
            .ok_or_else(|| bad_request("Could not resolve the home directory"))?,
    };
    let dir = asked
        .canonicalize()
        .map_err(|error| bad_request(format!("Cannot open {}: {error}", asked.display())))?;
    if !dir.is_dir() {
        return Err(bad_request(format!("{} is not a folder", dir.display())));
    }
    let entries = std::fs::read_dir(&dir)
        .map_err(|error| bad_request(format!("Cannot read {}: {error}", dir.display())))?;
    let mut folders: Vec<(String, Value)> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            // `is_dir` follows links, so a linked data folder is listed too.
            if (!hidden && name.starts_with('.')) || !path.is_dir() {
                return None;
            }
            let row = json!({"name": name, "path": path.to_string_lossy(), "git": is_repo(&path)});
            Some((name.to_lowercase(), row))
        })
        .collect();
    folders.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(json!({
        "path": dir.to_string_lossy(),
        "parent": dir.parent().map(|parent| parent.to_string_lossy().into_owned()),
        "home": home.map(|home| home.to_string_lossy().into_owned()),
        "git": is_repo(&dir),
        "folders": folders.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
    }))
}

fn is_repo(path: &Path) -> bool {
    path.join(".git").exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder under the system temp dir, removed when dropped.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("orx-folders-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_listing_shows_subfolders_by_name_and_marks_git_repositories() {
        let root = Scratch::new();
        for dir in ["beta", "Alpha/.git", ".cache"] {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        std::fs::write(root.path().join("notes.txt"), "").unwrap();
        let listed = listing(root.path().to_str(), false).unwrap();
        let names: Vec<&str> = listed["folders"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["Alpha", "beta"],
            "files and hidden folders are left out"
        );
        assert_eq!(listed["folders"][0]["git"], true);
        assert_eq!(listed["folders"][1]["git"], false);
        let root_path = root.path().canonicalize().unwrap();
        assert_eq!(listed["path"], root_path.to_string_lossy().as_ref());
        assert_eq!(
            listed["parent"],
            root_path.parent().unwrap().to_string_lossy().as_ref()
        );
        let with_hidden = listing(root.path().to_str(), true).unwrap();
        assert_eq!(with_hidden["folders"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn a_missing_folder_or_a_file_is_refused() {
        let root = Scratch::new();
        std::fs::write(root.path().join("notes.txt"), "").unwrap();
        assert!(listing(root.path().join("gone").to_str(), false).is_err());
        assert!(listing(root.path().join("notes.txt").to_str(), false).is_err());
    }
}
