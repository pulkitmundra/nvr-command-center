use aes::Aes256;
use base64::Engine;
use cipher::{KeyIvInit, StreamCipher};
use hmac::{Hmac, Mac};
use md5::Md5;
use quick_xml::events::Event as XmlEvent;
use quick_xml::Reader as XmlReader;
use rand::rngs::OsRng;
use rand::RngCore;
use sha1::Digest;
use sha2::Sha256;
use std::{
    cmp,
    collections::{hash_map::RandomState, HashMap, HashSet, VecDeque},
    hash::{BuildHasher, Hash, Hasher},
    io::ErrorKind,
    net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs},
    os::unix::io::AsRawFd,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{lookup_host, TcpListener, UdpSocket},
    sync::{mpsc, oneshot, watch, Notify, Semaphore},
    time,
};
use zeroize::{Zeroize, Zeroizing};

type Aes256Ofb = ofb::Ofb<Aes256>;
type HmacSha256 = Hmac<Sha256>;

// Public DHP2P wire-protocol specification constants (shared across all Dahua/CP Plus/Amcrest clients)
pub const MAIN_SERVER: &str = "www.easy4ipcloud.com:8800";
pub const DHP2P_WSSE_ID: &str = "cba1b29e32cb17aa46b8ff9e73c7f40b";
pub const DHP2P_WSSE_SEED: &str = "996103384cdf19179e19243e959bbf8b";

// InstaOn Cloud & Multi-Profile constants
pub const INSTAON_MAIN_SERVER: &str = "instaonserver.com:8800";
pub const INSTAON_CTC_SERVER: &str = "instaonserverctc.com:8800";
pub const DOLYNK_MAIN_SERVER: &str = "p2p.dolynkcloud.com:8800";

// KVMS Pro Lite (InstaOn Windows client)
pub const KVMS_WSSE_USERNAME: &str = "DuvCSF0EJcNIELdlJsgjXoXhh7m_kplc";
pub const KVMS_WSSE_USERKEY: &str = "CD3zmkztC63QH3NomRczYTB9lv7KXEuo";
pub const KVMS_CTC_WSSE_USERNAME: &str = "dS8dT3mS0uF4mX1fO3kA0kR5xB_cpcbc";
pub const KVMS_CTC_WSSE_USERKEY: &str = "fG6uM4eV9vS2dG3jU5aH7lZ7rU9dO9bK";

// gCMOB (InstaOn Android client)
pub const GCMOB_WSSE_USERNAME: &str = "89d61b61dc9a69fdcc03e4ff3a3be2f5";
pub const GCMOB_WSSE_USERKEY: &str = "993c7d7c95542c684c30ff9a9b831249";
pub const GCMOB_CTC_WSSE_USERNAME: &str = "zH4gW1gQ6eZ1bT1sH9yP6fR2f_cbmbap";
pub const GCMOB_CTC_WSSE_USERKEY: &str = "dN2cQ9gV2qU0iR3uG0wM8oI9sK0hT9mQ";

// DMSS (Dolynk Android client)
pub const DMSS_WSSE_USERNAME: &str = "793k5zdi4dd5f037sooag8yo_dolynkc";
pub const DMSS_WSSE_USERKEY: &str = "ef8hatgmcuk4qamgg4fxx19x33s9q1xy";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct P2PProfile {
    pub name: &'static str,
    pub server: &'static str,
    pub wsse_user: &'static str,
    pub wsse_key: &'static str,
    pub verb_get: &'static str,
    pub verb_post: &'static str,
    pub to_u_type: Option<&'static str>,
    pub version: Option<&'static str>,
    pub sversion: Option<&'static str>,
    pub app_header_order: bool,
    pub warmup_path: &'static str,
}

pub const PROFILE_SMARTPSS: P2PProfile = P2PProfile {
    name: "smartpss",
    server: MAIN_SERVER,
    wsse_user: DHP2P_WSSE_ID,
    wsse_key: DHP2P_WSSE_SEED,
    verb_get: "DHGET",
    verb_post: "DHPOST",
    to_u_type: None,
    version: None,
    sversion: None,
    app_header_order: false,
    warmup_path: "/probe/p2psrv",
};

pub const PROFILE_KVMS: P2PProfile = P2PProfile {
    name: "kvmsprolite",
    server: INSTAON_MAIN_SERVER,
    wsse_user: KVMS_WSSE_USERNAME,
    wsse_key: KVMS_WSSE_USERKEY,
    verb_get: "NFGET",
    verb_post: "NFPOST",
    to_u_type: Some("Client/KVMSProLite_Win"),
    version: Some("6.7.15"),
    sversion: Some("1.1.0"),
    app_header_order: true,
    warmup_path: "/online/stun",
};

pub const PROFILE_GCMOB: P2PProfile = P2PProfile {
    name: "gcmob",
    server: INSTAON_MAIN_SERVER,
    wsse_user: GCMOB_WSSE_USERNAME,
    wsse_key: GCMOB_WSSE_USERKEY,
    verb_get: "NFGET",
    verb_post: "NFPOST",
    to_u_type: Some("Client/gCMOB_Android"),
    version: Some("6.7.15"),
    sversion: Some("1.1.0"),
    app_header_order: true,
    warmup_path: "/online/stun",
};

pub const PROFILE_DMSS: P2PProfile = P2PProfile {
    name: "dmss",
    server: DOLYNK_MAIN_SERVER,
    wsse_user: DMSS_WSSE_USERNAME,
    wsse_key: DMSS_WSSE_USERKEY,
    verb_get: "NFGET",
    verb_post: "NFPOST",
    to_u_type: Some("Client/Dmss_Android"),
    version: Some("6.7.15"),
    sversion: Some("1.1.0"),
    app_header_order: true,
    warmup_path: "/online/stun",
};

pub const PROFILE_KVMS_CTC: P2PProfile = P2PProfile {
    name: "kvmsprolite-ctc",
    server: INSTAON_CTC_SERVER,
    wsse_user: KVMS_CTC_WSSE_USERNAME,
    wsse_key: KVMS_CTC_WSSE_USERKEY,
    verb_get: "NFGET",
    verb_post: "NFPOST",
    to_u_type: Some("Client/KVMSProLite_Win"),
    version: Some("6.7.15"),
    sversion: Some("1.1.0"),
    app_header_order: true,
    warmup_path: "/online/stun",
};

pub const PROFILE_GCMOB_CTC: P2PProfile = P2PProfile {
    name: "gcmob-ctc",
    server: INSTAON_CTC_SERVER,
    wsse_user: GCMOB_CTC_WSSE_USERNAME,
    wsse_key: GCMOB_CTC_WSSE_USERKEY,
    verb_get: "NFGET",
    verb_post: "NFPOST",
    to_u_type: Some("Client/gCMOB_Android"),
    version: Some("6.7.15"),
    sversion: Some("1.1.0"),
    app_header_order: true,
    warmup_path: "/online/stun",
};

pub const DEFAULT_PROFILES: &[P2PProfile] = &[
    PROFILE_SMARTPSS,
    PROFILE_KVMS,
    PROFILE_GCMOB,
    PROFILE_DMSS,
    PROFILE_KVMS_CTC,
    PROFILE_GCMOB_CTC,
];

pub fn compute_wsse_digest(nonce: &str, created: &str, user: &str, key: &str) -> String {
    let pwd = format!("{}{}DHP2P:{}:{}", nonce, created, user, key);
    let mut hasher = sha1::Sha1::new();
    hasher.update(pwd.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

const DHP2P_SESSION_IV: &[u8; 16] = b"2z52*lk9o6HRyJrf";
const DHP2P_INFO_MASK: &[u8; 32] = b"kRjmsUB&ezmdGLL67H#$ojw@XflcaIaf";
const DHP2P_INFO_IV: &[u8; 16] = b"MydvJw*Iw1w&i^kk";
const RECENT_PACKET_WINDOW: usize = 1024;
const REALM_CHANNEL_CAPACITY: usize = 512;
const MAX_ACTIVE_LOCAL_CLIENTS: usize = 40;
const MAX_REORDER_WINDOW_BYTES: u32 = 524_288; // 512 KiB max out-of-order PTCP window
const REORDER_BUF_MAX_PACKETS: usize = 512;
const DUP_ACK_COOLDOWN: Duration = Duration::from_millis(15);
const RETRANSMIT_COOLDOWN: Duration = Duration::from_millis(100);
const MAX_RETRANSMIT_BURST: usize = 4;

pub fn is_lan_reachable(host: &str, port: u16, timeout_ms: u64) -> bool {
    if port == 0 {
        return false;
    }
    let Ok(formatted_host) = crate::config::validate_nvr_host(host) else {
        return false;
    };
    let connect_budget = Duration::from_millis(timeout_ms.max(1));

    let raw_trimmed = host.trim();
    let raw_unbracketed = raw_trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(raw_trimmed);

    // Direct IpAddr literal without IPv6 %zone: connect immediately without DNS lookup
    if !raw_unbracketed.contains('%') {
        if let Ok(ip) = raw_unbracketed.parse::<IpAddr>() {
            return TcpStream::connect_timeout(&SocketAddr::new(ip, port), connect_budget).is_ok();
        }
    }

    let addr_str = if raw_unbracketed.contains('%') {
        format!("[{}]:{}", raw_unbracketed, port)
    } else {
        format!("{}:{}", formatted_host, port)
    };

    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let spawn_ok = std::thread::Builder::new()
        .name("nvr-lan-dns".to_string())
        .spawn(move || {
            let resolved: Option<Vec<SocketAddr>> =
                addr_str.to_socket_addrs().ok().map(|it| it.collect());
            let _ = tx.send(resolved);
        })
        .is_ok();
    if !spawn_ok {
        return false;
    }

    let dns_timeout = Duration::from_millis(timeout_ms.max(250));
    let Ok(Some(addrs)) = rx.recv_timeout(dns_timeout) else {
        return false;
    };
    if addrs.is_empty() {
        return false;
    }

    let deadline = std::time::Instant::now() + connect_budget;
    for (idx, addr) in addrs.iter().enumerate() {
        let now = std::time::Instant::now();
        if now >= deadline {
            break;
        }
        let rem = deadline - now;
        let rem_count = (addrs.len() - idx) as u32;
        let per_addr = if rem_count > 1 {
            (rem / rem_count).max(Duration::from_millis(80)).min(rem)
        } else {
            rem
        };
        if TcpStream::connect_timeout(addr, per_addr).is_ok() {
            return true;
        }
    }
    false
}

pub fn validate_p2p_serial(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.len() < 4 || trimmed.len() > 64 {
        return Err("Invalid P2P serial length".to_string());
    }
    if !trimmed
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("Invalid characters in P2P serial".to_string());
    }
    Ok(trimmed.to_string())
}

fn validate_p2p_token(raw: &str) -> Result<&str, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 256 {
        return Err("Invalid relay token length".to_string());
    }
    if !trimmed.bytes().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b':' | b'/' | b'=' | b'-')
    }) {
        return Err("Invalid characters in relay token".to_string());
    }
    Ok(trimmed)
}

