use std::{
    net::SocketAddr,
    sync::mpsc as std_mpsc,
    thread::{self, JoinHandle as ThreadJoinHandle},
    time::{Duration, Instant},
};

use lan_mouse_proto::AudioFrame;
use tokio::{
    sync::mpsc,
    task::{JoinHandle, spawn_local},
};

use crate::{
    audio::capture::{CaptureMessage, LoopbackCapture},
    listen::{ArcConn, ListenerConns},
};

const REOPEN_RETRY_DELAY: Duration = Duration::from_secs(2);

enum CaptureWorkerCmd {
    SetCapturing(bool),
    StreamBroken,
    Shutdown,
}

struct CaptureWorkerHandle {
    cmd_tx: std_mpsc::SyncSender<CaptureWorkerCmd>,
    thread: Option<ThreadJoinHandle<()>>,
}

impl CaptureWorkerHandle {
    fn spawn(chunk_tx: mpsc::Sender<CaptureMessage>) -> Self {
        let (cmd_tx, cmd_rx) = std_mpsc::sync_channel(16);
        let thread = thread::Builder::new()
            .name("lan-mouse-audio-in".to_owned())
            .spawn(move || run_capture_worker(cmd_rx, chunk_tx))
            .expect("failed to spawn audio capture worker");
        Self {
            cmd_tx,
            thread: Some(thread),
        }
    }

    fn send(&self, cmd: CaptureWorkerCmd) {
        let _ = self.cmd_tx.try_send(cmd);
    }

    fn shutdown(&mut self) {
        let _ = self.cmd_tx.send(CaptureWorkerCmd::Shutdown);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for CaptureWorkerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_capture_worker(
    cmd_rx: std_mpsc::Receiver<CaptureWorkerCmd>,
    chunk_tx: mpsc::Sender<CaptureMessage>,
) {
    let mut want_capturing = false;
    let mut current_capture: Option<LoopbackCapture> = None;
    let mut is_playing = false;
    let mut retry_after: Option<Instant> = None;
    let mut warned_open_failure = false;

    loop {
        let timeout = if want_capturing && current_capture.is_none() {
            match retry_after {
                Some(deadline) => deadline.saturating_duration_since(Instant::now()),
                None => Duration::ZERO,
            }
        } else {
            Duration::from_secs(3600)
        };

        if !timeout.is_zero() {
            match cmd_rx.recv_timeout(timeout) {
                Ok(CaptureWorkerCmd::Shutdown) | Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                    drop(current_capture);
                    return;
                }
                Ok(CaptureWorkerCmd::SetCapturing(active)) => {
                    want_capturing = active;
                    if active && retry_after.is_none() {
                        // Immediate start/resume.
                    }
                }
                Ok(CaptureWorkerCmd::StreamBroken) => {
                    current_capture = None;
                    is_playing = false;
                    retry_after = Some(Instant::now() + REOPEN_RETRY_DELAY);
                    continue;
                }
                Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            }
        } else {
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    CaptureWorkerCmd::Shutdown => {
                        drop(current_capture);
                        return;
                    }
                    CaptureWorkerCmd::SetCapturing(active) => {
                        want_capturing = active;
                    }
                    CaptureWorkerCmd::StreamBroken => {
                        current_capture = None;
                        is_playing = false;
                        retry_after = Some(Instant::now() + REOPEN_RETRY_DELAY);
                    }
                }
            }
        }

        if !want_capturing {
            if let Some(ref capture) = current_capture {
                if is_playing {
                    if let Err(e) = capture.pause() {
                        log::warn!("failed to pause audio loopback stream (dropping): {e}");
                        current_capture = None;
                    }
                    is_playing = false;
                }
            }
            continue;
        }

        if let Some(deadline) = retry_after {
            if Instant::now() < deadline {
                continue;
            }
            retry_after = None;
        }

