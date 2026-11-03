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
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
    sync::{mpsc, oneshot, Semaphore},
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

pub fn is_lan_reachable(host: &str, port: u16, timeout_ms: u64) -> bool {
    let addr_str = format!("{}:{}", host.trim(), port);
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

pub struct P2PTunnelManager {
    active_key: String,
    local_port: u16,
    stop_flag: Option<Arc<AtomicBool>>,
    connected: Arc<AtomicBool>,
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
            stop_flag: None,
            connected: Arc::new(AtomicBool::new(false)),
            status: Arc::new(Mutex::new("Idle".to_string())),
        }
    }

    pub fn is_connected_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.connected)
    }

    pub fn status_text(&self) -> String {
        self.status
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| "Cloud P2P".to_string())
    }

    pub fn stop(&mut self) {
        if let Some(flag) = self.stop_flag.take() {
            flag.store(true, Ordering::SeqCst);
        }
        self.connected.store(false, Ordering::Release);
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
        if self.active_key == key && self.stop_flag.is_some() {
            return self.local_port;
        }

        self.stop();
        self.active_key = key;
        self.connected.store(false, Ordering::Release);

        // Bind a persistent local port (prefer 15540, fallback to OS-assigned)
        let std_listener = std::net::TcpListener::bind(("127.0.0.1", DEFAULT_LOCAL_PORT))
            .or_else(|_| std::net::TcpListener::bind(("127.0.0.1", 0)))
            .expect("Failed to bind local P2P tunnel listener");
        let _ = std_listener.set_nonblocking(true);
        self.local_port = std_listener
            .local_addr()
            .map(|a| a.port())
            .unwrap_or(DEFAULT_LOCAL_PORT);

        let stop_flag = Arc::new(AtomicBool::new(false));
        self.stop_flag = Some(stop_flag.clone());

        let connected_flag = self.connected.clone();
        let status = self.status.clone();
        if let Ok(mut s) = status.lock() {
            *s = "Cloud P2P Connecting...".to_string();
        }

        let user_owned = username.to_string();
        let pass_owned = password.to_string();

        std::thread::Builder::new()
            .name("dahua-p2p-tunnel".to_string())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("Failed to build P2P Tokio runtime");

                rt.block_on(async move {
                    let Ok(listener) = TcpListener::from_std(std_listener) else {
                        return;
                    };
                    let listener = Arc::new(listener);

                    while !stop_flag.load(Ordering::SeqCst) {
                        connected_flag.store(false, Ordering::Release);
                        if let Ok(mut s) = status.lock() {
                            *s = "Cloud P2P Handshake...".to_string();
                        }

                        match run_tunnel_session(
                            listener.clone(),
                            &serial_clean,
                            &user_owned,
                            &pass_owned,
                            remote_port,
                            stop_flag.clone(),
                            connected_flag.clone(),
                            status.clone(),
                        )
                        .await
                        {
                            Ok(()) => break,
                            Err(e) => {
                                connected_flag.store(false, Ordering::Release);
                                if stop_flag.load(Ordering::SeqCst) {
                                    break;
                                }
                                if let Ok(mut s) = status.lock() {
                                    *s = format!("Cloud P2P Retry ({})", e);
                                }
                                time::sleep(Duration::from_secs(2)).await;
                            }
                        }
                    }
                    connected_flag.store(false, Ordering::Release);
                });
            })
            .ok();

        self.local_port
    }
}

impl Drop for P2PTunnelManager {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn run_tunnel_session(
    listener: Arc<TcpListener>,
    serial: &str,
    username: &str,
    password: &str,
    remote_port: u16,
    stop_flag: Arc<AtomicBool>,
    connected_flag: Arc<AtomicBool>,
    status: Arc<Mutex<String>>,
) -> Result<(), String> {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| e.to_string())?;

    let (socket, session) = time::timeout(
        Duration::from_secs(12),
        p2p_handshake(socket, serial, username, password),
    )
    .await
    .map_err(|_| "Handshake timeout".to_string())??;

    // Drain any stale CLOSE-WAIT sockets queued before the handshake finished
    while let Ok(Ok((stale_client, _))) =
        time::timeout(Duration::from_millis(5), listener.accept()).await
    {
        if is_tcp_established(&stale_client) {
            drop(stale_client);
        }
    }

    connected_flag.store(true, Ordering::Release);
    if let Ok(mut s) = status.lock() {
        *s = "Cloud P2P Connected".to_string();
    }

    let (dh_tx, dh_rx) = mpsc::channel::<PTCPEvent>(256);
    let session = Arc::new(Mutex::new(session));

    let channels = Arc::new(Mutex::new(HashMap::<u32, mpsc::Sender<Vec<u8>>>::new()));
    let conn_channels = Arc::new(Mutex::new(HashMap::<u32, oneshot::Sender<bool>>::new()));

