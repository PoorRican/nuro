pub mod aliases;
pub mod csv;
mod linter;
mod metadata;
pub mod path_policy;
mod registry;

pub use linter::{LintResult, LintSeverity, LintWarning, SkillLinter};
pub use metadata::SkillMetadata;
pub use registry::SkillRegistry;

use std::path::PathBuf;

/// A loaded skill: metadata + instructions + resource paths.
#[derive(Debug, Clone)]
pub struct Skill {
    /// Parsed frontmatter metadata.
    pub metadata: SkillMetadata,
    /// Full instruction text (body of the SKILL.md after frontmatter).
    /// None if only metadata has been loaded (progressive loading).
    pub instructions: Option<String>,
    /// Path to the skill directory.
    pub path: PathBuf,
    /// Resource files found in the skill directory.
    pub resources: Vec<PathBuf>,
}

impl Skill {
    pub fn is_fully_loaded(&self) -> bool {
        self.instructions.is_some()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("skill not found: {0}")]
    NotFound(String),

    #[error("failed to read skill file: {0}")]
    ReadError(String),

    #[error("failed to parse frontmatter: {0}")]
    ParseError(String),

    #[error("missing required field: {0}")]
    MissingField(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("lint failures: {0} warnings")]
    LintFailures(usize),

    #[error("path violation: {0}")]
    PathViolation(String),

    #[error("path unavailable: {0}")]
    PathUnavailable(String),

    #[error("csv file is empty: {0}")]
    CsvEmpty(String),

    #[error("csv column mismatch: expected {expected} columns, got {actual} in {path}")]
    CsvColumnMismatch {
        expected: usize,
        actual: usize,
        path: String,
    },
}
