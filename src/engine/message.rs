//! Thin aliases over `rig::message::*` so callers don't need a `rig::` import.
//!
//! Why aliasing rather than re-wrapping: rig's types are well-shaped, and
//! re-implementing them buys nothing except divergence drift when rig
//! evolves. The aliases give us a single import point if we ever do want
//! to swap implementations.

pub use rig::OneOrMany;
pub use rig::completion::ToolDefinition;
pub use rig::message::{AssistantContent, Message, ToolCall};
use rig::message::{ImageMediaType, UserContent};

use base64::Engine as _;

/// Sentinel emitted in wire text by
/// [`crate::tui::paste::PasteRegistry::build_wire`] at each real-image
/// position. We split on it here to interleave text and image content
/// parts in order when assembling the outbound user [`Message`].
pub use crate::tui::paste::IMAGE_PART_SENTINEL;

/// A user submission destined for the agent: scrubbed wire text plus the
/// ordered PNG payloads for any pasted images sent as real image parts
/// (vision models only — non-vision callers fold images into the text and
/// pass an empty `images`). Travels the daemon→driver path so image bytes
/// reach the prompt-assembly point without being mangled by the
/// text-only redaction/queue-folding plumbing.
///
/// `text` may contain [`IMAGE_PART_SENTINEL`] markers; there must be
/// exactly `images.len()` of them, in the same left-to-right order as
/// `images`. [`build_user_message`] consumes both.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct UserSubmission {
    pub text: String,
    /// PNG-encoded image bytes, one per real image part, in order.
    #[serde(default)]
    pub images: Vec<Vec<u8>>,
}

impl UserSubmission {
    /// Text-only submission (no images). Used everywhere the legacy
    /// string path fed a bare message.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }

    /// True when there are no image parts — the common case, letting the
    /// driver keep the cheap `Message::user(text)` path.
    pub fn is_text_only(&self) -> bool {
        self.images.is_empty()
    }
}

/// Build a user [`Message`] from a [`UserSubmission`]. With no images this
/// is exactly `Message::user(text)`. With images, the `text` is split on
/// [`IMAGE_PART_SENTINEL`] and reassembled as an ordered
/// `OneOrMany<UserContent>` of interleaved text + base64-PNG image parts,
/// which rig serializes as `image_url` data-URIs for OpenAI-compatible
/// chat completions (verified via kcl `rig-core`). Empty text segments
/// between/around images are dropped so we don't emit empty text parts.
pub fn build_user_message(sub: UserSubmission) -> Message {
    if sub.is_text_only() {
        return Message::user(sub.text);
    }
    let segments: Vec<&str> = sub.text.split(IMAGE_PART_SENTINEL).collect();
    let mut parts: Vec<UserContent> = Vec::new();
    let mut imgs = sub.images.into_iter();
    for (i, seg) in segments.iter().enumerate() {
        if !seg.is_empty() {
            parts.push(UserContent::text(*seg));
        }
        // A sentinel separated this segment from the next → an image part
        // belongs here (one fewer sentinel than there are segments).
        if i + 1 < segments.len()
            && let Some(png) = imgs.next()
        {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
            parts.push(UserContent::image_base64(
                b64,
                Some(ImageMediaType::PNG),
                None,
            ));
        }
    }
    // Any images without a matching sentinel (defensive — shouldn't
    // happen) are appended so bytes are never silently dropped.
    for png in imgs {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        parts.push(UserContent::image_base64(
            b64,
            Some(ImageMediaType::PNG),
            None,
        ));
    }
    match OneOrMany::many(parts) {
        Ok(content) => Message::User { content },
        // Empty content is unreachable (caller has images), but never
        // panic on the wire path — fall back to the plain text form.
        Err(_) => Message::user(sub.text),
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn user_parts(msg: &Message) -> Vec<UserContent> {
        match msg {
            Message::User { content } => content.iter().cloned().collect(),
            _ => panic!("expected a user message"),
        }
    }

    #[test]
    fn text_only_submission_is_a_plain_user_text_message() {
        let msg = build_user_message(UserSubmission::text("hello world"));
        let parts = user_parts(&msg);
        assert_eq!(parts.len(), 1);
        assert!(matches!(parts[0], UserContent::Text(_)));
    }

    #[test]
    fn vision_submission_interleaves_text_and_one_image_part() {
        // "see <img> done" with one PNG → text, image, text.
        let text = format!("see {IMAGE_PART_SENTINEL} done");
        let msg = build_user_message(UserSubmission {
            text,
            images: vec![vec![1u8, 2, 3]],
        });
        let parts = user_parts(&msg);
        assert_eq!(parts.len(), 3);
        assert!(matches!(parts[0], UserContent::Text(_)));
        assert!(matches!(parts[1], UserContent::Image(_)));
        assert!(matches!(parts[2], UserContent::Text(_)));
    }

    #[test]
    fn leading_image_drops_empty_text_segment() {
        // Sentinel at the very start → no empty leading text part.
        let text = format!("{IMAGE_PART_SENTINEL}after");
        let msg = build_user_message(UserSubmission {
            text,
            images: vec![vec![9u8]],
        });
        let parts = user_parts(&msg);
        assert_eq!(parts.len(), 2);
        assert!(matches!(parts[0], UserContent::Image(_)));
        assert!(matches!(parts[1], UserContent::Text(_)));
    }

    #[test]
    fn model_switch_round_trip_text_note_vs_image_part() {
        // The non-vision wire (a text note, no images) builds a plain text
        // message; the vision wire (sentinel + bytes) builds an image
        // part — the same paste, two model states, no re-paste.
        let note = build_user_message(UserSubmission::text(
            "[Pasted image #1: not sent — current model has no image support]",
        ));
        assert!(
            user_parts(&note)
                .iter()
                .all(|p| matches!(p, UserContent::Text(_)))
        );

        let img = build_user_message(UserSubmission {
            text: IMAGE_PART_SENTINEL.to_string(),
            images: vec![vec![1u8, 2]],
        });
        assert!(
            user_parts(&img)
                .iter()
                .any(|p| matches!(p, UserContent::Image(_)))
        );
    }
}
