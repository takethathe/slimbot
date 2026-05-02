use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Manages all application file paths with default resolution and security validation.
///
/// Path resolution priority:
/// - `data_dir`: explicit > default (`~/.slimbot`)
/// - `workspace_dir`: explicit > derived (`{data_dir}/workspace`)
/// - `config_path`: explicit (must exist) > default (`{data_dir}/config.json`)
#[derive(Debug)]
pub struct PathManager {
    config_path: PathBuf,
    data_dir: PathBuf,
    workspace_dir: PathBuf,
}

impl PathManager {
    /// Resolve and validate all application paths.
    ///
    /// - `config`: if set, must point to an existing file; otherwise defaults to `{data_dir}/config.json`.
    /// - `data_dir`: if set, used as-is; otherwise defaults to `~/.slimbot`.
    /// - `workspace_dir`: if set, used as-is; otherwise derived from `data_dir`.
    pub fn resolve(
        config: Option<&str>,
        data_dir: Option<&str>,
        workspace_dir: Option<&str>,
    ) -> Result<Self> {
        let explicit_workspace = workspace_dir.is_some();
        let resolved_data_dir = Self::resolve_data_dir(data_dir);
        let resolved_workspace = Self::resolve_workspace_dir(resolved_data_dir.clone(), workspace_dir);
        let config_path = Self::resolve_config_path(&resolved_data_dir, config)?;

        let data_dir = Self::ensure_dir(&resolved_data_dir)?;
        let workspace_dir = Self::ensure_dir(&resolved_workspace)?;

        // Validate workspace is under data_dir when explicitly provided
        if explicit_workspace && !workspace_dir.starts_with(&data_dir) {
            anyhow::bail!(
                "workspace_dir ({}) must be under data_dir ({})",
                workspace_dir.display(),
                data_dir.display()
            );
        }

        Ok(Self {
            config_path,
            data_dir,
            workspace_dir,
        })
    }

    fn resolve_data_dir(input: Option<&str>) -> PathBuf {
        match input {
            Some(dir) => expand_home(dir),
            None => default_data_dir(),
        }
    }

    fn resolve_workspace_dir(data_dir: PathBuf, input: Option<&str>) -> PathBuf {
        match input {
            Some(dir) => PathBuf::from(dir),
            None => data_dir.join("workspace"),
        }
    }

    fn resolve_config_path(data_dir: &Path, input: Option<&str>) -> Result<PathBuf> {
        match input {
            Some(path) => {
                let p = PathBuf::from(path);
                if !p.exists() {
                    anyhow::bail!(
                        "Config file not found: {} (use `setup` to create one)",
                        p.display()
                    );
                }
                Ok(p)
            }
            None => Ok(data_dir.join("config.json")),
        }
    }

    fn ensure_dir(path: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(path).context("Failed to create directory")?;
        path.canonicalize()
            .context("Failed to canonicalize directory")
    }

    // -- Accessors --

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn workspace_dir(&self) -> &Path {
        &self.workspace_dir
    }

    pub fn session_dir(&self) -> PathBuf {
        self.workspace_dir.join("sessions")
    }

    pub fn skills_dir(&self) -> PathBuf {
        self.workspace_dir.join("skills")
    }

    pub fn memory_dir(&self) -> PathBuf {
        self.workspace_dir.join("memory")
    }

    pub fn tool_results_dir(&self) -> PathBuf {
        self.workspace_dir.join(".tool_results")
    }

    pub fn bootstrap_file(&self, name: &str) -> PathBuf {
        self.workspace_dir.join(name)
    }

    /// Validate that a user-provided path stays within the workspace directory.
    pub fn validate_path_sandbox(&self, user_path: &str) -> Result<PathBuf> {
        let workspace_abs = self.workspace_dir.canonicalize().context(
            "Workspace directory does not exist or cannot be accessed",
        )?;

        let clean = user_path.trim_start_matches('/');
        let joined = workspace_abs.join(clean);

        if let Ok(resolved) = joined.canonicalize() {
            if !resolved.starts_with(&workspace_abs) {
                anyhow::bail!("Path escapes workspace directory: {}", user_path);
            }
            return Ok(resolved);
        }

        // For non-existent paths, walk up to the nearest existing ancestor.
        let mut ancestor = joined.clone();
        while !ancestor.exists() {
            if let Some(parent) = ancestor.parent() {
                ancestor = parent.to_path_buf();
            } else {
                break;
            }
        }

        let ancestor_abs = ancestor.canonicalize().map_err(|e| {
            anyhow::anyhow!("Cannot resolve base directory for '{}': {}", user_path, e)
        })?;

        if !ancestor_abs.starts_with(&workspace_abs) {
            anyhow::bail!("Path escapes workspace directory: {}", user_path);
        }

        let remaining = joined.strip_prefix(&ancestor).map_err(|_| {
            anyhow::anyhow!("Cannot resolve path relative to workspace: {}", user_path)
        })?;
        for comp in remaining.components() {
            if let std::path::Component::ParentDir = comp {
                anyhow::bail!(
                    "Path escapes workspace directory via '..': {}",
                    user_path
                );
            }
        }

        Ok(joined)
    }
}

