use aes::Aes256;
use base64::Engine;
use cipher::{KeyIvInit, StreamCipher};
use hmac::{Hmac, Mac};
use md5::Md5;
use sha1::Digest;
use sha2::Sha256;
use std::{
    cmp,
    collections::{HashMap, HashSet, VecDeque},
    hash::{DefaultHasher, Hash, Hasher},
    io::ErrorKind,
    net::{TcpStream, ToSocketAddrs},
    os::unix::io::AsRawFd,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
    sync::{mpsc, oneshot, watch, Notify, Semaphore},
    time,
};
use xml::reader::{EventReader, XmlEvent};

type Aes256Ofb = ofb::Ofb<Aes256>;
type HmacSha256 = Hmac<Sha256>;

// Public DHP2P v5 wire-protocol specification constants (shared across all Dahua/CP Plus/Amcrest clients)
const MAIN_SERVER: &str = "www.easy4ipcloud.com:8800";
const DHP2P_WSSE_ID: &str = "cba1b29e32cb17aa46b8ff9e73c7f40b";
const DHP2P_WSSE_SEED: &str = "996103384cdf19179e19243e959bbf8b";

const DHP2P_SESSION_IV: &[u8; 16] = b"2z52*lk9o6HRyJrf";
const DHP2P_INFO_MASK: &[u8; 32] = b"kRjmsUB&ezmdGLL67H#$ojw@XflcaIaf";
const DHP2P_INFO_IV: &[u8; 16] = b"MydvJw*Iw1w&i^kk";
const RECENT_PACKET_WINDOW: usize = 1024;
const DEFAULT_LOCAL_PORT: u16 = 15540;
const REALM_CHANNEL_CAPACITY: usize = 512;

pub fn is_lan_reachable(host: &str, port: u16, timeout_ms: u64) -> bool {
    let formatted_host = crate::config::format_host_for_url(host);
    if formatted_host.is_empty() {
        return false;
    }
    let addr_str = format!("{}:{}", formatted_host, port);
    let Ok(mut addrs) = addr_str.to_socket_addrs() else {
        return false;
    };
    let Some(addr) = addrs.next() else {
        return false;
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(timeout_ms)).is_ok()
}

fn is_tcp_established(stream: &tokio::net::TcpStream) -> bool {
    let fd = stream.as_raw_fd();
    let mut info: libc::tcp_info = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::tcp_info>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_TCP,
            libc::TCP_INFO,
            &mut info as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    // Linux TCP_ESTABLISHED == 1; sockets whose peer already closed are TCP_CLOSE_WAIT (8)
    rc == 0 && info.tcpi_state == 1
}

async fn wait_for_shutdown(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

fn set_connected_if_current(
    connected: &AtomicBool,
    generation: &AtomicU64,
    my_gen: u64,
    val: bool,
) {
    if generation.load(Ordering::Acquire) == my_gen {
        connected.store(val, Ordering::Release);
    }
}

fn set_status_if_current(
    status: &Mutex<String>,
    generation: &AtomicU64,
    my_gen: u64,
    msg: impl Into<String>,
) {
    if generation.load(Ordering::Acquire) == my_gen {
        if let Ok(mut s) = status.lock() {
            *s = msg.into();
        }
    }
}

fn is_p2p_auth_rejection(err: &str) -> bool {
    err.starts_with("AUTH_401:")
        || err.contains("auth error 401")
        || err.contains("auth error 403")
        || err.contains("HTTP 401")
}

pub struct P2PTunnelManager {
    active_key: String,
    local_port: u16,
    generation: Arc<AtomicU64>,
    shutdown_tx: Option<watch::Sender<bool>>,
    worker_handle: Option<std::thread::JoinHandle<()>>,
    connected: Arc<AtomicBool>,
    halted: Arc<AtomicBool>,
    status: Arc<Mutex<String>>,
}

impl Default for P2PTunnelManager {
    fn default() -> Self {
        Self::new()
    }
}

impl P2PTunnelManager {
    pub fn new() -> Self {
        Self {
            active_key: String::new(),
            local_port: DEFAULT_LOCAL_PORT,
            generation: Arc::new(AtomicU64::new(0)),
            shutdown_tx: None,
            worker_handle: None,
            connected: Arc::new(AtomicBool::new(false)),
            halted: Arc::new(AtomicBool::new(false)),
            status: Arc::new(Mutex::new("Idle".to_string())),
        }
    }

    pub fn is_connected_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.connected)
    }

    pub fn is_halted(&self) -> bool {
        self.halted.load(Ordering::Acquire)
            || self
                .worker_handle
                .as_ref()
                .is_none_or(|h| h.is_finished())
    }

    pub fn clear_active_key(&mut self) {
        self.active_key.clear();
    }

    pub fn status_text(&self) -> String {
        self.status
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| "Cloud P2P".to_string())
    }

    pub fn stop(&mut self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }
        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.join();
        }
        self.connected.store(false, Ordering::Release);
        self.halted.store(false, Ordering::Release);
        self.active_key.clear();
        if let Ok(mut s) = self.status.lock() {
            *s = "Stopped".to_string();
        }
    }

    pub fn ensure_started(
        &mut self,
        serial: &str,
        username: &str,
        password: &str,
        remote_port: u16,
    ) -> u16 {
        let serial_clean = serial.trim().to_string();
        if serial_clean.is_empty() {
            self.stop();
            return 0;
        }

        let key = format!("{}|{}|{}|{}", serial_clean, username, password, remote_port);
        if self.active_key == key
            && self.shutdown_tx.is_some()
            && !self.is_halted()
            && self.local_port > 0
        {
            return self.local_port;
        }

        self.stop();

        // Bind a persistent local port (prefer 15540, fallback to OS-assigned)
        let std_listener = match std::net::TcpListener::bind(("127.0.0.1", DEFAULT_LOCAL_PORT))
            .or_else(|_| std::net::TcpListener::bind(("127.0.0.1", 0)))
        {
            Ok(l) => l,
            Err(e) => {
                self.local_port = 0;
                self.halted.store(true, Ordering::Release);
                if let Ok(mut s) = self.status.lock() {
                    *s = format!("Port bind failed ({})", e);
                }
                return 0;
            }
        };

        if let Err(e) = std_listener.set_nonblocking(true) {
            self.local_port = 0;
            self.halted.store(true, Ordering::Release);
            if let Ok(mut s) = self.status.lock() {
                *s = format!("Socket setup failed ({})", e);
            }
            return 0;
        }

        self.local_port = match std_listener.local_addr() {
            Ok(addr) => addr.port(),
            Err(e) => {
                self.local_port = 0;
                self.halted.store(true, Ordering::Release);
                if let Ok(mut s) = self.status.lock() {
                    *s = format!("Local addr error ({})", e);
                }
                return 0;
            }
        };

        let my_gen = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

        self.active_key = key;
        self.shutdown_tx = Some(shutdown_tx);
        self.connected.store(false, Ordering::Release);
        self.halted.store(false, Ordering::Release);

        let generation = Arc::clone(&self.generation);
        let connected_flag = Arc::clone(&self.connected);
        let halted_flag = Arc::clone(&self.halted);
        let status = Arc::clone(&self.status);
        set_status_if_current(&status, &generation, my_gen, "Cloud P2P Connecting...");

        let user_owned = username.to_string();
        let pass_owned = password.to_string();

        let spawn_res = std::thread::Builder::new()
            .name("dahua-p2p-tunnel".to_string())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        halted_flag.store(true, Ordering::Release);
                        set_status_if_current(
                            &status,
                            &generation,
                            my_gen,
                            format!("P2P runtime error ({})", e),
                        );
                        return;
                    }
                };

                rt.block_on(async move {
                    let listener = match TcpListener::from_std(std_listener) {
                        Ok(l) => Arc::new(l),
                        Err(e) => {
                            halted_flag.store(true, Ordering::Release);
                            set_status_if_current(
                                &status,
                                &generation,
                                my_gen,
                                format!("P2P listener error ({})", e),
                            );
                            return;
                        }
                    };

                    let mut consecutive_errors: u32 = 0;

                    while !*shutdown_rx.borrow() {
                        set_connected_if_current(&connected_flag, &generation, my_gen, false);
                        set_status_if_current(
                            &status,
                            &generation,
                            my_gen,
                            "Cloud P2P Handshake...",
                        );

                        let session_established = Arc::new(AtomicBool::new(false));
                        let ctx = TunnelSessionCtx {
                            listener: Arc::clone(&listener),
                            serial: serial_clean.clone(),
                            username: user_owned.clone(),
                            password: pass_owned.clone(),
                            remote_port,
                            my_gen,
                            generation: Arc::clone(&generation),
                            connected_flag: Arc::clone(&connected_flag),
                            status: Arc::clone(&status),
                            session_established: Arc::clone(&session_established),
                        };

                        match run_tunnel_session(&ctx, shutdown_rx.clone()).await {
                            Ok(()) => break,
                            Err(e) => {
                                set_connected_if_current(
                                    &connected_flag,
                                    &generation,
                                    my_gen,
                                    false,
                                );
                                if *shutdown_rx.borrow() {
                                    break;
                                }
                                if is_p2p_auth_rejection(&e) {
                                    halted_flag.store(true, Ordering::Release);
                                    set_status_if_current(
                                        &status,
                                        &generation,
                                        my_gen,
                                        "Auth failed (401) - check password in Settings",
                                    );
                                    break;
                                }
                                if session_established.load(Ordering::Acquire) {
                                    consecutive_errors = 1;
                                } else {
                                    consecutive_errors = consecutive_errors.saturating_add(1);
                                }
                                let delay_secs = match consecutive_errors {
                                    1 => 2,
                                    2 => 4,
                                    3 => 8,
                                    4 => 15,
                                    _ => 30,
                                };
                                set_status_if_current(
                                    &status,
                                    &generation,
                                    my_gen,
                                    format!("Cloud P2P Retry ({})", e),
                                );
                                tokio::select! {
                                    biased;
                                    _ = wait_for_shutdown(&mut shutdown_rx) => break,
                                    _ = time::sleep(Duration::from_secs(delay_secs)) => {}
                                }
                            }
                        }
                    }

                    set_connected_if_current(&connected_flag, &generation, my_gen, false);
                    drop(listener);
                });

                rt.shutdown_background();
            });

        match spawn_res {
            Ok(handle) => {
                self.worker_handle = Some(handle);
                self.local_port
            }
            Err(e) => {
                self.shutdown_tx = None;
                self.active_key.clear();
                self.local_port = 0;
                self.halted.store(true, Ordering::Release);
                if let Ok(mut s) = self.status.lock() {
                    *s = format!("Thread spawn error ({})", e);
                }
                0
            }
        }
    }
}

