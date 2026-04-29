//! Test-helper free function preserving the legacy anonymous-session entry
//! point.
//!
//! Production traffic flows through [`crate::bridge::ClaudeBridge`] +
//! `bridge_core::serve_stream_with_session`. This helper exists so the
//! in-process smoke tests in `tests/smoke_in_process.rs` can drive a stream
//! through an already-constructed pool/index pair without going through the
//! builder.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as StdContext, Poll};

use alleycat_bridge_core::serve_stream_with_session;
use alleycat_bridge_core::session::{SessionRegistry, SessionRegistryConfig};
use alleycat_bridge_core::{LocalLauncher, ProcessLauncher};
use anyhow::Result;
use dashmap::DashMap;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::bridge::ClaudeBridge;
use crate::pool::ClaudePool;

type ThreadIndexArc = Arc<dyn crate::state::ThreadIndexHandle>;

/// Test-helper entry point: mints an anonymous session for the duration of the
/// stream. Used by `tests/smoke_in_process.rs` to drive a `ClaudeBridge`
/// assembled directly from a pool + thread index without going through the
/// builder.
pub async fn run_connection<R, W>(
    reader: R,
    writer: W,
    claude_pool: Arc<ClaudePool>,
    thread_index: ThreadIndexArc,
    codex_home: PathBuf,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let bridge = Arc::new(ClaudeBridge::__assemble(
        claude_pool,
        thread_index,
        codex_home,
        Arc::new(LocalLauncher) as Arc<dyn ProcessLauncher>,
        DashMap::new(),
    ));
    let stream = DuplexStream { reader, writer };
    let registry = SessionRegistry::new(SessionRegistryConfig::default());
    let session = registry.get_or_create("anonymous".into(), "claude");
    let result = serve_stream_with_session(bridge, stream, Arc::clone(&session), None).await;
    session.cancel_all_pending();
    result
}

/// Bring an `AsyncRead` and `AsyncWrite` together into one duplex stream so
/// the bridge-core `serve_stream_with_session` plumbing can drive both ends.
struct DuplexStream<R, W> {
    reader: R,
    writer: W,
}

impl<R: AsyncRead + Unpin, W: Unpin> AsyncRead for DuplexStream<R, W> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut StdContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.reader).poll_read(cx, buf)
    }
}

impl<R: Unpin, W: AsyncWrite + Unpin> AsyncWrite for DuplexStream<R, W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut StdContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.writer).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.writer).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut StdContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.writer).poll_shutdown(cx)
    }
}
