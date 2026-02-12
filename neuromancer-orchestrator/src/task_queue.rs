use std::collections::HashMap;

use neuromancer_core::task::{Task, TaskId, TaskState};

/// In-memory task queue with priority ordering and idempotency dedup.
pub struct TaskQueue {
    /// Priority-ordered queue. Tasks are sorted by (priority desc, created_at asc).
    queue: Vec<Task>,
    /// All known tasks (for persistence stub and idempotency checks).
    tasks: HashMap<TaskId, Task>,
    /// Idempotency key -> TaskId map for non-terminal tasks.
    idempotency_index: HashMap<String, TaskId>,
}

impl TaskQueue {
    pub fn new() -> Self {
        Self {
            queue: Vec::new(),
            tasks: HashMap::new(),
            idempotency_index: HashMap::new(),
        }
    }

    /// Enqueue a task. Returns the task ID.
    /// If a task with the same idempotency_key exists in a non-terminal state,
    /// the new task is dropped and the existing task's ID is returned.
    pub fn enqueue(&mut self, task: Task) -> TaskId {
        // Idempotency check
        if let Some(key) = &task.idempotency_key {
            if let Some(existing_id) = self.idempotency_index.get(key) {
                if let Some(existing) = self.tasks.get(existing_id) {
                    if !existing.state.is_terminal() {
                        tracing::debug!(
                            key = %key,
                            existing_id = %existing_id,
                            "deduplicating task by idempotency key"
                        );
                        return *existing_id;
                    }
                }
            }
        }

        let task_id = task.id;

        // Register idempotency key
        if let Some(key) = &task.idempotency_key {
            self.idempotency_index.insert(key.clone(), task_id);
        }

        self.tasks.insert(task_id, task.clone());
        self.queue.push(task);
        self.sort_queue();
        task_id
    }

    /// Dequeue the highest-priority task. Returns None if queue is empty.
    pub fn dequeue(&mut self) -> Option<Task> {
        if self.queue.is_empty() {
            return None;
        }

        let task = self.queue.remove(0);
        // Update the stored task to Dispatched state
        if let Some(stored) = self.tasks.get_mut(&task.id) {
            stored.state = TaskState::Dispatched;
            stored.updated_at = chrono::Utc::now();
        }
        Some(task)
    }

    /// Update a task's state in the persistence store.
    pub fn update_state(&mut self, task_id: TaskId, state: TaskState) {
        if let Some(task) = self.tasks.get_mut(&task_id) {
            task.state = state.clone();
            task.updated_at = chrono::Utc::now();

            // If terminal, remove from idempotency index
            if state.is_terminal() {
                if let Some(key) = &task.idempotency_key {
                    self.idempotency_index.remove(key);
                }
            }
        }
    }

    /// Requeue an existing task by ID.
    ///
    /// Returns true if the task exists and is now queued.
    pub fn requeue(&mut self, task_id: TaskId) -> bool {
        let Some(task) = self.tasks.get_mut(&task_id) else {
            return false;
        };

        if task.state.is_terminal() {
            return false;
        }

        task.state = TaskState::Queued;
        task.updated_at = chrono::Utc::now();

        if !self.queue.iter().any(|t| t.id == task_id) {
            self.queue.push(task.clone());
            self.sort_queue();
        }

        true
    }

    /// Get a task by ID.
    pub fn get(&self, task_id: &TaskId) -> Option<&Task> {
        self.tasks.get(task_id)
    }

    /// Get a mutable reference to a task by ID.
    pub fn get_mut(&mut self, task_id: &TaskId) -> Option<&mut Task> {
        self.tasks.get_mut(task_id)
    }

    /// Number of queued (not-yet-dispatched) tasks.
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// Total number of known tasks (all states).
    pub fn total_tasks(&self) -> usize {
        self.tasks.len()
    }

    /// List tasks matching a filter.
    pub fn list_by_state(&self, state: &TaskState) -> Vec<&Task> {
        self.tasks.values().filter(|t| &t.state == state).collect()
    }

    /// Reload tasks in active states (for crash recovery).
    pub fn active_tasks(&self) -> Vec<&Task> {
        self.tasks
            .values()
            .filter(|t| t.state.is_active())
            .collect()
    }

    fn sort_queue(&mut self) {
        self.queue.sort_by(|a, b| {
            // Higher priority first
            b.priority
                .cmp(&a.priority)
                // Then by creation time (oldest first)
                .then(a.created_at.cmp(&b.created_at))
        });
    }
}

impl Default for TaskQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neuromancer_core::task::{Task, TaskPriority};
    use neuromancer_core::trigger::TriggerSource;

    fn make_task(instruction: &str, priority: TaskPriority) -> Task {
        Task::new(
            TriggerSource::Internal,
            instruction.into(),
            "test-agent".into(),
        )
        .with_priority(priority)
    }

    #[test]
    fn priority_ordering() {
        let mut queue = TaskQueue::new();

        let low = make_task("low", TaskPriority::Low);
        let critical = make_task("critical", TaskPriority::Critical);
        let normal = make_task("normal", TaskPriority::Normal);

        queue.enqueue(low);
        queue.enqueue(critical);
        queue.enqueue(normal);

        let first = queue.dequeue().unwrap();
        assert_eq!(first.instruction, "critical");

        let second = queue.dequeue().unwrap();
        assert_eq!(second.instruction, "normal");

        let third = queue.dequeue().unwrap();
        assert_eq!(third.instruction, "low");
    }

    #[test]
    fn idempotency_dedup() {
        let mut queue = TaskQueue::new();

        let task1 = make_task("task-a", TaskPriority::Normal).with_idempotency_key("key-1".into());
        let id1 = queue.enqueue(task1);

        let task2 =
            make_task("task-a-dup", TaskPriority::Normal).with_idempotency_key("key-1".into());
        let id2 = queue.enqueue(task2);

        // Same ID returned, deduped
        assert_eq!(id1, id2);
        assert_eq!(queue.queue_len(), 1);
    }

    #[test]
    fn idempotency_allows_after_terminal() {
        let mut queue = TaskQueue::new();

        let task1 = make_task("task-a", TaskPriority::Normal).with_idempotency_key("key-1".into());
        let id1 = queue.enqueue(task1);

        // Mark as completed
        queue.update_state(id1, TaskState::Completed);

        // Now a new task with the same key should be allowed
        let task2 =
            make_task("task-a-new", TaskPriority::Normal).with_idempotency_key("key-1".into());
        let id2 = queue.enqueue(task2);

        assert_ne!(id1, id2);
    }

    #[test]
    fn empty_dequeue() {
        let mut queue = TaskQueue::new();
        assert!(queue.dequeue().is_none());
    }

    #[test]
    fn update_state_transitions() {
        let mut queue = TaskQueue::new();
        let task = make_task("work", TaskPriority::Normal);
        let id = queue.enqueue(task);

        queue.update_state(id, TaskState::Running);
        assert_eq!(queue.get(&id).unwrap().state, TaskState::Running);

        queue.update_state(id, TaskState::Completed);
        assert_eq!(queue.get(&id).unwrap().state, TaskState::Completed);
    }
}