impl Drop for P2PTunnelManager {
    fn drop(&mut self) {
        self.stop();
    }
}

struct TunnelSessionCtx {
    listener: Arc<TcpListener>,
    serial: String,
    username: String,
    password: String,
    remote_port: u16,
    my_gen: u64,
    generation: Arc<AtomicU64>,
    connected_flag: Arc<AtomicBool>,
    status: Arc<Mutex<String>>,
    session_established: Arc<AtomicBool>,
}

async fn run_tunnel_session(
    ctx: &TunnelSessionCtx,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), String> {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| e.to_string())?;

    let handshake_fut = time::timeout(
        Duration::from_secs(12),
        p2p_handshake(
            socket,
            &ctx.serial,
            &ctx.username,
            &ctx.password,
            shutdown_rx.clone(),
        ),
    );
    let (socket, session) = tokio::select! {
        biased;
        _ = wait_for_shutdown(&mut shutdown_rx) => return Ok(()),
        res = handshake_fut => res.map_err(|_| "Handshake timeout".to_string())??,
    };

    // Drain any stale CLOSE-WAIT sockets queued before the handshake finished
    while let Ok(Ok((stale_client, _))) =
        time::timeout(Duration::from_millis(2), ctx.listener.accept()).await
    {
        if is_tcp_established(&stale_client) {
            drop(stale_client);
        }
    }

    if *shutdown_rx.borrow() {
        return Ok(());
    }

    ctx.session_established.store(true, Ordering::Release);
    set_connected_if_current(&ctx.connected_flag, &ctx.generation, ctx.my_gen, true);
    set_status_if_current(
        &ctx.status,
        &ctx.generation,
        ctx.my_gen,
        "Cloud P2P Connected",
    );

    let (dh_tx, dh_rx) = mpsc::channel::<PTCPEvent>(512);
    let session = Arc::new(Mutex::new(session));

    let channels = Arc::new(Mutex::new(HashMap::<u32, mpsc::Sender<Vec<u8>>>::new()));
    let conn_channels = Arc::new(Mutex::new(HashMap::<u32, oneshot::Sender<bool>>::new()));

    let reader_sock = Arc::new(socket);
    let writer_sock = Arc::clone(&reader_sock);

    let session_reader = Arc::clone(&session);
    let channels_reader = Arc::clone(&channels);
    let conn_channels_reader = Arc::clone(&conn_channels);

    let session_alive = Arc::new(AtomicBool::new(true));
    let alive_hb = Arc::clone(&session_alive);
    let alive_reader = Arc::clone(&session_alive);
    let session_dead_notify = Arc::new(Notify::new());
    let dead_notify_reader = Arc::clone(&session_dead_notify);

    let hb_tx = dh_tx.clone();
    let mut hb_shutdown = shutdown_rx.clone();
    let hb_handle = tokio::spawn(async move {
        while alive_hb.load(Ordering::Acquire) {
            tokio::select! {
                biased;
                _ = wait_for_shutdown(&mut hb_shutdown) => break,
                _ = time::sleep(Duration::from_secs(5)) => {}
            }
            if hb_tx.send(PTCPEvent::Heartbeat).await.is_err() {
                break;
            }
        }
    });

    let mut writer_shutdown = shutdown_rx.clone();
    let remote_port_u32 = ctx.remote_port as u32;
    let writer_handle = tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut writer_shutdown) => {}
            _ = dh_writer(session, writer_sock, dh_rx, remote_port_u32) => {}
        }
    });

    let mut reader_shutdown = shutdown_rx.clone();
    let dh_tx_reader = dh_tx.clone();
    let reader_handle = tokio::spawn(async move {
        let res = tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut reader_shutdown) => Ok(()),
            r = dh_reader(
                session_reader,
                reader_sock,
                channels_reader,
                conn_channels_reader,
                dh_tx_reader,
            ) => r,
        };
        alive_reader.store(false, Ordering::Release);
        dead_notify_reader.notify_waiters();
        res
    });

    // Serialize PTCP Bind handshakes 1 at a time so multi-camera startup
    // never interleaves Bind sequence numbers over the UDP relay
    let bind_semaphore = Arc::new(Semaphore::new(1));
    let consecutive_bind_failures = Arc::new(AtomicU32::new(0));
    let mut health_tick = time::interval(Duration::from_millis(250));
    health_tick.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

    loop {
        let accept_res = tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut shutdown_rx) => {
                set_connected_if_current(&ctx.connected_flag, &ctx.generation, ctx.my_gen, false);
                hb_handle.abort();
                writer_handle.abort();
                reader_handle.abort();
                if let Ok(mut map) = channels.lock() {
                    map.clear();
                }
                if let Ok(mut map) = conn_channels.lock() {
                    map.clear();
                }
                return Ok(());
            }
            _ = session_dead_notify.notified() => {
                set_connected_if_current(&ctx.connected_flag, &ctx.generation, ctx.my_gen, false);
                hb_handle.abort();
                writer_handle.abort();
                reader_handle.abort();
                if let Ok(mut map) = channels.lock() {
                    map.clear();
                }
                if let Ok(mut map) = conn_channels.lock() {
                    map.clear();
                }
                return Err("Relay session closed".to_string());
            }
            _ = health_tick.tick() => {
                if !session_alive.load(Ordering::Acquire)
                    || consecutive_bind_failures.load(Ordering::Acquire) >= 12
                {
                    set_connected_if_current(&ctx.connected_flag, &ctx.generation, ctx.my_gen, false);
                    hb_handle.abort();
                    writer_handle.abort();
                    reader_handle.abort();
                    if let Ok(mut map) = channels.lock() {
                        map.clear();
                    }
                    if let Ok(mut map) = conn_channels.lock() {
                        map.clear();
                    }
                    return Err("Relay session closed".to_string());
                }
                continue;
            }
            res = ctx.listener.accept() => res,
        };

        let Ok((client, _addr)) = accept_res else {
            continue;
        };

        // Immediately drop dead sockets from timed-out ffmpeg processes in 0ms
        if !is_tcp_established(&client) {
            continue;
        }

        let dh_tx_client = dh_tx.clone();
        let channels_client = Arc::clone(&channels);
        let conn_channels_client = Arc::clone(&conn_channels);
        let sem_clone = Arc::clone(&bind_semaphore);
        let bind_failures = Arc::clone(&consecutive_bind_failures);
        let alive_flag = Arc::clone(&session_alive);
        let mut client_shutdown = shutdown_rx.clone();

        tokio::spawn(async move {
            let permit = tokio::select! {
                biased;
                _ = wait_for_shutdown(&mut client_shutdown) => return,
                res = sem_clone.acquire_owned() => match res {
                    Ok(p) => p,
                    Err(_) => return,
                },
            };
            if !alive_flag.load(Ordering::Acquire)
                || *client_shutdown.borrow()
                || !is_tcp_established(&client)
            {
                return;
            }

            let (tx, rx) = mpsc::channel::<Vec<u8>>(REALM_CHANNEL_CAPACITY);
            let (conn_tx, mut conn_rx) = oneshot::channel::<bool>();

            let realm_id = rand::random::<u32>();
            if let Ok(mut map) = channels_client.lock() {
                map.insert(realm_id, tx);
            }
            if let Ok(mut map) = conn_channels_client.lock() {
                map.insert(realm_id, conn_tx);
            }

            let mut bound = false;
            for _attempt in 0..3 {
                if *client_shutdown.borrow() || !is_tcp_established(&client) {
                    break;
                }
                if dh_tx_client
                    .send(PTCPEvent::Connect(realm_id))
                    .await
                    .is_err()
                {
                    break;
                }
                let wait_res = tokio::select! {
                    biased;
                    _ = wait_for_shutdown(&mut client_shutdown) => break,
                    res = time::timeout(Duration::from_millis(2000), &mut conn_rx) => res,
                };
                match wait_res {
                    Ok(Ok(true)) => {
                        bound = true;
                        break;
                    }
                    Ok(_) => break,
                    Err(_) => continue,
                }
            }

            if !bound {
                if let Ok(mut map) = channels_client.lock() {
                    map.remove(&realm_id);
                }
                if let Ok(mut map) = conn_channels_client.lock() {
                    map.remove(&realm_id);
                }
                let _ = dh_tx_client.try_send(PTCPEvent::Disconnect(realm_id));
                if !*client_shutdown.borrow() && is_tcp_established(&client) {
                    bind_failures.fetch_add(1, Ordering::AcqRel);
                }
                return;
            }

            bind_failures.store(0, Ordering::Release);
            drop(permit);

            let (reader, writer) = client.into_split();
            let reader_shutdown = client_shutdown.clone();
            let writer_shutdown = client_shutdown;
            let dh_tx_writer = dh_tx_client.clone();
            let channels_writer = Arc::clone(&channels_client);

            tokio::spawn(async move {
                process_reader(reader, realm_id, dh_tx_client, reader_shutdown).await;
            });
            tokio::spawn(async move {
                process_writer(
                    writer,
                    rx,
                    realm_id,
                    channels_writer,
                    dh_tx_writer,
                    writer_shutdown,
                )
                .await;
            });
        });
    }
}

fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

fn get_device_randsalt(info_b64: &str) -> String {
    if info_b64.is_empty() {
        return String::new();
    }
    let Ok(mut raw) = base64::engine::general_purpose::STANDARD.decode(info_b64) else {
        return String::new();
    };
    let mut cipher = Aes256Ofb::new(DHP2P_INFO_MASK.into(), DHP2P_INFO_IV.into());
    cipher.apply_keystream(&mut raw);
    let Ok(json_str) = String::from_utf8(raw) else {
        return String::new();
    };
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&json_str) {
        if let Some(s) = val.get("randsalt").and_then(|v| v.as_str()) {
            return s.to_string();
        }
    }
    // Fallback string search if tabs/formatting vary
    if let Some(pos) = json_str.find("\"randsalt\"") {
        let rest = &json_str[pos + "\"randsalt\"".len()..];
        if let Some(start) = rest.find('"') {
            let after_quote = &rest[start + 1..];
            if let Some(end) = after_quote.find('"') {
                return after_quote[..end].to_string();
            }
        }
    }
    String::new()
}

fn get_key(username: &str, password: &str, randsalt: &str) -> Vec<u8> {
    let input = format!("{}:Login to {}:{}", username, randsalt, password);
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    result
        .iter()
        .map(|b| format!("{:02X}", b))
        .collect::<String>()
        .into_bytes()
}

