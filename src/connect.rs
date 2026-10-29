#[cfg(feature = "audio")]
use crate::audio::playback::AudioPlayer;
use crate::client::ClientManager;
use crate::clipboard::ClipboardManager;
#[cfg(feature = "audio")]
use crate::config::AudioConfig;
use crate::config::local_commit;
use lan_mouse_ipc::{ClientHandle, DEFAULT_PORT};
use lan_mouse_proto::{ClipboardMsg, Datagram, MAX_DATAGRAM_SIZE, MAX_EVENT_SIZE, ProtoEvent};
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use thiserror::Error;
use tokio::{
    net::UdpSocket,
    sync::Mutex,
    task::{JoinSet, spawn_local},
};
use webrtc_dtls::{
    config::{Config, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
};
use webrtc_util::Conn;

#[derive(Debug, Error)]
pub(crate) enum LanMouseConnectionError {
    #[error(transparent)]
    Bind(#[from] io::Error),
    #[error(transparent)]
    Dtls(#[from] webrtc_dtls::Error),
    #[error(transparent)]
    Webrtc(#[from] webrtc_util::Error),
    #[error("not connected")]
    NotConnected,
    #[error("emulation is disabled on the target device")]
    TargetEmulationDisabled,
    #[error("Connection timed out")]
    Timeout,
}

const DEFAULT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);
const RECONNECT_BASE_SECS: u64 = 2;
const RECONNECT_MAX_SECS: u64 = 15;

fn reconnect_backoff(attempt: u32) -> Duration {
    let shift = attempt.min(3);
    let secs = (RECONNECT_BASE_SECS << shift).min(RECONNECT_MAX_SECS);
    Duration::from_secs(secs)
}

async fn connect(
    addr: SocketAddr,
    cert: Certificate,
    quiet: bool,
) -> Result<(Arc<dyn Conn + Sync + Send>, SocketAddr), (SocketAddr, LanMouseConnectionError)> {
    if quiet {
        log::debug!("connecting to {addr} ...");
    } else {
        log::info!("connecting to {addr} ...");
    }
    let conn = Arc::new(
        UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| (addr, e.into()))?,
    );
    conn.connect(addr).await.map_err(|e| (addr, e.into()))?;
    let config = Config {
        certificates: vec![cert],
        server_name: "ignored".to_owned(),
        insecure_skip_verify: true,
        extended_master_secret: ExtendedMasterSecretType::Require,
        ..Default::default()
    };
    let timeout = tokio::time::sleep(DEFAULT_CONNECTION_TIMEOUT);
    tokio::select! {
        _ = timeout => Err((addr, LanMouseConnectionError::Timeout)),
        result = DTLSConn::new(conn, config, true, None) => match result {
            Ok(dtls_conn) => Ok((Arc::new(dtls_conn), addr)),
            Err(e) => Err((addr, e.into())),
        }
    }
}

async fn connect_any(
    addrs: &[SocketAddr],
    cert: Certificate,
    quiet: bool,
) -> Result<(Arc<dyn Conn + Send + Sync>, SocketAddr), LanMouseConnectionError> {
    let mut joinset = JoinSet::new();
    for &addr in addrs {
        joinset.spawn_local(connect(addr, cert.clone(), quiet));
    }
    loop {
        match joinset.join_next().await {
            None => return Err(LanMouseConnectionError::NotConnected),
            Some(r) => match r.expect("join error") {
                Ok(conn) => return Ok(conn),
                Err((a, e)) => {
                    if quiet {
                        log::debug!("failed to connect to {a}: `{e}`");
                    } else {
                        log::warn!("failed to connect to {a}: `{e}`");
                    }
                }
            },
        };
    }
}

pub(crate) struct LanMouseConnection {
    cert: Certificate,
    client_manager: ClientManager,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    recv_rx: Receiver<(ClientHandle, ProtoEvent)>,
    recv_tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
    clipboard: Rc<RefCell<Option<ClipboardManager>>>,
    #[cfg(feature = "audio")]
    audio_player: Rc<RefCell<Option<AudioPlayer>>>,
}

impl LanMouseConnection {
    pub(crate) fn new(
        cert: Certificate,
        client_manager: ClientManager,
        #[cfg(feature = "audio")] audio: AudioConfig,
    ) -> Self {
        let (recv_tx, recv_rx) = channel();
        #[cfg(feature = "audio")]
        let player = if audio.receive {
            Some(AudioPlayer::new(audio.buffer_ms))
        } else {
            None
        };
        Self {
            cert,
            client_manager,
            conns: Default::default(),
            connecting: Default::default(),
            recv_rx,
            recv_tx,
            ping_response: Default::default(),
            clipboard: Default::default(),
            #[cfg(feature = "audio")]
            audio_player: Rc::new(RefCell::new(player)),
        }
    }

    pub(crate) fn set_clipboard(&self, clipboard: Option<ClipboardManager>) {
        let was_none = self.clipboard.borrow().is_none();
        let is_some = clipboard.is_some();
        *self.clipboard.borrow_mut() = clipboard;
        if was_none && is_some {
            for handle in self.client_manager.active_clients() {
                self.ensure_connected(handle);
            }
        }
    }

    pub(crate) async fn broadcast_clipboard(&self, msg: &ClipboardMsg) {
        let buf = msg.encode();
        let conns = self.conns.lock().await;
        for conn in conns.values() {
            let _ = conn.send(&buf).await;
        }
    }

    pub(crate) async fn recv(&mut self) -> (ClientHandle, ProtoEvent) {
        self.recv_rx.recv().await.expect("channel closed")
    }

    #[cfg(feature = "audio")]
    pub(crate) fn set_audio_config(&self, audio: AudioConfig) {
        let mut need_connect = false;
        {
            let mut guard = self.audio_player.borrow_mut();
            match (audio.receive, guard.as_ref()) {
                (true, Some(player)) => {
                    player.set_buffer_ms(audio.buffer_ms);
                }
                (true, None) => {
                    *guard = Some(AudioPlayer::new(audio.buffer_ms));
                    need_connect = true;
                }
                (false, Some(_)) => {
                    *guard = None;
                    log::info!("audio playback disabled");
                }
                (false, None) => {}
            }
        }
        if need_connect {
            for handle in self.client_manager.active_clients() {
                self.ensure_connected(handle);
            }
        }
    }

    pub(crate) fn ensure_connected(&self, handle: ClientHandle) {
        #[cfg(feature = "audio")]
        let has_audio = self.audio_player.borrow().is_some();
        #[cfg(not(feature = "audio"))]
        let has_audio = false;

        if !has_audio && self.clipboard.borrow().is_none() {
            return;
        }
        spawn_eager_connect(
            self.client_manager.clone(),
            self.cert.clone(),
            handle,
            self.conns.clone(),
            self.connecting.clone(),
            self.recv_tx.clone(),
            self.ping_response.clone(),
            self.clipboard.clone(),
            #[cfg(feature = "audio")]
            self.audio_player.clone(),
            Duration::ZERO,
            0,
        );
    }

    pub(crate) async fn send(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
    ) -> Result<(), LanMouseConnectionError> {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        let buf = &buf[..len];
        if let Some(addr) = self.client_manager.active_addr(handle) {
            let conn = {
                let conns = self.conns.lock().await;
                conns.get(&addr).cloned()
            };
            if let Some(conn) = conn {
                if !self.client_manager.alive(handle) {
                    return Err(LanMouseConnectionError::TargetEmulationDisabled);
                }
                match conn.send(buf).await {
                    Ok(_) => {}
                    Err(e) => {
                        log::warn!("client {handle} failed to send: {e}");
                        disconnect(&self.client_manager, handle, addr, &self.conns).await;
                    }
                }
                log::trace!("{event} >->->->->- {addr}");
                return Ok(());
            }
        }

        // check if we are already trying to connect
        let mut connecting = self.connecting.lock().await;
        if !connecting.contains(&handle) {
            connecting.insert(handle);
            // connect in the background
            spawn_local(connect_to_handle(
                self.client_manager.clone(),
                self.cert.clone(),
                handle,
                self.conns.clone(),
                self.connecting.clone(),
                self.recv_tx.clone(),
                self.ping_response.clone(),
                self.clipboard.clone(),
                #[cfg(feature = "audio")]
                self.audio_player.clone(),
                0,
            ));
        }
        Err(LanMouseConnectionError::NotConnected)
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_eager_connect(
    client_manager: ClientManager,
    cert: Certificate,
    handle: ClientHandle,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
    clipboard: Rc<RefCell<Option<ClipboardManager>>>,
    #[cfg(feature = "audio")] audio_player: Rc<RefCell<Option<AudioPlayer>>>,
    delay: Duration,
    attempt: u32,
) {
    spawn_local(async move {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        #[cfg(feature = "audio")]
        let has_audio = audio_player.borrow().is_some();
        #[cfg(not(feature = "audio"))]
        let has_audio = false;

        if !has_audio && clipboard.borrow().is_none() {
            return;
        }
        if !client_manager
            .get_state(handle)
            .is_some_and(|(_, s)| s.active)
        {
            return;
        }
        if let Some(addr) = client_manager.active_addr(handle) {
            if conns.lock().await.contains_key(&addr) {
                return;
            }
        }
        {
            let mut guard = connecting.lock().await;
            if guard.contains(&handle) {
                return;
            }
            guard.insert(handle);
        }
        let _ = connect_to_handle(
            client_manager,
            cert,
            handle,
            conns,
            connecting,
            tx,
            ping_response,
            clipboard,
            #[cfg(feature = "audio")]
            audio_player,
            attempt,
        )
        .await;
    });
}

#[allow(clippy::too_many_arguments)]
async fn connect_to_handle(
    client_manager: ClientManager,
    cert: Certificate,
    handle: ClientHandle,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
    clipboard: Rc<RefCell<Option<ClipboardManager>>>,
    #[cfg(feature = "audio")] audio_player: Rc<RefCell<Option<AudioPlayer>>>,
    attempt: u32,
) -> Result<(), LanMouseConnectionError> {
    let quiet = attempt > 0;
    if quiet {
        log::debug!("client {handle} reconnecting (attempt {}) ...", attempt + 1);
    } else {
        log::info!("client {handle} connecting ...");
    }
    // sending did not work, figure out active conn.
    if let Some(addrs) = client_manager.get_ips(handle) {
        if addrs.is_empty() {
            connecting.lock().await.remove(&handle);
            return Err(LanMouseConnectionError::NotConnected);
        }
        let port = client_manager.get_port(handle).unwrap_or(DEFAULT_PORT);
        let addrs = addrs
            .into_iter()
            .map(|a| SocketAddr::new(a, port))
            .collect::<Vec<_>>();
        if quiet {
            log::debug!("client ({handle}) connecting ... (ips: {addrs:?})");
        } else {
            log::info!("client ({handle}) connecting ... (ips: {addrs:?})");
        }
        let res = connect_any(&addrs, cert.clone(), quiet).await;
        let (conn, addr) = match res {
            Ok(c) => c,
            Err(e) => {
                connecting.lock().await.remove(&handle);
                #[cfg(feature = "audio")]
                let eager_needed = audio_player.borrow().is_some() || clipboard.borrow().is_some();
                #[cfg(not(feature = "audio"))]
                let eager_needed = clipboard.borrow().is_some();

                if eager_needed {
                    let next_attempt = attempt.saturating_add(1);
                    spawn_eager_connect(
                        client_manager,
                        cert,
                        handle,
                        conns,
                        connecting,
                        tx,
                        ping_response,
                        clipboard,
                        #[cfg(feature = "audio")]
                        audio_player,
                        reconnect_backoff(attempt),
                        next_attempt,
                    );
                }
                return Err(e);
            }
        };
        log::info!("client ({handle}) connected @ {addr}");
        client_manager.set_active_addr(handle, Some(addr));
        conns.lock().await.insert(addr, conn.clone());
        connecting.lock().await.remove(&handle);

        // Best-effort version handshake. Send our commit hash once
        // immediately after the DTLS handshake; the listen side
        // mirrors a Hello back so the receive loop can populate
        // `peer_commit`. Old peers will silently skip this event
        // per the forward-compat handler in [`receive_loop`].
        let (buf, len) = ProtoEvent::Hello {
            commit: local_commit(),
        }
        .into();
        if let Err(e) = conn.send(&buf[..len]).await {
            log::debug!("hello send to {addr} failed: {e}");
        }

        // poll connection for active
        spawn_local(ping_pong(addr, conn.clone(), ping_response.clone()));

        // receiver
        spawn_local(receive_loop(
            client_manager,
            cert,
            handle,
            addr,
            conn,
            conns,
            connecting,
            tx,
            ping_response.clone(),
            clipboard,
            #[cfg(feature = "audio")]
            audio_player,
        ));
        return Ok(());
    }
    connecting.lock().await.remove(&handle);
    Err(LanMouseConnectionError::NotConnected)
}

async fn ping_pong(
    addr: SocketAddr,
    conn: Arc<dyn Conn + Send + Sync>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) {
    loop {
        let (buf, len) = ProtoEvent::Ping.into();

        // send 4 pings, at least one must be answered
        for _ in 0..4 {
            if let Err(e) = conn.send(&buf[..len]).await {
                log::warn!("{addr}: send error `{e}`, closing connection");
                let _ = conn.close().await;
                break;
            }
            log::trace!("PING >->->->->- {addr}");

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        if !ping_response.borrow_mut().remove(&addr) {
            log::warn!("{addr} did not respond, closing connection");
            let _ = conn.close().await;
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn receive_loop(
    client_manager: ClientManager,
    cert: Certificate,
    handle: ClientHandle,
    addr: SocketAddr,
    conn: Arc<dyn Conn + Send + Sync>,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
    clipboard: Rc<RefCell<Option<ClipboardManager>>>,
    #[cfg(feature = "audio")] audio_player: Rc<RefCell<Option<AudioPlayer>>>,
) {
    let mut buf = [0u8; MAX_DATAGRAM_SIZE];
    while let Ok(n) = conn.recv(&mut buf).await {
        match Datagram::decode(&buf[..n]) {
            Ok(Datagram::Event(event)) => {
                log::trace!("{addr} <==<==<== {event}");
                match event {
                    ProtoEvent::Pong(b) => {
                        client_manager.set_active_addr(handle, Some(addr));
                        client_manager.set_alive(handle, b);
                        ping_response.borrow_mut().insert(addr);
                    }
                    ProtoEvent::Hello { commit } => {
                        client_manager.set_peer_commit(handle, Some(commit));
                    }
                    event => tx.send((handle, event)).expect("channel closed"),
                }
            }
            Ok(Datagram::Audio(frame)) => {
                log::trace!(
                    "{addr} <==<==<== Audio(seq={}, {} Hz, {} ch, {} samples)",
                    frame.seq,
                    frame.sample_rate,
                    frame.channels,
                    frame.samples.len()
                );
                #[cfg(feature = "audio")]
                if let Some(ref player) = *audio_player.borrow() {
                    player.push_packet(
                        frame.seq,
                        frame.sample_rate,
                        frame.channels,
                        &frame.samples,
                    );
                }
            }
            Ok(Datagram::Clipboard(msg)) => {
                log::trace!("received clipboard datagram on connect side from {addr}");
                if let Some(ref cb) = *clipboard.borrow() {
                    cb.handle_incoming(msg);
                }
            }
            // Skip undecodable datagrams without dropping the
            // connection. Each DTLS recv is one framed message, so
            // skipping is safe and keeps us forward-compatible with
            // peers that send event types we don't yet know about.
            Err(e) => log::debug!("ignoring undecodable event from {addr}: {e}"),
        }
    }
    log::warn!("recv error");
    disconnect(&client_manager, handle, addr, &conns).await;
    #[cfg(feature = "audio")]
    let eager_needed = audio_player.borrow().is_some() || clipboard.borrow().is_some();
    #[cfg(not(feature = "audio"))]
    let eager_needed = clipboard.borrow().is_some();

    if eager_needed {
        spawn_eager_connect(
            client_manager,
            cert,
            handle,
            conns,
            connecting,
            tx,
            ping_response,
            clipboard,
            #[cfg(feature = "audio")]
            audio_player,
            reconnect_backoff(0),
            1,
        );
    }
}

async fn disconnect(
    client_manager: &ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    conns: &Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>,
) {
    log::warn!("client ({handle}) @ {addr} connection closed");
    conns.lock().await.remove(&addr);
    client_manager.set_active_addr(handle, None);
    client_manager.set_peer_commit(handle, None);
    let active: Vec<SocketAddr> = conns.lock().await.keys().copied().collect();
    log::info!("active connections: {active:?}");
}

#[cfg(all(test, feature = "audio"))]
mod tests {
    use super::*;

    #[test]
    fn reconnect_backoff_starts_at_2s_and_caps_at_15s() {
        assert_eq!(reconnect_backoff(0), Duration::from_secs(2));
        assert_eq!(reconnect_backoff(1), Duration::from_secs(4));
        assert_eq!(reconnect_backoff(2), Duration::from_secs(8));
        assert_eq!(reconnect_backoff(3), Duration::from_secs(15));
        assert_eq!(reconnect_backoff(100), Duration::from_secs(15));
    }
}
