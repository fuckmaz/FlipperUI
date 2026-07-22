use crate::error::{FlipperError, Result};
use crate::flipper::client::FlipperClient;
use crate::flipper::diag;
use crate::flipper::framing::{read_response, write_message};
use crate::flipper::session::check_response;
use crate::flipper::transport::TransportKind;
use crate::pb;
use crate::pb::main::Content;
use crate::pb_storage;
use std::time::Duration;

const SERIAL_WRITE_CHUNK_SIZE: usize = 8192;
const BLE_WRITE_CHUNK_SIZE: usize = 512;
const BLE_WRITE_ACK_TIMEOUT: Duration = Duration::from_secs(30);
const NORMAL_RPC_TIMEOUT: Duration = Duration::from_secs(5);

fn write_chunk_size(kind: TransportKind) -> usize {
    match kind {
        TransportKind::Serial => SERIAL_WRITE_CHUNK_SIZE,
        TransportKind::Ble => BLE_WRITE_CHUNK_SIZE,
    }
}

fn read_matching_response(client: &mut FlipperClient, expected_id: u32) -> Result<pb::Main> {
    loop {
        let msg = read_response(&mut *client.transport)?;
        if msg.command_id == expected_id {
            check_response(&msg, expected_id)?;
            return Ok(msg);
        }
        diag::log_event(
            "StorageWriteForeignResponse",
            format!(
                "expected_command_id={} received_command_id={} has_next={}",
                expected_id, msg.command_id, msg.has_next
            ),
        );
    }
}

/// Send a single request and read one response, validating command_id + status.
fn send_single(client: &mut FlipperClient, content: Content) -> Result<pb::Main> {
    let id = client.next_command_id();
    let req = pb::Main {
        command_id: id,
        command_status: 0,
        has_next: false,
        content: Some(content),
    };
    write_message(&mut *client.transport, &req)?;
    let resp = read_response(&mut *client.transport)?;
    check_response(&resp, id)?;
    Ok(resp)
}

/// List the contents of a directory on the Flipper.
pub fn storage_list(client: &mut FlipperClient, path: &str) -> Result<Vec<pb_storage::File>> {
    let id = client.next_command_id();
    let req = pb::Main {
        command_id: id,
        command_status: 0,
        has_next: false,
        content: Some(Content::StorageListRequest(pb_storage::ListRequest {
            path: path.to_string(),
            include_md5: false,
            filter_max_size: 0,
        })),
    };
    write_message(&mut *client.transport, &req)?;

    let mut files = Vec::new();
    loop {
        let msg = read_response(&mut *client.transport)?;
        check_response(&msg, id)?;
        if let Some(Content::StorageListResponse(r)) = msg.content {
            files.extend(r.file);
        }
        if !msg.has_next {
            break;
        }
    }
    Ok(files)
}

/// Get metadata for a file or directory.
pub fn storage_stat(client: &mut FlipperClient, path: &str) -> Result<pb_storage::File> {
    let resp = send_single(
        client,
        Content::StorageStatRequest(pb_storage::StatRequest {
            path: path.to_string(),
        }),
    )?;
    match resp.content {
        Some(Content::StorageStatResponse(r)) => r.file.ok_or(FlipperError::UnexpectedResponse),
        _ => Err(FlipperError::UnexpectedResponse),
    }
}

