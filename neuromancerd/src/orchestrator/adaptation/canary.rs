use std::collections::HashMap;

use neuromancer_core::config::SelfImprovementThresholds;
use neuromancer_core::rpc::DelegatedRun;
use serde::{Deserialize, Serialize};

use crate::orchestrator::proposals::model::SkillQualityStats;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CanaryMetrics {
    pub total_runs: usize,
    pub failed_runs: usize,
    pub success_rate_pct: f64,
    pub tool_failure_rate_pct: f64,
    pub policy_denial_rate_pct: f64,
}

pub fn canary_metrics(
    runs_index: &HashMap<String, DelegatedRun>,
    skill_quality_stats: &HashMap<String, SkillQualityStats>,
) -> CanaryMetrics {
    let total_runs = runs_index.len();
    let failed_runs = runs_index
        .values()
        .filter(|run| run.state == "failed")
        .count();

    let success_runs = total_runs.saturating_sub(failed_runs);
    let success_rate_pct = if total_runs == 0 {
        100.0
    } else {
        (success_runs as f64 / total_runs as f64) * 100.0
    };

    let total_tool_calls: u64 = skill_quality_stats
        .values()
        .map(|stat| stat.invocations)
        .sum();
    let total_tool_failures: u64 = skill_quality_stats.values().map(|stat| stat.failures).sum();
    let tool_failure_rate_pct = if total_tool_calls == 0 {
        0.0
    } else {
        (total_tool_failures as f64 / total_tool_calls as f64) * 100.0
    };

    CanaryMetrics {
        total_runs,
        failed_runs,
        success_rate_pct,
        tool_failure_rate_pct,
        policy_denial_rate_pct: 0.0,
    }
}

pub fn rollback_reason(
    before: &CanaryMetrics,
    after: &CanaryMetrics,
    thresholds: &SelfImprovementThresholds,
    payload: &serde_json::Value,
) -> Option<String> {
    if payload
        .get("simulate_regression")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Some("simulated regression requested".to_string());
    }

    let success_drop = before.success_rate_pct - after.success_rate_pct;
    if success_drop > thresholds.max_success_rate_drop_pct {
        return Some(format!(
            "success rate dropped by {:.2}% (threshold {:.2}%)",
            success_drop, thresholds.max_success_rate_drop_pct
        ));
    }

    let tool_failure_increase = after.tool_failure_rate_pct - before.tool_failure_rate_pct;
    if tool_failure_increase > thresholds.max_tool_failure_increase_pct {
        return Some(format!(
            "tool failure rate increased by {:.2}% (threshold {:.2}%)",
            tool_failure_increase, thresholds.max_tool_failure_increase_pct
        ));
    }

    let policy_denial_increase = after.policy_denial_rate_pct - before.policy_denial_rate_pct;
    if policy_denial_increase > thresholds.max_policy_denial_increase_pct {
        return Some(format!(
            "policy denial rate increased by {:.2}% (threshold {:.2}%)",
            policy_denial_increase, thresholds.max_policy_denial_increase_pct
        ));
    }

    None
}
