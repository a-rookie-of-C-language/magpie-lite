//! An agent compacting its conversation.
//!
//! An agent compacting its conversation asks a model to summarize it, with
//! a prompt of its own in the system prompt or as the last user message
//! (the reference gateway's `compactSystems`/`compactAsks`/`compacting`,
//! read from each agent's release). The gateway's routing reads it for a
//! compaction rule's own match, and the Gemini encoder reads it for the
//! user turn put after a conversation ending with the model's turn: when
//! the agent compacts with the ask in its system prompt, "Continue." reads
//! as going on with the task and the summary isn't written, so the turn
//! asks for the summary the system prompt asks for instead (the
//! reference's `compactLastText`/`modelLastAsk`, `ce11b180`).
use crate::ir::{CompletionRequest, PartKind, Role};

/// Open (or are) the system prompt of a compaction request.
const COMPACT_SYSTEMS: [&str; 5] = [
    "You are a helpful AI assistant tasked with summarizing conversations.",
    "You are a context summarization agent.",
    "You are a context summarization assistant.",
    "You are a specialized system component responsible for distilling chat history into a structured XML <state_snapshot>",
    "You are the component that summarizes a conversation when its context window is about to overflow.",
];

/// In the last user message of a compaction request.
const COMPACT_ASKS: [&str; 3] = [
    "Your task is to create a detailed summary of the conversation so far",
    "You are performing a CONTEXT CHECKPOINT COMPACTION.",
    "You are now given a task to compact this conversation context",
];

/// Whether `req` is an agent compacting its conversation.
pub fn compacting(req: &CompletionRequest) -> bool {
    let system: String = req
        .system
        .iter()
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if COMPACT_SYSTEMS.iter().any(|p| system.contains(p)) {
        return true;
    }
    for m in req.messages.iter().rev() {
        if m.role != Role::User {
            continue;
        }
        for p in &m.parts {
            if p.kind() != PartKind::Text {
                continue;
            }
            if COMPACT_ASKS.iter().any(|a| p.text.contains(a)) {
                return true;
            }
        }
        break;
    }
    false
}