/// Read a file from the Flipper, returning its contents as bytes.
///
/// `on_progress(bytes_received, total_bytes)` is called after each chunk is received,
/// so the caller can report progress upstream (e.g. via Tauri events).
/// `cancelled` is checked between chunks — if set, the transfer aborts early.
pub fn storage_read<F>(
    client: &mut FlipperClient,
    path: &str,
    on_progress: F,
    cancelled: impl Fn() -> bool,
) -> Result<Vec<u8>>
where
    F: Fn(usize, usize),
{
    // Stat first to get total size for progress reporting
    let total_size = storage_stat(client, path)
        .map(|f| f.size as usize)
        .unwrap_or(0);

    let id = client.next_command_id();
    let req = pb::Main {
        command_id: id,
        command_status: 0,
        has_next: false,
        content: Some(Content::StorageReadRequest(pb_storage::ReadRequest {
            path: path.to_string(),
        })),
    };
    write_message(&mut *client.transport, &req)?;

    let mut data = Vec::new();
    loop {
        if cancelled() {
            // Drain remaining response frames so the protocol stays in sync
            loop {
                let drain = read_response(&mut *client.transport)?;
                if !drain.has_next {
                    break;
                }
            }
            return Err(FlipperError::TransferCancelled);
        }

        let msg = read_response(&mut *client.transport)?;
        check_response(&msg, id)?;
        match msg.content {
            Some(Content::StorageReadResponse(r)) => {
                let file = r.file.ok_or(FlipperError::UnexpectedResponse)?;
                data.extend_from_slice(&file.data);
                if total_size > 0 {
                    on_progress(data.len(), total_size);
                }
            }
            _ => return Err(FlipperError::UnexpectedResponse),
        }
        if !msg.has_next {
            break;
        }
    }
    Ok(data)
}

/// Write data to a file on the Flipper.
/// Large files are split into transport-specific chunks, each sent as a
/// separate protobuf frame with the same command_id. BLE follows the official
/// mobile app's 512-byte storage chunks; serial keeps larger 8 KiB chunks.
///
/// `on_progress(bytes_sent, total_bytes)` is called after each chunk is written
/// to the transport, so the caller can report progress upstream.
/// `cancelled` is checked between chunks — if set, the transfer aborts early.
pub fn storage_write<F>(
    client: &mut FlipperClient,
    path: &str,
    data: &[u8],
    on_progress: F,
    cancelled: impl Fn() -> bool,
) -> Result<()>
where
    F: Fn(usize, usize),
{
    let id = client.next_command_id();
    let kind = client.kind();
    let chunk_size = write_chunk_size(kind);
    diag::log_event(
        "StorageWriteStart",
        format!(
            "path={} bytes={} chunk_size={} transport={:?}",
            path,
            data.len(),
            chunk_size,
            kind
        ),
    );

    // Ensure at least one frame is sent even for empty files
    let chunks: Vec<&[u8]> = if data.is_empty() {
        vec![&[]]
    } else {
        data.chunks(chunk_size).collect()
    };
    let total_chunks = chunks.len();
    let total_bytes = data.len();
    let mut bytes_sent = 0usize;

    for (i, chunk) in chunks.iter().enumerate() {
        if cancelled() {
            // Send a final empty chunk to close the write stream cleanly
            let abort_frame = pb::Main {
                command_id: id,
                command_status: 0,
                has_next: false,
                content: Some(Content::StorageWriteRequest(pb_storage::WriteRequest {
                    path: path.to_string(),
                    file: Some(pb_storage::File {
                        r#type: 0,
                        name: String::new(),
                        size: 0,
                        data: Vec::new(),
                        md5sum: String::new(),
                    }),
                })),
            };
            write_message(&mut *client.transport, &abort_frame)?;
            let _ = read_response(&mut *client.transport); // read the response
                                                           // Delete the incomplete file
            let _ = storage_delete(client, path, false);
            return Err(FlipperError::TransferCancelled);
        }

        let is_last = i == total_chunks - 1;
        let frame = pb::Main {
            command_id: id,
            command_status: 0,
            has_next: !is_last,
            content: Some(Content::StorageWriteRequest(pb_storage::WriteRequest {
                path: path.to_string(),
                file: Some(pb_storage::File {
                    r#type: 0, // FileType::File
                    name: String::new(),
                    size: 0,
                    data: chunk.to_vec(),
                    md5sum: String::new(),
                }),
            })),
        };
        write_message(&mut *client.transport, &frame)?;
        bytes_sent = bytes_sent.saturating_add(chunk.len()).min(total_bytes);
        on_progress(bytes_sent, total_bytes);
    }

    // Device responds once with Empty after receiving all chunks
    diag::log_event(
        "StorageWriteAwaitAck",
        format!("command_id={} bytes={}", id, total_bytes),
    );
    if kind == TransportKind::Ble {
        client.transport.set_timeout(BLE_WRITE_ACK_TIMEOUT)?;
    }
    let ack_result = read_matching_response(client, id);
    if kind == TransportKind::Ble {
        let restore_result = client.transport.set_timeout(NORMAL_RPC_TIMEOUT);
        restore_result?;
    }
    ack_result?;
    diag::log_event(
        "StorageWriteComplete",
        format!("command_id={} bytes={}", id, total_bytes),
    );
    Ok(())
}