    let reader_sock = Arc::new(socket);
    let writer_sock = reader_sock.clone();

    let session_reader = session.clone();
    let channels_reader = channels.clone();
    let conn_channels_reader = conn_channels.clone();

    let session_alive = Arc::new(AtomicBool::new(true));
    let alive_hb = session_alive.clone();
    let alive_reader = session_alive.clone();

    let hb_tx = dh_tx.clone();
    let stop_hb = stop_flag.clone();
    tokio::spawn(async move {
        while !stop_hb.load(Ordering::SeqCst) && alive_hb.load(Ordering::SeqCst) {
            time::sleep(Duration::from_secs(5)).await;
            if hb_tx.send(PTCPEvent::Heartbeat).await.is_err() {
                break;
            }
        }
    });

    let writer_handle = tokio::spawn(async move {
        dh_writer(session, writer_sock, dh_rx, remote_port as u32).await;
    });

    let reader_handle = tokio::spawn(async move {
        let res = dh_reader(
            session_reader,
            reader_sock,
            channels_reader,
            conn_channels_reader,
        )
        .await;
        alive_reader.store(false, Ordering::SeqCst);
        res
    });

    // Serialize PTCP Bind handshakes 1 at a time so multi-camera startup
    // never interleaves Bind sequence numbers over the UDP relay
    let bind_semaphore = Arc::new(Semaphore::new(1));
    let consecutive_bind_failures = Arc::new(AtomicU32::new(0));

    loop {
        if stop_flag.load(Ordering::SeqCst) {
            connected_flag.store(false, Ordering::Release);
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
        if !session_alive.load(Ordering::SeqCst)
            || consecutive_bind_failures.load(Ordering::Acquire) >= 12
        {
            connected_flag.store(false, Ordering::Release);
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

        let accept_res = time::timeout(Duration::from_millis(300), listener.accept()).await;
        let Ok(Ok((client, _addr))) = accept_res else {
            continue;
        };

        // Immediately drop dead sockets from timed-out ffmpeg processes in 0ms
        if !is_tcp_established(&client) {
            continue;
        }

        let dh_tx_client = dh_tx.clone();
        let channels_client = channels.clone();
        let conn_channels_client = conn_channels.clone();
        let sem_clone = bind_semaphore.clone();
        let bind_failures = consecutive_bind_failures.clone();
        let alive_flag = session_alive.clone();

        tokio::spawn(async move {
            let Ok(_permit) = sem_clone.acquire_owned().await else {
                return;
            };
            if !alive_flag.load(Ordering::SeqCst) || !is_tcp_established(&client) {
                return;
            }

            let (tx, rx) = mpsc::channel::<Vec<u8>>(256);
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
                if !is_tcp_established(&client) {
                    break;
                }
                if dh_tx_client
                    .send(PTCPEvent::Connect(realm_id))
                    .await
                    .is_err()
                {
                    break;
                }
                match time::timeout(Duration::from_millis(2000), &mut conn_rx).await {
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
                let _ = dh_tx_client.send(PTCPEvent::Disconnect(realm_id)).await;
                if is_tcp_established(&client) {
                    bind_failures.fetch_add(1, Ordering::AcqRel);
                }
                return;
            }

            bind_failures.store(0, Ordering::Release);
            drop(_permit);

            let (reader, writer) = client.into_split();
            tokio::spawn(async move {
                process_reader(reader, realm_id, dh_tx_client).await;
            });
            tokio::spawn(async move {
                process_writer(writer, rx).await;
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
    let digest = base64::engine::general_purpose::STANDARD.encode(&hash_digest);

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

#[derive(Clone)]
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
        let realm = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let end = cmp::min(12 + length, data.len());
        let payload_data = data[12..end].to_vec();
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

            if self.reorder_buf.is_empty() {
                self.gap_since = None;
            } else {
                self.gap_since = Some(time::Instant::now());
            }

            let ack = self.send(PTCPBody::Empty);
            return (ready, Some(ack), Vec::new());
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
    let mut buf = [0u8; 8192];
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
) {
    while let Some(data) = rx.recv().await {
        if writer.write_all(&data).await.is_err() {
            break;
        }
    }
}

async fn process_reader(
    mut reader: tokio::net::tcp::OwnedReadHalf,
    realm_id: u32,
    dh_tx: mpsc::Sender<PTCPEvent>,
) {
    let mut buf = [0u8; 4096];
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) | Err(_) => {
                let _ = dh_tx.send(PTCPEvent::Disconnect(realm_id)).await;
                break;
            }
            Ok(n) => n,
        };
        if dh_tx
            .send(PTCPEvent::Data(realm_id, buf[0..n].to_vec()))
            .await
            .is_err()
        {
            break;
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
                    if tx.send(p.data).await.is_err() {
                        if let Ok(mut map) = channels.lock() {
                            map.remove(&p.realm);
                        }
                    }
                }
            }
        }
    }
}

