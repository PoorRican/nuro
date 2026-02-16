pub(crate) use neuromancer_skills::path_policy::{resolve_local_data_path, resolve_skill_script_path};

#[cfg(test)]
mod tests {
    use super::*;
    use neuromancer_skills::SkillError;

    fn temp_dir(prefix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("{}_{}", prefix, uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
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
        std::fs::create_dir_all(&data_dir).expect("data dir");
        let file_path = data_dir.join("accounts.csv");
        std::fs::write(&file_path, "account,balance\nchecking,1200").expect("write");

        let resolved = resolve_local_data_path(&local_root, "data/accounts.csv")
            .expect("relative path should resolve");
        assert_eq!(resolved, std::fs::canonicalize(file_path).expect("canonical"));
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
        std::fs::create_dir_all(&script_dir).expect("script dir");
        let script = script_dir.join("run.py");
        std::fs::write(&script, "print('{}')").expect("script write");

        let resolved = resolve_skill_script_path(&skill_root, "scripts/run.py")
            .expect("relative script path should resolve");
        assert_eq!(resolved, std::fs::canonicalize(script).expect("canonical"));
    }
}
