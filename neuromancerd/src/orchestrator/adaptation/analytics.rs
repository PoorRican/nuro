use std::collections::HashMap;

use neuromancer_core::rpc::DelegatedRun;

use crate::orchestrator::proposals::model::SkillQualityStats;

pub fn failure_clusters(runs_index: &HashMap<String, DelegatedRun>) -> serde_json::Value {
    let mut by_agent = HashMap::<String, usize>::new();
    let mut by_signature = HashMap::<String, usize>::new();
    for run in runs_index.values() {
        if run.state != "failed" {
            continue;
        }
        *by_agent.entry(run.agent_id.clone()).or_default() += 1;

        let signature = run
            .summary
            .as_deref()
            .and_then(|summary| summary.split_whitespace().next())
            .unwrap_or("unknown")
            .to_ascii_lowercase();
        *by_signature.entry(signature).or_default() += 1;
    }
    serde_json::json!({
        "by_agent": by_agent,
        "by_signature": by_signature,
    })
}

pub fn skill_scores(
    known_skills: impl IntoIterator<Item = String>,
    skill_quality_stats: &HashMap<String, SkillQualityStats>,
) -> serde_json::Value {
    let mut scores = Vec::<serde_json::Value>::new();
    for skill in known_skills {
        let stat = skill_quality_stats.get(&skill).cloned().unwrap_or_default();
        let score = if stat.invocations == 0 {
            1.0
        } else {
            1.0 - (stat.failures as f64 / stat.invocations as f64)
        };
        let stale = stat.invocations == 0;
        scores.push(serde_json::json!({
            "skill_id": skill,
            "quality_score": score,
            "invocations": stat.invocations,
            "failures": stat.failures,
            "stale_candidate": stale,
        }));
    }
    scores.sort_by(|a, b| {
        let a_id = a
            .get("skill_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let b_id = b
            .get("skill_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        a_id.cmp(b_id)
    });
    serde_json::Value::Array(scores)
}

pub fn routing_adaptation(runs_index: &HashMap<String, DelegatedRun>) -> serde_json::Value {
    let mut recommendations = Vec::<serde_json::Value>::new();
    let mut run_counts = HashMap::<String, (u64, u64)>::new();
    for run in runs_index.values() {
        let entry = run_counts.entry(run.agent_id.clone()).or_default();
        entry.0 += 1;
        if run.state == "completed" {
            entry.1 += 1;
        }
    }

    for (agent_id, (total, success)) in run_counts {
        let success_rate = if total == 0 {
            0.0
        } else {
            success as f64 / total as f64
        };
        recommendations.push(serde_json::json!({
            "agent_id": agent_id,
            "success_rate": success_rate,
            "recommended_weight": (success_rate * 100.0).round() / 100.0,
            "bounded_by_policy": true,
        }));
    }

    serde_json::Value::Array(recommendations)
}
