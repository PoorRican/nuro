use crate::orchestrator::error::OrchestratorRuntimeError;
use crate::orchestrator::proposals::model::ChangeProposal;

pub trait ExecutionGuard: Send + Sync {
    fn pre_verify_proposal(
        &self,
        proposal: &ChangeProposal,
    ) -> Result<(), OrchestratorRuntimeError>;
    fn pre_apply_proposal(&self, proposal: &ChangeProposal)
    -> Result<(), OrchestratorRuntimeError>;
    fn pre_skill_script_execution(
        &self,
        skill_id: &str,
        required_safeguards: &[String],
    ) -> Result<(), OrchestratorRuntimeError>;
}

#[derive(Default)]
pub struct PlaceholderExecutionGuard;

impl ExecutionGuard for PlaceholderExecutionGuard {
    fn pre_verify_proposal(
        &self,
        proposal: &ChangeProposal,
    ) -> Result<(), OrchestratorRuntimeError> {
        if requires_unimplemented_sandbox(proposal.required_safeguards()) {
            return Err(OrchestratorRuntimeError::GuardBlocked(
                "blocked_missing_sandbox".to_string(),
            ));
        }
        Ok(())
    }

    fn pre_apply_proposal(
        &self,
        proposal: &ChangeProposal,
    ) -> Result<(), OrchestratorRuntimeError> {
        if requires_unimplemented_sandbox(proposal.required_safeguards()) {
            return Err(OrchestratorRuntimeError::GuardBlocked(
                "blocked_missing_sandbox".to_string(),
            ));
        }
        Ok(())
    }

    fn pre_skill_script_execution(
        &self,
        _skill_id: &str,
        required_safeguards: &[String],
    ) -> Result<(), OrchestratorRuntimeError> {
        if requires_unimplemented_sandbox(required_safeguards) {
            return Err(OrchestratorRuntimeError::GuardBlocked(
                "blocked_missing_sandbox".to_string(),
            ));
        }
        Ok(())
    }
}

pub fn requires_unimplemented_sandbox(required_safeguards: &[String]) -> bool {
    required_safeguards.iter().any(|safeguard| {
        let normalized = safeguard.to_ascii_lowercase();
        normalized.contains("sandbox") || normalized.contains("container_required")
    })
}