/// Rename or move a file/directory on the Flipper.
pub fn storage_rename(client: &mut FlipperClient, old_path: &str, new_path: &str) -> Result<()> {
    send_single(
        client,
        Content::StorageRenameRequest(pb_storage::RenameRequest {
            old_path: old_path.to_string(),
            new_path: new_path.to_string(),
        }),
    )?;
    Ok(())
}

/// Create a directory on the Flipper.
pub fn storage_mkdir(client: &mut FlipperClient, path: &str) -> Result<()> {
    send_single(
        client,
        Content::StorageMkdirRequest(pb_storage::MkdirRequest {
            path: path.to_string(),
        }),
    )?;
    Ok(())
}

/// Delete a file or directory on the Flipper.
pub fn storage_delete(client: &mut FlipperClient, path: &str, recursive: bool) -> Result<()> {
    send_single(
        client,
        Content::StorageDeleteRequest(pb_storage::DeleteRequest {
            path: path.to_string(),
            recursive,
        }),
    )?;
    Ok(())
}

/// Recursively sum the size of all files under `path`. Useful for reporting
/// usage of the `/int` namespace, which on modern Flipper firmware is aliased
/// onto the SD card (so `storage_info("/int")` returns the SD card's total/free,
/// not the internal namespace's actual footprint).
pub fn storage_du(client: &mut FlipperClient, path: &str) -> Result<u64> {
    let mut queue: Vec<String> = vec![path.to_string()];
    let mut total: u64 = 0;
    while let Some(dir) = queue.pop() {
        // A missing `/int` (pre-alias firmware, or SD ejected) should surface
        // as zero usage, not a hard error.
        let entries = match storage_list(client, &dir) {
            Ok(v) => v,
            Err(FlipperError::Rpc { .. }) => continue,
            Err(e) => return Err(e),
        };
        for e in entries {
            // FileType::DIR = 1 in the protobuf.
            if e.r#type == 1 {
                let sub = crate::flipper::library_walk::join_path(&dir, &e.name)?;
                queue.push(sub);
            } else {
                total = total.saturating_add(e.size as u64);
            }
        }
    }
    Ok(total)
}

/// Get storage space info (total/free bytes) for a storage path (e.g. "/ext" or "/int").
pub fn storage_info(client: &mut FlipperClient, path: &str) -> Result<(u64, u64)> {
    let resp = send_single(
        client,
        Content::StorageInfoRequest(pb_storage::InfoRequest {
            path: path.to_string(),
        }),
    )?;
    match resp.content {
        Some(Content::StorageInfoResponse(r)) => Ok((r.total_space, r.free_space)),
        _ => Err(FlipperError::UnexpectedResponse),
    }
}

/// Get the modification timestamp of a file (Unix epoch seconds).
pub fn storage_timestamp(client: &mut FlipperClient, path: &str) -> Result<u32> {
    let resp = send_single(
        client,
        Content::StorageTimestampRequest(pb_storage::TimestampRequest {
            path: path.to_string(),
        }),
    )?;
    match resp.content {
        Some(Content::StorageTimestampResponse(r)) => Ok(r.timestamp),
        _ => Err(FlipperError::UnexpectedResponse),
    }
}

