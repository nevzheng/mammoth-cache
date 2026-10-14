//! Splitting one byte stream into several, so an entry can be written to more than one tier while
//! it is read once.
//!
//! The source is driven by the main stream. Each copy is a [`ByteStream`] fed through a bounded
//! channel. A copy that cannot see the whole entry (the source failed, the main stream was dropped
//! early, or a lossy copy fell behind) ends with an error rather than cleanly, so a store never
//! keeps a partial entry.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use bytes::Bytes;
use futures_util::{StreamExt, stream};
use tokio::sync::mpsc;

use crate::store::ByteStream;

/// Chunks a copy may lag behind the main stream.
const CHANNEL_CHUNKS: usize = 32;

/// How the main stream treats a copy that is not keeping up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Wait for every copy: the slowest one paces the source. For uploads, where every copy must
    /// be written.
    Lockstep,
    /// Never wait: a copy whose channel is full is abandoned. For promotions, which must not slow
    /// the client being served.
    Lossy,
}

struct Branch {
    tx: Option<mpsc::Sender<Bytes>>,
    aborted: Arc<AtomicBool>,
}

impl Branch {
    fn abort(&mut self) {
        self.aborted.store(true, Ordering::Release);
        self.tx = None;
    }
}

struct State {
    source: ByteStream,
    copies: Vec<Branch>,
    mode: Mode,
    /// Bytes still expected, when the length is known.
    remaining: Option<u64>,
    finished: bool,
}

impl Drop for State {
    fn drop(&mut self) {
        // Dropped before the source ended: the copies are incomplete.
        if !self.finished {
            for copy in &mut self.copies {
                copy.abort();
            }
        }
    }
}

/// Splits `source` into a main stream and `copies` copies.
///
/// With `len`, the source is read to its end as soon as the last expected byte arrives, before
/// that byte is passed on. A reader that stops once it has `len` bytes, as an HTTP server does
/// with `Content-Length`, then still completes the copies and sees any error the source reports
/// at its end, such as a failed checksum.
pub(crate) fn tee(
    source: ByteStream,
    copies: usize,
    mode: Mode,
    len: Option<u64>,
) -> (ByteStream, Vec<ByteStream>) {
    let mut senders = Vec::with_capacity(copies);
    let mut receivers = Vec::with_capacity(copies);
    for _ in 0..copies {
        let (tx, rx) = mpsc::channel(CHANNEL_CHUNKS);
        let aborted = Arc::new(AtomicBool::new(false));
        receivers.push(receiver_stream(rx, aborted.clone()));
        senders.push(Branch {
            tx: Some(tx),
            aborted,
        });
    }
    let state = State {
        source,
        copies: senders,
        mode,
        remaining: len,
        finished: false,
    };
    let main = stream::unfold(Some(state), |state| async move {
        let mut state = state?;
        if state.finished {
            return None;
        }
        match state.source.next().await {
            Some(Ok(chunk)) => {
                if let Err(e) = state.finish_if_complete(&chunk).await {
                    return Some((Err(e), None));
                }
                state.forward(&chunk).await;
                if state.finished {
                    // Closing the channels ends the copies cleanly.
                    for copy in &mut state.copies {
                        copy.tx = None;
                    }
                }
                Some((Ok(chunk), Some(state)))
            }
            // Dropping the state aborts the copies.
            Some(Err(e)) => Some((Err(e), None)),
            None => {
                state.finished = true;
                None
            }
        }
    });
    (Box::pin(main), receivers)
}

impl State {
    /// Once `chunk` completes the expected length, reads the source to its end. Fails if the
    /// source reports an error there or has more bytes than expected.
    async fn finish_if_complete(&mut self, chunk: &Bytes) -> io::Result<()> {
        let Some(remaining) = &mut self.remaining else {
            return Ok(());
        };
        *remaining = remaining
            .checked_sub(chunk.len() as u64)
            .ok_or_else(too_long)?;
        if *remaining > 0 {
            return Ok(());
        }
        match self.source.next().await {
            None => {
                self.finished = true;
                Ok(())
            }
            Some(Ok(_)) => Err(too_long()),
            Some(Err(e)) => Err(e),
        }
    }

    async fn forward(&mut self, chunk: &Bytes) {
        for copy in &mut self.copies {
            let Some(tx) = &copy.tx else { continue };
            let sent = match self.mode {
                Mode::Lockstep => tx.send(chunk.clone()).await.is_ok(),
                Mode::Lossy => tx.try_send(chunk.clone()).is_ok(),
            };
            if !sent {
                // The store gave up on its copy, or (lossy) fell behind.
                copy.abort();
            }
        }
    }
}

fn too_long() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "the stream is longer than its declared length",
    )
}

/// Marks the error a copy ends with when it cannot see the whole entry.
#[derive(Debug)]
struct Abandoned;