fn get_enc(key: &[u8], nonce: u32, data: &str) -> String {
    let salt = nonce.to_string();
    let mut dk = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(key, salt.as_bytes(), 20000, &mut dk);
    let mut buf = data.as_bytes().to_vec();
    let mut cipher = Aes256Ofb::new((&dk).into(), DHP2P_SESSION_IV.into());
    cipher.apply_keystream(&mut buf);
    base64::engine::general_purpose::STANDARD.encode(&buf)
}

fn get_auth(username: &str, key: &[u8], nonce: &str, randsalt: &str, payload: &str) -> String {
    let curdate = chrono::Utc::now().timestamp();
    let message = format!("{}{}{}", nonce, curdate, payload);
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC key");
    mac.update(message.as_bytes());
    let auth = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    let salt = if !randsalt.is_empty() {
        format!("<RandSalt>{}</RandSalt>", xml_escape(randsalt))
    } else {
        String::new()
    };
    format!(
        "<CreateDate>{}</CreateDate><DevAuth>{}</DevAuth><Nonce>{}</Nonce>{}<UserName>{}</UserName>",
        curdate,
        auth,
        xml_escape(nonce),
        salt,
        xml_escape(username)
    )
}

async fn p2p_handshake(
    socket: UdpSocket,
    serial: &str,
    username: &str,
    password: &str,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(UdpSocket, PTCPSession), String> {
    tokio::select! {
        biased;
        _ = wait_for_shutdown(&mut shutdown_rx) => Err("Tunnel stopped".to_string()),
        res = p2p_handshake_inner(socket, serial, username, password) => res,
    }
}

async fn p2p_handshake_inner(
    socket: UdpSocket,
    serial: &str,
    username: &str,
    password: &str,
) -> Result<(UdpSocket, PTCPSession), String> {
    let mut cseq = 0;

    socket
        .connect(MAIN_SERVER)
        .await
        .map_err(|e| e.to_string())?;

    dh_request(&socket, "/probe/p2psrv", None, &mut cseq).await?;
    let _ = dh_read(&socket).await?;

    dh_request(
        &socket,
        &format!("/online/p2psrv/{}", serial),
        None,
        &mut cseq,
    )
    .await?;
    let p2psrv = dh_read(&socket)
        .await?
        .body
        .and_then(|b| b.get("body/US").cloned())
        .ok_or_else(|| "Missing US p2psrv".to_string())?;

    // Pin signaling requests to the NVR's authoritative cluster node on port 8800
    // so Geo-DNS round-robin differences on remote networks never misroute relay-channel
    let cluster_host = p2psrv.split(':').next().unwrap_or("www.easy4ipcloud.com");
    let cluster_main = format!("{}:8800", cluster_host);
    socket
        .connect(&cluster_main)
        .await
        .map_err(|e| e.to_string())?;

    let socket_info = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| e.to_string())?;
    socket_info
        .connect(&p2psrv)
        .await
        .map_err(|e| e.to_string())?;

    dh_request(
        &socket_info,
        &format!("/probe/device/{}", serial),
        None,
        &mut cseq,
    )
    .await?;
    let _ = dh_read(&socket_info).await?;

    dh_request(
        &socket_info,
        &format!("/info/device/{}", serial),
        None,
        &mut cseq,
    )
    .await?;
    let info_res = dh_read_raw(&socket_info).await?;
    let randsalt = if info_res.code < 300 {
        info_res
            .body
            .and_then(|b| b.get("body/Info").cloned())
            .map(|i| get_device_randsalt(&i))
            .unwrap_or_default()
    } else {
        String::new()
    };

    dh_request(&socket, "/online/relay", None, &mut cseq).await?;
    let relay = dh_read(&socket)
        .await?
        .body
        .and_then(|b| b.get("body/Address").cloned())
        .ok_or_else(|| "Missing relay address".to_string())?;

    let socket2 = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| e.to_string())?;

    let cid: [u8; 8] = rand::random();
    let key = get_key(username, password, &randsalt);
    let nonce = rand::random::<u32>() & 0x7FFF_FFFF;
    let nonce_str = nonce.to_string();
    let local_port = socket.local_addr().map(|a| a.port()).unwrap_or(0);
    let raw_laddr = format!("127.0.0.1:{}", local_port);
    let laddr_enc = get_enc(&key, nonce, &raw_laddr);
    let auth_xml = get_auth(username, &key, &nonce_str, &randsalt, &laddr_enc);

    let channel_body = format!(
        "<body>{}<Identify>{}</Identify><IpEncrptV2>true</IpEncrptV2><LocalAddr>{}</LocalAddr><version>5.0.0</version></body>",
        auth_xml,
        cid.iter().map(|b| format!("{:x}", b)).collect::<Vec<_>>().join(" "),
        laddr_enc,
    );

    dh_request(
        &socket,
        &format!("/device/{}/p2p-channel", serial),
        Some(&channel_body),
        &mut cseq,
    )
    .await?;

    socket2.connect(&relay).await.map_err(|e| e.to_string())?;

    dh_request(&socket2, "/relay/agent", None, &mut cseq).await?;
    let relay_data = dh_read(&socket2)
        .await?
        .body
        .ok_or_else(|| "Empty relay agent body".to_string())?;
    let token = relay_data
        .get("body/Token")
        .cloned()
        .ok_or_else(|| "Missing relay Token".to_string())?;
    let agent = relay_data
        .get("body/Agent")
        .cloned()
        .ok_or_else(|| "Missing relay Agent".to_string())?;

    socket2.connect(&agent).await.map_err(|e| e.to_string())?;

    dh_request(
        &socket2,
        &format!("/relay/start/{}", token),
        Some("<body><Client>:0</Client></body>"),
        &mut cseq,
    )
    .await?;
    let _ = dh_read(&socket2).await?;

    let mut res = dh_read_raw(&socket).await?;
    for _ in 0..4 {
        if res.code >= 200 {
            break;
        }
        res = dh_read_raw(&socket).await?;
    }

    if res.code == 401 || res.code == 403 {
        return Err(format!("AUTH_401: P2P auth error {} ({})", res.code, res.status));
    }
    if res.code >= 400 {
        return Err(format!("P2P auth error {} ({})", res.code, res.status));
    }

    let data = res.body.unwrap_or_default();
    let resp_nonce = data
        .get("body/Nonce")
        .cloned()
        .unwrap_or_else(|| nonce_str.clone());

    socket2
        .connect(&cluster_main)
        .await
        .map_err(|e| e.to_string())?;

    let relay_auth = get_auth(username, &key, &resp_nonce, &randsalt, "");
    let relay_body = format!("<body>{}<agentAddr>{}</agentAddr></body>", relay_auth, agent);
    dh_request(
        &socket2,
        &format!("/device/{}/relay-channel", serial),
        Some(&relay_body),
        &mut cseq,
    )
    .await?;

    socket2.connect(&agent).await.map_err(|e| e.to_string())?;
    let _ = time::timeout(Duration::from_secs(4), dh_read_raw(&socket2)).await;

    let mut session = PTCPSession::new();
    ptcp_request(&socket2, session.send(PTCPBody::Sync)).await?;
    let sync_pkt = time::timeout(Duration::from_secs(5), ptcp_read(&socket2))
        .await
        .map_err(|_| "Relay PTCP Sync timeout".to_string())??;
    session.recv_sync(sync_pkt);

    Ok((socket2, session))
}