/// Extract a .tar archive on the Flipper's filesystem.
pub fn storage_tar_extract(
    client: &mut FlipperClient,
    tar_path: &str,
    out_path: &str,
) -> Result<()> {
    send_single(
        client,
        Content::StorageTarExtractRequest(pb_storage::TarExtractRequest {
            tar_path: tar_path.to_string(),
            out_path: out_path.to_string(),
        }),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flipper::framing::read_varint;
    use crate::flipper::transport::{Transport, TransportKind};
    use prost::Message;
    use std::collections::VecDeque;
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct RecordingTransport {
        kind: TransportKind,
        writes: Arc<Mutex<Vec<u8>>>,
        reads: VecDeque<u8>,
        timeout_when_empty: bool,
    }

    impl RecordingTransport {
        fn new(kind: TransportKind, response_command_id: u32) -> (Self, Arc<Mutex<Vec<u8>>>) {
            Self::with_responses(kind, vec![Self::ok_response(response_command_id)])
        }

        fn with_responses(
            kind: TransportKind,
            responses: Vec<pb::Main>,
        ) -> (Self, Arc<Mutex<Vec<u8>>>) {
            let writes = Arc::new(Mutex::new(Vec::new()));
            let mut framed = Vec::new();
            for response in responses {
                let encoded = response.encode_to_vec();
                framed.extend_from_slice(&encode_varint(encoded.len() as u64));
                framed.extend_from_slice(&encoded);
            }

            (
                Self {
                    kind,
                    writes: Arc::clone(&writes),
                    reads: framed.into(),
                    timeout_when_empty: false,
                },
                writes,
            )
        }

        fn ok_response(response_command_id: u32) -> pb::Main {
            pb::Main {
                command_id: response_command_id,
                command_status: 0,
                has_next: false,
                content: Some(Content::Empty(pb::Empty {})),
            }
        }

        fn timeout(kind: TransportKind) -> (Self, Arc<Mutex<Vec<u8>>>) {
            let writes = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    kind,
                    writes: Arc::clone(&writes),
                    reads: VecDeque::new(),
                    timeout_when_empty: true,
                },
                writes,
            )
        }
    }

    impl Transport for RecordingTransport {
        fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
            for slot in buf {
                *slot = self.reads.pop_front().ok_or_else(|| {
                    if self.timeout_when_empty {
                        io::Error::new(io::ErrorKind::TimedOut, "BLE read timeout")
                    } else {
                        io::Error::new(io::ErrorKind::UnexpectedEof, "no response bytes")
                    }
                })?;
            }
            Ok(())
        }

        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let take = buf.len().min(self.reads.len());
            for slot in &mut buf[..take] {
                *slot = self.reads.pop_front().unwrap();
            }
            Ok(take)
        }

        fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
            self.writes.lock().unwrap().extend_from_slice(buf);
            Ok(())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn set_timeout(&mut self, _dur: Duration) -> io::Result<()> {
            Ok(())
        }

        fn unread(&mut self, bytes: &[u8]) {
            for b in bytes.iter().rev() {
                self.reads.push_front(*b);
            }
        }

        fn kind(&self) -> TransportKind {
            self.kind
        }
    }

    fn encode_varint(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
        out
    }

    fn decode_written_messages(bytes: &[u8]) -> Vec<pb::Main> {
        struct CursorTransport {
            bytes: VecDeque<u8>,
        }

        impl Transport for CursorTransport {
            fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
                for slot in buf {
                    *slot = self
                        .bytes
                        .pop_front()
                        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no bytes"))?;
                }
                Ok(())
            }

            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let take = buf.len().min(self.bytes.len());
                for slot in &mut buf[..take] {
                    *slot = self.bytes.pop_front().unwrap();
                }
                Ok(take)
            }

            fn write_all(&mut self, _buf: &[u8]) -> io::Result<()> {
                Ok(())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }

            fn set_timeout(&mut self, _dur: Duration) -> io::Result<()> {
                Ok(())
            }

            fn unread(&mut self, bytes: &[u8]) {
                for b in bytes.iter().rev() {
                    self.bytes.push_front(*b);
                }
            }

            fn kind(&self) -> TransportKind {
                TransportKind::Serial
            }
        }

        let mut t = CursorTransport {
            bytes: bytes.iter().copied().collect(),
        };
        let mut messages = Vec::new();
        while !t.bytes.is_empty() {
            let len = read_varint(&mut t).unwrap() as usize;
            let mut body = vec![0; len];
            t.read_exact(&mut body).unwrap();
            messages.push(pb::Main::decode(body.as_slice()).unwrap());
        }
        messages
    }

    #[test]
    fn ble_storage_write_uses_512_byte_payload_chunks() {
        let (transport, writes) = RecordingTransport::new(TransportKind::Ble, 1);
        let mut client = FlipperClient::new(Box::new(transport));
        let data = vec![0x42; 80 * 1024];

        storage_write(
            &mut client,
            "/ext/apps/ACAB/app.fap",
            &data,
            |_, _| {},
            || false,
        )
        .unwrap();

        let written = writes.lock().unwrap().clone();
        let frames = decode_written_messages(&written);
        assert_eq!(frames.len(), 160); // 80 * 1024 / 512
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(frame.command_id, 1);
            assert_eq!(frame.has_next, i + 1 != frames.len());
            let Some(Content::StorageWriteRequest(req)) = &frame.content else {
                panic!("expected StorageWriteRequest");
            };
            assert_eq!(req.file.as_ref().unwrap().data.len(), BLE_WRITE_CHUNK_SIZE);
        }
    }

    #[test]
    fn serial_storage_write_keeps_8k_payload_chunks() {
        let (transport, writes) = RecordingTransport::new(TransportKind::Serial, 1);
        let mut client = FlipperClient::new(Box::new(transport));
        let data = vec![0x42; 80 * 1024];

        storage_write(
            &mut client,
            "/ext/apps/ACAB/app.fap",
            &data,
            |_, _| {},
            || false,
        )
        .unwrap();

        let written = writes.lock().unwrap().clone();
        let frames = decode_written_messages(&written);
        assert_eq!(frames.len(), 10); // 80 * 1024 / 8192
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(frame.command_id, 1);
            assert_eq!(frame.has_next, i + 1 != frames.len());
            let Some(Content::StorageWriteRequest(req)) = &frame.content else {
                panic!("expected StorageWriteRequest");
            };
            assert_eq!(
                req.file.as_ref().unwrap().data.len(),
                SERIAL_WRITE_CHUNK_SIZE
            );
        }
    }

    #[test]
    fn storage_write_final_ack_timeout_is_error_after_chunks() {
        let (transport, writes) = RecordingTransport::timeout(TransportKind::Ble);
        let mut client = FlipperClient::new(Box::new(transport));
        let data = vec![0x42; 1024];
        let progress = Arc::new(Mutex::new(Vec::new()));
        let progress_for_callback = Arc::clone(&progress);

        let err = storage_write(
            &mut client,
            "/ext/apps/ACAB/app.fap",
            &data,
            |sent, total| progress_for_callback.lock().unwrap().push((sent, total)),
            || false,
        )
        .unwrap_err();

        assert!(matches!(err, FlipperError::Io(ref io) if io.kind() == io::ErrorKind::TimedOut));

        let written = writes.lock().unwrap().clone();
        let frames = decode_written_messages(&written);
        assert_eq!(frames.len(), 2); // BLE uses 512-byte storage chunks.
        assert_eq!(progress.lock().unwrap().last(), Some(&(1024, 1024)));
    }

    #[test]
    fn storage_write_ignores_foreign_response_before_matching_ack() {
        let responses = vec![
            RecordingTransport::ok_response(99),
            RecordingTransport::ok_response(1),
        ];
        let (transport, writes) = RecordingTransport::with_responses(TransportKind::Ble, responses);
        let mut client = FlipperClient::new(Box::new(transport));
        let data = vec![0x42; 512];

        storage_write(
            &mut client,
            "/ext/apps/ACAB/app.fap",
            &data,
            |_, _| {},
            || false,
        )
        .unwrap();

        let written = writes.lock().unwrap().clone();
        let frames = decode_written_messages(&written);
        assert_eq!(frames.len(), 1);
    }
}