pub fn is_public_routable_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            let is_cgnat = o[0] == 100 && (o[1] & 0b1100_0000) == 64; // 100.64.0.0/10
            let is_benchmark = o[0] == 198 && (o[1] == 18 || o[1] == 19); // 198.18.0.0/15
            let is_doc = (o[0] == 192 && o[1] == 0 && o[2] == 2)
                || (o[0] == 198 && o[1] == 51 && o[2] == 100)
                || (o[0] == 203 && o[1] == 0 && o[2] == 113);
            let is_reserved = o[0] >= 240;
            !v4.is_loopback()
                && !v4.is_private()
                && !v4.is_link_local()
                && !v4.is_broadcast()
                && !v4.is_multicast()
                && !v4.is_unspecified()
                && o[0] != 0
                && !is_cgnat
                && !is_benchmark
                && !is_doc
                && !is_reserved
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_public_routable_ip(IpAddr::V4(mapped));
            }
            let seg0 = v6.segments()[0];
            let is_ula = (seg0 & 0xfe00) == 0xfc00; // fc00::/7
            let is_link_local = (seg0 & 0xffc0) == 0xfe80; // fe80::/10
            let is_doc = seg0 == 0x2001 && v6.segments()[1] == 0x0db8;
            !v6.is_loopback()
                && !v6.is_unspecified()
                && !v6.is_multicast()
                && !is_ula
                && !is_link_local
                && !is_doc
        }
    }
}

async fn connect_public_udp(socket: &UdpSocket, endpoint: &str) -> Result<SocketAddr, String> {
    let trimmed = endpoint.trim();
    if trimmed.is_empty()
        || trimmed.len() > 255
        || trimmed.bytes().any(|b| b <= 0x20 || b == 0x7f)
    {
        return Err("Malformed P2P server endpoint".to_string());
    }
    let addrs: Vec<SocketAddr> = lookup_host(trimmed)
        .await
        .map_err(|e| format!("DNS lookup failed: {}", e))?
        .filter(|a| a.port() > 0 && is_public_routable_ip(a.ip()))
        .collect();
    let Some(&target) = addrs.first() else {
        return Err("Refused non-public P2P server address".to_string());
    };
    socket
        .connect(target)
        .await
        .map_err(|e| format!("UDP connect error: {}", e))?;
    Ok(target)
}

#[cfg(target_os = "linux")]
fn format_proc_ipv6_endpoint(ip: std::net::Ipv6Addr, port: u16) -> String {
    let o = ip.octets();
    let w0 = u32::from_ne_bytes([o[0], o[1], o[2], o[3]]);
    let w1 = u32::from_ne_bytes([o[4], o[5], o[6], o[7]]);
    let w2 = u32::from_ne_bytes([o[8], o[9], o[10], o[11]]);
    let w3 = u32::from_ne_bytes([o[12], o[13], o[14], o[15]]);
    format!("{:08X}{:08X}{:08X}{:08X}:{:04X}", w0, w1, w2, w3, port)
}

#[cfg(target_os = "linux")]
fn lookup_proc_tcp_uid(
    proc_path: &str,
    expected_client: &str,
    expected_server: &str,
) -> Option<libc::uid_t> {
    let proc_tcp = std::fs::read_to_string(proc_path).ok()?;
    for line in proc_tcp.lines().skip(1) {
        let mut cols = line.split_whitespace();
        let _slot = cols.next();
        let Some(local_col) = cols.next() else {
            continue;
        };
        let Some(rem_col) = cols.next() else {
            continue;
        };
        if local_col.eq_ignore_ascii_case(expected_client)
            && rem_col.eq_ignore_ascii_case(expected_server)
        {
            let _ = cols.next();
            let _ = cols.next();
            let _ = cols.next();
            let _ = cols.next();
            return cols.next().and_then(|s| s.parse::<libc::uid_t>().ok());
        }
    }
    None
}