#[derive(Debug)]
struct DHResponse {
    code: u16,
    status: String,
    body: Option<HashMap<String, String>>,
}

impl DHResponse {
    fn parse_body(body: &str) -> HashMap<String, String> {
        let mut parser = EventReader::from_str(body);
        let mut stack = Vec::new();
        let mut tree = HashMap::new();

        loop {
            match parser.next() {
                Ok(XmlEvent::StartElement { name, .. }) => {
                    stack.push(name.local_name);
                }
                Ok(XmlEvent::EndElement { .. }) => {
                    let _ = stack.pop();
                }
                Ok(XmlEvent::Characters(s)) => {
                    let key = stack.as_slice().join("/");
                    tree.insert(key, s);
                }
                Ok(XmlEvent::EndDocument) | Err(_) => {
                    break;
                }
                _ => {}
            }
        }
        tree
    }

    fn parse_response(res: &str) -> Result<DHResponse, String> {
        let mut parts = res.split("\r\n\r\n");
        let head = parts.next().ok_or_else(|| "Empty DH response".to_string())?;
        let body = parts.next().unwrap_or("");

        let mut head_parts = head.split("\r\n");
        let status_line_str = head_parts
            .next()
            .ok_or_else(|| "Missing status line".to_string())?;
        let mut status_line = status_line_str.split(' ');
        let _version = status_line.next().unwrap_or("");
        let code = status_line
            .next()
            .and_then(|c| c.parse::<u16>().ok())
            .ok_or_else(|| "Invalid status code".to_string())?;
        let status = status_line.collect::<Vec<_>>().join(" ");

        let body_map = if body.trim().is_empty() {
            None
        } else {
            Some(DHResponse::parse_body(body))
        };

        Ok(DHResponse {
            code,
            status,
            body: body_map,
        })
    }
}