impl std::fmt::Display for Abandoned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the copied stream ended early")
    }
}

impl std::error::Error for Abandoned {}

pub(crate) fn abandoned() -> io::Error {
    io::Error::other(Abandoned)
}

/// Whether a store failed because its copy was cut short, which says nothing about the store.
pub(crate) fn is_abandoned(e: &io::Error) -> bool {
    e.get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<Abandoned>)
}

fn receiver_stream(rx: mpsc::Receiver<Bytes>, aborted: Arc<AtomicBool>) -> ByteStream {
    Box::pin(stream::unfold(Some((rx, aborted)), |state| async move {
        let (mut rx, aborted) = state?;
        match rx.recv().await {
            Some(chunk) => Some((Ok(chunk), Some((rx, aborted)))),
            None if aborted.load(Ordering::Acquire) => Some((Err(abandoned()), None)),
            None => None,
        }
    }))
}

#[cfg(test)]
mod tests {
    use futures_util::TryStreamExt;

    use super::*;

    fn source(chunks: Vec<io::Result<&'static [u8]>>) -> ByteStream {
        Box::pin(stream::iter(
            chunks.into_iter().map(|c| c.map(Bytes::from_static)),
        ))
    }

    async fn collect(s: ByteStream) -> io::Result<Vec<u8>> {
        let chunks: Vec<Bytes> = s.try_collect().await?;
        Ok(chunks.concat())
    }

    #[tokio::test]
    async fn every_copy_gets_every_byte() {
        let (main, copies) = tee(source(vec![Ok(b"ab"), Ok(b"cd")]), 2, Mode::Lockstep, None);
        let mut copies = copies.into_iter();
        let (a, b, c) = tokio::join!(
            collect(main),
            collect(copies.next().unwrap()),
            collect(copies.next().unwrap())
        );
        for out in [a, b, c] {
            assert_eq!(out.unwrap(), b"abcd");
        }
    }

    #[tokio::test]
    async fn a_failed_source_fails_the_copies() {
        let failing = source(vec![Ok(b"ab"), Err(io::Error::other("client went away"))]);
        let (main, mut copies) = tee(failing, 1, Mode::Lockstep, None);
        let (main, copy) = tokio::join!(collect(main), collect(copies.remove(0)));
        assert!(main.is_err());
        assert!(copy.is_err());
    }

    #[tokio::test]
    async fn dropping_the_main_stream_early_fails_the_copies() {
        let (mut main, mut copies) = tee(source(vec![Ok(b"ab"), Ok(b"cd")]), 1, Mode::Lossy, None);
        main.next().await.unwrap().unwrap();
        drop(main);
        assert!(collect(copies.remove(0)).await.is_err());
    }

    #[tokio::test]
    async fn a_lossy_copy_that_falls_behind_is_abandoned() {
        let chunks = (0..=CHANNEL_CHUNKS).map(|_| Ok(&b"x"[..])).collect();
        let (main, mut copies) = tee(source(chunks), 1, Mode::Lossy, None);
        // The copy is not read until the main stream has finished, so its channel fills.
        assert_eq!(collect(main).await.unwrap().len(), CHANNEL_CHUNKS + 1);
        assert!(collect(copies.remove(0)).await.is_err());
    }

    #[tokio::test]
    async fn a_reader_that_stops_at_the_length_still_completes_the_copies() {
        let (mut main, mut copies) =
            tee(source(vec![Ok(b"ab"), Ok(b"cd")]), 1, Mode::Lossy, Some(4));
        main.next().await.unwrap().unwrap();
        main.next().await.unwrap().unwrap();
        drop(main); // never polled for its end
        assert_eq!(collect(copies.remove(0)).await.unwrap(), b"abcd");
    }

    #[tokio::test]
    async fn an_error_at_the_end_reaches_the_reader_before_the_last_byte() {
        let failing = source(vec![
            Ok(b"ab"),
            Ok(b"cd"),
            Err(io::Error::other("bad checksum")),
        ]);
        let (mut main, mut copies) = tee(failing, 1, Mode::Lossy, Some(4));
        main.next().await.unwrap().unwrap();
        assert!(main.next().await.unwrap().is_err());
        drop(main);
        assert!(collect(copies.remove(0)).await.is_err());
    }

    #[tokio::test]
    async fn a_source_longer_than_declared_fails() {
        let (main, _copies) = tee(source(vec![Ok(b"abc")]), 0, Mode::Lossy, Some(2));
        assert!(collect(main).await.is_err());
    }

    #[tokio::test]
    async fn a_copy_that_stops_reading_does_not_stall_lockstep() {
        let (main, mut copies) = tee(source(vec![Ok(b"ab"), Ok(b"cd")]), 1, Mode::Lockstep, None);
        drop(copies.remove(0));
        assert_eq!(collect(main).await.unwrap(), b"abcd");
    }
}
