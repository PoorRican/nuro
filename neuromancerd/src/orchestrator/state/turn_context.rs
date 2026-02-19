use neuromancer_core::trigger::TriggerType;

pub(crate) struct TurnContext {
    pub(crate) current_trigger_type: TriggerType,
    pub(crate) current_turn_id: uuid::Uuid,
    pub(crate) current_turn_user_query: String,
}

impl TurnContext {
    pub(crate) fn new() -> Self {
        Self {
            current_trigger_type: TriggerType::Admin,
            current_turn_id: uuid::Uuid::nil(),
            current_turn_user_query: String::new(),
        }
    }
}
