use arboard::{Clipboard, ImageData};
use lan_mouse_proto::{CLIPBOARD_CHUNK_SIZE, ClipboardFormat, ClipboardMsg};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    collections::HashMap,
    io::Cursor,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
        mpsc::{Receiver as StdReceiver, Sender as StdSender, channel as std_channel},
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// Maximum clipboard payload size we accept or send (16 MB for PNG / text).
pub const MAX_CLIPBOARD_PAYLOAD: usize = 16 * 1024 * 1024;

/// Computes SHA-256 hash of format and data for echo-loop suppression.
pub fn content_hash(format: ClipboardFormat, data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([format as u8]);
    hasher.update(data);
    hasher.finalize().into()
}

/// Encodes raw RGBA8 pixels into PNG bytes using the `png` crate.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    if (width as usize) * (height as usize) * 4 != rgba.len() {
        return Err("rgba buffer size mismatch".to_string());
    }
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    drop(writer);
    Ok(out)
}

/// Decodes PNG bytes into `(width, height, rgba_bytes)` using the `png` crate.
pub fn decode_png(png_bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let cursor = Cursor::new(png_bytes);
    let decoder = png::Decoder::new(cursor);
    let mut reader = decoder
        .read_info()
        .map_err(|e: png::DecodingError| e.to_string())?;
    let buf_size = reader
        .output_buffer_size()
        .unwrap_or_else(|| reader.info().raw_bytes());
    let mut buf = vec![0; buf_size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e: png::DecodingError| e.to_string())?;
    buf.truncate(info.buffer_size());

    let (width, height) = (info.width, info.height);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => {
            let pixel_count = (width as usize) * (height as usize);
            let mut out = Vec::with_capacity(pixel_count * 4);
            for chunk in buf.chunks_exact(3) {
                out.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let pixel_count = (width as usize) * (height as usize);
            let mut out = Vec::with_capacity(pixel_count * 4);
            for &b in &buf {
                out.extend_from_slice(&[b, b, b, 255]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let pixel_count = (width as usize) * (height as usize);
            let mut out = Vec::with_capacity(pixel_count * 4);
            for chunk in buf.chunks_exact(2) {
                out.extend_from_slice(&[chunk[0], chunk[0], chunk[0], chunk[1]]);
            }
            out
        }
        other => return Err(format!("unsupported png color type: {other:?}")),
    };
    Ok((width, height, rgba))
}

/// Commands sent to the background clipboard OS thread.
enum WorkerCmd {
    WriteText(String, [u8; 32]),
    WriteImage {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        hash: [u8; 32],
    },
    Shutdown,
}

/// Inbound content detected on local clipboard to broadcast to peers.
#[derive(Clone, Debug)]
pub struct OutboundClipboard {
    pub format: ClipboardFormat,
    pub data: Vec<u8>,
}

/// Assembler collecting chunks of incoming transfers.
#[derive(Debug, Default)]
pub struct ClipboardAssembler {
    transfers: HashMap<u32, TransferState>,
}

#[derive(Debug)]
struct TransferState {
    format: ClipboardFormat,
    total: u32,
    chunks: HashMap<u32, Vec<u8>>,
    updated_at: Instant,
}

impl ClipboardAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds a [`ClipboardMsg`] chunk. Returns `Some((format, data))` when
    /// the transfer completes.
    pub fn push(&mut self, msg: ClipboardMsg) -> Option<(ClipboardFormat, Vec<u8>)> {
        let (id, index, total, format, data) = match msg {
            ClipboardMsg::Chunk {
                id,
                index,
                total,
                format,
                data,
            } => (id, index, total, format, data),
            ClipboardMsg::Ack { .. } => return None,
        };

        if total == 0 || index >= total {
            return None;
        }

        // Clean up stale transfers older than 15 seconds
        let now = Instant::now();
        self.transfers
            .retain(|_, s| now.duration_since(s.updated_at) < Duration::from_secs(15));

        let state = self.transfers.entry(id).or_insert_with(|| TransferState {
            format,
            total,
            chunks: HashMap::with_capacity(total.min(64) as usize),
            updated_at: now,
        });

        state.updated_at = now;
        state.chunks.insert(index, data);

        if state.chunks.len() == state.total as usize {
            let mut assembled = Vec::new();
            for i in 0..state.total {
                if let Some(chunk) = state.chunks.get(&i) {
                    assembled.extend_from_slice(chunk);
                } else {
                    return None;
                }
            }
            let format = state.format;
            self.transfers.remove(&id);
            Some((format, assembled))
        } else {
            None
        }
    }
}

/// Splits a payload into [`ClipboardMsg::Chunk`] datagrams.
pub fn chunk_payload(id: u32, format: ClipboardFormat, data: &[u8]) -> Vec<ClipboardMsg> {
    if data.is_empty() {
        return vec![ClipboardMsg::Chunk {
            id,
            index: 0,
            total: 1,
            format,
            data: Vec::new(),
        }];
    }
    let total = data.chunks(CLIPBOARD_CHUNK_SIZE).len() as u32;
    data.chunks(CLIPBOARD_CHUNK_SIZE)
        .enumerate()
        .map(|(index, chunk)| ClipboardMsg::Chunk {
            id,
            index: index as u32,
            total,
            format,
            data: chunk.to_vec(),
        })
        .collect()
}

/// Handle to the clipboard synchronization manager.
#[derive(Clone, Debug)]
pub struct ClipboardManager {
    cmd_tx: StdSender<WorkerCmd>,
    transfer_id: Arc<AtomicU32>,
    assembler: Arc<std::sync::Mutex<ClipboardAssembler>>,
}

impl ClipboardManager {
    /// Starts the clipboard synchronization worker.
    /// Returns `(ClipboardManager, outbound_rx)` where `outbound_rx` yields
    /// locally copied clipboard items that should be sent to peers.
    pub fn new() -> (Self, UnboundedReceiver<OutboundClipboard>) {
        let (cmd_tx, cmd_rx) = std_channel();
        let (out_tx, out_rx) = unbounded_channel();
        let transfer_id = Arc::new(AtomicU32::new(1));
        let assembler = Arc::new(std::sync::Mutex::new(ClipboardAssembler::new()));

        thread::Builder::new()
            .name("lan-mouse-clipboard".to_string())
            .spawn(move || {
                run_clipboard_worker(cmd_rx, out_tx);
            })
            .expect("spawn clipboard worker");

        (
            Self {
                cmd_tx,
                transfer_id,
                assembler,
            },
            out_rx,
        )
    }

    /// Next unique transfer ID for outbound transfers.
    pub fn next_transfer_id(&self) -> u32 {
        self.transfer_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Feeds an incoming [`ClipboardMsg`] chunk from the network. When all chunks
    /// arrive, writes the complete content to the local OS clipboard.
    pub fn handle_incoming(&self, msg: ClipboardMsg) {
        let assembled = {
            let mut guard = self.assembler.lock().unwrap();
            guard.push(msg)
        };
        if let Some((format, data)) = assembled {
            log::info!(
                "clipboard: assembled {format:?} ({} bytes) from remote peer",
                data.len()
            );
            self.write_remote(format, data);
        }
    }

    /// Writes received clipboard content into the OS clipboard.
    pub fn write_remote(&self, format: ClipboardFormat, data: Vec<u8>) {
        let hash = content_hash(format, &data);
        match format {
            ClipboardFormat::Text => {
                if let Ok(text) = String::from_utf8(data) {
                    let _ = self.cmd_tx.send(WorkerCmd::WriteText(text, hash));
                }
            }
            ClipboardFormat::Png => match decode_png(&data) {
                Ok((width, height, rgba)) => {
                    let _ = self.cmd_tx.send(WorkerCmd::WriteImage {
                        width,
                        height,
                        rgba,
                        hash,
                    });
                }
                Err(e) => {
                    log::warn!("failed to decode incoming png clipboard: {e}");
                }
            },
        }
    }
}

impl Drop for ClipboardManager {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(WorkerCmd::Shutdown);
    }
}

fn run_clipboard_worker(
    cmd_rx: StdReceiver<WorkerCmd>,
    out_tx: UnboundedSender<OutboundClipboard>,
) {
    let mut clipboard = match Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("failed to initialize system clipboard: {e}");
            return;
        }
    };

    let mut last_hash: Option<[u8; 32]> = None;

    // Initialize with current clipboard content so we don't spam peers on startup
    if let Ok(text) = clipboard.get_text() {
        if !text.is_empty() {
            last_hash = Some(content_hash(ClipboardFormat::Text, text.as_bytes()));
        }
    } else if let Ok(img) = clipboard.get_image() {
        last_hash = Some(content_hash(ClipboardFormat::Png, &img.bytes));
    }

    loop {
        // Drain any incoming write commands from remote peers
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                WorkerCmd::WriteText(text, hash) => {
                    last_hash = Some(hash);
                    if let Err(e) = clipboard.set_text(text) {
                        log::warn!("failed to set clipboard text: {e}");
                    } else {
                        log::info!("clipboard: text updated from remote peer");
                    }
                }
                WorkerCmd::WriteImage {
                    width,
                    height,
                    rgba,
                    hash,
                } => {
                    last_hash = Some(hash);
                    let img_data = ImageData {
                        width: width as usize,
                        height: height as usize,
                        bytes: Cow::Owned(rgba),
                    };
                    if let Err(e) = clipboard.set_image(img_data) {
                        log::warn!("failed to set clipboard image: {e}");
                    } else {
                        log::info!(
                            "clipboard: image ({}x{}) updated from remote peer",
                            width,
                            height
                        );
                    }
                }
                WorkerCmd::Shutdown => return,
            }
        }

        // Poll system clipboard for local changes
        if let Ok(text) = clipboard.get_text() {
            if !text.is_empty() {
                let bytes = text.into_bytes();
                let hash = content_hash(ClipboardFormat::Text, &bytes);
                if last_hash != Some(hash) {
                    last_hash = Some(hash);
                    log::info!("clipboard: local text copied ({} bytes)", bytes.len());
                    let _ = out_tx.send(OutboundClipboard {
                        format: ClipboardFormat::Text,
                        data: bytes,
                    });
                }
            }
        } else if let Ok(img) = clipboard.get_image() {
            let raw_hash = content_hash(ClipboardFormat::Png, &img.bytes);
            if last_hash != Some(raw_hash) {
                last_hash = Some(raw_hash);
                match encode_png(img.width as u32, img.height as u32, &img.bytes) {
                    Ok(png_bytes) => {
                        log::info!(
                            "clipboard: local image copied ({}x{}, {} bytes PNG)",
                            img.width,
                            img.height,
                            png_bytes.len()
                        );
                        let _ = out_tx.send(OutboundClipboard {
                            format: ClipboardFormat::Png,
                            data: png_bytes,
                        });
                    }
                    Err(e) => {
                        log::warn!("failed to encode clipboard image to PNG: {e}");
                    }
                }
            }
        }

        thread::sleep(Duration::from_millis(350));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_round_trip() {
        let width = 16u32;
        let height = 16u32;
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                rgba.extend_from_slice(&[x as u8 * 16, y as u8 * 16, 128, 255]);
            }
        }
        let png = encode_png(width, height, &rgba).expect("encode");
        assert!(!png.is_empty());
        let (w, h, decoded) = decode_png(&png).expect("decode");
        assert_eq!(w, width);
        assert_eq!(h, height);
        assert_eq!(decoded, rgba);
    }

    #[test]
    fn chunk_and_assembly_round_trip() {
        let mut assembler = ClipboardAssembler::new();
        let payload = vec![0x42u8; 3500]; // 4 chunks
        let chunks = chunk_payload(42, ClipboardFormat::Text, &payload);
        assert_eq!(chunks.len(), 4);

        // Feed chunks in reverse order to ensure assembly handles out-of-order
        let mut result = None;
        for chunk in chunks.into_iter().rev() {
            result = assembler.push(chunk);
        }
        let (format, assembled) = result.expect("assembled");
        assert_eq!(format, ClipboardFormat::Text);
        assert_eq!(assembled, payload);
    }

    #[test]
    fn empty_payload_chunks_to_one_message() {
        let mut assembler = ClipboardAssembler::new();
        let chunks = chunk_payload(99, ClipboardFormat::Text, &[]);
        assert_eq!(chunks.len(), 1);
        let (format, assembled) = assembler
            .push(chunks.into_iter().next().unwrap())
            .expect("assembled");
        assert_eq!(format, ClipboardFormat::Text);
        assert!(assembled.is_empty());
    }

    #[test]
    fn content_hash_differs_by_format_and_content() {
        let h1 = content_hash(ClipboardFormat::Text, b"hello");
        let h2 = content_hash(ClipboardFormat::Text, b"world");
        let h3 = content_hash(ClipboardFormat::Png, b"hello");
        assert_ne!(h1, h2);
        assert_ne!(h1, h3);
    }
}
