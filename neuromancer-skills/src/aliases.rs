use std::collections::{HashMap, HashSet};

pub fn build_skill_tool_aliases(
    allowed_skills: &[String],
) -> (HashMap<String, String>, HashMap<String, Vec<String>>) {
    let canonical_skills: HashSet<&str> = allowed_skills.iter().map(String::as_str).collect();
    let mut alias_to_canonical = HashMap::<String, String>::new();
    let mut aliases_by_tool = HashMap::<String, Vec<String>>::new();

    for canonical in allowed_skills {
        let alias = canonical.replace('-', "_");
        if alias == *canonical {
            continue;
        }

        if canonical_skills.contains(alias.as_str()) {
            continue;
        }

        if alias_to_canonical.contains_key(&alias) {
            continue;
        }

        alias_to_canonical.insert(alias.clone(), canonical.clone());
        aliases_by_tool
            .entry(canonical.clone())
            .or_default()
            .push(alias);
    }

    (alias_to_canonical, aliases_by_tool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_underscore_variants() {
        let (alias_to_canonical, aliases_by_tool) =
            build_skill_tool_aliases(&["manage-bills".to_string(), "manage-accounts".to_string()]);

        assert_eq!(
            alias_to_canonical.get("manage_bills"),
            Some(&"manage-bills".to_string())
        );
        assert_eq!(
            alias_to_canonical.get("manage_accounts"),
            Some(&"manage-accounts".to_string())
        );
        assert_eq!(
            aliases_by_tool.get("manage-bills"),
            Some(&vec!["manage_bills".to_string()])
        );
        assert_eq!(
            aliases_by_tool.get("manage-accounts"),
            Some(&vec!["manage_accounts".to_string()])
        );
    }

    #[test]
    fn skips_conflicting_names() {
        let (alias_to_canonical, aliases_by_tool) =
            build_skill_tool_aliases(&["manage-bills".to_string(), "manage_bills".to_string()]);

        assert!(!alias_to_canonical.contains_key("manage_bills"));
        assert!(!aliases_by_tool.contains_key("manage-bills"));
    }
}
