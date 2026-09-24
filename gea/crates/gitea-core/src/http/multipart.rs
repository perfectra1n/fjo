//! Streaming file uploads for the three `multipart/form-data` operations.
//!
//! The spec has exactly three of them — `repoCreateReleaseAttachment`,
//! `issueCreateIssueAttachment`, `issueCreateIssueCommentAttachment` — and all three can be
//! handed a very large file. A release asset is routinely hundreds of megabytes and can be
//! several gigabytes.
//!
//! So nothing here reads a file into memory. Bodies stream from disk through
//! `tokio_util::io::ReaderStream` in 8 KB chunks, which keeps `gea release create v1 ./big.iso`
//! at a flat few hundred kilobytes of RSS instead of the file's size. The obvious alternative —
//! `Part::bytes(fs::read(path)?)` — works perfectly on every test fixture and then gets a
//! bug report about a 2 GB upload being OOM-killed.
//!
//! [`Progress`] exists so a progress bar can be attached without this module knowing what a
//! progress bar is. It counts bytes and optionally calls back; `indicatif` lives in the binary,
//! where a published SDK's users are not forced to link it.

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use futures::{Stream, TryStreamExt};
use tokio_util::io::ReaderStream;

use super::Source;
use crate::error::{Error, ErrorKind, Result};

/// A stream of body chunks being sent. Concrete for the same reason
/// [`super::ByteStream`] is: it appears in generated signatures.
pub type UploadStream = Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>;

/// One `multipart/form-data` field.
#[derive(Clone, Debug)]
pub struct Part {
    /// The form field name. For all three Gitea operations this is `attachment`.
    pub name: String,
    /// The filename to report. Gitea uses it as the attachment's display name, so it is
    /// worth setting deliberately rather than letting it default to the local path.
    pub filename: Option<String>,
    pub mime: Option<String>,
    pub src: Source,
}

impl Part {
    /// A file part read from disk. The filename defaults to the path's basename, which is
    /// almost always what the user meant.
    pub fn file(name: impl Into<String>, path: impl Into<std::path::PathBuf>) -> Self {
        let path = path.into();
        let filename = path.file_name().map(|n| n.to_string_lossy().into_owned());
        Self { name: name.into(), filename, mime: None, src: Source::Path(path) }
    }

    /// An in-memory part, for small generated content.
    pub fn bytes(name: impl Into<String>, data: impl Into<Vec<u8>>) -> Self {
        Self { name: name.into(), filename: None, mime: None, src: Source::Bytes(data.into()) }
    }

    /// A part read from stdin, for `gea release create v1 -` style piping.
    ///
    /// A filename is mandatory here in practice: there is no path to derive one from, and
    /// Gitea rejects an attachment with no name.
    pub fn stdin(name: impl Into<String>, filename: impl Into<String>) -> Self {
        Self { name: name.into(), filename: Some(filename.into()), mime: None, src: Source::Stdin }
    }

    pub fn with_filename(mut self, filename: impl Into<String>) -> Self {
        self.filename = Some(filename.into());
        self
    }

    pub fn with_mime(mut self, mime: impl Into<String>) -> Self {
        self.mime = Some(mime.into());
        self
    }
}

type Sink = Arc<dyn Fn(u64, Option<u64>) + Send + Sync>;

/// A byte counter with an optional callback, shared across every part of one upload.
///
/// Cheap to clone (two `Arc`s) and safe to ignore: [`Progress::default`] counts nothing and
/// calls nothing, so the non-interactive path pays an atomic add per chunk and no more.
#[derive(Clone, Default)]
pub struct Progress {
    sent: Arc<AtomicU64>,
    /// `0` means unknown — stdin has no length until it ends.
    total: Arc<AtomicU64>,
    sink: Option<Sink>,
}

impl fmt::Debug for Progress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Progress")
            .field("sent", &self.sent())
            .field("total", &self.total())
            .finish_non_exhaustive()
    }
}

impl Progress {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a callback invoked with `(bytes_sent_so_far, total_if_known)` on every chunk.
    ///
    /// Called from inside the body stream, so it must not block — draw a frame, do not write to
    /// a database.
    pub fn with_callback<F>(mut self, f: F) -> Self
    where
        F: Fn(u64, Option<u64>) + Send + Sync + 'static,
    {
        self.sink = Some(Arc::new(f));
        self
    }

    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    pub fn total(&self) -> Option<u64> {
        match self.total.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        }
    }

    /// Add to the known total. Additive because a multipart body has several parts, each
    /// discovering its own length.
    pub fn add_total(&self, n: u64) {
        self.total.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_sent(&self, n: u64) {
        let sent = self.sent.fetch_add(n, Ordering::Relaxed) + n;
        if let Some(sink) = &self.sink {
            sink(sent, self.total());
        }
    }
}

/// Wrap a stream so every chunk that passes through is counted.
pub fn counting<S, E>(stream: S, progress: Progress) -> impl Stream<Item = Result<Bytes, E>> + Send
where
    S: Stream<Item = Result<Bytes, E>> + Send,
{
    stream.inspect_ok(move |chunk| progress.add_sent(chunk.len() as u64))
}

