use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// Tag identifying which codex item kind a `Part` translates to. Cached when
/// `message.part.updated` is observed (T3) so `message.part.delta` (T4) can
/// route field-keyed deltas to the right item topic without re-parsing the
/// part union each time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    /// Assistant text part — deltas go to `item/agentMessage/delta`.
    Text,
    /// Reasoning part — deltas go to `item/reasoning/textDelta`.
    Reasoning,
    /// `tool` part with `tool == "bash"` — `output` field deltas go to
    /// `item/commandExecution/outputDelta`.
    ToolBash,
    /// `tool` part with `tool` matching `<server>__<name>` — `output` field
    /// deltas go to `item/mcpToolCall/progress`.
    ToolMcp,
    /// `tool` part with `tool` in {`write`, `edit`, `patch`, `apply_patch`} —
    /// `output` deltas go to `item/fileChange/outputDelta`.
    ToolFileChange,
    /// `tool` part with any other tool name — `output` deltas go to
    /// `item/dynamicToolCall/outputDelta` (or are dropped pending a codex
    /// surface). T4 currently logs and drops.
    ToolDynamic,
    /// Anything else — deltas dropped at debug level.
    Other,
}

#[derive(Debug, Clone)]
pub struct ActiveTurn {
    pub turn_id: String,
    pub model: Option<String>,
    /// Opencode session id this turn was launched against. Cached so SSE
    /// translators can correlate session-scoped events back to the codex
    /// `(threadId, turnId)` pair without re-reading the index.
    pub session_id: Option<String>,
    /// Id of the in-flight assistant `Message` for this turn — populated by
    /// the `message.updated` SSE branch (T3) when the first assistant
    /// message is observed. Used by T3/T4 to drive `item/started` /
    /// `item/agentMessage/delta` / `item/completed`.
    pub current_assistant_message_id: Option<String>,
}

/// Codex `TokenUsageBreakdown` mirror — kept as plain i64 fields so we can
/// accumulate without going through serde. Serialized to camelCase JSON when
/// emitted in `thread/tokenUsage/updated`.
#[derive(Debug, Clone, Default)]
pub struct TokenUsageBreakdown {
    pub total_tokens: i64,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_output_tokens: i64,
}

