use std::collections::{HashMap, HashSet};

use neuromancer_core::config::SelfImprovementConfig;

use crate::orchestrator::proposals::model::SkillQualityStats;

#[derive(Clone)]
pub(crate) struct SelfImprovementState {
    pub(crate) config: SelfImprovementConfig,
    pub(crate) allowlisted_tools: HashSet<String>,
    pub(crate) known_skill_ids: HashSet<String>,
    pub(crate) managed_skills: HashMap<String, String>,
    pub(crate) managed_agents: HashMap<String, serde_json::Value>,
    pub(crate) config_snapshot: serde_json::Value,
    pub(crate) config_patch_history: Vec<String>,
    pub(crate) last_known_good_snapshot: serde_json::Value,
    pub(crate) lessons_learned: Vec<String>,
    pub(crate) skill_quality_stats: HashMap<String, SkillQualityStats>,
}

impl SelfImprovementState {
    pub(crate) fn new(
        config: SelfImprovementConfig,
        allowlisted_tools: HashSet<String>,
        known_skill_ids: HashSet<String>,
        config_snapshot: serde_json::Value,
    ) -> Self {
        Self {
            config,
            allowlisted_tools,
            known_skill_ids,
            managed_skills: HashMap::new(),
            managed_agents: HashMap::new(),
            config_snapshot: config_snapshot.clone(),
            config_patch_history: Vec::new(),
            last_known_good_snapshot: config_snapshot,
            lessons_learned: Vec::new(),
            skill_quality_stats: HashMap::new(),
        }
    }
}