        if let Some(ref capture) = current_capture {
            if !is_playing {
                match capture.play() {
                    Ok(()) => {
                        is_playing = true;
                        continue;
                    }
                    Err(e) => {
                        log::warn!("failed to resume audio loopback stream (reopening): {e}");
                        current_capture = None;
                    }
                }
            } else {
                continue;
            }
        }

        match LoopbackCapture::open(chunk_tx.clone()) {
            Ok(capture) => match capture.play() {
                Ok(()) => {
                    let info = capture.info();
                    log::info!(
                        "audio loopback capture opened on '{}' ({} Hz, {} ch {:?} -> {} Hz, {} ch s16le)",
                        info.name,
                        info.native_sample_rate,
                        info.native_channels,
                        info.native_sample_format,
                        info.out_sample_rate,
                        info.out_channels,
                    );
                    warned_open_failure = false;
                    is_playing = true;
                    current_capture = Some(capture);
                }
                Err(e) => {
                    log::warn!("failed to start audio loopback capture (will retry in 2s): {e}");
                    warned_open_failure = true;
                    retry_after = Some(Instant::now() + REOPEN_RETRY_DELAY);
                }
            },
            Err(e) => {
                if !warned_open_failure {
                    log::warn!("failed to open audio loopback capture (will retry in 2s): {e}");
                    warned_open_failure = true;
                } else {
                    log::debug!("audio loopback capture reopen attempt failed: {e}");
                }
                retry_after = Some(Instant::now() + REOPEN_RETRY_DELAY);
            }
        }
    }
}

enum SenderCmd {
    PeerConnected(SocketAddr),
    PeerDisconnected(SocketAddr),
    Terminate,
}

pub(crate) struct AudioSender {
    cmd_tx: mpsc::UnboundedSender<SenderCmd>,
    task: JoinHandle<()>,
}

impl AudioSender {
    pub(crate) fn new(conns: ListenerConns, enabled: bool) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let sender_task = SenderTask::new(conns, cmd_rx, enabled);
        let task = spawn_local(sender_task.run());
        Self { cmd_tx, task }
    }

    pub(crate) fn peer_connected(&self, addr: SocketAddr) {
        let _ = self.cmd_tx.send(SenderCmd::PeerConnected(addr));
    }

    pub(crate) fn peer_disconnected(&self, addr: SocketAddr) {
        let _ = self.cmd_tx.send(SenderCmd::PeerDisconnected(addr));
    }

    pub(crate) async fn terminate(&mut self) {
        let _ = self.cmd_tx.send(SenderCmd::Terminate);
        let _ = (&mut self.task).await;
    }
}

struct SenderTask {
    conns: ListenerConns,
    cmd_rx: mpsc::UnboundedReceiver<SenderCmd>,
    chunk_tx: mpsc::Sender<CaptureMessage>,
    chunk_rx: mpsc::Receiver<CaptureMessage>,
    worker: Option<CaptureWorkerHandle>,
    enabled: bool,
    warned_unsupported: bool,
    connected_peers: Vec<SocketAddr>,
    current_dest: Option<SocketAddr>,
    cached_conn: Option<(SocketAddr, ArcConn)>,
    next_seq: u32,
    wire_buf: Vec<u8>,
}

impl SenderTask {
    fn new(
        conns: ListenerConns,
        cmd_rx: mpsc::UnboundedReceiver<SenderCmd>,
        enabled: bool,
    ) -> Self {
        let (chunk_tx, chunk_rx) = mpsc::channel(64);
        Self {
            conns,
            cmd_rx,
            chunk_tx,
            chunk_rx,
            worker: None,
            enabled,
            warned_unsupported: false,
            connected_peers: Vec::new(),
            current_dest: None,
            cached_conn: None,
            next_seq: 0,
            wire_buf: Vec::with_capacity(1024),
        }
    }

