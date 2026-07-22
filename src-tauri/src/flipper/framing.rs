use prost::Message;
use std::time::{Duration, Instant};

use crate::error::{FlipperError, Result};
use crate::flipper::diag;
use crate::flipper::transport::Transport;
use crate::pb;
use crate::pb::main::Content;

/// Hard cap on a single PB.Main frame. Real Flipper RPC messages are well
/// under 64 KB (storage writes use 8 KB chunks plus protobuf envelope);
/// 1 MiB is a generous ceiling that still bounds memory if a corrupt or
/// malicious stream announces a huge length prefix.
pub const MAX_FRAME_SIZE: usize = 1 << 20;

/// Error from a deadline-aware framed read.
#[derive(Debug)]
pub enum DeadlineReadError {
    DeadlineElapsed,
    Flipper(FlipperError),
}

impl From<FlipperError> for DeadlineReadError {
    fn from(error: FlipperError) -> Self {
        Self::Flipper(error)
    }
}

/// Read a protobuf-style varint from the transport.
///
/// Returns a u32 message length. Errors if the decoded value exceeds u32::MAX
/// (which would mean a >4 GB message — clearly corrupt framing).
///
/// Transactional: if reading times out (or hits another I/O error) mid-varint,
/// the bytes already consumed are pushed back via `Transport::unread` so the
/// next call can resume cleanly. Without this, a screen-stream reader using a
/// short timeout will pop a varint byte, time out on the next, drop the byte,
/// and desync framing — surfacing as "Protobuf decode error: invalid tag".
pub fn read_varint(t: &mut dyn Transport) -> Result<u32> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    let mut byte = [0u8; 1];
    let mut consumed: [u8; 5] = [0; 5];
    let mut consumed_len = 0usize;
    loop {
        if let Err(e) = t.read_exact(&mut byte) {
            if consumed_len > 0 {
                t.unread(&consumed[..consumed_len]);
            }
            return Err(e.into());
        }
        if consumed_len < consumed.len() {
            consumed[consumed_len] = byte[0];
            consumed_len += 1;
        }
        let b = byte[0] as u64;
        result |= (b & 0x7F) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 35 {
            // A u32 needs at most 5 varint bytes (35 bits). Anything larger
            // is either a corrupt stream or a >4 GB value we can't handle.
            return Err(FlipperError::Decode(prost::DecodeError::new(
                "varint overflow",
            )));
        }
    }
    if result > u32::MAX as u64 {
        return Err(FlipperError::Decode(prost::DecodeError::new(
            "message length exceeds u32::MAX",
        )));
    }
    Ok(result as u32)
}

/// Encode a u64 as a varint into `buf`. Returns the number of bytes written.
fn encode_varint(mut value: u64, buf: &mut [u8; 10]) -> usize {
    let mut i = 0;
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            buf[i] = byte | 0x80;
        } else {
            buf[i] = byte;
            i += 1;
            break;
        }
        i += 1;
    }
    i
}

/// Read one complete `PB.Main` message from the transport.
///
/// Transactional: on a mid-frame timeout while reading the body, the varint
/// length prefix is re-encoded and pushed back via `Transport::unread` so the
/// next call resumes from the same frame boundary. (`SerialTransport` /
/// `BleTransport` already push back any partial body bytes on their side.)
pub fn read_message(t: &mut dyn Transport) -> Result<pb::Main> {
    let len = read_varint(t)?;
    if (len as usize) > MAX_FRAME_SIZE {
        return Err(FlipperError::Decode(prost::DecodeError::new(format!(
            "frame length {len} exceeds MAX_FRAME_SIZE ({MAX_FRAME_SIZE})"
        ))));
    }
    let mut buf = vec![0u8; len as usize];
    if let Err(e) = t.read_exact(&mut buf) {
        let mut varint_buf = [0u8; 10];
        let n = encode_varint(len as u64, &mut varint_buf);
        t.unread(&varint_buf[..n]);
        return Err(e.into());
    }
    let msg = pb::Main::decode(buf.as_slice())?;
    diag::log(diag::Direction::Rx, &msg, len as usize);
    Ok(msg)
}

