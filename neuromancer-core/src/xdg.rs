use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XdgLayout {
    home_dir: PathBuf,
    config_root: PathBuf,
    runtime_root: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum XdgError {
    #[error("HOME environment variable is not set")]
    MissingHome,

    #[error("invalid path '{input}'")]
    InvalidPath { input: String },

    #[error("prompt file does not exist: {0}")]
    MissingPrompt(PathBuf),

    #[error("prompt file must have .md extension: {0}")]
    InvalidPromptExtension(PathBuf),

    #[error("prompt file is empty: {0}")]
    EmptyPrompt(PathBuf),

    #[error("failed to read prompt file '{path}': {source}")]
    PromptRead {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl XdgLayout {
    pub fn from_env() -> Result<Self, XdgError> {
        let home = std::env::var("HOME").map_err(|_| XdgError::MissingHome)?;
        let xdg_home = std::env::var("XDG_HOME")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        Ok(Self::from_home_and_xdg_home(PathBuf::from(home), xdg_home))
    }

    pub fn new(home_dir: PathBuf) -> Self {
        Self::from_home_and_xdg_home(home_dir, None)
    }

    pub fn from_home_and_xdg_home(home_dir: PathBuf, xdg_home: Option<PathBuf>) -> Self {
        let root = xdg_home.unwrap_or_else(|| home_dir.clone());
        let config_root = root.join(".config/neuromancer");
        let runtime_root = root.join(".local/neuromancer");
        Self {
            home_dir,
            config_root,
            runtime_root,
        }
    }

    pub fn from_home_and_xdg_home_str(home_dir: &str, xdg_home: Option<&str>) -> Self {
        Self::from_home_and_xdg_home(
            PathBuf::from(home_dir),
            xdg_home.map(PathBuf::from),
        )
    }

    pub fn from_home(home_dir: &str) -> Self {
        Self::from_home_and_xdg_home(PathBuf::from(home_dir), None)
    }

    pub fn home_dir(&self) -> &Path {
        &self.home_dir
    }

    pub fn config_root(&self) -> PathBuf {
        self.config_root.clone()
    }

    pub fn runtime_root(&self) -> PathBuf {
        self.runtime_root.clone()
    }

    pub fn skills_dir(&self) -> PathBuf {
        self.config_root.join("skills")
    }

    pub fn default_orchestrator_system_prompt_path(&self) -> PathBuf {
        self.config_root.join("orchestrator/SYSTEM.md")
    }

    pub fn default_agent_system_prompt_path(&self, agent_id: &str) -> PathBuf {
        self.config_root.join(format!("agents/{agent_id}/SYSTEM.md"))
    }
}

pub fn resolve_path(
    raw: Option<&str>,
    default_path: PathBuf,
    config_dir: &Path,
    home_dir: &Path,
) -> Result<PathBuf, XdgError> {
    let Some(input) = raw else {
        return Ok(default_path);
    };

    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(XdgError::InvalidPath {
            input: input.to_string(),
        });
    }

    if let Some(rest) = trimmed.strip_prefix("~/") {
        return Ok(home_dir.join(rest));
    }
    if trimmed == "~" {
        return Ok(home_dir.to_path_buf());
    }

    let path = PathBuf::from(trimmed);
    if path.is_absolute() {
        return Ok(path);
    }

    Ok(config_dir.join(path))
}

pub fn validate_markdown_prompt_file(path: &Path) -> Result<(), XdgError> {
    if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
        return Err(XdgError::InvalidPromptExtension(path.to_path_buf()));
    }

    if !path.exists() {
        return Err(XdgError::MissingPrompt(path.to_path_buf()));
    }

    let content = fs::read_to_string(path).map_err(|source| XdgError::PromptRead {
        path: path.to_path_buf(),
        source,
    })?;

    if content.trim().is_empty() {
        return Err(XdgError::EmptyPrompt(path.to_path_buf()));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_file(prefix: &str, suffix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}_{nanos}{suffix}"))
    }

    #[test]
    fn default_layout_paths_are_stable() {
        let layout = XdgLayout::new(PathBuf::from("/tmp/home"));
        assert_eq!(layout.config_root(), PathBuf::from("/tmp/home/.config/neuromancer"));
        assert_eq!(layout.runtime_root(), PathBuf::from("/tmp/home/.local/neuromancer"));
        assert_eq!(
            layout.default_orchestrator_system_prompt_path(),
            PathBuf::from("/tmp/home/.config/neuromancer/orchestrator/SYSTEM.md")
        );
        assert_eq!(
            layout.default_agent_system_prompt_path("planner"),
            PathBuf::from("/tmp/home/.config/neuromancer/agents/planner/SYSTEM.md")
        );
    }

    #[test]
    fn xdg_home_overrides_default_roots() {
        let layout = XdgLayout::from_home_and_xdg_home_str("/home/user", Some("/xdg"));
        assert_eq!(layout.config_root(), PathBuf::from("/xdg/.config/neuromancer"));
        assert_eq!(layout.runtime_root(), PathBuf::from("/xdg/.local/neuromancer"));
        assert_eq!(layout.home_dir(), Path::new("/home/user"));
    }

    #[test]
    fn resolve_path_expands_tilde() {
        let resolved = resolve_path(
            Some("~/x/y.md"),
            PathBuf::from("/fallback.md"),
            Path::new("/cfg"),
            Path::new("/home/test"),
        )
        .expect("path should resolve");
        assert_eq!(resolved, PathBuf::from("/home/test/x/y.md"));
    }

    #[test]
    fn resolve_path_uses_config_dir_for_relative() {
        let resolved = resolve_path(
            Some("prompts/SYSTEM.md"),
            PathBuf::from("/fallback.md"),
            Path::new("/cfg/root"),
            Path::new("/home/test"),
        )
        .expect("path should resolve");
        assert_eq!(resolved, PathBuf::from("/cfg/root/prompts/SYSTEM.md"));
    }

    #[test]
    fn validate_markdown_prompt_file_rejects_missing() {
        let path = unique_temp_file("missing_prompt", ".md");
        let err = validate_markdown_prompt_file(&path).expect_err("missing file must fail");
        assert!(matches!(err, XdgError::MissingPrompt(_)));
    }

    #[test]
    fn validate_markdown_prompt_file_rejects_empty() {
        let path = unique_temp_file("empty_prompt", ".md");
        fs::write(&path, " \n\t").expect("write");
        let err = validate_markdown_prompt_file(&path).expect_err("empty prompt must fail");
        assert!(matches!(err, XdgError::EmptyPrompt(_)));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn validate_markdown_prompt_file_rejects_non_markdown() {
        let path = unique_temp_file("prompt", ".txt");
        fs::write(&path, "hello").expect("write");
        let err = validate_markdown_prompt_file(&path).expect_err("non-markdown must fail");
        assert!(matches!(err, XdgError::InvalidPromptExtension(_)));
        let _ = fs::remove_file(path);
    }
}
