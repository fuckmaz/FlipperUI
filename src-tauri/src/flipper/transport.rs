use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A byte-oriented, blocking, framed-protocol-agnostic transport to the Flipper.
///
/// All methods are blocking and honor the timeout set by [`Transport::set_timeout`].
/// Reads return `io::ErrorKind::TimedOut` on deadline miss (matching serialport
/// semantics so existing reader loops need no changes). Permanent disconnection
/// surfaces as `io::ErrorKind::BrokenPipe`.
pub trait Transport: Send {
    /// Read exactly `buf.len()` bytes or return `TimedOut` / `BrokenPipe`.
    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()>;

    /// Short read: copy up to `buf.len()` bytes, returning how many were written.
    /// Used by byte-drain / byte-by-byte handshake loops (e.g. `session::open_session`).
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;

    /// Write every byte. On BLE this respects peer-side flow-control.
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()>;

    /// Flush buffered writes. On BLE this waits for backpressure to drain.
    fn flush(&mut self) -> io::Result<()>;

    /// Set the blocking timeout applied to subsequent read calls.
    fn set_timeout(&mut self, dur: Duration) -> io::Result<()>;

    /// Push bytes back so the next `read_exact` / `read` returns them first.
    /// Used by the framing layer to roll back a partial read on mid-frame
    /// timeout — without it, a varint byte popped before the timeout would
    /// be lost, desyncing the protobuf framing.
    fn unread(&mut self, bytes: &[u8]);

    /// Which physical transport backs this — used by upper layers for feature gating.
    fn kind(&self) -> TransportKind;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    Serial,
    Ble,
}

/// Adapter wrapping `Box<dyn serialport::SerialPort>` in the [`Transport`] trait.
pub struct SerialTransport {
    pub port: Box<dyn serialport::SerialPort>,
    /// Bytes pushed back via `unread` — drained before any new port read.
    pushback: VecDeque<u8>,
}

impl SerialTransport {
    pub fn new(port: Box<dyn serialport::SerialPort>) -> Self {
        Self {
            port,
            pushback: VecDeque::new(),
        }
    }
}

fn prepend_bytes(pushback: &mut VecDeque<u8>, bytes: &[u8]) {
    for byte in bytes.iter().rev() {
        pushback.push_front(*byte);
    }
}

/// Fill `buf` transactionally from pushback followed by the underlying port.
///
/// `Read::read_exact` cannot report how many bytes it consumed before an
/// error. Keeping the loop here lets us restore every byte already placed in
/// `buf`, including bytes drained from pushback, so a later framing attempt
/// starts at the exact same byte boundary.
fn read_exact_transactional(
    pushback: &mut VecDeque<u8>,
    buf: &mut [u8],
    mut read_port: impl FnMut(&mut [u8]) -> io::Result<usize>,
) -> io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        let Some(byte) = pushback.pop_front() else {
            break;
        };
        buf[filled] = byte;
        filled += 1;
    }
    while filled < buf.len() {
        match read_port(&mut buf[filled..]) {
            Ok(0) => {
                prepend_bytes(pushback, &buf[..filled]);
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "serial port returned 0 bytes",
                ));
            }
            Ok(read) => filled += read,
            Err(error) => {
                prepend_bytes(pushback, &buf[..filled]);
                return Err(error);
            }
        }
    }
    Ok(())
}

impl Transport for SerialTransport {
    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        let Self { port, pushback } = self;
        read_exact_transactional(pushback, buf, |remaining| {
            std::io::Read::read(port, remaining)
        })
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.pushback.is_empty() {
            let take = buf.len().min(self.pushback.len());
            for slot in &mut buf[..take] {
                *slot = self.pushback.pop_front().unwrap();
            }
            return Ok(take);
        }
        std::io::Read::read(&mut self.port, buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        std::io::Write::write_all(&mut self.port, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        std::io::Write::flush(&mut self.port)
    }

    fn set_timeout(&mut self, dur: Duration) -> io::Result<()> {
        self.port.set_timeout(dur).map_err(io::Error::other)
    }

    fn unread(&mut self, bytes: &[u8]) {
        // Prepend so the original byte order is preserved on next read.
        prepend_bytes(&mut self.pushback, bytes);
    }

    fn kind(&self) -> TransportKind {
        TransportKind::Serial
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transactional_exact_read_restores_pushback_and_partial_port_bytes_on_error() {
        let mut pushback = VecDeque::from([1, 2]);
        let mut buffer = [0; 5];
        let mut calls = 0;

        let error = read_exact_transactional(&mut pushback, &mut buffer, |remaining| {
            calls += 1;
            if calls == 1 {
                remaining[..2].copy_from_slice(&[3, 4]);
                Ok(2)
            } else {
                Err(io::Error::new(io::ErrorKind::TimedOut, "test timeout"))
            }
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(pushback, VecDeque::from([1, 2, 3, 4]));

        read_exact_transactional(&mut pushback, &mut buffer, |remaining| {
            remaining[0] = 5;
            Ok(1)
        })
        .unwrap();
        assert_eq!(buffer, [1, 2, 3, 4, 5]);
        assert!(pushback.is_empty());
    }

    #[test]
    fn transactional_exact_read_restores_pushback_and_partial_port_bytes_after_zero_read() {
        let mut pushback = VecDeque::from([7]);
        let mut buffer = [0; 4];
        let mut calls = 0;

        let error = read_exact_transactional(&mut pushback, &mut buffer, |remaining| {
            calls += 1;
            if calls == 1 {
                remaining[..2].copy_from_slice(&[8, 9]);
                Ok(2)
            } else {
                Ok(0)
            }
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(pushback, VecDeque::from([7, 8, 9]));

        read_exact_transactional(&mut pushback, &mut buffer, |remaining| {
            remaining[0] = 10;
            Ok(1)
        })
        .unwrap();
        assert_eq!(buffer, [7, 8, 9, 10]);
        assert!(pushback.is_empty());
    }
}
