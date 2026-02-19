use crate::error::NeuromancerError;
use crate::proposal::ChangeProposal;

pub trait ExecutionGuard: Send + Sync {
    fn pre_verify_proposal(&self, proposal: &ChangeProposal) -> Result<(), NeuromancerError>;
    fn pre_apply_proposal(&self, proposal: &ChangeProposal) -> Result<(), NeuromancerError>;
    fn pre_skill_script_execution(
        &self,
        skill_id: &str,
        required_safeguards: &[String],
    ) -> Result<(), NeuromancerError>;
}
