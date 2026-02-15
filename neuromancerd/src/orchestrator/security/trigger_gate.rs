use neuromancer_core::trigger::TriggerType;

pub fn ensure_admin_trigger(
    trigger_type: TriggerType,
    require_admin: bool,
    action: &str,
) -> Result<(), String> {
    if require_admin && trigger_type != TriggerType::Admin {
        return Err(format!("{action} requires an admin trigger"));
    }
    Ok(())
}
