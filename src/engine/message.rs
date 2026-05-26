//! Thin aliases over `rig::message::*` so callers don't need a `rig::` import.
//!
//! Why aliasing rather than re-wrapping: rig's types are well-shaped, and
//! re-implementing them buys nothing except divergence drift when rig
//! evolves. The aliases give us a single import point if we ever do want
//! to swap implementations.

pub use rig::OneOrMany;
pub use rig::completion::ToolDefinition;
pub use rig::message::{AssistantContent, Message, ToolCall};

/// Extract concatenated text from an assistant turn's content vector.
pub fn extract_text(choice: &OneOrMany<AssistantContent>) -> String {
    choice
        .iter()
        .filter_map(|c| match c {
            AssistantContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collect all `ToolCall`s from an assistant turn's content vector.
pub fn collect_tool_calls(choice: &OneOrMany<AssistantContent>) -> Vec<ToolCall> {
    choice
        .iter()
        .filter_map(|c| match c {
            AssistantContent::ToolCall(tc) => Some(tc.clone()),
            _ => None,
        })
        .collect()
}

/// Build the tool-result message rig expects in the next request, given a
/// `ToolCall` and the (already-serialized) output string.
pub fn tool_result_message(tc: &ToolCall, output: String) -> Message {
    Message::tool_result_with_call_id(tc.id.clone(), tc.call_id.clone(), output)
}