/// Read one framed message without allowing any individual prefix/body read to
/// outlive `deadline`.
///
/// The remaining monotonic budget is recomputed immediately before every
/// blocking read and applied to the transport. Prefix and body rollback match
/// [`read_varint`] and [`read_message`], including the same frame-size cap.
pub fn read_message_until(
    t: &mut dyn Transport,
    deadline: Instant,
    max_read_slice: Duration,
) -> std::result::Result<pb::Main, DeadlineReadError> {
    read_message_with_budget(t, max_read_slice, || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
    })
}

fn read_message_with_budget(
    t: &mut dyn Transport,
    max_read_slice: Duration,
    mut remaining: impl FnMut() -> Option<Duration>,
) -> std::result::Result<pb::Main, DeadlineReadError> {
    let len = read_varint_with_budget(t, max_read_slice, &mut remaining)?;
    if (len as usize) > MAX_FRAME_SIZE {
        return Err(FlipperError::Decode(prost::DecodeError::new(format!(
            "frame length {len} exceeds MAX_FRAME_SIZE ({MAX_FRAME_SIZE})"
        )))
        .into());
    }

    let mut buf = vec![0u8; len as usize];
    if let Err(error) = read_body_with_budget(t, &mut buf, max_read_slice, &mut remaining) {
        let mut varint_buf = [0u8; 10];
        let n = encode_varint(len as u64, &mut varint_buf);
        t.unread(&varint_buf[..n]);
        return Err(error);
    }

    let msg = pb::Main::decode(buf.as_slice()).map_err(FlipperError::from)?;
    diag::log(diag::Direction::Rx, &msg, len as usize);
    Ok(msg)
}

fn read_varint_with_budget(
    t: &mut dyn Transport,
    max_read_slice: Duration,
    remaining: &mut impl FnMut() -> Option<Duration>,
) -> std::result::Result<u32, DeadlineReadError> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    let mut byte = [0u8; 1];
    let mut consumed = [0u8; 5];
    let mut consumed_len = 0usize;

    loop {
        if let Err(error) = read_exact_with_budget(t, &mut byte, max_read_slice, remaining) {
            if consumed_len > 0 {
                t.unread(&consumed[..consumed_len]);
            }
            return Err(error);
        }
        if consumed_len < consumed.len() {
            consumed[consumed_len] = byte[0];
            consumed_len += 1;
        }
        let value = u64::from(byte[0]);
        result |= (value & 0x7f) << shift;
        if value & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 35 {
            return Err(FlipperError::Decode(prost::DecodeError::new("varint overflow")).into());
        }
    }

    if result > u64::from(u32::MAX) {
        return Err(FlipperError::Decode(prost::DecodeError::new(
            "message length exceeds u32::MAX",
        ))
        .into());
    }
    Ok(result as u32)
}

fn read_exact_with_budget(
    t: &mut dyn Transport,
    buffer: &mut [u8],
    max_read_slice: Duration,
    remaining: &mut impl FnMut() -> Option<Duration>,
) -> std::result::Result<(), DeadlineReadError> {
    let budget = remaining().ok_or(DeadlineReadError::DeadlineElapsed)?;
    t.set_timeout(budget.min(max_read_slice))
        .map_err(|error| DeadlineReadError::Flipper(error.into()))?;
    match t.read_exact(buffer) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::Interrupted
            ) && remaining().is_none() =>
        {
            Err(DeadlineReadError::DeadlineElapsed)
        }
        Err(error) => Err(DeadlineReadError::Flipper(error.into())),
    }
}