/// Expand a leading `~` or `~/` in a path to the user's home directory.
fn expand_home(path: &str) -> PathBuf {
    if path == "~" || path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            let rest = path.trim_start_matches('~');
            return home.join(rest.trim_start_matches('/'));
        }
    }
    PathBuf::from(path)
}

fn default_data_dir() -> PathBuf {
    dirs::home_dir()
        .map(|p| p.join(".slimbot"))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_expand_home() {
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expand_home("~/.slimbot"), home.join(".slimbot"));
            assert_eq!(expand_home("~"), home);
            assert_eq!(expand_home("~/foo/bar"), home.join("foo/bar"));
        }
        assert_eq!(expand_home("/absolute/path"), PathBuf::from("/absolute/path"));
        assert_eq!(expand_home("relative/path"), PathBuf::from("relative/path"));
    }

    #[test]
    fn test_resolve_defaults() {
        let pm = PathManager::resolve(None, None, None).unwrap();
        assert!(pm.config_path().ends_with("config.json"));
        assert!(pm.data_dir().ends_with(".slimbot"));
        assert!(pm.workspace_dir().ends_with("workspace"));
        assert!(pm.session_dir().ends_with("sessions"));
        assert!(pm.skills_dir().ends_with("skills"));
        assert!(pm.memory_dir().ends_with("memory"));
    }

    #[test]
    fn test_resolve_custom_data_dir_derives_workspace() {
        let tmp = tempdir().unwrap();
        let data = tmp.path().join("mydata");
        let pm = PathManager::resolve(None, Some(data.to_str().unwrap()), None).unwrap();
        assert!(pm.data_dir().ends_with("mydata"));
        assert!(pm.workspace_dir().ends_with("mydata/workspace"));
    }

    #[test]
    fn test_resolve_custom_workspace() {
        let tmp = tempdir().unwrap();
        let data = tmp.path().join("data");
        // workspace must be under data_dir
        let ws = data.join("custom_ws");
        let pm = PathManager::resolve(
            None,
            Some(data.to_str().unwrap()),
            Some(ws.to_str().unwrap()),
        )
        .unwrap();
        assert!(pm.workspace_dir().ends_with("data/custom_ws"));
    }

    #[test]
    fn test_resolve_config_not_found() {
        let result = PathManager::resolve(Some("/nonexistent/config.json"), None, None);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Config file not found"));
    }

    #[test]
    fn test_resolve_config_existing() {
        let tmp = tempdir().unwrap();
        let data = tmp.path().join("data");
        let config_file = data.join("config.json");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(&config_file, "{}").unwrap();

        let pm = PathManager::resolve(
            Some(config_file.to_str().unwrap()),
            Some(data.to_str().unwrap()),
            None,
        )
        .unwrap();
        assert!(pm.config_path().ends_with("data/config.json"));
    }

    #[test]
    fn test_workspace_escape_rejected() {
        let tmp = tempdir().unwrap();
        let data = tmp.path().join("data");
        // workspace outside data dir
        let ws = tmp.path().join("outside_ws");
        let result = PathManager::resolve(
            None,
            Some(data.to_str().unwrap()),
            Some(ws.to_str().unwrap()),
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("must be under data_dir"));
    }

    #[test]
    fn test_validate_path_sandbox() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        let pm = PathManager::resolve(None, Some(ws.to_str().unwrap()), None).unwrap();

        // Valid subpath
        let resolved = pm.validate_path_sandbox("subdir/file.txt").unwrap();
        assert!(resolved.starts_with(pm.workspace_dir()));

        // Path traversal attempt
        let result = pm.validate_path_sandbox("../escape.txt");
        assert!(result.is_err());
    }

    #[test]
    fn test_bootstrap_file_path() {
        let tmp = tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();

        let pm = PathManager::resolve(None, Some(data.to_str().unwrap()), None).unwrap();
        assert!(pm.bootstrap_file("SOUL.md").ends_with("workspace/SOUL.md"));
        assert!(pm.bootstrap_file("AGENTS.md").ends_with("workspace/AGENTS.md"));
    }
}