    fn desired_dest(&self) -> Option<SocketAddr> {
        if !self.enabled {
            return None;
        }
        if !cfg!(windows) {
            return None;
        }
        self.connected_peers.last().copied()
    }

    fn sync_state(&mut self) {
        if self.enabled && !cfg!(windows) && !self.warned_unsupported {
            log::warn!("audio send is enabled, but loopback capture is only supported on Windows");
            self.warned_unsupported = true;
        }

        let desired = self.desired_dest();
        if desired == self.current_dest {
            return;
        }

        match (self.current_dest, desired) {
            (None, Some(addr)) => {
                log::info!("audio streaming started -> {addr}");
                if self.worker.is_none() {
                    self.worker = Some(CaptureWorkerHandle::spawn(self.chunk_tx.clone()));
                }
                if let Some(ref w) = self.worker {
                    w.send(CaptureWorkerCmd::SetCapturing(true));
                }
            }
            (Some(old), Some(new)) => {
                log::info!("audio streaming switched {old} -> {new}");
                self.cached_conn = None;
            }
            (Some(old), None) => {
                log::info!("audio streaming stopped (was {old})");
                if let Some(ref w) = self.worker {
                    w.send(CaptureWorkerCmd::SetCapturing(false));
                }
                while self.chunk_rx.try_recv().is_ok() {}
                self.cached_conn = None;
            }
            (None, None) => {}
        }

        self.current_dest = desired;
    }

    async fn resolve_conn(&mut self, dest: SocketAddr) -> Option<ArcConn> {
        if let Some((addr, ref conn)) = self.cached_conn {
            if addr == dest {
                return Some(conn.clone());
            }
        }
        let conn = self.conns.conn(dest).await?;
        self.cached_conn = Some((dest, conn.clone()));
        Some(conn)
    }

    async fn run(mut self) {
        self.sync_state();
        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => match cmd {
                    Some(SenderCmd::PeerConnected(addr)) => {
                        self.connected_peers.retain(|&a| a != addr);
                        self.connected_peers.push(addr);
                        self.cached_conn = None;
                        self.sync_state();
                    }
                    Some(SenderCmd::PeerDisconnected(addr)) => {
                        self.connected_peers.retain(|&a| a != addr);
                        if self.cached_conn.as_ref().is_some_and(|(a, _)| *a == addr) {
                            self.cached_conn = None;
                        }
                        self.sync_state();
                    }
                    Some(SenderCmd::Terminate) | None => break,
                },
                msg = self.chunk_rx.recv() => match msg {
                    Some(CaptureMessage::Chunk(chunk)) => {
                        let Some(dest) = self.current_dest else {
                            continue;
                        };
                        let Some(conn) = self.resolve_conn(dest).await else {
                            continue;
                        };
                        let seq = self.next_seq;
                        self.next_seq = self.next_seq.wrapping_add(1);
                        let frame = AudioFrame {
                            seq,
                            sample_rate: chunk.sample_rate,
                            channels: chunk.channels,
                            samples: chunk.samples,
                        };
                        frame.encode_into(&mut self.wire_buf);
                        log::trace!(
                            "Audio(seq={seq}, {} Hz, {} ch, {} B) >=>=>=>=>=> {dest}",
                            frame.sample_rate,
                            frame.channels,
                            self.wire_buf.len()
                        );
                        if let Err(e) = conn.send(&self.wire_buf).await {
                            log::debug!("audio send to {dest} failed: {e}");
                            self.cached_conn = None;
                        }
                    }
                    Some(CaptureMessage::StreamError(err)) => {
                        log::warn!("audio loopback stream error (reopening in 2s): {err}");
                        if let Some(ref w) = self.worker {
                            w.send(CaptureWorkerCmd::StreamBroken);
                        }
                    }
                    None => break,
                }
            }
        }
        if let Some(mut worker) = self.worker.take() {
            tokio::task::spawn_blocking(move || worker.shutdown())
                .await
                .ok();
        }
    }
}
