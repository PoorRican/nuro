use neuromancer_core::error::{InfraError, NeuromancerError};
use neuromancer_core::proposal::ChangeProposal;

pub use neuromancer_core::security::ExecutionGuard;

#[derive(Default)]
pub struct PlaceholderExecutionGuard;

impl ExecutionGuard for PlaceholderExecutionGuard {
    fn pre_verify_proposal(&self, proposal: &ChangeProposal) -> Result<(), NeuromancerError> {
        guard_sandbox(proposal.required_safeguards())
    }

    fn pre_apply_proposal(&self, proposal: &ChangeProposal) -> Result<(), NeuromancerError> {
        guard_sandbox(proposal.required_safeguards())
    }

    fn pre_skill_script_execution(
        &self,
        _skill_id: &str,
        required_safeguards: &[String],
    ) -> Result<(), NeuromancerError> {
        guard_sandbox(required_safeguards)
    }
}

fn guard_sandbox(required_safeguards: &[String]) -> Result<(), NeuromancerError> {
    if requires_unimplemented_sandbox(required_safeguards) {
        return Err(NeuromancerError::Infra(InfraError::ContainerRuntime(
            "blocked_missing_sandbox".to_string(),
        )));
    }
    Ok(())
}

pub fn requires_unimplemented_sandbox(required_safeguards: &[String]) -> bool {
    required_safeguards.iter().any(|safeguard| {
        let normalized = safeguard.to_ascii_lowercase();
        normalized.contains("sandbox") || normalized.contains("container_required")
    })
}