fn verify_same_uid_peer(stream: &tokio::net::TcpStream, peer_addr: SocketAddr) -> bool {
    if !peer_addr.ip().is_loopback() {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        let my_uid = unsafe { libc::geteuid() };
        let fd = stream.as_raw_fd();
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if rc == 0
            && (len as usize) == std::mem::size_of::<libc::ucred>()
            && cred.uid != libc::uid_t::MAX
        {
            return cred.uid == my_uid;
        }

        // On Linux AF_INET/AF_INET6 TCP sockets, sk_peer_cred is unset (uid == uid_t::MAX);
        // verify the client's kernel socket UID from /proc/net/tcp and /proc/net/tcp6, failing CLOSED.
        let Ok(local_addr) = stream.local_addr() else {
            return false;
        };
        match peer_addr.ip() {
            IpAddr::V4(v4) => {
                let client_hex = u32::from_ne_bytes(v4.octets());
                let server_hex = u32::from_ne_bytes([127, 0, 0, 1]);
                let expected_client = format!("{:08X}:{:04X}", client_hex, peer_addr.port());
                let expected_server = format!("{:08X}:{:04X}", server_hex, local_addr.port());
                if let Some(sock_uid) =
                    lookup_proc_tcp_uid("/proc/net/tcp", &expected_client, &expected_server)
                {
                    return sock_uid == my_uid;
                }
                let expected_client6 =
                    format_proc_ipv6_endpoint(v4.to_ipv6_mapped(), peer_addr.port());
                let expected_server6 = format_proc_ipv6_endpoint(
                    std::net::Ipv4Addr::LOCALHOST.to_ipv6_mapped(),
                    local_addr.port(),
                );
                if let Some(sock_uid) =
                    lookup_proc_tcp_uid("/proc/net/tcp6", &expected_client6, &expected_server6)
                {
                    return sock_uid == my_uid;
                }
                false
            }
            IpAddr::V6(v6) => {
                let expected_client6 = format_proc_ipv6_endpoint(v6, peer_addr.port());
                let server_v6 = match local_addr.ip() {
                    IpAddr::V6(sv6) => sv6,
                    IpAddr::V4(sv4) => sv4.to_ipv6_mapped(),
                };
                let expected_server6 = format_proc_ipv6_endpoint(server_v6, local_addr.port());
                if let Some(sock_uid) =
                    lookup_proc_tcp_uid("/proc/net/tcp6", &expected_client6, &expected_server6)
                {
                    return sock_uid == my_uid;
                }
                false
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = stream;
        true
    }
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

fn os_rand_u32() -> u32 {
    OsRng.next_u32()
}

fn compute_config_digest(
    serial: &str,
    username: &str,
    password: &str,
    remote_rtsp_port: u16,
    remote_http_port: u16,
) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(serial.as_bytes());
    h.update(b"\0");
    h.update(username.as_bytes());
    h.update(b"\0");
    h.update(password.as_bytes());
    h.update(b"\0");
    h.update(remote_rtsp_port.to_be_bytes());
    h.update(remote_http_port.to_be_bytes());
    let out = h.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    arr
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
            let mut clean: String = msg
                .into()
                .chars()
                .filter(|c| !c.is_control())
                .take(120)
                .collect();
            if clean.is_empty() {
                clean.push_str("Cloud P2P");
            }
            *s = clean;
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
    active_key_hash: Option<[u8; 32]>,
    local_port: u16,
    local_http_port: u16,
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
            active_key_hash: None,
            local_port: 0,
            local_http_port: 0,
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
        self.active_key_hash = None;
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
        self.active_key_hash = None;
        self.local_port = 0;
        self.local_http_port = 0;
        if let Ok(mut s) = self.status.lock() {
            *s = "Stopped".to_string();
        }
    }

    /// Starts (or reuses) a single authenticated Cloud P2P session that simultaneously tunnels:
    /// - `local_rtsp_port` (`127.0.0.1:0`, `SO_PEERCRED`-guarded) -> NVR `remote_rtsp_port` (e.g. `554`)
    /// - `local_http_port` (`127.0.0.1:0`, `SO_PEERCRED`-guarded) -> NVR `remote_http_port` (e.g. `80`)
    pub fn ensure_started(
        &mut self,
        serial: &str,
        username: &str,
        password: &str,
        remote_rtsp_port: u16,
        remote_http_port: u16,
    ) -> (u16, u16) {
        let Ok(serial_clean) = validate_p2p_serial(serial) else {
            self.stop();
            if !serial.trim().is_empty() {
                self.halted.store(true, Ordering::Release);
                if let Ok(mut s) = self.status.lock() {
                    *s = "Invalid P2P Serial format".to_string();
                }
            }
            return (0, 0);
        };

        let r_port = remote_rtsp_port.max(1);
        let h_port = remote_http_port.max(1);
        let key_hash = compute_config_digest(&serial_clean, username, password, r_port, h_port);

        if self.active_key_hash == Some(key_hash)
            && self.shutdown_tx.is_some()
            && !self.is_halted()
            && self.local_port > 0
            && self.local_http_port > 0
        {
            return (self.local_port, self.local_http_port);
        }

        self.stop();

        let bind_ephemeral = || -> Result<(std::net::TcpListener, u16), String> {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0))
                .map_err(|e| format!("Port bind failed ({})", e))?;
            l.set_nonblocking(true)
                .map_err(|e| format!("Socket setup failed ({})", e))?;
            let port = l
                .local_addr()
                .map_err(|e| format!("Local addr error ({})", e))?
                .port();
            Ok((l, port))
        };

        let (std_rtsp_listener, rtsp_local_port) = match bind_ephemeral() {
            Ok(v) => v,
            Err(msg) => {
                self.halted.store(true, Ordering::Release);
                if let Ok(mut s) = self.status.lock() {
                    *s = msg;
                }
                return (0, 0);
            }
        };

        let (std_http_listener, http_local_port) = match bind_ephemeral() {
            Ok(v) => v,
            Err(msg) => {
                self.halted.store(true, Ordering::Release);
                if let Ok(mut s) = self.status.lock() {
                    *s = msg;
                }
                return (0, 0);
            }
        };

        self.local_port = rtsp_local_port;
        self.local_http_port = http_local_port;

        let my_gen = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

        self.active_key_hash = Some(key_hash);
        self.shutdown_tx = Some(shutdown_tx);
        self.connected.store(false, Ordering::Release);
        self.halted.store(false, Ordering::Release);

        let generation = Arc::clone(&self.generation);
        let connected_flag = Arc::clone(&self.connected);
        let halted_flag = Arc::clone(&self.halted);
        let status = Arc::clone(&self.status);
        set_status_if_current(&status, &generation, my_gen, "Cloud P2P Connecting...");

        let user_owned = username.to_string();
        let pass_owned = Zeroizing::new(password.to_string());

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
                    let rtsp_listener = match TcpListener::from_std(std_rtsp_listener) {
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
                    let http_listener = match TcpListener::from_std(std_http_listener) {
                        Ok(l) => Arc::new(l),
                        Err(e) => {
                            halted_flag.store(true, Ordering::Release);
                            set_status_if_current(
                                &status,
                                &generation,
                                my_gen,
                                format!("P2P HTTP listener error ({})", e),
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
                            rtsp_listener: Arc::clone(&rtsp_listener),
                            http_listener: Arc::clone(&http_listener),
                            serial: serial_clean.clone(),
                            username: user_owned.clone(),
                            password: pass_owned.clone(),
                            remote_rtsp_port: r_port,
                            remote_http_port: h_port,
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
                    drop(rtsp_listener);
                    drop(http_listener);
                });

                rt.shutdown_background();
            });

        match spawn_res {
            Ok(handle) => {
                self.worker_handle = Some(handle);
                (self.local_port, self.local_http_port)
            }
            Err(e) => {
                self.shutdown_tx = None;
                self.active_key_hash = None;
                self.local_port = 0;
                self.local_http_port = 0;
                self.halted.store(true, Ordering::Release);
                if let Ok(mut s) = self.status.lock() {
                    *s = format!("Thread spawn error ({})", e);
                }
                (0, 0)
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
    rtsp_listener: Arc<TcpListener>,
    http_listener: Arc<TcpListener>,
    serial: String,
    username: String,
    password: Zeroizing<String>,
    remote_rtsp_port: u16,
    remote_http_port: u16,
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
        Duration::from_secs(20),
        p2p_handshake(
            socket,
            &ctx.serial,
            &ctx.username,
            ctx.password.as_str(),
            shutdown_rx.clone(),
        ),
    );
    let (socket, session) = tokio::select! {
        biased;
        _ = wait_for_shutdown(&mut shutdown_rx) => return Ok(()),
        res = handshake_fut => res.map_err(|_| "Handshake timeout".to_string())??,
    };

    // Drain any stale sockets queued before the handshake finished
    for listener in [&ctx.rtsp_listener, &ctx.http_listener] {
        while let Ok(Ok((stale_client, _))) =
            time::timeout(Duration::from_millis(2), listener.accept()).await
        {
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
    let writer_handle = tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut writer_shutdown) => {}
            _ = dh_writer(session, writer_sock, dh_rx) => {}
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

    // Separate RTSP vs HTTP bind semaphores so HTTP timeout retries never stall live RTSP binds,
    // and cap total active local clients.
    let rtsp_bind_semaphore = Arc::new(Semaphore::new(1));
    let http_bind_semaphore = Arc::new(Semaphore::new(1));
    let client_cap_semaphore = Arc::new(Semaphore::new(MAX_ACTIVE_LOCAL_CLIENTS));
    let consecutive_bind_failures = Arc::new(AtomicU32::new(0));
    let mut health_tick = time::interval(Duration::from_millis(250));
    health_tick.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

    loop {
        let accepted: (std::io::Result<(tokio::net::TcpStream, SocketAddr)>, u16, bool) = tokio::select! {
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
            res = ctx.rtsp_listener.accept() => (res, ctx.remote_rtsp_port, false),
            res = ctx.http_listener.accept() => (res, ctx.remote_http_port, true),
        };

        let (Ok((client, peer_addr)), target_remote_port, is_http) = accepted else {
            continue;
        };

        // Kernel SO_PEERCRED + /proc/net/tcp + /proc/net/tcp6 guard: reject foreign UIDs
        if !verify_same_uid_peer(&client, peer_addr) || !is_tcp_established(&client) {
            drop(client);
            continue;
        }

        let Ok(client_slot_permit) = client_cap_semaphore.clone().try_acquire_owned() else {
            drop(client);
            continue;
        };

        let dh_tx_client = dh_tx.clone();
        let channels_client = Arc::clone(&channels);
        let conn_channels_client = Arc::clone(&conn_channels);
        let sem_clone = if is_http {
            Arc::clone(&http_bind_semaphore)
        } else {
            Arc::clone(&rtsp_bind_semaphore)
        };
        let bind_failures = Arc::clone(&consecutive_bind_failures);
        let alive_flag = Arc::clone(&session_alive);
        let mut client_shutdown = shutdown_rx.clone();

        tokio::spawn(async move {
            let _active_permit = client_slot_permit;
            let bind_permit = tokio::select! {
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
            let mut bound_realm: Option<u32> = None;
            let mut actively_rejected = false;

            for _attempt in 0..3 {
                if *client_shutdown.borrow() || !is_tcp_established(&client) {
                    break;
                }
                let attempt_realm =
                    allocate_unique_realm_id(&channels_client, &conn_channels_client);
                let (conn_tx, mut conn_rx) = oneshot::channel::<bool>();
                if let Ok(mut map) = channels_client.lock() {
                    map.insert(attempt_realm, tx.clone());
                }
                if let Ok(mut map) = conn_channels_client.lock() {
                    map.insert(attempt_realm, conn_tx);
                }

                if dh_tx_client
                    .send(PTCPEvent::Connect(attempt_realm, target_remote_port))
                    .await
                    .is_err()
                {
                    if let Ok(mut map) = channels_client.lock() {
                        map.remove(&attempt_realm);
                    }
                    if let Ok(mut map) = conn_channels_client.lock() {
                        map.remove(&attempt_realm);
                    }
                    break;
                }

                let wait_res = tokio::select! {
                    biased;
                    _ = wait_for_shutdown(&mut client_shutdown) => {
                        if let Ok(mut map) = channels_client.lock() {
                            map.remove(&attempt_realm);
                        }
                        if let Ok(mut map) = conn_channels_client.lock() {
                            map.remove(&attempt_realm);
                        }
                        break;
                    }
                    res = time::timeout(Duration::from_millis(2000), &mut conn_rx) => res,
                };
                match wait_res {
                    Ok(Ok(true)) => {
                        bound_realm = Some(attempt_realm);
                        break;
                    }
                    Ok(_) => {
                        actively_rejected = true;
                        if let Ok(mut map) = channels_client.lock() {
                            map.remove(&attempt_realm);
                        }
                        if let Ok(mut map) = conn_channels_client.lock() {
                            map.remove(&attempt_realm);
                        }
                        break;
                    }
                    Err(_) => {
                        if let Ok(mut map) = channels_client.lock() {
                            map.remove(&attempt_realm);
                        }
                        if let Ok(mut map) = conn_channels_client.lock() {
                            map.remove(&attempt_realm);
                        }
                        let _ = dh_tx_client.try_send(PTCPEvent::Disconnect(attempt_realm));
                        continue;
                    }
                }
            }

            // Drop stack sender copy so `channels_client` holds the sole Sender<Vec<u8>>
            drop(tx);

            let Some(realm_id) = bound_realm else {
                // Active DISC on HTTP proves the P2P session is alive while NVR HTTP port is closed
                if !(is_http && actively_rejected)
                    && !*client_shutdown.borrow()
                    && is_tcp_established(&client)
                {
                    bind_failures.fetch_add(1, Ordering::AcqRel);
                }
                return;
            };

            bind_failures.store(0, Ordering::Release);
            drop(bind_permit);

            let (reader, writer) = client.into_split();
            let reader_shutdown = client_shutdown.clone();
            let writer_shutdown = client_shutdown;
            let dh_tx_writer = dh_tx_client.clone();
            let channels_writer = Arc::clone(&channels_client);
            let channels_reader = Arc::clone(&channels_client);

            let w_task = tokio::spawn(async move {
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

            process_reader(
                reader,
                realm_id,
                channels_reader,
                dh_tx_client,
                reader_shutdown,
            )
            .await;
            let _ = w_task.await;
        });
    }
}

fn allocate_unique_realm_id(
    channels: &Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>,
    conn_channels: &Mutex<HashMap<u32, oneshot::Sender<bool>>>,
) -> u32 {
    loop {
        let candidate = os_rand_u32();
        if candidate == 0 {
            continue;
        }
        let in_channels = channels
            .lock()
            .map(|m| m.contains_key(&candidate))
            .unwrap_or(false);
        if in_channels {
            continue;
        }
        let in_conn = conn_channels
            .lock()
            .map(|m| m.contains_key(&candidate))
            .unwrap_or(false);
        if !in_conn {
            return candidate;
        }
    }
}

pub fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
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
    if info_b64.is_empty() || info_b64.len() > 16_384 {
        return String::new();
    }
    let Ok(mut raw) = base64::engine::general_purpose::STANDARD.decode(info_b64.trim()) else {
        return String::new();
    };
    let mut cipher = Aes256Ofb::new(DHP2P_INFO_MASK.into(), DHP2P_INFO_IV.into());
    cipher.apply_keystream(&mut raw);
    let Ok(json_str) = String::from_utf8(raw) else {
        return String::new();
    };
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&json_str) {
        if let Some(s) = val.get("randsalt").and_then(|v| v.as_str()) {
            let clean: String = s
                .chars()
                .filter(|c| c.is_ascii_graphic())
                .take(128)
                .collect();
            return clean;
        }
    }
    String::new()
}

fn get_key(username: &str, password: &str, randsalt: &str) -> Zeroizing<Vec<u8>> {
    let mut input = Zeroizing::new(format!("{}:Login to {}:{}", username, randsalt, password));
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    input.zeroize();
    let result = hasher.finalize();
    Zeroizing::new(
        result
            .iter()
            .map(|b| format!("{:02X}", b))
            .collect::<String>()
            .into_bytes(),
    )
}

fn get_enc(key: &[u8], nonce: u32, data: &str) -> String {
    let salt = nonce.to_string();
    let mut dk = Zeroizing::new([0u8; 32]);
    pbkdf2::pbkdf2_hmac::<Sha256>(key, salt.as_bytes(), 20000, &mut *dk);
    let mut buf = data.as_bytes().to_vec();
    let mut cipher = Aes256Ofb::new((&*dk).into(), DHP2P_SESSION_IV.into());
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
    _socket: UdpSocket,
    serial: &str,
    username: &str,
    password: &str,
) -> Result<(UdpSocket, PTCPSession), String> {
    let mut last_err = String::new();
    for profile in DEFAULT_PROFILES {
        match try_profile_handshake(profile, serial, username, password).await {
            Ok(res) => return Ok(res),
            Err(e) => {
                if is_p2p_auth_rejection(&e) {
                    return Err(e);
                }
                last_err = e;
            }
        }
    }
    Err(format!("All P2P profiles failed (last error: {})", last_err))
}

async fn try_profile_handshake(
    profile: &P2PProfile,
    serial: &str,
    username: &str,
    password: &str,
) -> Result<(UdpSocket, PTCPSession), String> {
    let serial_safe = validate_p2p_serial(serial)?;
    let mut cseq = 0;
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| e.to_string())?;

    // Public-IP SSRF validation applies to every profile's signaling server.
    connect_public_udp(&socket, profile.server)
        .await
        .map_err(|e| format!("{}: connect error: {}", profile.name, e))?;

    if profile.warmup_path == "/online/stun" {
        dh_request(&socket, "/online/stun", None, &mut cseq, profile, true).await?;
        let _ = time::timeout(Duration::from_millis(1500), dh_read_raw(&socket)).await;
    }

    dh_request(&socket, "/probe/p2psrv", None, &mut cseq, profile, false).await?;
    let _ = time::timeout(Duration::from_millis(1500), dh_read(&socket)).await;

    dh_request(
        &socket,
        &format!("/online/p2psrv/{}", serial_safe),
        None,
        &mut cseq,
        profile,
        false,
    )
    .await?;

    let p2psrv_res = time::timeout(Duration::from_millis(2500), dh_read(&socket))
        .await
        .map_err(|_| format!("{}: Timeout waiting for /online/p2psrv", profile.name))?
        .map_err(|e| {
            // The p2psrv lookup happens BEFORE any NVR credential is sent, so a 401/403 here only
            // means this cloud does not accept this client profile. Strip the auth markers so the
            // profile loop continues to the next cloud instead of halting as a bad password.
            let neutral = e
                .replace("AUTH_401:", "profile rejected:")
                .replace("HTTP 401", "HTTP 4xx")
                .replace("HTTP 403", "HTTP 4xx");
            format!("{}: /online/p2psrv failed: {}", profile.name, neutral)
        })?;

    let p2psrv = p2psrv_res
        .body
        .and_then(|b| b.get("body/US").cloned())
        .ok_or_else(|| format!("{}: Missing US p2psrv", profile.name))?;

    // Pin signaling requests to the NVR's authoritative cluster node on port 8800
    // so Geo-DNS round-robin differences on remote networks never misroute relay-channel
    let cluster_host = p2psrv
        .split(':')
        .next()
        .unwrap_or_else(|| profile.server.split(':').next().unwrap_or(""));
    let cluster_main = format!("{}:8800", cluster_host);
    connect_public_udp(&socket, &cluster_main).await?;

    let socket_info = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| e.to_string())?;
    connect_public_udp(&socket_info, &p2psrv).await?;

    dh_request(
        &socket_info,
        &format!("/probe/device/{}", serial_safe),
        None,
        &mut cseq,
        profile,
        false,
    )
    .await?;
    let _ = time::timeout(Duration::from_millis(2000), dh_read(&socket_info)).await;

    dh_request(
        &socket_info,
        &format!("/info/device/{}", serial_safe),
        None,
        &mut cseq,
        profile,
        false,
    )
    .await?;
    let info_res = time::timeout(Duration::from_millis(2500), dh_read_raw(&socket_info))
        .await
        .map_err(|_| "Timeout waiting for /info/device".to_string())?
        .unwrap_or(DHResponse {
            code: 500,
            status: "Error".to_string(),
            body: None,
        });

    let randsalt = if info_res.code < 300 {
        info_res
            .body
            .and_then(|b| b.get("body/Info").cloned())
            .map(|i| get_device_randsalt(&i))
            .unwrap_or_default()
    } else {
        String::new()
    };

    dh_request(&socket, "/online/relay", None, &mut cseq, profile, false).await?;
    let relay = time::timeout(Duration::from_millis(2500), dh_read(&socket))
        .await
        .map_err(|_| "Timeout waiting for /online/relay".to_string())??
        .body
        .and_then(|b| b.get("body/Address").cloned())
        .ok_or_else(|| "Missing relay address".to_string())?;

    let socket2 = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| e.to_string())?;

    let mut cid = [0u8; 8];
    OsRng.fill_bytes(&mut cid);
    let key = get_key(username, password, &randsalt);
    let nonce = os_rand_u32() & 0x7FFF_FFFF;
    let nonce_str = nonce.to_string();
    let local_port = socket.local_addr().map(|a| a.port()).unwrap_or(0);
    let raw_laddr = format!("127.0.0.1:{}", local_port);
    let laddr_enc = get_enc(&key, nonce, &raw_laddr);
    let auth_xml = get_auth(username, &key, &nonce_str, &randsalt, &laddr_enc);

    let channel_body = format!(
        "<body>{}<Identify>{}</Identify><IpEncrptV2>true</IpEncrptV2><LocalAddr>{}</LocalAddr><version>5.0.0</version></body>",
        auth_xml,
        cid.iter()
            .map(|b| format!("{:x}", b))
            .collect::<Vec<_>>()
            .join(" "),
        laddr_enc,
    );

    dh_request(
        &socket,
        &format!("/device/{}/p2p-channel", serial_safe),
        Some(&channel_body),
        &mut cseq,
        profile,
        false,
    )
    .await?;

    connect_public_udp(&socket2, &relay).await?;

    dh_request(&socket2, "/relay/agent", None, &mut cseq, profile, false).await?;
    let relay_data = time::timeout(Duration::from_millis(2500), dh_read(&socket2))
        .await
        .map_err(|_| "Timeout waiting for /relay/agent".to_string())??
        .body
        .ok_or_else(|| "Empty relay agent body".to_string())?;
    let token_raw = relay_data
        .get("body/Token")
        .cloned()
        .ok_or_else(|| "Missing relay Token".to_string())?;
    let token_safe = validate_p2p_token(&token_raw)?;
    let agent = relay_data
        .get("body/Agent")
        .cloned()
        .ok_or_else(|| "Missing relay Agent".to_string())?;

    connect_public_udp(&socket2, &agent).await?;

    dh_request(
        &socket2,
        &format!("/relay/start/{}", token_safe),
        Some("<body><Client>:0</Client></body>"),
        &mut cseq,
        profile,
        false,
    )
    .await?;
    let _ = time::timeout(Duration::from_millis(2500), dh_read(&socket2)).await;

    let mut res = time::timeout(Duration::from_millis(3500), dh_read_raw(&socket))
        .await
        .map_err(|_| "Timeout waiting for p2p-channel response".to_string())??;
    for _ in 0..4 {
        if res.code >= 200 {
            break;
        }
        res = time::timeout(Duration::from_millis(1500), dh_read_raw(&socket))
            .await
            .map_err(|_| "Timeout reading p2p-channel frame".to_string())??;
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
        .filter(|s| s.len() <= 128 && s.bytes().all(|b| b.is_ascii_graphic()))
        .cloned()
        .unwrap_or_else(|| nonce_str.clone());

    connect_public_udp(&socket2, &cluster_main).await?;

    let relay_auth = get_auth(username, &key, &resp_nonce, &randsalt, "");
    let relay_body = format!(
        "<body>{}<agentAddr>{}</agentAddr></body>",
        relay_auth,
        xml_escape(&agent)
    );
    dh_request(
        &socket2,
        &format!("/device/{}/relay-channel", serial_safe),
        Some(&relay_body),
        &mut cseq,
        profile,
        false,
    )
    .await?;

    connect_public_udp(&socket2, &agent).await?;
    let mut peek_buf = [0u8; 8];
    if let Ok(Ok(n)) = time::timeout(Duration::from_secs(4), socket2.peek(&mut peek_buf)).await {
        if n < 4 || &peek_buf[..4] != b"PTCP" {
            let _ = time::timeout(Duration::from_secs(1), dh_read_raw(&socket2)).await;
        }
    }

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
    /// Safe entity-free XML parser via `quick-xml` (capped depth & size, rejects DTD internal entities).
    fn parse_body(body: &str) -> HashMap<String, String> {
        let mut tree = HashMap::new();
        if body.len() > 65_536 {
            return tree;
        }
        let mut reader = XmlReader::from_str(body);
        reader.config_mut().trim_text(true);
        let mut stack: Vec<String> = Vec::with_capacity(8);

        loop {
            match reader.read_event() {
                Ok(XmlEvent::DocType(_)) => {
                    // Reject DTD entity definitions completely
                    return HashMap::new();
                }
                Ok(XmlEvent::Start(e)) => {
                    if stack.len() >= 16 {
                        break;
                    }
                    let local = e.local_name();
                    stack.push(String::from_utf8_lossy(local.as_ref()).into_owned());
                }
                Ok(XmlEvent::End(_)) => {
                    let _ = stack.pop();
                }
                Ok(XmlEvent::Text(e)) => {
                    if !stack.is_empty() && tree.len() < 128 {
                        if let Ok(decoded) = e.decode() {
                            let val = quick_xml::escape::unescape(&decoded)
                                .map(|c| c.into_owned())
                                .unwrap_or_else(|_| decoded.into_owned());
                            if val.len() <= 8192 {
                                tree.insert(stack.join("/"), val);
                            }
                        }
                    }
                }
                Ok(XmlEvent::Eof) | Err(_) => break,
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
        let status: String = status_line
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .filter(|c| !c.is_control())
            .take(64)
            .collect();

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
    profile: &P2PProfile,
    warmup: bool,
) -> Result<(), String> {
    if path.is_empty() || path.bytes().any(|b| b <= 0x20 || b == 0x7f) {
        return Err("Invalid signaling request path".to_string());
    }
    let method = match body {
        Some(_) => profile.verb_post,
        None => profile.verb_get,
    };
    let body = body.unwrap_or("");

    // OS CSPRNG nonce (signed i32 like the official InstaOn clients; SmartPSS-style profiles stay positive)
    let nonce_raw = os_rand_u32();
    let nonce_str = if profile.app_header_order {
        (nonce_raw as i32).to_string()
    } else {
        (nonce_raw & 0x7FFF_FFFF).to_string()
    };
    let currdate = if profile.app_header_order {
        chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
    } else {
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    };

    *seq += 1;
    let cseq_val = *seq;

    let auth_block = if warmup && profile.warmup_path == "/online/stun" {
        String::new()
    } else {
        let digest = compute_wsse_digest(&nonce_str, &currdate, profile.wsse_user, profile.wsse_key);
        format!(
            "Authorization: WSSE profile=\"UsernameToken\"\r\nX-WSSE: UsernameToken Username=\"{}\", PasswordDigest=\"{}\", Nonce=\"{}\", Created=\"{}\"\r\n",
            profile.wsse_user, digest, nonce_str, currdate
        )
    };

    let content_headers = if !body.is_empty() {
        format!("Content-Type: \r\nContent-Length: {}\r\n", body.len())
    } else {
        String::new()
    };

    let req = if profile.app_header_order {
        let mut h = format!("{} {} HTTP/1.1\r\n", method, path);
        if !warmup {
            if let Some(v) = profile.version {
                h.push_str(&format!("X-Version: {}\r\n", v));
            }
            if let Some(sv) = profile.sversion {
                h.push_str(&format!("X-Sversion: {}\r\n", sv));
            }
        }
        if let Some(tou) = profile.to_u_type {
            h.push_str(&format!("X-ToUType: {}\r\n", tou));
        }
        h.push_str(&format!("CSeq: {}\r\n", cseq_val));
        h.push_str(&auth_block);
        h.push_str(&content_headers);
        h.push_str("\r\n");
        h.push_str(body);
        h
    } else {
        format!(
            "{} {} HTTP/1.1\r\nCSeq: {}\r\n{}{}\r\n{}",
            method, path, cseq_val, auth_block, content_headers, body
        )
    };

    socket
        .send(req.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

async fn dh_read_raw(socket: &UdpSocket) -> Result<DHResponse, String> {
    let mut buf = [0u8; 8192];
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
    Connect(u32, u16),
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PTCPBody {
    Empty,
    Sync,
    Heartbeat,
    Command(Vec<u8>),
    Bind(u32, u32),
    Status(u32, String),
    Payload(PTCPPayload),
}

fn looks_like_ptcp_subrecord(rem: &[u8]) -> bool {
    if rem.len() < 12 {
        return false;
    }
    match rem[0] {
        0x10 => {
            let len = u16::from_be_bytes([rem[2], rem[3]]) as usize;
            12 + len <= rem.len()
        }
        0x11 => rem.len() >= 16 && rem[1] == 0,
        0x12 => {
            rem.len() >= 16
                && rem[1] == 0
                && rem[12..16].iter().all(|b| b.is_ascii_graphic())
        }
        0x13 => rem[1..4] == [0, 0, 0],
        _ => false,
    }
}

impl PTCPBody {
    fn parse_all(data: &[u8]) -> Result<Vec<Self>, String> {
        if data.len() < 4 {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        let mut offset = 0usize;

        while offset < data.len() {
            let slice = &data[offset..];
            if slice.len() < 4 {
                if slice.iter().all(|&b| b == 0) {
                    break;
                }
                if records.is_empty() {
                    records.push(Self::Command(slice.to_vec()));
                }
                break;
            }

            match slice[0] {
                0x00 => {
                    records.push(Self::Sync);
                    break;
                }
                0x10 => {
                    let payload = PTCPPayload::parse(slice)?;
                    let consumed = 12 + payload.data.len();
                    records.push(Self::Payload(payload));
                    offset += consumed;
                }
                0x11 if slice.len() >= 16 => {
                    let len_field = u16::from_be_bytes([slice[2], slice[3]]) as usize;
                    let rec_len = if len_field > 0 && 12 + len_field <= slice.len() {
                        12 + len_field
                    } else {
                        20.min(slice.len())
                    };
                    let realm = u32::from_be_bytes([slice[4], slice[5], slice[6], slice[7]]);
                    let port = u32::from_be_bytes([slice[12], slice[13], slice[14], slice[15]]);
                    records.push(Self::Bind(realm, port));
                    offset += rec_len;
                }
                0x12 if slice.len() >= 12 => {
                    let realm = u32::from_be_bytes([slice[4], slice[5], slice[6], slice[7]]);
                    let len_field = u16::from_be_bytes([slice[2], slice[3]]) as usize;
                    let rec_len = if len_field > 0 && 12 + len_field <= slice.len() {
                        12 + len_field
                    } else if slice.len() >= 16
                        && slice[12..16].iter().all(|b| b.is_ascii_graphic())
                    {
                        let mut k = 16usize;
                        while k < slice.len() && slice[k] == 0 {
                            k += 1;
                        }
                        if k == slice.len() || looks_like_ptcp_subrecord(&slice[k..]) {
                            k
                        } else if looks_like_ptcp_subrecord(&slice[16..]) {
                            16
                        } else {
                            slice.len()
                        }
                    } else {
                        slice.len()
                    };
                    let status = String::from_utf8_lossy(&slice[12..rec_len])
                        .trim_matches('\0')
                        .trim()
                        .to_string();
                    records.push(Self::Status(realm, status));
                    offset += rec_len;
                }
                0x13 if slice.len() >= 12 => {
                    let len_field = u16::from_be_bytes([slice[2], slice[3]]) as usize;
                    let rec_len = if len_field > 0 && 12 + len_field <= slice.len() {
                        12 + len_field
                    } else {
                        12
                    };
                    records.push(Self::Heartbeat);
                    offset += rec_len;
                }
                _ => {
                    if records.is_empty() {
                        records.push(Self::Command(slice.to_vec()));
                    }
                    break;
                }
            }
        }

        Ok(records)
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

    fn hash_into<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::Empty => 0u8.hash(state),
            Self::Sync => 1u8.hash(state),
            Self::Heartbeat => 2u8.hash(state),
            Self::Command(d) => {
                3u8.hash(state);
                d.hash(state);
            }
            Self::Bind(r, p) => {
                4u8.hash(state);
                r.hash(state);
                p.hash(state);
            }
            Self::Status(r, s) => {
                5u8.hash(state);
                r.hash(state);
                s.hash(state);
            }
            Self::Payload(p) => {
                6u8.hash(state);
                p.realm.hash(state);
                p.data.hash(state);
            }
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
    extra_bodies: Vec<PTCPBody>,
    wire_body_len: usize,
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
        let wire_body_len = raw.len() - 24;
        let mut records = PTCPBody::parse_all(&raw[24..])?;
        let (body, extra_bodies) = if records.is_empty() {
            (PTCPBody::Empty, Vec::new())
        } else {
            let first = records.remove(0);
            (first, records)
        };

        Ok(Self {
            sent,
            recv,
            pid,
            lmid,
            rmid,
            body,
            extra_bodies,
            wire_body_len,
            duplicate: false,
        })
    }

    fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24 + self.wire_body_len);
        out.extend_from_slice(b"PTCP");
        out.extend_from_slice(&self.sent.to_be_bytes());
        out.extend_from_slice(&self.recv.to_be_bytes());
        out.extend_from_slice(&self.pid.to_be_bytes());
        out.extend_from_slice(&self.lmid.to_be_bytes());
        out.extend_from_slice(&self.rmid.to_be_bytes());
        out.extend_from_slice(&self.body.serialize());
        for extra in &self.extra_bodies {
            out.extend_from_slice(&extra.serialize());
        }
        out
    }

    fn bodies(&self) -> impl Iterator<Item = &PTCPBody> {
        std::iter::once(&self.body).chain(self.extra_bodies.iter())
    }

    fn into_bodies(self) -> Vec<PTCPBody> {
        let mut out = Vec::with_capacity(1 + self.extra_bodies.len());
        out.push(self.body);
        out.extend(self.extra_bodies);
        out
    }

    fn identity(&self, hash_builder: &RandomState) -> PacketIdentity {
        let mut hasher = hash_builder.build_hasher();
        self.body.hash_into(&mut hasher);
        for extra in &self.extra_bodies {
            extra.hash_into(&mut hasher);
        }
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
    hash_builder: RandomState,
    recent_packets: HashSet<PacketIdentity>,
    recent_packet_order: VecDeque<PacketIdentity>,
    reorder_buf: HashMap<u32, PTCPPacket>,
    gap_since: Option<time::Instant>,
    last_dup_ack: Option<time::Instant>,
    last_retransmit: Option<time::Instant>,
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
            hash_builder: RandomState::new(),
            recent_packets: HashSet::new(),
            recent_packet_order: VecDeque::new(),
            reorder_buf: HashMap::new(),
            gap_since: None,
            last_dup_ack: None,
            last_retransmit: None,
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
        let wire_body_len = body.len();
        let body_len = wire_body_len as u32;

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
            extra_bodies: Vec::new(),
            wire_body_len,
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

    fn retransmit_from_rate_limited(&mut self, missing_sent: u32) -> Vec<Vec<u8>> {
        if let Some(last) = self.last_retransmit {
            if last.elapsed() < RETRANSMIT_COOLDOWN {
                return Vec::new();
            }
        }
        self.last_retransmit = Some(time::Instant::now());
        self.sent_history
            .iter()
            .filter(|&&(pkt_sent, _, _)| {
                pkt_sent == missing_sent || sequence_is_after(pkt_sent, missing_sent)
            })
            .take(MAX_RETRANSMIT_BURST)
            .map(|(_, _, raw)| raw.clone())
            .collect()
    }

    fn recv_sync(&mut self, mut packet: PTCPPacket) -> PTCPPacket {
        let identity = packet.identity(&self.hash_builder);
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
        let remote_end = cmp::max(packet.sent.wrapping_add(packet.wire_body_len as u32), 4);
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
            let retrans = self.retransmit_from_rate_limited(packet.recv);
            return (Vec::new(), None, retrans);
        }

        if (matches!(&packet.body, PTCPBody::Empty) && packet.extra_bodies.is_empty())
            || packet.wire_body_len == 0
        {
            self.rmid = packet.lmid;
            return (Vec::new(), None, Vec::new());
        }

        let body_len = packet.wire_body_len as u32;
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
                self.recv = self.recv.wrapping_add(next_pkt.wire_body_len as u32);
                self.rmid = next_pkt.lmid;
                ready.push(next_pkt);
            }

            let cur_recv = self.recv;
            self.reorder_buf.retain(|&s, _| sequence_is_after(s, cur_recv));

            if self.reorder_buf.is_empty() {
                self.gap_since = None;
                self.last_dup_ack = None;
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

        // Drop out-of-window packets > 512 KiB ahead so forged sequence jumps cannot desync the session
        if packet.sent.wrapping_sub(self.recv) > MAX_REORDER_WINDOW_BYTES {
            return (Vec::new(), None, Vec::new());
        }

        if self.reorder_buf.len() < REORDER_BUF_MAX_PACKETS {
            self.reorder_buf.entry(packet.sent).or_insert(packet);
        }
        if self.gap_since.is_none() {
            self.gap_since = Some(time::Instant::now());
        }

        // NEVER skip unreceived sequence gaps in framed RTSP/HTTP TCP streams!
        // Instead emit a rate-limited duplicate ACK carrying current `self.recv` so the NVR
        // fast-retransmits the missing packet.
        let emit_dup_ack = self
            .last_dup_ack
            .map(|t| t.elapsed() >= DUP_ACK_COOLDOWN)
            .unwrap_or(true);
        let dup_ack = if emit_dup_ack {
            self.last_dup_ack = Some(time::Instant::now());
            Some(self.send(PTCPBody::Empty))
        } else {
            None
        };

        (Vec::new(), dup_ack, Vec::new())
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
            let _ = writer.shutdown().await;
            break;
        };
        let write_res = tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut shutdown_rx) => return,
            res = time::timeout(Duration::from_secs(4), writer.write_all(&data)) => res,
        };
        match write_res {
            Ok(Ok(())) => {}
            Ok(Err(_)) | Err(_) => {
                let removed = channels
                    .lock()
                    .ok()
                    .and_then(|mut map| map.remove(&realm_id))
                    .is_some();
                if removed {
                    let _ = time::timeout(
                        Duration::from_millis(500),
                        dh_tx.send(PTCPEvent::Disconnect(realm_id)),
                    )
                    .await;
                }
                let _ = writer.shutdown().await;
                break;
            }
        }
    }
}

async fn process_reader(
    mut reader: tokio::net::tcp::OwnedReadHalf,
    realm_id: u32,
    channels: Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>>,
    dh_tx: mpsc::Sender<PTCPEvent>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    // Keep PTCPPayload UDP datagrams at <= 1236 bytes so they never IP-fragment across 1400-1500 MTU links
    let mut buf = [0u8; 1200];
    loop {
        let read_res = tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut shutdown_rx) => break,
            res = reader.read(&mut buf) => res,
        };
        let n = match read_res {
            Ok(0) | Err(_) => break,
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

    // Remove `realm_id` from `channels` immediately so `process_writer` sees `rx.recv() == None`
    // after draining queued bytes and exits without hanging `w_task.await` or leaking permits!
    let removed = channels
        .lock()
        .ok()
        .and_then(|mut map| map.remove(&realm_id))
        .is_some();
    if removed && !*shutdown_rx.borrow() {
        let _ = time::timeout(
            Duration::from_millis(500),
            dh_tx.send(PTCPEvent::Disconnect(realm_id)),
        )
        .await;
    }
}

async fn dh_writer(
    session: Arc<Mutex<PTCPSession>>,
    socket: Arc<UdpSocket>,
    mut dh_rx: mpsc::Receiver<PTCPEvent>,
) {
    while let Some(ev) = dh_rx.recv().await {
        let is_data_or_bind = matches!(ev, PTCPEvent::Connect(_, _) | PTCPEvent::Data(_, _));
        let pkt = {
            let Ok(mut sess) = session.lock() else {
                continue;
            };
            match ev {
                PTCPEvent::Heartbeat => sess.send(PTCPBody::Heartbeat),
                PTCPEvent::Connect(realm, remote_port) => {
                    sess.send(PTCPBody::Bind(realm, remote_port as u32))
                }
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

async fn process_incoming_ptcp_packet(
    raw_pkt: PTCPPacket,
    session: &Mutex<PTCPSession>,
    socket: Option<&UdpSocket>,
    channels: &Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>,
    conn_channels: &Mutex<HashMap<u32, oneshot::Sender<bool>>>,
    dh_tx: &mpsc::Sender<PTCPEvent>,
) {
    let (in_seq_window, not_behind_recv) = {
        let Ok(sess) = session.lock() else {
            return;
        };
        let ahead = raw_pkt.sent.wrapping_sub(sess.recv);
        let behind = sess.recv.wrapping_sub(raw_pkt.sent);
        (
            ahead <= MAX_REORDER_WINDOW_BYTES || behind <= 65_536,
            ahead <= MAX_REORDER_WINDOW_BYTES,
        )
    };

    // Pre-order check ONLY for pending Bind handshakes in `conn_channels`:
    // - Deliver CONN when `raw_pkt.sent` is not behind `sess.recv` (or if `conn_channels` still awaits it)
    // - Deliver DISC ONLY if `conn_channels` still contains `realm` (pre-CONN bind rejection).
    // NEVER remove established realms from `channels` before `recv_ordered`!
    if in_seq_window {
        for body in raw_pkt.bodies() {
            if let PTCPBody::Status(realm, ref status) = *body {
                let st = status.trim_matches('\0').trim();
                if st == "CONN" {
                    if let Ok(mut map) = conn_channels.lock() {
                        if not_behind_recv || map.contains_key(&realm) {
                            if let Some(connected) = map.remove(&realm) {
                                let _ = connected.send(true);
                            }
                        }
                    }
                } else if st == "DISC" {
                    if let Ok(mut map) = conn_channels.lock() {
                        if let Some(waiter) = map.remove(&realm) {
                            let _ = waiter.send(false);
                        }
                    }
                }
            }
        }
    }

    let (ready_packets, ack_opt, retransmits) = {
        let Ok(mut sess) = session.lock() else {
            return;
        };
        sess.recv_ordered(raw_pkt)
    };

    if let Some(sock) = socket {
        for raw in retransmits {
            let _ = sock.send(&raw).await;
        }
        if let Some(ack_pkt) = ack_opt {
            let _ = ptcp_request(sock, ack_pkt).await;
        }
    }

    // Process Payload and Status(DISC) strictly in sequence order so trailing HTTP/DHAV bytes
    // are always delivered to `tx` BEFORE `DISC` closes `channels`!
    for packet in ready_packets {
        for body in packet.into_bodies() {
            match body {
                PTCPBody::Payload(p) => {
                    let tx = channels.lock().ok().and_then(|map| map.get(&p.realm).cloned());
                    if let Some(tx) = tx {
                        let send_ok = matches!(
                            time::timeout(Duration::from_secs(2), tx.send(p.data)).await,
                            Ok(Ok(()))
                        );
                        if !send_ok {
                            let removed = channels
                                .lock()
                                .ok()
                                .and_then(|mut map| map.remove(&p.realm))
                                .is_some();
                            if removed {
                                let _ = time::timeout(
                                    Duration::from_millis(500),
                                    dh_tx.send(PTCPEvent::Disconnect(p.realm)),
                                )
                                .await;
                            }
                        }
                    }
                }
                PTCPBody::Status(realm, status) => {
                    let st = status.trim_matches('\0').trim();
                    if st == "CONN" {
                        if let Ok(mut map) = conn_channels.lock() {
                            if let Some(connected) = map.remove(&realm) {
                                let _ = connected.send(true);
                            }
                        }
                    } else if st == "DISC" {
                        if let Ok(mut map) = conn_channels.lock() {
                            if let Some(waiter) = map.remove(&realm) {
                                let _ = waiter.send(false);
                            }
                        }
                        if let Ok(mut map) = channels.lock() {
                            map.remove(&realm);
                        }
                    }
                }
                _ => {}
            }
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

        process_incoming_ptcp_packet(
            raw_pkt,
            &session,
            Some(&socket),
            &channels,
            &conn_channels,
            &dh_tx,
        )
        .await;
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

        let truncated = &raw[..raw.len() - 3];
        assert!(PTCPPayload::parse(truncated).is_err());
    }

    #[test]
    fn test_ptcp_session_prunes_stale_reorder_keys_and_caps_window() {
        let mut sess = PTCPSession::new();
        sess.recv = 100;

        let make_payload_pkt = |sent: u32, payload_bytes: usize| {
            let body = PTCPBody::Payload(PTCPPayload {
                realm: 7,
                data: vec![0xAA; payload_bytes],
            });
            let wire_body_len = body.len();
            PTCPPacket {
                sent,
                recv: 0,
                pid: 1,
                lmid: 1,
                rmid: 0,
                body,
                extra_bodies: Vec::new(),
                wire_body_len,
                duplicate: false,
            }
        };

        // Out-of-window forged packet (+10 MB) MUST be dropped without poisoning reorder_buf
        let (forged_ready, _, _) = sess.recv_ordered(make_payload_pkt(10_000_100, 8));
        assert!(forged_ready.is_empty());
        assert!(sess.reorder_buf.is_empty());

        // Valid out-of-order packet starting at 120 (len = 8 + 12 = 20, end = 140)
        let (ready1, dup_ack1, _) = sess.recv_ordered(make_payload_pkt(120, 8));
        assert!(ready1.is_empty());
        assert!(dup_ack1.is_some());
        assert_eq!(sess.reorder_buf.len(), 1);

        // Coalesced retransmission starting at 100 covering up to 160 (len = 48 + 12 = 60)
        let (ready2, _, _) = sess.recv_ordered(make_payload_pkt(100, 48));
        assert_eq!(ready2.len(), 1);
        assert_eq!(sess.recv, 160);
        assert!(sess.reorder_buf.is_empty());
        assert!(sess.gap_since.is_none());
    }

    #[tokio::test]
    async fn test_crit04_recv_ordered_never_skips_missing_gap_and_emits_dup_ack() {
        let mut sess = PTCPSession::new();
        sess.recv = 100;

        let make_pkt = |sent: u32, byte: u8| {
            let body = PTCPBody::Payload(PTCPPayload {
                realm: 9,
                data: vec![byte; 8], // wire len = 20
            });
            let wire_body_len = body.len();
            PTCPPacket {
                sent,
                recv: 0,
                pid: 1,
                lmid: 1,
                rmid: 0,
                body,
                extra_bodies: Vec::new(),
                wire_body_len,
                duplicate: false,
            }
        };

        // Buffer 40 out-of-order packets after gap [100..120) and wait > 30ms:
        // MUST NEVER jump over missing sequence 100!
        for i in 0..40u32 {
            let seq = 120 + i * 20;
            let (ready, ack_opt, _) = sess.recv_ordered(make_pkt(seq, (i as u8).wrapping_add(1)));
            assert!(ready.is_empty());
            if i == 0 {
                let ack = ack_opt.expect("First out-of-order packet must emit duplicate ACK");
                assert_eq!(ack.recv, 100);
            }
        }

        time::sleep(Duration::from_millis(30)).await;
        let (ready_late, ack_after_wait, _) = sess.recv_ordered(make_pkt(120 + 40 * 20, 0xFE));
        assert!(
            ready_late.is_empty(),
            "recv_ordered must never skip unreceived bytes even after >20ms and >32 buffered packets"
        );
        assert_eq!(sess.recv, 100);
        assert_eq!(ack_after_wait.expect("Rate-limited dup ACK after >15ms").recv, 100);

        // Now deliver missing packet at seq 100 -> all 42 contiguous packets drain in exact order!
        let (drained, _, _) = sess.recv_ordered(make_pkt(100, 0x00));
        assert_eq!(drained.len(), 42);
        assert_eq!(sess.recv, 100 + 42 * 20);
        assert!(sess.reorder_buf.is_empty());
    }

    #[tokio::test]
    async fn test_crit02_ordered_disc_and_high01_coalesced_payload_plus_disc() {
        let session = Mutex::new({
            let mut s = PTCPSession::new();
            s.recv = 1000;
            s
        });
        let channels = Mutex::new(HashMap::new());
        let conn_channels = Mutex::new(HashMap::new());
        let (dh_tx, _dh_rx) = mpsc::channel(16);

        let realm = 55u32;
        let (tx, mut rx) = mpsc::channel(16);
        channels.lock().unwrap().insert(realm, tx);

        // Build Packet 1 (seq 1000): Payload("HTTP/1.1 200 OK\r\n\r\nPART1") -> wire body 12 + 24 = 36 (ends at 1036)
        let p1_bytes = [
            b"PTCP".as_slice(),
            &1000u32.to_be_bytes(),
            &0u32.to_be_bytes(),
            &1u32.to_be_bytes(),
            &1u32.to_be_bytes(),
            &0u32.to_be_bytes(),
            &PTCPBody::Payload(PTCPPayload {
                realm,
                data: b"HTTP/1.1 200 OK\r\n\r\nPART1".to_vec(),
            })
            .serialize(),
        ]
        .concat();

        // Build Packet 2 (seq 1036): COALESCED Payload("PART2") + NUL-padded Status("DISC\0\0\0\0") in a SINGLE UDP datagram!
        let mut coalesced_body = PTCPPayload {
            realm,
            data: b"PART2".to_vec(),
        }
        .serialize(); // 17 bytes
        coalesced_body.extend_from_slice(&[0x12, 0x00, 0x00, 0x00]);
        coalesced_body.extend_from_slice(&realm.to_be_bytes());
        coalesced_body.extend_from_slice(&0u32.to_be_bytes());
        coalesced_body.extend_from_slice(b"DISC\0\0\0\0"); // 20 bytes -> total wire body 37 bytes (ends at 1073)

        let p2_bytes = [
            b"PTCP".as_slice(),
            &1036u32.to_be_bytes(),
            &0u32.to_be_bytes(),
            &2u32.to_be_bytes(),
            &2u32.to_be_bytes(),
            &0u32.to_be_bytes(),
            &coalesced_body,
        ]
        .concat();

        let pkt1 = PTCPPacket::parse(&p1_bytes).unwrap();
        let pkt2 = PTCPPacket::parse(&p2_bytes).unwrap();
        assert_eq!(pkt2.wire_body_len, 37);

        // Deliver Packet 2 (containing DISC!) OUT OF ORDER before Packet 1!
        process_incoming_ptcp_packet(pkt2, &session, None, &channels, &conn_channels, &dh_tx).await;
        // Established realm MUST still be in `channels` while Packet 1 is missing!
        assert!(channels.lock().unwrap().contains_key(&realm));

        // Now deliver Packet 1 -> both payloads arrive in order, and THEN DISC closes `channels`!
        process_incoming_ptcp_packet(pkt1, &session, None, &channels, &conn_channels, &dh_tx).await;
        assert!(!channels.lock().unwrap().contains_key(&realm));
        assert_eq!(session.lock().unwrap().recv, 1073);

        assert_eq!(rx.recv().await.unwrap(), b"HTTP/1.1 200 OK\r\n\r\nPART1");
        assert_eq!(rx.recv().await.unwrap(), b"PART2");
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn test_crit03_local_client_close_exits_writer_and_frees_channel_immediately() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (client_res, accept_res) = tokio::join!(tokio::net::TcpStream::connect(addr), listener.accept());
        let local_client = client_res.unwrap();
        let (server_side, _) = accept_res.unwrap();

        let realm_id = 777u32;
        let (tx, rx) = mpsc::channel::<Vec<u8>>(16);
        let channels = Arc::new(Mutex::new(HashMap::new()));
        channels.lock().unwrap().insert(realm_id, tx);

        let (dh_tx, mut dh_rx) = mpsc::channel::<PTCPEvent>(16);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        let (reader, writer) = server_side.into_split();
        let channels_w = Arc::clone(&channels);
        let dh_tx_w = dh_tx.clone();
        let shutdown_w = shutdown_rx.clone();

        let w_task = tokio::spawn(async move {
            process_writer(writer, rx, realm_id, channels_w, dh_tx_w, shutdown_w).await;
        });

        // Close local client first (just like early-stopped HTTP/RTSP client)
        drop(local_client);

        process_reader(reader, realm_id, Arc::clone(&channels), dh_tx, shutdown_rx).await;

        // `w_task` MUST exit immediately without hanging!
        time::timeout(Duration::from_millis(300), w_task)
            .await
            .expect("process_writer hung after local client closed!")
            .unwrap();
        assert!(!channels.lock().unwrap().contains_key(&realm_id));

        let ev = time::timeout(Duration::from_millis(200), dh_rx.recv())
            .await
            .unwrap()
            .expect("Expected Disconnect event");
        assert!(matches!(ev, PTCPEvent::Disconnect(777)));
    }

    #[test]
    fn test_public_ip_filter_blocks_ssrf_targets() {
        assert!(!is_public_routable_ip("127.0.0.1".parse().unwrap()));
        assert!(!is_public_routable_ip("192.168.1.108".parse().unwrap()));
        assert!(!is_public_routable_ip("10.0.0.1".parse().unwrap()));
        assert!(!is_public_routable_ip("172.16.5.4".parse().unwrap()));
        assert!(!is_public_routable_ip("169.254.169.254".parse().unwrap()));
        assert!(!is_public_routable_ip("100.64.1.1".parse().unwrap()));
        assert!(!is_public_routable_ip("::1".parse().unwrap()));
        assert!(!is_public_routable_ip("fe80::1".parse().unwrap()));
        assert!(!is_public_routable_ip("::ffff:127.0.0.1".parse().unwrap()));

        assert!(is_public_routable_ip("159.89.236.116".parse().unwrap()));
        assert!(is_public_routable_ip("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn test_quick_xml_parser_rejects_dtd_billion_laughs() {
        let normal = "<body><US>52.10.20.30:8800</US><Token>abc_123</Token></body>";
        let parsed = DHResponse::parse_body(normal);
        assert_eq!(parsed.get("body/US").map(String::as_str), Some("52.10.20.30:8800"));
        assert_eq!(parsed.get("body/Token").map(String::as_str), Some("abc_123"));

        let dtd_bomb = "<!DOCTYPE lolz [<!ENTITY lol \"lol\">]><body><US>&lol;</US></body>";
        assert!(DHResponse::parse_body(dtd_bomb).is_empty());
    }

    #[test]
    fn test_serial_and_token_validation_blocks_crlf() {
        assert!(validate_p2p_serial("9H0E1234PAZ9876").is_ok());
        assert!(validate_p2p_serial("9H0E\r\nInjected: 1").is_err());
        assert!(validate_p2p_token("tok_123/abc-XYZ").is_ok());
        assert!(validate_p2p_token("tok\r\nHeader: bad").is_err());
    }

    #[test]
    fn test_generation_guard_prevents_stale_clobber() {
        let connected = AtomicBool::new(true);
        let generation = AtomicU64::new(2);
        let status = Mutex::new("Cloud P2P Connected".to_string());

        set_connected_if_current(&connected, &generation, 1, false);
        set_status_if_current(&status, &generation, 1, "Stopped");
        assert!(connected.load(Ordering::Acquire));
        assert_eq!(status.lock().unwrap().as_str(), "Cloud P2P Connected");

        set_connected_if_current(&connected, &generation, 2, false);
        assert!(!connected.load(Ordering::Acquire));
    }

    #[test]
    fn test_ipv6_is_lan_reachable_no_panic_on_raw_literal() {
        assert!(!is_lan_reachable("::1", 1, 20));
        assert!(!is_lan_reachable("[::1]", 1, 20));
    }

    #[test]
    fn test_profile_table_sanity_and_auth_classification() {
        // Every profile must point at a public hostname:8800, use 32-char key material,
        // and only emit signaling verbs / header values that cannot inject CRLF.
        let mut names = std::collections::HashSet::new();
        for p in DEFAULT_PROFILES {
            assert!(names.insert(p.name), "duplicate profile name {}", p.name);
            assert!(p.server.ends_with(":8800"), "{} server", p.name);
            assert!(!p.server.starts_with("127.") && !p.server.starts_with("10."));
            assert_eq!(p.wsse_key.len(), 32, "{} key length", p.name);
            assert!(!p.wsse_user.is_empty());
            for v in [Some(p.verb_get), Some(p.verb_post), p.to_u_type, p.version, p.sversion]
                .into_iter()
                .flatten()
            {
                assert!(
                    v.bytes().all(|b| b.is_ascii_graphic() && b != b'\r' && b != b'\n'),
                    "{} has unsafe header value",
                    p.name
                );
            }
        }
        // First profile must stay the proven Easy4IP/SmartPSS path so existing Dahua users are unaffected.
        assert_eq!(DEFAULT_PROFILES[0].name, "smartpss");

        // Real password rejection halts the search, a profile rejection does not.
        assert!(is_p2p_auth_rejection("AUTH_401: P2P auth error 401 (Unauthorized)"));
        let neutral = "kvmsprolite: /online/p2psrv failed: profile rejected: Signaling HTTP 4xx (Unauthorized)";
        assert!(!is_p2p_auth_rejection(neutral));
    }

    #[test]
    fn test_wsse_digest_vectors() {
        // SmartPSS golden vector
        let d_sp = compute_wsse_digest("42", "2026-09-06T12:00:00Z", PROFILE_SMARTPSS.wsse_user, PROFILE_SMARTPSS.wsse_key);
        assert_eq!(d_sp, "2+go9D1pKW5ykn3ePUMZkhMmITo=");

        // DMSS golden vector
        let d_dmss = compute_wsse_digest("42", "2026-09-06T12:00:00+03:00", PROFILE_DMSS.wsse_user, PROFILE_DMSS.wsse_key);
        assert_eq!(d_dmss, "VSIOT9Z5Ht/rmuQ677AZ5CzIbOk=");

        // KVMS Pro Lite live capture vector
        let d_kvms = compute_wsse_digest("-1024243796", "2026-11-18T22:17:41+05:30", PROFILE_KVMS.wsse_user, PROFILE_KVMS.wsse_key);
        assert_eq!(d_kvms, "uo1krIyCADVZPr5ncY/qu1vMFjw=");

        // KVMS Pro Lite CTC live capture vector
        let d_ctc = compute_wsse_digest("-328519825", "2026-11-18T22:17:45+05:30", PROFILE_KVMS_CTC.wsse_user, PROFILE_KVMS_CTC.wsse_key);
        assert_eq!(d_ctc, "CRYtxd/C3U0Vy81a+KsbrALknZ0=");
    }

    #[test]
    fn test_verify_same_uid_peer_accepts_local_tcp_socket() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();
        rt.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let client_fut = tokio::net::TcpStream::connect(addr);
            let accept_fut = listener.accept();
            let (client_res, accept_res) = tokio::join!(client_fut, accept_fut);
            let _client = client_res.unwrap();
            let (server_stream, peer_addr) = accept_res.unwrap();
            assert!(verify_same_uid_peer(&server_stream, peer_addr));
            assert!(is_tcp_established(&server_stream));
        });
    }
}