fn read_body_with_budget(
    t: &mut dyn Transport,
    buffer: &mut [u8],
    max_read_slice: Duration,
    remaining: &mut impl FnMut() -> Option<Duration>,
) -> std::result::Result<(), DeadlineReadError> {
    let mut filled = 0;
    while filled < buffer.len() {
        let read_result = match remaining() {
            Some(budget) => t
                .set_timeout(budget.min(max_read_slice))
                .map_err(|error| DeadlineReadError::Flipper(error.into()))
                .and_then(|()| {
                    t.read(&mut buffer[filled..]).map_err(|error| {
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::Interrupted
                        ) && remaining().is_none()
                        {
                            DeadlineReadError::DeadlineElapsed
                        } else {
                            DeadlineReadError::Flipper(error.into())
                        }
                    })
                }),
            None => Err(DeadlineReadError::DeadlineElapsed),
        };

        match read_result {
            Ok(0) => {
                t.unread(&buffer[..filled]);
                return Err(DeadlineReadError::Flipper(
                    std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "transport returned 0 bytes while reading a frame body",
                    )
                    .into(),
                ));
            }
            Ok(read) => filled += read,
            Err(error) => {
                t.unread(&buffer[..filled]);
                return Err(error);
            }
        }
    }
    Ok(())
}

/// Read the next RPC response, silently discarding any unsolicited screen-stream
/// frames that may be sitting in the rx buffer.
///
/// This exists because BLE (and to a lesser extent USB) shares one transport
/// between the screen-stream reader and any other RPC command. While
/// `screen_stream_start` is active, the firmware emits `GuiScreenFrame` messages
/// continuously; if a periodic command (ping, power_info, …) writes its
/// request and then calls `read_message`, the next bytes off the wire are
/// frequently a screen frame the reader thread didn't get to first. Treating
/// that frame as a "wrong command_id" response made the legacy caller tear down the
/// session and silently kill the screen reader. Skipping the frame here keeps
/// both consumers happy at the cost of dropping a single frame per racing call.
///
/// This is the helper every RPC command path should use; the screen reader
/// itself keeps calling `read_message` directly because it actually wants
/// frames.
pub fn read_response(t: &mut dyn Transport) -> Result<pb::Main> {
    loop {
        let msg = read_message(t)?;
        if matches!(msg.content, Some(Content::GuiScreenFrame(_))) {
            continue;
        }
        return Ok(msg);
    }
}

