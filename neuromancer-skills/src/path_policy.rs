use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::SkillError;

pub fn resolve_local_data_path(local_root: &Path, relative: &str) -> Result<PathBuf, SkillError> {
    resolve_relative_path_under_root(local_root, relative, "data file")
}

pub fn resolve_skill_script_path(skill_root: &Path, relative: &str) -> Result<PathBuf, SkillError> {
    resolve_relative_path_under_root(skill_root, relative, "skill script")
}

fn resolve_relative_path_under_root(
    root: &Path,
    relative: &str,
    file_type: &str,
) -> Result<PathBuf, SkillError> {
    let input = Path::new(relative);
    if input.is_absolute() {
        return Err(SkillError::PathViolation(format!(
            "absolute paths are not allowed: {relative}"
        )));
    }

    for component in input.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(SkillError::PathViolation(format!(
                "path traversal is not allowed: {relative}"
            )));
        }
    }

    let root_canonical = fs::canonicalize(root).map_err(|err| {
        SkillError::PathUnavailable(format!(
            "{} root '{}' is unavailable: {err}",
            file_type,
            root.display()
        ))
    })?;

    let full_path = root.join(input);
    let target_canonical = fs::canonicalize(&full_path).map_err(|err| {
        SkillError::PathUnavailable(format!(
            "{} '{}' is unavailable: {err}",
            file_type,
            full_path.display()
        ))
    })?;

    if !target_canonical.starts_with(&root_canonical) {
        return Err(SkillError::PathViolation(format!(
            "resolved path '{}' escapes '{}'",
            target_canonical.display(),
            root_canonical.display()
        )));
    }

    Ok(target_canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("{}_{}", prefix, uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn resolve_local_data_path_rejects_absolute_paths() {
        let local_root = temp_dir("nm_local_root");
        let absolute = local_root.join("data/accounts.csv");
        let err = resolve_local_data_path(&local_root, absolute.to_string_lossy().as_ref())
            .expect_err("absolute path should be rejected");
        assert!(matches!(err, SkillError::PathViolation(_)));
    }

    #[test]
    fn resolve_local_data_path_rejects_parent_traversal() {
        let local_root = temp_dir("nm_local_root");
        let err = resolve_local_data_path(&local_root, "../secrets.txt")
            .expect_err("parent traversal should be rejected");
        assert!(matches!(err, SkillError::PathViolation(_)));
    }

    #[test]
    fn resolve_local_data_path_accepts_valid_relative_path() {
        let local_root = temp_dir("nm_local_root");
        let data_dir = local_root.join("data");
        fs::create_dir_all(&data_dir).expect("data dir");
        let file_path = data_dir.join("accounts.csv");
        fs::write(&file_path, "account,balance\nchecking,1200").expect("write");

        let resolved = resolve_local_data_path(&local_root, "data/accounts.csv")
            .expect("relative path should resolve");
        assert_eq!(resolved, fs::canonicalize(file_path).expect("canonical"));
    }

    #[test]
    fn resolve_skill_script_path_rejects_absolute_paths() {
        let skill_root = temp_dir("nm_skill_root");
        let absolute = skill_root.join("scripts/run.py");
        let err = resolve_skill_script_path(&skill_root, absolute.to_string_lossy().as_ref())
            .expect_err("absolute path should be rejected");
        assert!(matches!(err, SkillError::PathViolation(_)));
    }

    #[test]
    fn resolve_skill_script_path_rejects_parent_traversal() {
        let skill_root = temp_dir("nm_skill_root");
        let err = resolve_skill_script_path(&skill_root, "../run.py")
            .expect_err("parent traversal should be rejected");
        assert!(matches!(err, SkillError::PathViolation(_)));
    }

    #[test]
    fn resolve_skill_script_path_accepts_valid_relative_path() {
        let skill_root = temp_dir("nm_skill_root");
        let script_dir = skill_root.join("scripts");
        fs::create_dir_all(&script_dir).expect("script dir");
        let script = script_dir.join("run.py");
        fs::write(&script, "print('{}')").expect("script write");

        let resolved = resolve_skill_script_path(&skill_root, "scripts/run.py")
            .expect("relative script path should resolve");
        assert_eq!(resolved, fs::canonicalize(script).expect("canonical"));
    }
}
