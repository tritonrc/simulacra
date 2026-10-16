//! How `file_read` turns a file's bytes into what the model sees. A host
//! supplies a presenter to set its own budgets and to answer with an image
//! block; without one, `file_read` returns the bytes decoded lossily.

use std::future::Future;
use std::pin::Pin;

use simulacra_types::{ToolError, ToolOutput};

/// What `file_read` was asked for. `offset` and `limit` are bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReadRequest {
    pub path: String,
    pub offset: Option<u64>,
    pub limit: Option<u64>,
}

pub type PresentFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + 'a>>;

/// Receives the whole file: windows, budgets and media handling are the
/// presenter's choice, so a host can refuse or summarise without a second read.
pub trait FilePresenter: Send + Sync {
    fn present<'a>(&'a self, request: FileReadRequest, bytes: Vec<u8>) -> PresentFuture<'a>;
}

/// The requested byte window, decoded lossily.
pub struct LossyTextPresenter;

impl FilePresenter for LossyTextPresenter {
    fn present<'a>(&'a self, request: FileReadRequest, bytes: Vec<u8>) -> PresentFuture<'a> {
        Box::pin(async move {
            let window = byte_window(&bytes, request.offset, request.limit);
            Ok(ToolOutput::success(
                String::from_utf8_lossy(window).into_owned(),
            ))
        })
    }
}

/// `bytes[offset .. offset + limit]`, clamped to the file; past the end is empty.
pub fn byte_window(bytes: &[u8], offset: Option<u64>, limit: Option<u64>) -> &[u8] {
    let start = usize::try_from(offset.unwrap_or(0))
        .unwrap_or(usize::MAX)
        .min(bytes.len());
    let end = match limit {
        Some(limit) => start
            .saturating_add(usize::try_from(limit).unwrap_or(usize::MAX))
            .min(bytes.len()),
        None => bytes.len(),
    };
    &bytes[start..end]
}
