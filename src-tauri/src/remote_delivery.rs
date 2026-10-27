//! Destination-side staging: a failed transfer never destroys a local encode
//! or exposes a partially copied remote output.
use crate::{events::EventSink, output_transaction::OutputTransaction};
use std::path::Path;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

pub fn staging_directory(root: &Path) -> Result<std::path::PathBuf, String> {
    std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
    // Deliberately keep the directory: only empty-directory cleanup is safe
    // after a failure, since it may contain the sole completed encode.
    tempfile::Builder::new()
        .prefix("vt-wave-")
        .tempdir_in(root)
        .map(|dir| dir.keep())
        .map_err(|e| e.to_string())
}

async fn copy_into_pending<R: AsyncRead + Unpin>(
    source: &mut R,
    expected_len: u64,
    transaction: &mut OutputTransaction,
    sink: &dyn EventSink,
) -> Result<(), String> {
    let mut output = tokio::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(transaction.pending_path())
        .await
        .map_err(|e| e.to_string())?;
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut copied = 0u64;
    loop {
        if sink.cancellation_requested() {
            return Err("Delivery cancelled".into());
        }
        let count = source.read(&mut buffer).await.map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        output
            .write_all(&buffer[..count])
            .await
            .map_err(|e| e.to_string())?;
        copied += count as u64;
    }
    if copied != expected_len {
        return Err(format!(
            "Incomplete transfer: expected {expected_len} bytes, received {copied}"
        ));
    }
    output.flush().await.map_err(|e| e.to_string())?;
    output.sync_all().await.map_err(|e| e.to_string())?;
    drop(output);
    if sink.cancellation_requested() {
        return Err("Delivery cancelled".into());
    }
    transaction.commit()
}

pub async fn deliver(
    transaction: &mut OutputTransaction,
    local: &Path,
    sink: &dyn EventSink,
) -> Result<(), String> {
    sink.processing_stage("finalizing", "Delivering completed output...", None);
    let mut source = tokio::fs::File::open(local)
        .await
        .map_err(|e| e.to_string())?;
    let expected_len = source.metadata().await.map_err(|e| e.to_string())?.len();
    copy_into_pending(&mut source, expected_len, transaction, sink).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::NoopSink;
    use std::{
        io,
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::io::ReadBuf;

    struct BrokenTransfer(bool);
    impl AsyncRead for BrokenTransfer {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if self.0 {
                Poll::Ready(Err(io::Error::other("simulated network disconnect")))
            } else {
                self.0 = true;
                buf.put_slice(b"partial network data");
                Poll::Ready(Ok(()))
            }
        }
    }

    #[tokio::test]
    async fn partial_transfer_preserves_remote_source_existing_output_and_local_recovery() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.mkv");
        let remote = root.path().join("remote.mkv");
        let local = root.path().join("completed.mkv");
        std::fs::write(&source, b"original").unwrap();
        std::fs::write(&remote, b"previous output").unwrap();
        std::fs::write(&local, b"completed encoding").unwrap();
        for destination in [&remote, &source] {
            let mut tx = OutputTransaction::new(&source, destination, true).unwrap();
            let pending = tx.pending_path().to_path_buf();
            assert!(
                copy_into_pending(&mut BrokenTransfer(false), 100, &mut tx, &NoopSink)
                    .await
                    .is_err()
            );
            assert!(tx.remove_source().is_err());
            // A clean EOF is not success if fewer bytes than the completed
            // encode's recorded size were delivered.
            assert!(
                copy_into_pending(&mut &b"short"[..], 100, &mut tx, &NoopSink)
                    .await
                    .unwrap_err()
                    .contains("Incomplete transfer")
            );
            drop(tx);
            assert!(!pending.exists());
            assert_eq!(std::fs::read(&source).unwrap(), b"original");
            assert_eq!(std::fs::read(&remote).unwrap(), b"previous output");
            assert_eq!(std::fs::read(&local).unwrap(), b"completed encoding");
        }
    }

    #[tokio::test]
    async fn changed_destination_rejects_delivery_and_keeps_local_encode() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let remote = root.path().join("remote");
        let local = root.path().join("completed");
        std::fs::write(&source, b"original").unwrap();
        std::fs::write(&remote, b"previous").unwrap();
        std::fs::write(&local, b"completed").unwrap();
        let mut tx = OutputTransaction::new(&source, &remote, false).unwrap();
        std::fs::write(&remote, b"another process changed output").unwrap();
        assert!(deliver(&mut tx, &local, &NoopSink).await.is_err());
        assert_eq!(std::fs::read(&local).unwrap(), b"completed");
        assert_eq!(
            std::fs::read(&remote).unwrap(),
            b"another process changed output"
        );
    }

    #[tokio::test]
    async fn cancellation_preserves_both_copies_and_success_installs_all_bytes() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let remote = root.path().join("remote");
        let local = root.path().join("completed");
        std::fs::write(&source, b"original").unwrap();
        std::fs::write(&remote, b"previous").unwrap();
        let bytes = vec![0x5a; 2 * 1024 * 1024 + 17];
        std::fs::write(&local, &bytes).unwrap();
        let mut tx = OutputTransaction::new(&source, &remote, false).unwrap();
        let control = crate::test_helpers::CancellableBatchControl::new();
        control
            .cancel_all
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let sink = crate::pipeline_progress::PipelineSink {
            inner: &NoopSink,
            control: &control,
        };
        assert!(deliver(&mut tx, &local, &sink)
            .await
            .unwrap_err()
            .contains("cancelled"));
        assert_eq!(std::fs::read(&remote).unwrap(), b"previous");
        assert_eq!(std::fs::read(&local).unwrap(), bytes);
        drop(tx);
        let mut tx = OutputTransaction::new(&source, &remote, false).unwrap();
        deliver(&mut tx, &local, &NoopSink).await.unwrap();
        assert_eq!(std::fs::read(&remote).unwrap(), bytes);
        assert!(local.exists()); // caller alone decides when recovery is removed
        assert_eq!(std::fs::read(&source).unwrap(), b"original");
    }

    #[test]
    fn waves_use_unique_directories_and_empty_cleanup_cannot_delete_recovery() {
        let root = tempfile::tempdir().unwrap();
        let first = staging_directory(root.path()).unwrap();
        let second = staging_directory(root.path()).unwrap();
        assert_ne!(first, second);
        let recovery = first.join("completed.mkv");
        std::fs::write(&recovery, b"completed encoding").unwrap();
        assert!(std::fs::remove_dir(&first).is_err());
        std::fs::remove_dir(second).unwrap();
        assert_eq!(std::fs::read(recovery).unwrap(), b"completed encoding");
    }
}
