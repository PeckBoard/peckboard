//! One user turn — text plus the files it carries.
//!
//! Splitting this off from the bare `String` we used to pass around lets
//! the dispatch path resolve attachment IDs to bytes ONCE and hand the
//! result straight to the provider, instead of asking every provider to
//! look up attachments on its own.
//!
//! Multimodal providers (Claude in stream-json mode) read
//! `attachments` and emit image content blocks; per-turn providers
//! (mock, ollama) read only `text`. The durable queue persists
//! `attachment_ids` (not bytes); delivery re-resolves them from the
//! attachments dir, so a queued send keeps its images.

/// One user turn bound for an `AgentProvider`.
#[derive(Debug, Clone, Default)]
pub struct UserMessage {
    pub text: String,
    pub attachments: Vec<UserAttachment>,
    /// Ids of `attachments` in the session's attachments dir. Carried so
    /// the durable queue can persist a mid-turn send without copying the
    /// bytes into SQLite — delivery re-resolves ids to bytes.
    pub attachment_ids: Vec<String>,
}

/// One attached file. Bytes are owned so the provider doesn't have to
/// reach back into the data dir to fetch them — the dispatch path does
/// the disk read once on the way in.
#[derive(Debug, Clone)]
pub struct UserAttachment {
    pub filename: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

impl UserMessage {
    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            attachments: Vec::new(),
            attachment_ids: Vec::new(),
        }
    }
}

impl From<&str> for UserMessage {
    fn from(s: &str) -> Self {
        UserMessage::from_text(s)
    }
}

impl From<String> for UserMessage {
    fn from(s: String) -> Self {
        UserMessage::from_text(s)
    }
}

/// Best-effort MIME type from a filename extension. Anything we don't
/// recognise falls back to `application/octet-stream` — the Claude
/// frame builder drops non-image attachments before they reach the
/// model, so a wrong guess here can't smuggle a non-image past it.
pub fn mime_from_filename(filename: &str) -> &'static str {
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_from_filename_covers_image_types() {
        assert_eq!(mime_from_filename("a.png"), "image/png");
        assert_eq!(mime_from_filename("A.PNG"), "image/png");
        assert_eq!(mime_from_filename("a.jpg"), "image/jpeg");
        assert_eq!(mime_from_filename("a.jpeg"), "image/jpeg");
        assert_eq!(mime_from_filename("a.gif"), "image/gif");
        assert_eq!(mime_from_filename("a.webp"), "image/webp");
    }

    #[test]
    fn mime_from_filename_falls_back_for_unknown() {
        assert_eq!(mime_from_filename("a.bin"), "application/octet-stream");
        assert_eq!(mime_from_filename("noext"), "application/octet-stream");
        assert_eq!(mime_from_filename(""), "application/octet-stream");
    }

    #[test]
    fn from_text_yields_empty_attachments() {
        let m = UserMessage::from_text("hi");
        assert_eq!(m.text, "hi");
        assert!(m.attachments.is_empty());
    }
}
