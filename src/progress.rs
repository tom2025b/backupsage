//! Frontend-independent indexing events and cooperative cancellation.

use std::io::{self, Read};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use crate::format::Format;

#[derive(Debug, Clone, Copy)]
pub enum SourceKind {
    Archive(Format),
    Directory,
}

#[derive(Debug, Clone, Copy)]
pub enum ProgressUnit {
    Bytes,
    Files,
}

/// Events contain unsanitized source data. Frontends own escaping and rendering.
#[derive(Debug)]
pub enum ProgressEvent<'a> {
    IndexStarted {
        source: &'a Path,
        destination: &'a Path,
        kind: SourceKind,
    },
    Total {
        amount: u64,
        unit: ProgressUnit,
    },
    Advanced {
        amount: u64,
    },
    Entry {
        path: &'a str,
        number: u64,
    },
    Warning {
        message: &'a str,
    },
    IndexDiscovered {
        path: &'a Path,
    },
    /// The staged index is finalized; cancellation can still prevent promotion.
    ReadyToPromote,
    /// The new index was successfully promoted to its destination.
    Finished,
}

/// Callbacks run synchronously on the indexing thread. Shared access allows
/// stream-byte events and entry diagnostics to use the same observer.
pub trait ProgressObserver {
    fn on_event(&self, event: ProgressEvent<'_>);
}

struct Silent;
impl ProgressObserver for Silent {
    fn on_event(&self, _: ProgressEvent<'_>) {}
}

/// Clone a token to request cancellation from another thread or an observer.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("operation cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Cancellation is cooperative, not a timeout or a way to interrupt a blocked
/// OS read. Indexing checks between entries, read chunks and before promotion.
pub struct OperationControl<'a> {
    observer: &'a dyn ProgressObserver,
    cancellation: CancellationToken,
}

impl Default for OperationControl<'_> {
    fn default() -> Self {
        Self::new(&Silent, CancellationToken::default())
    }
}

impl<'a> OperationControl<'a> {
    pub fn new(observer: &'a dyn ProgressObserver, cancellation: CancellationToken) -> Self {
        Self {
            observer,
            cancellation,
        }
    }

    pub fn check_cancelled(&self) -> Result<(), Cancelled> {
        if self.cancellation.is_cancelled() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }

    pub(crate) fn emit(&self, event: ProgressEvent<'_>) {
        self.observer.on_event(event);
    }
}

/// Count compressed input bytes, including the archive tail, without depending
/// on a terminal progress implementation.
pub(crate) struct ProgressReader<'a, R> {
    pub inner: R,
    pub control: &'a OperationControl<'a>,
}

impl<R: Read> Read for ProgressReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.control.check_cancelled().map_err(io::Error::other)?;
        let n = self.inner.read(buf)?;
        self.control
            .emit(ProgressEvent::Advanced { amount: n as u64 });
        self.control.check_cancelled().map_err(io::Error::other)?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    struct CountingReader {
        bytes: io::Cursor<&'static [u8]>,
        reads: usize,
    }

    impl Read for CountingReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            self.bytes.read(buf)
        }
    }

    // Exercise the compressed-input wrapper directly: no tar iterator,
    // process_reader or run_index error remapping can catch cancellation here.
    #[test]
    fn compressed_reader_cancellation_prevents_the_underlying_read() {
        let token = CancellationToken::default();
        let control = OperationControl::new(&Silent, token.clone());
        let mut reader = ProgressReader {
            inner: CountingReader {
                bytes: io::Cursor::new(b"abcd"),
                reads: 0,
            },
            control: &control,
        };
        let mut buf = [0; 2];
        assert_eq!(reader.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf, b"ab");
        assert_eq!(reader.inner.reads, 1);

        token.cancel();
        let error = reader.read(&mut buf).unwrap_err();
        assert!(error.get_ref().unwrap().is::<Cancelled>());
        // A surviving post-read check could still return Cancelled, but must
        // not conceal the forbidden underlying read if the first check goes.
        assert_eq!(reader.inner.reads, 1);
        assert_eq!(&buf, b"ab");
    }

    #[test]
    fn compressed_reader_cancellation_from_advance_fails_that_same_read() {
        struct CancelOnAdvance {
            token: CancellationToken,
            enabled: Cell<bool>,
            advances: RefCell<Vec<u64>>,
        }
        impl ProgressObserver for CancelOnAdvance {
            fn on_event(&self, event: ProgressEvent<'_>) {
                if let ProgressEvent::Advanced { amount } = event {
                    self.advances.borrow_mut().push(amount);
                    if self.enabled.get() {
                        self.token.cancel();
                    }
                }
            }
        }
        let observer = CancelOnAdvance {
            token: CancellationToken::default(),
            enabled: Cell::new(false),
            advances: RefCell::new(Vec::new()),
        };
        let control = OperationControl::new(&observer, observer.token.clone());
        let mut reader = ProgressReader {
            inner: io::Cursor::new(b"abcd"),
            control: &control,
        };
        let mut buf = [0; 2];
        assert_eq!(reader.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf, b"ab");
        assert!(!observer.token.is_cancelled());

        observer.enabled.set(true);
        let error = reader.read(&mut buf).unwrap_err();
        assert!(error.get_ref().unwrap().is::<Cancelled>());
        assert_eq!(&*observer.advances.borrow(), &[2, 2]);
        assert_eq!(reader.inner.position(), 4);
    }
}
