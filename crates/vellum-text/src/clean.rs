//! Removing the one wrapper that is unambiguously a chat artefact.
//!
//! Both the translation and the API-OCR path ask for bare text, and models
//! occasionally prepend a label to it. That label is removed here rather than
//! trusted to the prompt: instructions reduce how often it happens, they do not
//! make it impossible, and a `译文：` sitting in the result window reads as
//! vellum's own bug.
//!
//! What is deliberately NOT removed, and why it would be a bug to remove it:
//!
//!   * A code fence that spans the whole answer. The translation prompt
//!     promises to preserve Markdown and code blocks, so a fenced answer may BE
//!     the content; and for OCR, a screenshot of a Markdown block is literally a
//!     fenced block. Stripping it would delete something the user can see in the
//!     image.
//!   * Quotes that span the whole answer. A screenshot of a JSON or config
//!     fragment is `"production"`, and dropping those quotes is silent data
//!     loss: the result window is editable, but nobody can restore a character
//!     they never knew was there.
//!
//! The prompt is what prevents wrappers. This function only cleans up the
//! failure that survives a prompt and cannot be confused with content.

/// Strip a label a model prepended to its answer.
///
/// Only a prefix at the very start counts, and only when something follows it:
/// a label in the middle is content, and an answer that is nothing but a label
/// must not become empty.
pub(crate) fn strip_leading_label(text: &str, labels: &[&str]) -> String {
    let out = text.trim();
    for label in labels {
        if let Some(rest) = out.strip_prefix(label) {
            let rest = rest.trim();
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    out.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LABELS: &[&str] = &["译文：", "Translation:"];

    #[test]
    fn a_leading_label_is_removed() {
        assert_eq!(strip_leading_label("译文：hello", LABELS), "hello");
        assert_eq!(strip_leading_label("Translation: hello", LABELS), "hello");
        assert_eq!(strip_leading_label("  译文：  hello  ", LABELS), "hello");
    }

    #[test]
    fn a_label_anywhere_else_is_content() {
        assert_eq!(
            strip_leading_label("他说：译文：去掉", LABELS),
            "他说：译文：去掉"
        );
        assert_eq!(
            strip_leading_label("hello\n译文：bye", LABELS),
            "hello\n译文：bye"
        );
    }

    /// The negative half of this module, and the reason it is labels-only. Each
    /// of these is content that a fence/quote stripper would have deleted.
    #[test]
    fn a_fence_or_a_quote_is_never_removed() {
        assert_eq!(
            strip_leading_label("```\nhello\n```", LABELS),
            "```\nhello\n```"
        );
        assert_eq!(
            strip_leading_label("```rust\nfn main() {}\n```", LABELS),
            "```rust\nfn main() {}".to_string() + "\n```"
        );
        assert_eq!(
            strip_leading_label("\"production\"", LABELS),
            "\"production\""
        );
        assert_eq!(strip_leading_label("“你好”", LABELS), "“你好”");
        assert_eq!(
            strip_leading_label("he said \"hi\"", LABELS),
            "he said \"hi\""
        );
    }

    #[test]
    fn an_answer_that_is_only_a_label_is_kept() {
        // Returning empty would read as "the model replied with nothing".
        assert_eq!(strip_leading_label("译文：", LABELS), "译文：");
        assert_eq!(strip_leading_label("译文：   ", LABELS), "译文：");
    }

    #[test]
    fn plain_text_survives_untouched() {
        assert_eq!(strip_leading_label("  hello  ", LABELS), "hello");
        assert_eq!(
            strip_leading_label("line one\nline two", LABELS),
            "line one\nline two"
        );
        assert_eq!(strip_leading_label("", LABELS), "");
    }
}
