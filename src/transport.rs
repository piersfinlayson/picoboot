// Copyright (C) 2026 Piers Finlayson <piers@piers.rocks>
//
// MIT License

//! The interface to a device's connection, for example USB.
//!
//! A [`Transport`] is one device, open or closed.  [`Picoboot::connect()`]
//! opens it and the last copy of the [`Connection`] closes it.  While it is
//! open each [`Transport::command()`] is one whole PICOBOOT [`Transaction`].
//!
//! [`Picoboot::connect()`]: crate::Picoboot::connect
//! [`Connection`]: crate::Connection

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::cmd::PicobootStatusCmd;
use crate::{Direction, Error, Target};

/// The future a [`Transport`]'s async methods return.  The same definition as
/// futures-core's.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A connection to a device, for example USB.
///
/// A transport is closed until [`Self::open()`].  It counts opens: the first
/// open claims the device and the last [`Self::close()`] releases it.  Copies
/// of a transport share the count.
///
/// picoboot calls every method through `&self`, from copies of a
/// [`Connection`](crate::Connection) that may run at the same time.  A
/// transport keeps each [`Transaction`] whole.
///
/// `Display` is [`Picoboot::info()`](crate::Picoboot::info).
pub trait Transport: fmt::Debug + fmt::Display + Send + Sync {
    /// The device's target, where the transport can tell.
    /// [`Picoboot::new()`](crate::Picoboot::new) fails when it is `None`.
    fn target(&self) -> Option<Target>;

    /// Opens the device.  The first open claims it.  A failure to claim it is
    /// [`TransportErrorKind::Access`].
    fn open<'a>(
        &'a self,
        target: &'a Target,
        timeouts: &'a Timeouts,
    ) -> BoxFuture<'a, Result<(), Error>>;

    /// Closes the device.  The last close releases it.  Failures are logged.
    fn close(&self);

    /// Runs one transaction and returns its IN data, empty for any other
    /// transaction.  A closed transport fails with
    /// [`TransportErrorKind::Closed`].
    fn command<'a>(
        &'a self,
        transaction: Transaction<'a>,
        timeouts: &'a Timeouts,
    ) -> BoxFuture<'a, Result<Vec<u8>, Error>>;

    /// The device's status for the last transaction.
    fn command_status<'a>(
        &'a self,
        timeouts: &'a Timeouts,
    ) -> BoxFuture<'a, Result<PicobootStatusCmd, Error>>;

    /// Resets the device's PICOBOOT interface.
    fn reset<'a>(&'a self, timeouts: &'a Timeouts) -> BoxFuture<'a, Result<(), Error>>;

    /// The largest dTransferLength the transport accepts, `None` for no
    /// limit.
    fn max_transfer(&self) -> Option<usize> {
        None
    }
}

/// One PICOBOOT transaction: the command packet, its data and its completion.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Transaction<'a> {
    /// The 32-byte command packet, RP2350 datasheet section 5.6.4.
    pub packet: &'a [u8; 32],
    pub direction: Direction,
    /// dTransferLength.
    pub transfer_len: usize,
    /// The OUT data.  Empty for an IN transaction or a zero length.
    pub out: &'a [u8],
}

/// Timeouts for PICOBOOT operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timeouts {
    /// Each transfer of a transaction.
    pub endpoint: Duration,

    /// Reading the command status.
    pub command_status: Duration,

    /// Resetting the interface.
    pub reset: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            endpoint: Duration::from_secs(5),
            command_status: Duration::from_secs(1),
            reset: Duration::from_secs(1),
        }
    }
}

/// A transport's failure, within [`Error::Transport`].
#[derive(Debug)]
pub struct TransportError {
    kind: TransportErrorKind,
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl TransportError {
    /// `source` is the transport's own error.
    pub fn new(
        kind: TransportErrorKind,
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        Self {
            kind,
            source: source.into(),
        }
    }

    pub fn kind(&self) -> TransportErrorKind {
        self.kind
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(f)
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// The class of a [`TransportError`].  [`Self::AfterSend`] is the only kind
/// under which a transaction may have run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransportErrorKind {
    /// The transport couldn't get the device.  The transaction didn't run.
    Access,
    /// The transaction may have run.  A transport reports it whenever it
    /// can't rule out that the device received the command packet.
    AfterSend,
    /// The transport is closed.  The transaction didn't run.
    Closed,
    /// Any other failure.  From [`Transport::command()`] the transaction
    /// didn't run.
    Other,
}
