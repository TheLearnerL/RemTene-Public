//! Clipboard text writer for platforms without a native implementation.
//!
//! Explicit “复制全部” must never report success unless the requested text was
//! actually placed on the system clipboard. Unsupported platforms therefore
//! return one fixed, content-free error.

use remtene_application::ports::{ClipboardTextWriter, PortError, PortFuture};

pub struct UnsupportedClipboardTextWriter;

impl UnsupportedClipboardTextWriter {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for UnsupportedClipboardTextWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardTextWriter for UnsupportedClipboardTextWriter {
    fn write_text(&self, _text: String) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async {
            Err(PortError {
                code: "clipboard_text.unsupported_platform".to_owned(),
                safe_message_key: "errors.clipboard_text.unsupported_platform".to_owned(),
                retryable: false,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_writer_never_claims_success_or_leaks_text() {
        let writer = UnsupportedClipboardTextWriter::new();
        let private_text = "不应进入错误的正文";
        let error = futures::executor::block_on(writer.write_text(private_text.to_owned()))
            .expect_err("unsupported writer must fail explicitly");

        assert_eq!(error.code, "clipboard_text.unsupported_platform");
        assert_eq!(
            error.safe_message_key,
            "errors.clipboard_text.unsupported_platform"
        );
        assert!(!error.retryable);
        assert!(!error.code.contains(private_text));
        assert!(!error.safe_message_key.contains(private_text));
    }
}