impl TokenUsageBreakdown {
    fn add_assign(&mut self, other: &Self) {
        self.total_tokens += other.total_tokens;
        self.input_tokens += other.input_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_output_tokens += other.reasoning_output_tokens;
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "totalTokens": self.total_tokens,
            "inputTokens": self.input_tokens,
            "cachedInputTokens": self.cached_input_tokens,
            "outputTokens": self.output_tokens,
            "reasoningOutputTokens": self.reasoning_output_tokens,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct ThreadTokenUsageState {
    pub total: TokenUsageBreakdown,
    pub last: TokenUsageBreakdown,
}

#[derive(Default)]
pub struct BridgeState {
    active_turns: Mutex<HashMap<String, ActiveTurn>>,
    token_usage: Mutex<HashMap<String, ThreadTokenUsageState>>,
    /// Per-message accumulated assistant text. Keyed by opencode message id.
    /// Populated by `message.part.delta` for `text` parts; consumed by
    /// `message.updated` (on completion) when emitting `item/completed`.
    text_accumulator: Mutex<HashMap<String, String>>,
    /// Set of message ids for which we've already emitted `item/started`.
    /// Prevents duplicate item-started events when opencode sends multiple
    /// `message.updated` events as the message progresses.
    started_messages: Mutex<HashSet<String>>,
    /// Set of part ids for which we've already emitted `item/started` (used
    /// for tool parts which appear via `message.part.updated`).
    started_parts: Mutex<HashSet<String>>,
    /// Cache of part kind keyed by part id, populated by `message.part.updated`
    /// and consumed by `message.part.delta` (T4) for routing.
    part_kind: Mutex<HashMap<String, PartKind>>,
}

impl BridgeState {
    pub fn set_active_turn(&self, thread_id: String, turn: ActiveTurn) {
        self.active_turns.lock().unwrap().insert(thread_id, turn);
    }

    pub fn take_active_turn(&self, thread_id: &str) -> Option<ActiveTurn> {
        self.active_turns.lock().unwrap().remove(thread_id)
    }

    pub fn active_turn(&self, thread_id: &str) -> Option<ActiveTurn> {
        self.active_turns.lock().unwrap().get(thread_id).cloned()
    }

    /// Mutate the in-flight `ActiveTurn` for `thread_id` if any. Returns the
    /// updated value if it existed, else `None`.
    pub fn update_active_turn<F>(&self, thread_id: &str, mutate: F) -> Option<ActiveTurn>
    where
        F: FnOnce(&mut ActiveTurn),
    {
        let mut map = self.active_turns.lock().unwrap();
        let entry = map.get_mut(thread_id)?;
        mutate(entry);
        Some(entry.clone())
    }

    /// Record a step-finish breakdown against the thread's running totals and
    /// return the resulting `(total, last)` snapshot for emission.
    pub fn record_token_usage(
        &self,
        thread_id: &str,
        last: TokenUsageBreakdown,
    ) -> ThreadTokenUsageState {
        let mut map = self.token_usage.lock().unwrap();
        let entry = map.entry(thread_id.to_string()).or_default();
        entry.total.add_assign(&last);
        entry.last = last;
        entry.clone()
    }

    /// Mark a message as started; returns `true` if this is the first time
    /// we've seen it (caller should emit `item/started`).
    pub fn mark_message_started(&self, message_id: &str) -> bool {
        self.started_messages
            .lock()
            .unwrap()
            .insert(message_id.to_string())
    }

    /// Append `delta` to the accumulator for `message_id`. Returns the new
    /// total length (used in tests; callers usually ignore).
    pub fn append_message_text(&self, message_id: &str, delta: &str) -> usize {
        let mut map = self.text_accumulator.lock().unwrap();
        let entry = map.entry(message_id.to_string()).or_default();
        entry.push_str(delta);
        entry.len()
    }

    /// Take the accumulated text for `message_id` and forget the entry. Also
    /// drops the message from the started-set. Returns the empty string if
    /// the message was never seen.
    pub fn take_message_text(&self, message_id: &str) -> String {
        self.started_messages.lock().unwrap().remove(message_id);
        self.text_accumulator
            .lock()
            .unwrap()
            .remove(message_id)
            .unwrap_or_default()
    }

    /// Drop bookkeeping for a removed message without emitting anything.
    pub fn forget_message(&self, message_id: &str) {
        self.started_messages.lock().unwrap().remove(message_id);
        self.text_accumulator.lock().unwrap().remove(message_id);
    }

    /// Mark a part as started; returns `true` if this is the first time. Used
    /// to gate `item/started` for tool parts.
    pub fn mark_part_started(&self, part_id: &str) -> bool {
        self.started_parts
            .lock()
            .unwrap()
            .insert(part_id.to_string())
    }

    /// Forget the started-state of a part (called after `item/completed`).
    pub fn forget_part(&self, part_id: &str) {
        self.started_parts.lock().unwrap().remove(part_id);
        self.part_kind.lock().unwrap().remove(part_id);
    }

    /// Cache the kind of `part_id` so deltas for it can be routed correctly.
    pub fn set_part_kind(&self, part_id: &str, kind: PartKind) {
        self.part_kind
            .lock()
            .unwrap()
            .insert(part_id.to_string(), kind);
    }

    /// Look up the cached kind of `part_id`, defaulting to `Other`.
    pub fn part_kind(&self, part_id: &str) -> PartKind {
        self.part_kind
            .lock()
            .unwrap()
            .get(part_id)
            .copied()
            .unwrap_or(PartKind::Other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_usage_accumulates_across_steps() {
        let state = BridgeState::default();
        let first = TokenUsageBreakdown {
            total_tokens: 30,
            input_tokens: 20,
            cached_input_tokens: 5,
            output_tokens: 10,
            reasoning_output_tokens: 0,
        };
        let snap1 = state.record_token_usage("thread-1", first.clone());
        assert_eq!(snap1.total.total_tokens, 30);
        assert_eq!(snap1.total.input_tokens, 20);
        assert_eq!(snap1.total.cached_input_tokens, 5);
        assert_eq!(snap1.last.total_tokens, 30);

        let second = TokenUsageBreakdown {
            total_tokens: 12,
            input_tokens: 8,
            cached_input_tokens: 1,
            output_tokens: 4,
            reasoning_output_tokens: 2,
        };
        let snap2 = state.record_token_usage("thread-1", second);
        assert_eq!(snap2.total.total_tokens, 42);
        assert_eq!(snap2.total.input_tokens, 28);
        assert_eq!(snap2.total.cached_input_tokens, 6);
        assert_eq!(snap2.total.output_tokens, 14);
        assert_eq!(snap2.total.reasoning_output_tokens, 2);
        assert_eq!(snap2.last.total_tokens, 12);
    }

    #[test]
    fn token_usage_separates_threads() {
        let state = BridgeState::default();
        state.record_token_usage(
            "thread-a",
            TokenUsageBreakdown {
                total_tokens: 10,
                input_tokens: 7,
                cached_input_tokens: 0,
                output_tokens: 3,
                reasoning_output_tokens: 0,
            },
        );
        let snap_b = state.record_token_usage(
            "thread-b",
            TokenUsageBreakdown {
                total_tokens: 99,
                input_tokens: 50,
                cached_input_tokens: 10,
                output_tokens: 49,
                reasoning_output_tokens: 5,
            },
        );
        assert_eq!(snap_b.total.total_tokens, 99);
        assert_eq!(snap_b.total.input_tokens, 50);
    }

    #[test]
    fn token_usage_breakdown_serializes_camel_case() {
        let breakdown = TokenUsageBreakdown {
            total_tokens: 100,
            input_tokens: 60,
            cached_input_tokens: 5,
            output_tokens: 40,
            reasoning_output_tokens: 7,
        };
        let json = breakdown.to_json();
        assert_eq!(json["totalTokens"], 100);
        assert_eq!(json["inputTokens"], 60);
        assert_eq!(json["cachedInputTokens"], 5);
        assert_eq!(json["outputTokens"], 40);
        assert_eq!(json["reasoningOutputTokens"], 7);
    }
}