/// Write one `PB.Main` message to the transport with a varint length prefix.
pub fn write_message(t: &mut dyn Transport, msg: &pb::Main) -> Result<()> {
    let encoded = msg.encode_to_vec();
    let mut varint_buf = [0u8; 10];
    let varint_len = encode_varint(encoded.len() as u64, &mut varint_buf);
    t.write_all(&varint_buf[..varint_len])?;
    t.write_all(&encoded)?;
    t.flush()?;
    diag::log(diag::Direction::Tx, msg, encoded.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        let cases: &[u32] = &[0, 1, 127, 128, 255, 300, 16383, 16384, 65535, u32::MAX];
        for &v in cases {
            let mut buf = [0u8; 10];
            let len = encode_varint(v as u64, &mut buf);
            // Decode using a cursor
            let mut cursor = std::io::Cursor::new(&buf[..len]);
            let mut result: u64 = 0;
            let mut shift = 0u32;
            loop {
                let mut b = [0u8; 1];
                std::io::Read::read_exact(&mut cursor, &mut b).unwrap();
                let byte = b[0] as u64;
                result |= (byte & 0x7F) << shift;
                if byte & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            assert_eq!(result as u32, v, "varint roundtrip failed for {v}");
        }
    }

    /// Test transport that hands out queued chunks of bytes. Each `read_exact`
    /// pops from the next chunk; if the chunk has fewer bytes than requested,
    /// it returns TimedOut to simulate a mid-frame BLE timeout.
    struct ChunkedTransport {
        chunks: std::collections::VecDeque<Vec<u8>>,
        pushback: std::collections::VecDeque<u8>,
        timeouts: Vec<std::time::Duration>,
    }

    impl ChunkedTransport {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                chunks: chunks.into(),
                pushback: std::collections::VecDeque::new(),
                timeouts: Vec::new(),
            }
        }
    }

    impl crate::flipper::transport::Transport for ChunkedTransport {
        fn read_exact(&mut self, buf: &mut [u8]) -> std::io::Result<()> {
            let mut filled = 0;
            while filled < buf.len() && !self.pushback.is_empty() {
                buf[filled] = self.pushback.pop_front().unwrap();
                filled += 1;
            }
            if filled == buf.len() {
                return Ok(());
            }
            let Some(chunk) = self.chunks.pop_front() else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "no more chunks",
                ));
            };
            let need = buf.len() - filled;
            if chunk.len() < need {
                // Simulate mid-frame timeout: caller's read_exact partially
                // satisfied. We push the chunk into pushback so it survives,
                // mirroring how real BleTransport keeps unconsumed bytes.
                self.pushback.extend(chunk);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "chunk underflow",
                ));
            }
            buf[filled..].copy_from_slice(&chunk[..need]);
            // Any extra in this chunk beyond `need` is leftover available to
            // the next read.
            for &b in &chunk[need..] {
                self.pushback.push_back(b);
            }
            Ok(())
        }
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            if !self.pushback.is_empty() {
                let take = buf.len().min(self.pushback.len());
                for slot in &mut buf[..take] {
                    *slot = self.pushback.pop_front().unwrap();
                }
                return Ok(take);
            }
            let Some(chunk) = self.chunks.pop_front() else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "no more chunks",
                ));
            };
            let take = buf.len().min(chunk.len());
            buf[..take].copy_from_slice(&chunk[..take]);
            self.pushback.extend(chunk[take..].iter().copied());
            Ok(take)
        }
        fn write_all(&mut self, _buf: &[u8]) -> std::io::Result<()> {
            unimplemented!()
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        fn set_timeout(&mut self, dur: std::time::Duration) -> std::io::Result<()> {
            self.timeouts.push(dur);
            Ok(())
        }
        fn unread(&mut self, bytes: &[u8]) {
            for b in bytes.iter().rev() {
                self.pushback.push_front(*b);
            }
        }
        fn kind(&self) -> crate::flipper::transport::TransportKind {
            crate::flipper::transport::TransportKind::Ble
        }
    }

    struct SlowDripTransport {
        source: std::collections::VecDeque<u8>,
        pushback: std::collections::VecDeque<u8>,
        max_short_read: usize,
        short_read_calls: usize,
        timeouts: Vec<Duration>,
    }

    impl SlowDripTransport {
        fn new(bytes: Vec<u8>, max_short_read: usize) -> Self {
            Self {
                source: bytes.into(),
                pushback: std::collections::VecDeque::new(),
                max_short_read,
                short_read_calls: 0,
                timeouts: Vec::new(),
            }
        }

        fn pop_byte(&mut self) -> Option<u8> {
            self.pushback
                .pop_front()
                .or_else(|| self.source.pop_front())
        }
    }

    impl crate::flipper::transport::Transport for SlowDripTransport {
        fn read_exact(&mut self, buf: &mut [u8]) -> std::io::Result<()> {
            let mut filled = 0;
            while filled < buf.len() {
                let Some(byte) = self.pop_byte() else {
                    self.unread(&buf[..filled]);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "slow-drip source exhausted",
                    ));
                };
                buf[filled] = byte;
                filled += 1;
            }
            Ok(())
        }

        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.short_read_calls += 1;
            let limit = buf.len().min(self.max_short_read);
            let mut filled = 0;
            while filled < limit {
                let Some(byte) = self.pop_byte() else {
                    break;
                };
                buf[filled] = byte;
                filled += 1;
            }
            Ok(filled)
        }

        fn write_all(&mut self, _buf: &[u8]) -> std::io::Result<()> {
            unimplemented!()
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        fn set_timeout(&mut self, dur: Duration) -> std::io::Result<()> {
            self.timeouts.push(dur);
            Ok(())
        }

        fn unread(&mut self, bytes: &[u8]) {
            for byte in bytes.iter().rev() {
                self.pushback.push_front(*byte);
            }
        }

        fn kind(&self) -> crate::flipper::transport::TransportKind {
            crate::flipper::transport::TransportKind::Serial
        }
    }

    #[test]
    fn read_message_recovers_after_mid_varint_timeout() {
        // Encode a real message and split its bytes into pathological chunks
        // that strand the reader mid-varint and mid-body. Two failed attempts
        // followed by a successful one must decode to the original message.
        let msg = pb::Main {
            command_id: 7,
            command_status: 0,
            has_next: false,
            content: None,
        };
        let encoded = msg.encode_to_vec();
        let mut framed = Vec::new();
        let mut varint_buf = [0u8; 10];
        let n = encode_varint(encoded.len() as u64, &mut varint_buf);
        framed.extend_from_slice(&varint_buf[..n]);
        framed.extend_from_slice(&encoded);

        // Single-byte chunks force the body read to underflow on the first
        // attempt: varint succeeds, body pop sees only 1 of N bytes and times
        // out. Without rollback, the varint and partial body byte would be
        // lost; with it, the second attempt can resume and decode cleanly.
        assert!(framed.len() >= 3, "framed too short to test");
        let chunks: Vec<Vec<u8>> = framed.iter().map(|b| vec![*b]).collect();
        let mut t = ChunkedTransport::new(chunks);

        let r1 = read_message(&mut t);
        assert!(r1.is_err(), "first call should time out");
        let decoded = read_message(&mut t).expect("second call should succeed");
        assert_eq!(decoded.command_id, 7);
    }

    #[test]
    fn message_roundtrip() {
        let msg = pb::Main {
            command_id: 42,
            command_status: 0,
            has_next: false,
            content: None,
        };
        let encoded = msg.encode_to_vec();
        let decoded = pb::Main::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.command_id, 42);
        assert!(!decoded.has_next);
    }

    fn framed_screen_message() -> (pb::Main, Vec<u8>) {
        let message = pb::Main {
            command_id: 3,
            command_status: 0,
            has_next: false,
            content: Some(Content::GuiScreenFrame(crate::pb_gui::ScreenFrame {
                data: vec![0x5a; 256],
                orientation: 0,
                bg_color: 0,
                fg_color: 1,
            })),
        };
        let encoded = message.encode_to_vec();
        assert!(encoded.len() >= 128, "test requires a multi-byte varint");
        let mut framed = Vec::new();
        let mut varint = [0u8; 10];
        let prefix_len = encode_varint(encoded.len() as u64, &mut varint);
        assert_eq!(prefix_len, 2, "fixture should use two prefix reads");
        framed.extend_from_slice(&varint[..prefix_len]);
        framed.extend_from_slice(&encoded);
        (message, framed)
    }

    fn message_with_encoded_len(target: usize) -> pb::Main {
        let mut data_len = target;
        for _ in 0..8 {
            let message = pb::Main {
                command_id: 1,
                command_status: 0,
                has_next: false,
                content: Some(Content::GuiScreenFrame(crate::pb_gui::ScreenFrame {
                    data: vec![0x5a; data_len],
                    orientation: 0,
                    bg_color: 0,
                    fg_color: 1,
                })),
            };
            let encoded_len = message.encoded_len();
            if encoded_len == target {
                return message;
            }
            if encoded_len > target {
                data_len -= encoded_len - target;
            } else {
                data_len += target - encoded_len;
            }
        }
        panic!("could not build a protobuf message with encoded length {target}");
    }

    #[test]
    fn frame_size_cap_accepts_exact_boundary_and_rejects_one_over() {
        let exact = message_with_encoded_len(MAX_FRAME_SIZE);
        let exact_body = exact.encode_to_vec();
        assert_eq!(exact_body.len(), MAX_FRAME_SIZE);
        let mut exact_frame = Vec::with_capacity(MAX_FRAME_SIZE + 4);
        let mut prefix = [0; 10];
        let prefix_len = encode_varint(exact_body.len() as u64, &mut prefix);
        exact_frame.extend_from_slice(&prefix[..prefix_len]);
        exact_frame.extend_from_slice(&exact_body);

        let mut exact_transport = ChunkedTransport::new(vec![exact_frame]);
        let decoded = read_message(&mut exact_transport).unwrap();
        assert_eq!(decoded, exact);

        let mut over_prefix = [0; 10];
        let over_prefix_len = encode_varint((MAX_FRAME_SIZE + 1) as u64, &mut over_prefix);
        let mut over_transport =
            ChunkedTransport::new(vec![over_prefix[..over_prefix_len].to_vec()]);
        let error = read_message(&mut over_transport).unwrap_err();
        assert!(matches!(error, FlipperError::Decode(_)));
        assert!(error.to_string().contains("exceeds MAX_FRAME_SIZE"));
    }

    #[test]
    fn deadline_reader_recomputes_decreasing_timeout_for_each_prefix_and_body_read() {
        let (message, framed) = framed_screen_message();
        let mut transport = ChunkedTransport::new(vec![framed]);
        let mut budgets = [
            Some(Duration::from_millis(9)),
            Some(Duration::from_millis(6)),
            Some(Duration::from_millis(3)),
        ]
        .into_iter();

        let decoded = read_message_with_budget(&mut transport, Duration::from_secs(1), || {
            budgets.next().flatten()
        })
        .unwrap();

        assert_eq!(decoded.command_id, message.command_id);
        assert_eq!(
            transport.timeouts,
            [
                Duration::from_millis(9),
                Duration::from_millis(6),
                Duration::from_millis(3)
            ]
        );
    }

    #[test]
    fn deadline_before_body_rolls_back_prefix_for_a_later_transactional_read() {
        let (message, framed) = framed_screen_message();
        let mut transport = ChunkedTransport::new(vec![framed]);
        let mut budgets = [
            Some(Duration::from_millis(9)),
            Some(Duration::from_millis(6)),
            None,
        ]
        .into_iter();

        let deadline = read_message_with_budget(&mut transport, Duration::from_secs(1), || {
            budgets.next().flatten()
        });
        assert!(matches!(deadline, Err(DeadlineReadError::DeadlineElapsed)));

        let decoded = read_message(&mut transport).unwrap();
        assert_eq!(decoded.command_id, message.command_id);
    }

    #[test]
    fn deadline_mid_body_stops_before_another_read_and_recovers_exact_frame() {
        let (message, framed) = framed_screen_message();
        let mut transport = SlowDripTransport::new(framed, 3);
        let mut budgets = [
            Some(Duration::from_millis(9)),
            Some(Duration::from_millis(8)),
            Some(Duration::from_millis(7)),
            Some(Duration::from_millis(6)),
            None,
        ]
        .into_iter();

        let result = read_message_with_budget(&mut transport, Duration::from_secs(1), || {
            budgets.next().flatten()
        });

        assert!(matches!(result, Err(DeadlineReadError::DeadlineElapsed)));
        assert_eq!(transport.short_read_calls, 2);
        assert_eq!(transport.timeouts.len(), 4);

        let recovered = read_message(&mut transport).unwrap();
        assert_eq!(recovered, message);
    }

    #[test]
    fn repeated_mid_body_deadlines_restore_the_same_frame_each_time() {
        let (message, framed) = framed_screen_message();
        let mut transport = SlowDripTransport::new(framed, 2);

        for attempt in 1..=3 {
            let mut budgets = [
                Some(Duration::from_millis(9)),
                Some(Duration::from_millis(8)),
                Some(Duration::from_millis(7)),
                None,
            ]
            .into_iter();
            let result = read_message_with_budget(&mut transport, Duration::from_secs(1), || {
                budgets.next().flatten()
            });
            assert!(matches!(result, Err(DeadlineReadError::DeadlineElapsed)));
            assert_eq!(transport.short_read_calls, attempt);
        }

        let recovered = read_message(&mut transport).unwrap();
        assert_eq!(recovered, message);
    }
}