/// Turn a [`Source`] into a counted byte stream plus its length, when the length is knowable.
///
/// The length matters: with it we can send `Content-Length` and the server can enforce its
/// upload limit before receiving the body, and a progress bar can show a percentage. Without
/// it (stdin) the request goes out chunked, which is correct but means a rejected oversized
/// upload is only discovered after transferring it.
pub async fn source_stream(
    src: &Source,
    progress: &Progress,
) -> Result<(UploadStream, Option<u64>)> {
    match src {
        Source::Bytes(data) => {
            let len = data.len() as u64;
            progress.add_total(len);
            let data = Bytes::from(data.clone());
            let s = counting(futures::stream::once(async move { Ok(data) }), progress.clone());
            Ok((Box::pin(s), Some(len)))
        }
        Source::Path(path) => {
            let file = tokio::fs::File::open(path).await.map_err(|e| open_error(path, e))?;
            // Metadata from the open handle, not the path: a `stat` on the path could race with
            // a rename and report a length that does not match the bytes we are about to send,
            // which is a truncated upload with a successful exit code.
            let len = file.metadata().await.ok().map(|m| m.len());
            if let Some(n) = len {
                progress.add_total(n);
            }
            let s = counting(ReaderStream::new(file), progress.clone());
            Ok((Box::pin(s), len))
        }
        Source::Stdin => {
            // No length. Deliberately not buffered to discover one — buffering stdin defeats
            // the entire purpose of streaming.
            let s = counting(ReaderStream::new(tokio::io::stdin()), progress.clone());
            Ok((Box::pin(s), None))
        }
    }
}

/// Build a `reqwest` multipart form whose parts stream rather than buffer.
pub async fn build_form(parts: &[Part], progress: &Progress) -> Result<reqwest::multipart::Form> {
    let mut form = reqwest::multipart::Form::new();
    for part in parts {
        let (stream, len) = source_stream(&part.src, progress).await?;
        let body = reqwest::Body::wrap_stream(stream);
        let mut p = match len {
            Some(n) => reqwest::multipart::Part::stream_with_length(body, n),
            None => reqwest::multipart::Part::stream(body),
        };
        if let Some(name) = &part.filename {
            p = p.file_name(name.clone());
        }
        if let Some(mime) = &part.mime {
            p = p.mime_str(mime).map_err(|e| {
                Error::new(ErrorKind::Usage(format!("{mime:?} is not a valid MIME type: {e}")))
            })?;
        }
        form = form.part(part.name.clone(), p);
    }
    Ok(form)
}

/// A failure to open an upload source is a *usage* problem, not an I/O curiosity: the user named
/// a file that is missing or unreadable, and the message should name it.
fn open_error(path: &std::path::Path, e: std::io::Error) -> Error {
    Error::new(ErrorKind::Usage(format!("cannot read {} for upload: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::io::Write;
    use std::sync::atomic::AtomicUsize;

    async fn drain(mut s: UploadStream) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(chunk) = s.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn a_file_streams_its_contents_and_reports_its_length() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"hello upload").unwrap();
        f.flush().unwrap();

        let progress = Progress::new();
        let (stream, len) =
            source_stream(&Source::Path(f.path().to_owned()), &progress).await.unwrap();
        assert_eq!(len, Some(12));
        assert_eq!(progress.total(), Some(12));
        assert_eq!(drain(stream).await, b"hello upload");
        assert_eq!(progress.sent(), 12, "every chunk must be counted");
    }

    /// The counter is what a progress bar reads, and it must reflect bytes actually handed to
    /// the transport — not the file size, which would show 100% before anything was sent.
    #[test]
    fn progress_counts_only_what_passed_through() {
        let p = Progress::new();
        p.add_total(100);
        assert_eq!((p.sent(), p.total()), (0, Some(100)));
        p.add_sent(40);
        assert_eq!(p.sent(), 40);
    }

    #[tokio::test]
    async fn the_callback_fires_per_chunk_with_the_running_total() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(AtomicU64::new(0));
        let progress = {
            let (calls, seen) = (calls.clone(), seen.clone());
            Progress::new().with_callback(move |sent, total| {
                calls.fetch_add(1, Ordering::Relaxed);
                seen.store(sent, Ordering::Relaxed);
                assert_eq!(total, Some(6));
            })
        };
        let (stream, _) =
            source_stream(&Source::Bytes(b"abcdef".to_vec()), &progress).await.unwrap();
        drain(stream).await;
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(seen.load(Ordering::Relaxed), 6);
    }

    /// stdin has no length, and we must not invent one by buffering it — buffering is exactly
    /// what this module exists to avoid.
    #[tokio::test]
    async fn stdin_has_no_length_and_is_not_buffered_to_find_one() {
        let progress = Progress::new();
        let (_stream, len) = source_stream(&Source::Stdin, &progress).await.unwrap();
        assert_eq!(len, None);
        assert_eq!(progress.total(), None);
    }

    /// A missing upload file must produce advice naming the path, not a bare `ENOENT`.
    #[tokio::test]
    async fn a_missing_file_names_itself_in_the_error() {
        let Err(e) =
            source_stream(&Source::Path("/nonexistent/asset.tar.gz".into()), &Progress::new())
                .await
        else {
            // `expect_err` needs `Debug` on the Ok side, and a boxed stream has none.
            panic!("opening a missing file must fail");
        };
        let msg = format!("{:?}", e.kind());
        assert!(msg.contains("/nonexistent/asset.tar.gz"), "{msg}");
    }

    #[test]
    fn a_file_part_defaults_its_name_to_the_basename() {
        let p = Part::file("attachment", "/tmp/dist/gea-v1.2.3.tar.gz");
        assert_eq!(p.filename.as_deref(), Some("gea-v1.2.3.tar.gz"));
        assert_eq!(p.name, "attachment");
    }

    #[tokio::test]
    async fn a_form_accumulates_the_total_across_parts() {
        let progress = Progress::new();
        let parts =
            vec![Part::bytes("a", b"1234".to_vec()), Part::bytes("attachment", b"567".to_vec())];
        build_form(&parts, &progress).await.unwrap();
        assert_eq!(progress.total(), Some(7));
    }
}