async fn dh_request(
    socket: &UdpSocket,
    path: &str,
    body: Option<&str>,
    seq: &mut u32,
) -> Result<(), String> {
    let method = match body {
        Some(_) => "DHPOST",
        None => "DHGET",
    };
    let body = body.unwrap_or("");

    let nonce = rand::random::<u32>() & 0x7FFF_FFFF;
    let currdate = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let pwd = format!("{}{}DHP2P:{}:{}", nonce, currdate, DHP2P_WSSE_ID, DHP2P_WSSE_SEED);

    let mut hasher = sha1::Sha1::new();
    hasher.update(pwd);
    let hash_digest = hasher.finalize();
    let digest = base64::engine::general_purpose::STANDARD.encode(hash_digest);

    *seq += 1;

    let content_headers = if !body.is_empty() {
        format!("Content-Type: \r\nContent-Length: {}\r\n", body.len())
    } else {
        String::new()
    };

    let req = format!(
        "{} {} HTTP/1.1\r\nCSeq: {}\r\nAuthorization: WSSE profile=\"UsernameToken\"\r\nX-WSSE: UsernameToken Username=\"{}\", PasswordDigest=\"{}\", Nonce=\"{}\", Created=\"{}\"\r\n{}\r\n{}",
        method, path, seq, DHP2P_WSSE_ID, digest, nonce, currdate, content_headers, body
    );

    socket
        .send(req.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

async fn dh_read_raw(socket: &UdpSocket) -> Result<DHResponse, String> {
    let mut buf = [0u8; 4096];
    let n = time::timeout(Duration::from_secs(5), socket.recv(&mut buf))
        .await
        .map_err(|_| "UDP signaling read timeout".to_string())?
        .map_err(|e| e.to_string())?;
    let res = String::from_utf8_lossy(&buf[0..n]);
    DHResponse::parse_response(&res)
}

async fn dh_read(socket: &UdpSocket) -> Result<DHResponse, String> {
    let res = dh_read_raw(socket).await?;
    if res.code == 401 || res.code == 403 {
        return Err(format!("AUTH_401: Signaling HTTP {} ({})", res.code, res.status));
    }
    if res.code >= 300 {
        return Err(format!("Signaling HTTP {} ({})", res.code, res.status));
    }
    Ok(res)
}

pub enum PTCPEvent {
    Heartbeat,
    Connect(u32),
    Disconnect(u32),
    Data(u32, Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PTCPPayload {
    pub realm: u32,
    pub data: Vec<u8>,
}

impl PTCPPayload {
    fn parse(data: &[u8]) -> Result<Self, String> {
        if data.len() < 12 || data[0] != 0x10 {
            return Err("Invalid PTCPPayload header".to_string());
        }
        let header = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let length = (header & 0xFFFF) as usize;
        if data.len() < 12 + length {
            return Err(format!(
                "Truncated PTCPPayload datagram: expected {} bytes, got {}",
                12 + length,
                data.len()
            ));
        }
        let realm = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let payload_data = data[12..12 + length].to_vec();
        Ok(Self {
            realm,
            data: payload_data,
        })
    }

    fn serialize(&self) -> Vec<u8> {
        let length = self.data.len() as u32;
        let header = (0x10000000u32 | (length & 0xFFFF)).to_be_bytes();
        let realm = self.realm.to_be_bytes();
        let padding = 0u32.to_be_bytes();

        [
            header.to_vec(),
            realm.to_vec(),
            padding.to_vec(),
            self.data.clone(),
        ]
        .concat()
    }
}

pub enum PTCPBody {
    Empty,
    Sync,
    Heartbeat,
    Command(Vec<u8>),
    Bind(u32, u32),
    Status(u32, String),
    Payload(PTCPPayload),
}

impl PTCPBody {
    fn parse(data: &[u8]) -> Result<Self, String> {
        if data.len() < 4 {
            return Ok(Self::Empty);
        }
        match data[0] {
            0x00 => Ok(Self::Sync),
            0x10 => Ok(Self::Payload(PTCPPayload::parse(data)?)),
            0x11 if data.len() >= 16 => Ok(Self::Bind(
                u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
                u32::from_be_bytes([data[12], data[13], data[14], data[15]]),
            )),
            0x12 if data.len() >= 12 => {
                let realm = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
                let status = String::from_utf8_lossy(&data[12..]).to_string();
                Ok(Self::Status(realm, status))
            }
            0x13 => Ok(Self::Heartbeat),
            _ => Ok(Self::Command(data.to_vec())),
        }
    }

    fn serialize(&self) -> Vec<u8> {
        match self {
            Self::Empty => Vec::new(),
            Self::Sync => b"\x00\x03\x01\x00".to_vec(),
            Self::Command(data) => data.clone(),
            Self::Payload(payload) => payload.serialize(),
            Self::Bind(realm, port) => [
                b"\x11\x00\x00\x00".to_vec(),
                realm.to_be_bytes().to_vec(),
                b"\x00\x00\x00\x00".to_vec(),
                port.to_be_bytes().to_vec(),
                b"\x7f\x00\x00\x01".to_vec(),
            ]
            .concat(),
            Self::Status(realm, status) => [
                b"\x12\x00\x00\x00".to_vec(),
                realm.to_be_bytes().to_vec(),
                b"\x00\x00\x00\x00".to_vec(),
                status.as_bytes().to_vec(),
            ]
            .concat(),
            Self::Heartbeat => b"\x13\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00".to_vec(),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Sync => 4,
            Self::Command(data) => data.len(),
            Self::Payload(payload) => payload.data.len() + 12,
            Self::Bind(_, _) => 20,
            Self::Status(_, status) => status.len() + 12,
            Self::Heartbeat => 12,
            Self::Empty => 0,
        }
    }
}

pub struct PTCPPacket {
    sent: u32,
    recv: u32,
    pid: u32,
    lmid: u32,
    rmid: u32,
    body: PTCPBody,
    duplicate: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct PacketIdentity {
    sent: u32,
    recv: u32,
    pid: u32,
    lmid: u32,
    body_hash: u64,
}

fn sequence_is_after(candidate: u32, baseline: u32) -> bool {
    candidate != baseline && candidate.wrapping_sub(baseline) < 0x8000_0000
}

impl PTCPPacket {
    fn parse(raw: &[u8]) -> Result<Self, String> {
        if raw.len() < 24 || &raw[0..4] != b"PTCP" {
            return Err("Invalid PTCP magic".to_string());
        }
        let sent = u32::from_be_bytes(raw[4..8].try_into().unwrap());
        let recv = u32::from_be_bytes(raw[8..12].try_into().unwrap());
        let pid = u32::from_be_bytes(raw[12..16].try_into().unwrap());
        let lmid = u32::from_be_bytes(raw[16..20].try_into().unwrap());
        let rmid = u32::from_be_bytes(raw[20..24].try_into().unwrap());
        let body = PTCPBody::parse(&raw[24..])?;

        Ok(Self {
            sent,
            recv,
            pid,
            lmid,
            rmid,
            body,
            duplicate: false,
        })
    }

    fn serialize(&self) -> Vec<u8> {
        [
            b"PTCP".to_vec(),
            self.sent.to_be_bytes().to_vec(),
            self.recv.to_be_bytes().to_vec(),
            self.pid.to_be_bytes().to_vec(),
            self.lmid.to_be_bytes().to_vec(),
            self.rmid.to_be_bytes().to_vec(),
            self.body.serialize(),
        ]
        .concat()
    }

    fn identity(&self) -> PacketIdentity {
        let mut hasher = DefaultHasher::new();
        self.body.serialize().hash(&mut hasher);
        PacketIdentity {
            sent: self.sent,
            recv: self.recv,
            pid: self.pid,
            lmid: self.lmid,
            body_hash: hasher.finish(),
        }
    }
}

struct PTCPSession {
    sent: u32,
    recv: u32,
    count: u32,
    id: u32,
    rmid: u32,
    recent_packets: HashSet<PacketIdentity>,
    recent_packet_order: VecDeque<PacketIdentity>,
    reorder_buf: HashMap<u32, PTCPPacket>,
    gap_since: Option<time::Instant>,
    sent_history: VecDeque<(u32, u32, Vec<u8>)>,
}

impl PTCPSession {
    fn new() -> Self {
        Self {
            sent: 0,
            recv: 0,
            count: 0,
            id: 0,
            rmid: 0,
            recent_packets: HashSet::new(),
            recent_packet_order: VecDeque::new(),
            reorder_buf: HashMap::new(),
            gap_since: None,
            sent_history: VecDeque::new(),
        }
    }

    fn send(&mut self, body: PTCPBody) -> PTCPPacket {
        let sent = self.sent;
        let recv = self.recv;
        let pid = match body {
            PTCPBody::Sync => 0x0002FFFF,
            _ => 0x0000FFFFu32.wrapping_sub(self.count),
        };
        let lmid = self.id;
        let rmid = self.rmid;
        let body_len = body.len() as u32;

        self.sent = self.sent.wrapping_add(body_len);
        self.id = self.id.wrapping_add(1);
        self.count = self.count.wrapping_add(match body {
            PTCPBody::Sync | PTCPBody::Empty => 0,
            _ => 1,
        });

        let pkt = PTCPPacket {
            sent,
            recv,
            pid,
            lmid,
            rmid,
            body,
            duplicate: false,
        };

        if body_len > 0 {
            let end_seq = sent.wrapping_add(body_len);
            self.sent_history.push_back((sent, end_seq, pkt.serialize()));
            while self.sent_history.len() > 128 {
                self.sent_history.pop_front();
            }
        }

        pkt
    }

    fn prune_acked(&mut self, remote_recv: u32) {
        while let Some(&(_, end_seq, _)) = self.sent_history.front() {
            if !sequence_is_after(end_seq, remote_recv) {
                self.sent_history.pop_front();
            } else {
                break;
            }
        }
    }

    fn retransmit_from(&self, missing_sent: u32) -> Vec<Vec<u8>> {
        self.sent_history
            .iter()
            .filter(|&&(pkt_sent, _, _)| {
                pkt_sent == missing_sent || sequence_is_after(pkt_sent, missing_sent)
            })
            .take(16)
            .map(|(_, _, raw)| raw.clone())
            .collect()
    }

    fn recv_sync(&mut self, mut packet: PTCPPacket) -> PTCPPacket {
        let identity = packet.identity();
        if self.recent_packets.contains(&identity) {
            packet.duplicate = true;
            self.rmid = packet.lmid;
            return packet;
        }
        self.recent_packets.insert(identity);
        self.recent_packet_order.push_back(identity);
        if self.recent_packet_order.len() > RECENT_PACKET_WINDOW {
            if let Some(expired) = self.recent_packet_order.pop_front() {
                self.recent_packets.remove(&expired);
            }
        }
        let remote_end = cmp::max(packet.sent.wrapping_add(packet.body.len() as u32), 4);
        if sequence_is_after(remote_end, self.recv) {
            self.recv = remote_end;
        }
        self.rmid = packet.lmid;
        self.prune_acked(packet.recv);
        packet
    }

    fn recv_ordered(
        &mut self,
        packet: PTCPPacket,
    ) -> (Vec<PTCPPacket>, Option<PTCPPacket>, Vec<Vec<u8>>) {
        self.prune_acked(packet.recv);

        if matches!(&packet.body, PTCPBody::Command(_)) || (packet.pid & 0xFF00_0000) != 0 {
            self.rmid = packet.lmid;
            let retrans = self.retransmit_from(packet.recv);
            return (Vec::new(), None, retrans);
        }

        if matches!(&packet.body, PTCPBody::Empty) {
            self.rmid = packet.lmid;
            return (Vec::new(), None, Vec::new());
        }

        let body_len = packet.body.len() as u32;
        let remote_end = packet.sent.wrapping_add(body_len);

        // Already processed in-order packet (retransmission of old sequence)
        if !sequence_is_after(remote_end, self.recv) {
            self.rmid = packet.lmid;
            return (Vec::new(), Some(self.send(PTCPBody::Empty)), Vec::new());
        }

        // Exact next contiguous packet in the PTCP byte stream
        if packet.sent == self.recv {
            self.recv = remote_end;
            self.rmid = packet.lmid;
            let mut ready = vec![packet];

            while let Some(next_pkt) = self.reorder_buf.remove(&self.recv) {
                self.recv = self.recv.wrapping_add(next_pkt.body.len() as u32);
                self.rmid = next_pkt.lmid;
                ready.push(next_pkt);
            }

            let cur_recv = self.recv;
            self.reorder_buf.retain(|&s, _| sequence_is_after(s, cur_recv));

            if self.reorder_buf.is_empty() {
                self.gap_since = None;
            } else {
                self.gap_since = Some(time::Instant::now());
            }

            let ack = self.send(PTCPBody::Empty);
            return (ready, Some(ack), Vec::new());
        }

        // Ignore partially overlapping retransmissions whose start precedes self.recv
        if !sequence_is_after(packet.sent, self.recv) {
            self.rmid = packet.lmid;
            return (Vec::new(), Some(self.send(PTCPBody::Empty)), Vec::new());
        }

        // Out-of-order future packet (e.g. small trailing keyframe fragment arriving ahead of full-MTU chunks)
        self.reorder_buf.entry(packet.sent).or_insert(packet);
        if self.gap_since.is_none() {
            self.gap_since = Some(time::Instant::now());
        }

        let gap_elapsed = self
            .gap_since
            .map(|t| t.elapsed())
            .unwrap_or(Duration::ZERO);

        if self.reorder_buf.len() > 32 || gap_elapsed > Duration::from_millis(20) {
            let cur_recv = self.recv;
            self.reorder_buf.retain(|&s, _| sequence_is_after(s, cur_recv));

            let next_sent = self
                .reorder_buf
                .keys()
                .copied()
                .min_by_key(|&s| s.wrapping_sub(self.recv));
            if let Some(jump_sent) = next_sent {
                self.recv = jump_sent;
                let mut ready = Vec::new();
                while let Some(next_pkt) = self.reorder_buf.remove(&self.recv) {
                    self.recv = self.recv.wrapping_add(next_pkt.body.len() as u32);
                    self.rmid = next_pkt.lmid;
                    ready.push(next_pkt);
                }
                let updated_recv = self.recv;
                self.reorder_buf
                    .retain(|&s, _| sequence_is_after(s, updated_recv));
                self.gap_since = if self.reorder_buf.is_empty() {
                    None
                } else {
                    Some(time::Instant::now())
                };
                let ack = self.send(PTCPBody::Empty);
                return (ready, Some(ack), Vec::new());
            }
        }

        (Vec::new(), None, Vec::new())
    }
}

async fn ptcp_request(socket: &UdpSocket, packet: PTCPPacket) -> Result<(), String> {
    let raw = packet.serialize();
    socket.send(&raw).await.map_err(|e| e.to_string())?;
    Ok(())
}

async fn ptcp_read(socket: &UdpSocket) -> Result<PTCPPacket, String> {
    let mut buf = [0u8; 16384];
    loop {
        match socket.recv(&mut buf).await {
            Ok(n) => {
                if let Ok(pkt) = PTCPPacket::parse(&buf[0..n]) {
                    return Ok(pkt);
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::ConnectionRefused | ErrorKind::Interrupted
                ) =>
            {
                time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

async fn process_writer(
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    mut rx: mpsc::Receiver<Vec<u8>>,
    realm_id: u32,
    channels: Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>>,
    dh_tx: mpsc::Sender<PTCPEvent>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        let maybe_data = tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut shutdown_rx) => return,
            msg = rx.recv() => msg,
        };
        let Some(data) = maybe_data else {
            break;
        };
        let write_res = tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut shutdown_rx) => return,
            res = time::timeout(Duration::from_secs(3), writer.write_all(&data)) => res,
        };
        match write_res {
            Ok(Ok(())) => {}
            Ok(Err(_)) | Err(_) => {
                if let Ok(mut map) = channels.lock() {
                    map.remove(&realm_id);
                }
                let _ = dh_tx.try_send(PTCPEvent::Disconnect(realm_id));
                let _ = writer.shutdown().await;
                break;
            }
        }
    }
}

async fn process_reader(
    mut reader: tokio::net::tcp::OwnedReadHalf,
    realm_id: u32,
    dh_tx: mpsc::Sender<PTCPEvent>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut buf = [0u8; 4096];
    loop {
        let read_res = tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut shutdown_rx) => break,
            res = reader.read(&mut buf) => res,
        };
        let n = match read_res {
            Ok(0) | Err(_) => {
                let _ = dh_tx.try_send(PTCPEvent::Disconnect(realm_id));
                break;
            }
            Ok(n) => n,
        };
        tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut shutdown_rx) => break,
            res = dh_tx.send(PTCPEvent::Data(realm_id, buf[0..n].to_vec())) => {
                if res.is_err() {
                    break;
                }
            }
        }
    }
}

async fn dh_writer(
    session: Arc<Mutex<PTCPSession>>,
    socket: Arc<UdpSocket>,
    mut dh_rx: mpsc::Receiver<PTCPEvent>,
    remote_port: u32,
) {
    while let Some(ev) = dh_rx.recv().await {
        let is_data_or_bind = matches!(ev, PTCPEvent::Connect(_) | PTCPEvent::Data(_, _));
        let pkt = {
            let Ok(mut sess) = session.lock() else {
                continue;
            };
            match ev {
                PTCPEvent::Heartbeat => sess.send(PTCPBody::Heartbeat),
                PTCPEvent::Connect(realm) => sess.send(PTCPBody::Bind(realm, remote_port)),
                PTCPEvent::Disconnect(realm) => {
                    sess.send(PTCPBody::Status(realm, "DISC".to_string()))
                }
                PTCPEvent::Data(realm, data) => {
                    sess.send(PTCPBody::Payload(PTCPPayload { realm, data }))
                }
            }
        };
        let _ = ptcp_request(&socket, pkt).await;
        if is_data_or_bind {
            time::sleep(Duration::from_millis(6)).await;
        }
    }
}

async fn dh_reader(
    session: Arc<Mutex<PTCPSession>>,
    socket: Arc<UdpSocket>,
    channels: Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>>,
    conn_channels: Arc<Mutex<HashMap<u32, oneshot::Sender<bool>>>>,
    dh_tx: mpsc::Sender<PTCPEvent>,
) -> Result<(), String> {
    loop {
        let raw_pkt = time::timeout(Duration::from_secs(20), ptcp_read(&socket))
            .await
            .map_err(|_| "Relay heartbeat timeout".to_string())??;

        // Immediately notify CONN/DISC status before reorder buffering so Bind never waits on video packets
        if let PTCPBody::Status(realm, ref status) = raw_pkt.body {
            if status == "CONN" {
                if let Ok(mut map) = conn_channels.lock() {
                    if let Some(connected) = map.remove(&realm) {
                        let _ = connected.send(true);
                    }
                }
            } else if status == "DISC" {
                if let Ok(mut map) = conn_channels.lock() {
                    map.remove(&realm);
                }
                if let Ok(mut map) = channels.lock() {
                    map.remove(&realm);
                }
            }
        }

        let (ready_packets, ack_opt, retransmits) = {
            let Ok(mut sess) = session.lock() else {
                continue;
            };
            sess.recv_ordered(raw_pkt)
        };

        for raw in retransmits {
            let _ = socket.send(&raw).await;
        }

        if let Some(ack_pkt) = ack_opt {
            let _ = ptcp_request(&socket, ack_pkt).await;
        }

        for packet in ready_packets {
            if let PTCPBody::Payload(p) = packet.body {
                let tx = channels.lock().ok().and_then(|map| map.get(&p.realm).cloned());
                if let Some(tx) = tx {
                    if tx.try_send(p.data).is_err() {
                        let removed = channels
                            .lock()
                            .ok()
                            .and_then(|mut map| map.remove(&p.realm))
                            .is_some();
                        if removed {
                            let _ = dh_tx.try_send(PTCPEvent::Disconnect(p.realm));
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ptcp_payload_rejects_truncated_datagram() {
        let valid = PTCPPayload {
            realm: 42,
            data: b"hello_rtsp".to_vec(),
        };
        let raw = valid.serialize();
        assert_eq!(PTCPPayload::parse(&raw).unwrap(), valid);

        // Truncate 3 bytes off the end: must return Err instead of silently advancing recv
        let truncated = &raw[..raw.len() - 3];
        assert!(PTCPPayload::parse(truncated).is_err());
    }

    #[test]
    fn test_ptcp_session_prunes_stale_reorder_keys() {
        let mut sess = PTCPSession::new();
        sess.recv = 100;

        let make_payload_pkt = |sent: u32, payload_bytes: usize| PTCPPacket {
            sent,
            recv: 0,
            pid: 1,
            lmid: 1,
            rmid: 0,
            body: PTCPBody::Payload(PTCPPayload {
                realm: 7,
                data: vec![0xAA; payload_bytes], // body.len() == payload_bytes + 12
            }),
            duplicate: false,
        };

        // Out-of-order packet starting at 120 (len = 8 + 12 = 20, end = 140)
        let (ready1, _, _) = sess.recv_ordered(make_payload_pkt(120, 8));
        assert!(ready1.is_empty());
        assert_eq!(sess.reorder_buf.len(), 1);

        // Coalesced retransmission starting at 100 covering up to 160 (len = 48 + 12 = 60)
        let (ready2, _, _) = sess.recv_ordered(make_payload_pkt(100, 48));
        assert_eq!(ready2.len(), 1);
        assert_eq!(sess.recv, 160);
        // Stale key 120 (< 160) MUST have been pruned so gap_since is disarmed and recv never jumps backward
        assert!(sess.reorder_buf.is_empty());
        assert!(sess.gap_since.is_none());
    }

    #[test]
    fn test_generation_guard_prevents_stale_clobber() {
        let connected = AtomicBool::new(true);
        let generation = AtomicU64::new(2);
        let status = Mutex::new("Cloud P2P Connected".to_string());

        // Stale generation 1 tries to clobber connected=false and status
        set_connected_if_current(&connected, &generation, 1, false);
        set_status_if_current(&status, &generation, 1, "Stopped");
        assert!(connected.load(Ordering::Acquire));
        assert_eq!(status.lock().unwrap().as_str(), "Cloud P2P Connected");

        // Current generation 2 updates cleanly
        set_connected_if_current(&connected, &generation, 2, false);
        assert!(!connected.load(Ordering::Acquire));
    }

    #[test]
    fn test_ipv6_is_lan_reachable_no_panic_on_raw_literal() {
        // Unreachable port on loopback IPv6 literal should return false cleanly without failing parse
        assert!(!is_lan_reachable("::1", 1, 20));
        assert!(!is_lan_reachable("[::1]", 1, 20));
    }
}

