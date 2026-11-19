use crate::config::{
    format_host_for_url, validate_custom_rtsp_url, validate_nvr_host, NvrEnvConfig,
};
use std::collections::HashMap;
use std::io;
use std::net::TcpListener as StdTcpListener;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Mutex as AsyncMutex, Semaphore};
use tokio::time;
use zeroize::Zeroizing;

const MAX_CONCURRENT_SESSIONS: usize = 48;
const MAX_PENDING_GRANTS: usize = 256;
const TOKEN_TTL: Duration = Duration::from_secs(20);
const FIRST_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const CANARY_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_RTSP_HEADER_BYTES: usize = 32 * 1024;
const MAX_RTSP_BODY_BYTES: usize = 512 * 1024;
const MAX_IN_FLIGHT_REQUESTS: usize = 16;

/// Reusable RFC 2617 / RFC 7616 HTTP & RTSP Digest authentication primitives.
pub mod digest {
    use base64::{engine::general_purpose::STANDARD as BASE64_STD, Engine as _};
    use md5::{Digest as _, Md5};
    use rand::{rngs::OsRng, RngCore};
    use sha2::Sha256;
    use zeroize::Zeroizing;

    pub const MAX_CHALLENGE_PARAM_CHARS: usize = 256;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum DigestAlgorithm {
        Md5,
        Md5Sess,
        Sha256,
        Sha256Sess,
    }

    impl DigestAlgorithm {
        pub fn header_name(self) -> &'static str {
            match self {
                Self::Md5 => "MD5",
                Self::Md5Sess => "MD5-sess",
                Self::Sha256 => "SHA-256",
                Self::Sha256Sess => "SHA-256-sess",
            }
        }

        pub fn is_sha256_family(self) -> bool {
            matches!(self, Self::Sha256 | Self::Sha256Sess)
        }

        pub fn is_md5_family(self) -> bool {
            matches!(self, Self::Md5 | Self::Md5Sess)
        }

        fn preference_rank(self) -> u8 {
            match self {
                Self::Sha256 => 4,
                Self::Sha256Sess => 3,
                Self::Md5 => 2,
                Self::Md5Sess => 1,
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct DigestChallenge {
        pub realm: String,
        pub nonce: String,
        pub opaque: Option<String>,
        pub qop_auth: bool,
        pub algorithm: DigestAlgorithm,
        pub algorithm_explicit: bool,
        pub stale: bool,
    }

    /// Strips ASCII control characters (`< 0x20` and `0x7f`) and caps length at `max_chars`.
    pub fn sanitize_header_token(raw: &str, max_chars: usize) -> String {
        raw.chars()
            .filter(|&c| {
                let code = c as u32;
                code >= 0x20 && code != 0x7f
            })
            .take(max_chars)
            .collect()
    }

    /// Sanitizes control characters, caps at `max_chars`, and escapes `\` and `"` for quoted-string headers.
    pub fn escape_quoted_header_val(raw: &str, max_chars: usize) -> String {
        let clean = sanitize_header_token(raw, max_chars);
        let mut out = String::with_capacity(clean.len() + 4);
        for ch in clean.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                _ => out.push(ch),
            }
        }
        out
    }

    pub fn hex_lower(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
        out
    }

    pub fn md5_upper_hex(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        let digest = Md5::digest(bytes);
        let mut out = String::with_capacity(32);
        for &b in digest.as_slice() {
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
        out
    }

    /// Generates 16 random bytes (128 bits) from OS CSPRNG formatted as 32 lowercase hex chars.
    pub fn generate_random_hex_16() -> String {
        let mut bytes = [0u8; 16];
        OsRng.fill_bytes(&mut bytes);
        hex_lower(&bytes)
    }

    fn hash_hex(alg: DigestAlgorithm, data: &[u8]) -> String {
        match alg {
            DigestAlgorithm::Md5 | DigestAlgorithm::Md5Sess => {
                let d = Md5::digest(data);
                hex_lower(d.as_slice())
            }
            DigestAlgorithm::Sha256 | DigestAlgorithm::Sha256Sess => {
                let d = Sha256::digest(data);
                hex_lower(d.as_slice())
            }
        }
    }

    /// Parses all `WWW-Authenticate` headers and returns all valid `DigestChallenge`s sorted
    /// with `SHA-256` / `SHA-256-sess` first, then `MD5` / `MD5-sess`.
    pub fn parse_www_authenticate_all(headers: &[(String, String)]) -> Vec<DigestChallenge> {
        let mut out = Vec::new();
        for (name, value) in headers {
            if !name.eq_ignore_ascii_case("www-authenticate") {
                continue;
            }
            let trimmed = value.trim();
            if trimmed.is_empty() {
                continue;
            }
            for (scheme, params_str) in split_auth_challenges(trimmed) {
                if scheme.eq_ignore_ascii_case("digest") {
                    if let Some(ch) = parse_digest_params(&params_str) {
                        let dup = out.iter().any(|existing: &DigestChallenge| {
                            existing.algorithm == ch.algorithm
                                && existing.realm == ch.realm
                                && existing.nonce == ch.nonce
                        });
                        if !dup {
                            out.push(ch);
                        }
                    }
                }
            }
        }
        out.sort_by_key(|a| std::cmp::Reverse(a.algorithm.preference_rank()));
        out
    }

    /// Parses all `WWW-Authenticate` headers and returns `(best_digest_challenge, basic_offered)`.
    pub fn parse_www_authenticate_headers(
        headers: &[(String, String)],
    ) -> (Option<DigestChallenge>, bool) {
        let mut has_basic = false;
        for (name, value) in headers {
            if !name.eq_ignore_ascii_case("www-authenticate") {
                continue;
            }
            let trimmed = value.trim();
            if trimmed.is_empty() {
                continue;
            }
            for (scheme, _) in split_auth_challenges(trimmed) {
                if scheme.eq_ignore_ascii_case("basic") {
                    has_basic = true;
                }
            }
        }
        let best_digest = parse_www_authenticate_all(headers).into_iter().next();
        (best_digest, has_basic)
    }

    fn split_auth_challenges(line: &str) -> Vec<(String, String)> {
        // Scan tokens at top level (outside quotes). A new challenge begins when we see
        // `<Scheme> <rest>` where `<Scheme>` has no `=` after it before whitespace.
        let bytes = line.as_bytes();
        let n = bytes.len();
        let mut challenges: Vec<(usize, usize, String)> = Vec::new(); // (scheme_start, params_start, scheme)
        let mut i = 0;
        let mut in_quotes = false;

        while i < n {
            if in_quotes {
                if bytes[i] == b'\\' && i + 1 < n {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    in_quotes = false;
                }
                i += 1;
                continue;
            }

            if bytes[i] == b'"' {
                in_quotes = true;
                i += 1;
                continue;
            }

            let at_boundary = i == 0 || bytes[i - 1].is_ascii_whitespace() || bytes[i - 1] == b',';
            if at_boundary && bytes[i].is_ascii_alphabetic() {
                let start = i;
                while i < n && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-') {
                    i += 1;
                }
                let word_end = i;
                let mut ws = i;
                while ws < n && (bytes[ws] == b' ' || bytes[ws] == b'\t') {
                    ws += 1;
                }
                // Scheme is followed either by end-of-line (e.g. bare `Basic`) or by non-`=` params
                let is_scheme = if ws == n {
                    word_end > start
                } else {
                    ws > word_end && bytes[ws] != b'=' && bytes[ws] != b','
                };
                if is_scheme {
                    let word = &line[start..word_end];
                    if word.eq_ignore_ascii_case("digest") || word.eq_ignore_ascii_case("basic") {
                        challenges.push((start, ws, word.to_string()));
                        i = ws;
                        continue;
                    }
                }
                continue;
            }
            i += 1;
        }

        let mut out = Vec::with_capacity(challenges.len());
        for idx in 0..challenges.len() {
            let (_, params_start, ref scheme) = challenges[idx];
            let end = if idx + 1 < challenges.len() {
                challenges[idx + 1].0
            } else {
                n
            };
            let params_slice = line[params_start..end]
                .trim()
                .trim_end_matches(',')
                .trim()
                .to_string();
            out.push((scheme.clone(), params_slice));
        }
        out
    }

    fn parse_digest_params(params: &str) -> Option<DigestChallenge> {
        let mut realm: Option<String> = None;
        let mut nonce: Option<String> = None;
        let mut opaque: Option<String> = None;
        let mut qop_auth = false;
        let mut algorithm = DigestAlgorithm::Md5;
        let mut algorithm_explicit = false;
        let mut stale = false;

        let bytes = params.as_bytes();
        let n = bytes.len();
        let mut i = 0;

        while i < n {
            while i < n && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
                i += 1;
            }
            if i >= n {
                break;
            }

            let key_start = i;
            while i < n && bytes[i] != b'=' && bytes[i] != b',' && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let key = params[key_start..i].trim();
            while i < n && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            if i >= n || bytes[i] != b'=' {
                while i < n && bytes[i] != b',' {
                    i += 1;
                }
                continue;
            }
            i += 1; // skip '='
            while i < n && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }

            let val = if i < n && bytes[i] == b'"' {
                i += 1; // skip opening quote
                let mut unescaped = String::new();
                while i < n {
                    let ch_byte = bytes[i];
                    if ch_byte == b'\\' && i + 1 < n {
                        i += 1;
                        if let Some(s) = params.get(i..) {
                            if let Some(ch) = s.chars().next() {
                                unescaped.push(ch);
                                i += ch.len_utf8();
                                continue;
                            }
                        }
                        break;
                    } else if ch_byte == b'"' {
                        i += 1;
                        break;
                    } else if let Some(s) = params.get(i..) {
                        if let Some(ch) = s.chars().next() {
                            unescaped.push(ch);
                            i += ch.len_utf8();
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }
                unescaped
            } else {
                let val_start = i;
                while i < n && bytes[i] != b',' && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                params[val_start..i].to_string()
            };

            match key.to_ascii_lowercase().as_str() {
                "realm" => {
                    realm = Some(sanitize_header_token(&val, MAX_CHALLENGE_PARAM_CHARS));
                }
                "nonce" => {
                    nonce = Some(sanitize_header_token(&val, MAX_CHALLENGE_PARAM_CHARS));
                }
                "opaque" => {
                    opaque = Some(sanitize_header_token(&val, MAX_CHALLENGE_PARAM_CHARS));
                }
                "qop" => {
                    if val
                        .split(',')
                        .any(|tok| tok.trim().eq_ignore_ascii_case("auth"))
                    {
                        qop_auth = true;
                    }
                }
                "stale" => {
                    stale = val.trim().eq_ignore_ascii_case("true");
                }
                "algorithm" => {
                    algorithm_explicit = true;
                    match val.trim().to_ascii_uppercase().as_str() {
                        "MD5" => algorithm = DigestAlgorithm::Md5,
                        "MD5-SESS" => algorithm = DigestAlgorithm::Md5Sess,
                        "SHA-256" | "SHA256" => algorithm = DigestAlgorithm::Sha256,
                        "SHA-256-SESS" | "SHA256-SESS" => algorithm = DigestAlgorithm::Sha256Sess,
                        _ => return None,
                    }
                }
                _ => {}
            }
        }

        let clean_nonce = nonce.filter(|s| !s.is_empty())?;
        let clean_realm = realm.unwrap_or_default();

        Some(DigestChallenge {
            realm: clean_realm,
            nonce: clean_nonce,
            opaque,
            qop_auth,
            algorithm,
            algorithm_explicit,
            stale,
        })
    }

    /// Computes the Digest `response` hash value for testing and header generation.
    pub fn compute_digest_response(
        challenge: &DigestChallenge,
        username: &str,
        password: &str,
        method: &str,
        uri: &str,
        nc: u32,
        cnonce: &str,
    ) -> String {
        let clean_user = sanitize_header_token(username, MAX_CHALLENGE_PARAM_CHARS);
        let clean_method = sanitize_header_token(method, 32);
        let clean_uri = sanitize_header_token(uri, 2048);

        let a1_base = Zeroizing::new(format!("{}:{}:{}", clean_user, challenge.realm, password));
        let ha1_inner = Zeroizing::new(hash_hex(challenge.algorithm, a1_base.as_bytes()));
        let ha1 = match challenge.algorithm {
            DigestAlgorithm::Md5 | DigestAlgorithm::Sha256 => ha1_inner,
            DigestAlgorithm::Md5Sess | DigestAlgorithm::Sha256Sess => {
                let sess_input = Zeroizing::new(format!(
                    "{}:{}:{}",
                    ha1_inner.as_str(),
                    challenge.nonce,
                    cnonce
                ));
                Zeroizing::new(hash_hex(challenge.algorithm, sess_input.as_bytes()))
            }
        };

        let a2 = format!("{}:{}", clean_method, clean_uri);
        let ha2 = hash_hex(challenge.algorithm, a2.as_bytes());

        if challenge.qop_auth {
            let kd = Zeroizing::new(format!(
                "{}:{}:{:08x}:{}:auth:{}",
                ha1.as_str(),
                challenge.nonce,
                nc,
                cnonce,
                ha2
            ));
            hash_hex(challenge.algorithm, kd.as_bytes())
        } else {
            let kd = Zeroizing::new(format!("{}:{}:{}", ha1.as_str(), challenge.nonce, ha2));
            hash_hex(challenge.algorithm, kd.as_bytes())
        }
    }

    /// Builds a complete sanitized `Digest ...` value for an `Authorization:` header.
    pub fn build_digest_authorization(
        challenge: &DigestChallenge,
        username: &str,
        password: &str,
        method: &str,
        uri: &str,
        nc: u32,
        cnonce_override: Option<&str>,
    ) -> String {
        let cnonce = match cnonce_override {
            Some(c) => sanitize_header_token(c, 128),
            None => generate_random_hex_16(),
        };
        let response = compute_digest_response(
            challenge, username, password, method, uri, nc, &cnonce,
        );

        let user_esc = escape_quoted_header_val(username, MAX_CHALLENGE_PARAM_CHARS);
        let realm_esc = escape_quoted_header_val(&challenge.realm, MAX_CHALLENGE_PARAM_CHARS);
        let nonce_esc = escape_quoted_header_val(&challenge.nonce, MAX_CHALLENGE_PARAM_CHARS);
        let uri_esc = escape_quoted_header_val(uri, 2048);

        let mut header = format!(
            "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\"",
            user_esc, realm_esc, nonce_esc, uri_esc
        );

        if challenge.algorithm != DigestAlgorithm::Md5 || challenge.algorithm_explicit {
            header.push_str(&format!(", algorithm={}", challenge.algorithm.header_name()));
        }

        if challenge.qop_auth {
            let cnonce_esc = escape_quoted_header_val(&cnonce, 128);
            header.push_str(&format!(
                ", response=\"{}\", qop=auth, nc={:08x}, cnonce=\"{}\"",
                response, nc, cnonce_esc
            ));
        } else {
            header.push_str(&format!(", response=\"{}\"", response));
        }

        if let Some(ref opaque) = challenge.opaque {
            let opaque_esc = escape_quoted_header_val(opaque, MAX_CHALLENGE_PARAM_CHARS);
            header.push_str(&format!(", opaque=\"{}\"", opaque_esc));
        }

        header
    }

    /// Builds a sanitized `Basic <base64>` value for LAN-only fallback when permitted.
    pub fn build_basic_authorization(username: &str, password: &str) -> String {
        let clean_user = sanitize_header_token(username, MAX_CHALLENGE_PARAM_CHARS);
        let clean_pass = sanitize_header_token(password, MAX_CHALLENGE_PARAM_CHARS);
        let combined = Zeroizing::new(format!("{}:{}", clean_user, clean_pass));
        let encoded = BASE64_STD.encode(combined.as_bytes());
        format!("Basic {}", encoded)
    }
}

/// Process-wide circuit breaker tripped immediately on explicit NVR credential rejection (401)
/// or Cloud P2P Basic-auth downgrade attempt so worker threads never lock out the NVR account.
pub struct AuthCircuitBreaker {
    tripped: AtomicBool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanaryProbeOutcome {
    AuthOk,
    AuthRejected(String),
    Inconclusive,
}

impl Default for AuthCircuitBreaker {
    fn default() -> Self {
        Self {
            tripped: AtomicBool::new(false),
        }
    }
}

impl AuthCircuitBreaker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn is_tripped(&self) -> bool {
        self.tripped.load(Ordering::Acquire)
    }

    pub fn trip(&self) {
        self.tripped.store(true, Ordering::Release);
    }

    pub fn reset(&self) {
        self.tripped.store(false, Ordering::Release);
    }
}

struct TokenGrant {
    token: String,
    upstream_host: String,
    upstream_port: u16,
    rtsp_path_and_query: String,
    username: String,
    password: Zeroizing<String>,
    allow_basic_auth: bool,
    playback_scale: Option<f32>,
    expires_at: Instant,
}

pub struct AuthGateway {
    local_port: u16,
    circuit_breaker: Arc<AuthCircuitBreaker>,
    grants: Arc<Mutex<HashMap<String, TokenGrant>>>,
    rt_handle: tokio::runtime::Handle,
    shutdown_tx: watch::Sender<bool>,
    is_shutdown: AtomicBool,
    worker_thread: Mutex<Option<JoinHandle<()>>>,
}

impl AuthGateway {
    pub fn start(circuit_breaker: Arc<AuthCircuitBreaker>) -> io::Result<Arc<Self>> {
        let std_listener = StdTcpListener::bind("127.0.0.1:0")?;
        std_listener.set_nonblocking(true)?;
        let local_port = std_listener.local_addr()?.port();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let rt_handle = runtime.handle().clone();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let grants: Arc<Mutex<HashMap<String, TokenGrant>>> = Arc::new(Mutex::new(HashMap::new()));
        let grants_for_loop = Arc::clone(&grants);
        let cb_for_loop = Arc::clone(&circuit_breaker);

        let worker = thread::Builder::new()
            .name("nvr-auth-gw".to_string())
            .spawn(move || {
                runtime.block_on(async move {
                    let Ok(listener) = TcpListener::from_std(std_listener) else {
                        return;
                    };
                    run_accept_loop(
                        listener,
                        local_port,
                        grants_for_loop,
                        cb_for_loop,
                        shutdown_rx,
                    )
                    .await;
                });
            })?;

        Ok(Arc::new(Self {
            local_port,
            circuit_breaker,
            grants,
            rt_handle,
            shutdown_tx,
            is_shutdown: AtomicBool::new(false),
            worker_thread: Mutex::new(Some(worker)),
        }))
    }

    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    pub fn circuit_breaker(&self) -> Arc<AuthCircuitBreaker> {
        Arc::clone(&self.circuit_breaker)
    }

    pub fn mint_rtsp_url(
        &self,
        env_cfg: &NvrEnvConfig,
        rtsp_path_and_query: &str,
        playback_scale: Option<f32>,
    ) -> Result<String, String> {
        if self.is_shutdown.load(Ordering::Acquire) {
            return Err("AuthGateway is shut down".to_string());
        }
        let upstream_host = validate_nvr_host(env_cfg.active_host())?;
        let upstream_port = env_cfg.active_port();
        if upstream_port == 0 {
            return Err("Active RTSP port is 0 (tunnel not ready)".to_string());
        }

        let trimmed_pq = rtsp_path_and_query.trim();
        if trimmed_pq.is_empty() {
            return Err("RTSP path cannot be empty".to_string());
        }
        if trimmed_pq.bytes().any(|b| b <= 0x20 || b == 0x7f) {
            return Err("RTSP path contains forbidden whitespace or control characters".to_string());
        }
        let normalized_pq = if trimmed_pq.starts_with('/') {
            trimmed_pq.to_string()
        } else {
            format!("/{}", trimmed_pq)
        };

        let validated_scale = match playback_scale {
            Some(s) => {
                if !s.is_finite() || s <= 0.0 || s > 64.0 {
                    return Err(format!("Invalid playback scale: {}", s));
                }
                Some(s)
            }
            None => None,
        };

        let token = digest::generate_random_hex_16();
        let now = Instant::now();
        let grant = TokenGrant {
            token: token.clone(),
            upstream_host,
            upstream_port,
            rtsp_path_and_query: normalized_pq,
            username: env_cfg.username.clone(),
            password: Zeroizing::new(env_cfg.password.clone()),
            allow_basic_auth: !env_cfg.is_cloud_p2p_active(),
            playback_scale: validated_scale,
            expires_at: now + TOKEN_TTL,
        };

        {
            let mut map = self
                .grants
                .lock()
                .map_err(|_| "AuthGateway grant table lock poisoned".to_string())?;
            map.retain(|_, g| g.expires_at > now);
            if map.len() >= MAX_PENDING_GRANTS {
                if let Some(oldest_key) = map
                    .iter()
                    .min_by_key(|(_, g)| g.expires_at)
                    .map(|(k, _)| k.clone())
                {
                    map.remove(&oldest_key);
                }
            }
            map.insert(token.clone(), grant);
        }

        Ok(format!("rtsp://127.0.0.1:{}/{}", self.local_port, token))
    }

    pub fn probe_canary_auth_detailed(&self, env_cfg: &NvrEnvConfig) -> CanaryProbeOutcome {
        if self.is_shutdown.load(Ordering::Acquire) {
            return CanaryProbeOutcome::Inconclusive;
        }

        let mut eff_cfg = env_cfg.clone();
        if env_cfg.brand.eq_ignore_ascii_case("custom")
            && !env_cfg.is_cloud_p2p_active()
            && !env_cfg.use_tailscale
        {
            let host_fmt = format_host_for_url(env_cfg.active_host());
            let candidate = env_cfg
                .custom_url_template
                .replace("{user}", "u")
                .replace("{password}", "p")
                .replace("{host}", &host_fmt)
                .replace("{port}", &env_cfg.port.to_string())
                .replace("{channel}", "1")
                .replace("{subtype}", "1");
            if let Ok(parsed) = validate_custom_rtsp_url(&candidate) {
                if let Some(h) = parsed.host_str() {
                    eff_cfg.host = h.to_string();
                }
                if let Some(p) = parsed.port() {
                    eff_cfg.port = p;
                }
            }
        }

        let Ok(host) = validate_nvr_host(eff_cfg.active_host()) else {
            return CanaryProbeOutcome::Inconclusive;
        };
        let port = eff_cfg.active_port();
        if port == 0 {
            return CanaryProbeOutcome::Inconclusive;
        }

        let path = eff_cfg.build_live_rtsp_path(1, 1);
        let username = eff_cfg.username.clone();
        let password = Zeroizing::new(eff_cfg.password.clone());
        let allow_basic = !eff_cfg.is_cloud_p2p_active();
        let cb = Arc::clone(&self.circuit_breaker);

        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.rt_handle.spawn(async move {
            let outcome = match time::timeout(
                CANARY_TIMEOUT,
                probe_canary_auth_async(&host, port, &path, &username, &password, allow_basic, &cb),
            )
            .await
            {
                Ok(inner) => inner,
                Err(_) => CanaryProbeOutcome::Inconclusive,
            };
            let _ = tx.send(outcome);
        });

        match rx.recv_timeout(CANARY_TIMEOUT + Duration::from_millis(250)) {
            Ok(outcome) => outcome,
            Err(_) => CanaryProbeOutcome::Inconclusive,
        }
    }

    #[allow(dead_code)]
    pub fn probe_canary_auth(&self, env_cfg: &NvrEnvConfig) -> Result<(), String> {
        match self.probe_canary_auth_detailed(env_cfg) {
            CanaryProbeOutcome::AuthRejected(e) => Err(e),
            CanaryProbeOutcome::AuthOk | CanaryProbeOutcome::Inconclusive => Ok(()),
        }
    }

    pub fn shutdown(&self) {
        if !self.is_shutdown.swap(true, Ordering::AcqRel) {
            let _ = self.shutdown_tx.send(true);
            if let Ok(mut guard) = self.grants.lock() {
                guard.clear();
            }
            if let Ok(mut join_guard) = self.worker_thread.lock() {
                if let Some(handle) = join_guard.take() {
                    if handle.thread().id() != thread::current().id() {
                        let _ = handle.join();
                    }
                }
            }
        }
    }
}

impl Drop for AuthGateway {
    fn drop(&mut self) {
        self.shutdown();
    }
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
            // Skip st, tx_queue:rx_queue, tr:tm->when, retrnsmt -> col 7 is kernel socket uid
            let _ = cols.next();
            let _ = cols.next();
            let _ = cols.next();
            let _ = cols.next();
            return cols.next().and_then(|s| s.parse::<libc::uid_t>().ok());
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn verify_peer_uid(
    stream: &TcpStream,
    peer_addr: std::net::SocketAddr,
    local_port: u16,
) -> bool {
    use std::os::unix::io::AsRawFd;

    if !peer_addr.ip().is_loopback() {
        return false;
    }

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
    match peer_addr.ip() {
        std::net::IpAddr::V4(v4) => {
            let client_hex = u32::from_ne_bytes(v4.octets());
            let server_hex = u32::from_ne_bytes([127, 0, 0, 1]);
            let expected_client = format!("{:08X}:{:04X}", client_hex, peer_addr.port());
            let expected_server = format!("{:08X}:{:04X}", server_hex, local_port);
            if let Some(sock_uid) =
                lookup_proc_tcp_uid("/proc/net/tcp", &expected_client, &expected_server)
            {
                return sock_uid == my_uid;
            }
            let mapped = v4.to_ipv6_mapped();
            let expected_client6 = format_proc_ipv6_endpoint(mapped, peer_addr.port());
            let expected_server6 =
                format_proc_ipv6_endpoint(std::net::Ipv4Addr::LOCALHOST.to_ipv6_mapped(), local_port);
            if let Some(sock_uid) =
                lookup_proc_tcp_uid("/proc/net/tcp6", &expected_client6, &expected_server6)
            {
                return sock_uid == my_uid;
            }
            false
        }
        std::net::IpAddr::V6(v6) => {
            let expected_client6 = format_proc_ipv6_endpoint(v6, peer_addr.port());
            let server_v6 = stream
                .local_addr()
                .ok()
                .map(|sa| match sa.ip() {
                    std::net::IpAddr::V6(sv6) => sv6,
                    std::net::IpAddr::V4(sv4) => sv4.to_ipv6_mapped(),
                })
                .unwrap_or(std::net::Ipv6Addr::LOCALHOST);
            let expected_server6 = format_proc_ipv6_endpoint(server_v6, local_port);
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
fn verify_peer_uid(
    _stream: &TcpStream,
    peer_addr: std::net::SocketAddr,
    _local_port: u16,
) -> bool {
    peer_addr.ip().is_loopback()
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

async fn run_accept_loop(
    listener: TcpListener,
    local_port: u16,
    grants: Arc<Mutex<HashMap<String, TokenGrant>>>,
    circuit_breaker: Arc<AuthCircuitBreaker>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_SESSIONS));

    loop {
        let accepted = tokio::select! {
            _ = wait_for_shutdown(&mut shutdown_rx) => break,
            res = listener.accept() => res,
        };

        let Ok((client_stream, peer_addr)) = accepted else {
            time::sleep(Duration::from_millis(20)).await;
            continue;
        };

        if !verify_peer_uid(&client_stream, peer_addr, local_port) {
            drop(client_stream);
            continue;
        }

        let Ok(permit) = Arc::clone(&semaphore).try_acquire_owned() else {
            drop(client_stream);
            continue;
        };

        let _ = client_stream.set_nodelay(true);
        let session_grants = Arc::clone(&grants);
        let session_cb = Arc::clone(&circuit_breaker);
        let mut session_shutdown = shutdown_rx.clone();

        tokio::spawn(async move {
            let _permit = permit;
            tokio::select! {
                _ = wait_for_shutdown(&mut session_shutdown) => {}
                _ = handle_client_session(client_stream, local_port, session_grants, session_cb) => {}
            }
        });
    }
}

#[derive(Debug, Clone)]
struct RtspMessage {
    start_line: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

enum WireFrame {
    Rtp(usize),
    Rtsp(RtspMessage),
}

async fn read_wire_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    rtp_scratch: &mut Vec<u8>,
) -> io::Result<Option<WireFrame>> {
    let mut first = [0u8; 1];
    let mut skipped_crlf = 0usize;
    loop {
        if reader.read(&mut first).await? == 0 {
            return Ok(None);
        }
        if first[0] == b'\r' || first[0] == b'\n' {
            skipped_crlf += 1;
            if skipped_crlf > 64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Excessive leading CRLF bytes on RTSP stream",
                ));
            }
            continue;
        }
        break;
    }

    if first[0] == b'$' {
        let mut hdr_tail = [0u8; 3];
        reader.read_exact(&mut hdr_tail).await?;
        let payload_len = u16::from_be_bytes([hdr_tail[1], hdr_tail[2]]) as usize;
        let total_len = 4 + payload_len;
        if rtp_scratch.len() < total_len {
            rtp_scratch.resize(total_len, 0);
        }
        rtp_scratch[0] = b'$';
        rtp_scratch[1..4].copy_from_slice(&hdr_tail);
        reader
            .read_exact(&mut rtp_scratch[4..total_len])
            .await?;
        return Ok(Some(WireFrame::Rtp(total_len)));
    }

    let raw_hdr = read_until_double_crlf(reader, first[0], MAX_RTSP_HEADER_BYTES).await?;
    let header_slice = &raw_hdr[..raw_hdr.len() - 4];
    let header_str = std::str::from_utf8(header_slice)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Non-UTF8 RTSP header"))?;

    let mut lines = header_str.split("\r\n");
    let start_line = lines
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Empty RTSP start line"))?
        .trim()
        .to_string();
    if start_line.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Blank RTSP start line",
        ));
    }

    let mut headers = Vec::with_capacity(12);
    let mut content_length: usize = 0;

    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, val)) = line.split_once(':') {
            let clean_name = name.trim().to_string();
            let clean_val = val.trim().to_string();
            if clean_name.eq_ignore_ascii_case("content-length") {
                let parsed = clean_val.parse::<usize>().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "Invalid RTSP Content-Length")
                })?;
                if parsed > MAX_RTSP_BODY_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "RTSP Content-Length exceeds 512 KiB limit",
                    ));
                }
                content_length = parsed;
            }
            headers.push((clean_name, clean_val));
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await?;
    }

    Ok(Some(WireFrame::Rtsp(RtspMessage {
        start_line,
        headers,
        body,
    })))
}

async fn read_until_double_crlf<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    first_byte: u8,
    max_len: usize,
) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(512);
    buf.push(first_byte);

    loop {
        if buf.ends_with(b"\r\n\r\n") {
            return Ok(buf);
        }
        if buf.len() >= max_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "RTSP header exceeds 32 KiB limit",
            ));
        }

        let avail = reader.fill_buf().await?;
        if avail.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "EOF before end of RTSP headers",
            ));
        }

        let mut consumed = 0;
        for &b in avail {
            buf.push(b);
            consumed += 1;
            if buf.ends_with(b"\r\n\r\n") || buf.len() >= max_len {
                break;
            }
        }
        reader.consume(consumed);
    }
}

fn parse_rtsp_request_line(start_line: &str) -> Option<(String, String, String)> {
    let mut parts = start_line.split_whitespace();
    let method = parts.next()?.to_string();
    let uri = parts.next()?.to_string();
    let version = parts.next()?.to_string();
    if !version.to_ascii_uppercase().starts_with("RTSP/") {
        return None;
    }
    Some((method, uri, version))
}

fn parse_rtsp_status_code(start_line: &str) -> Option<u16> {
    let mut parts = start_line.split_whitespace();
    let proto = parts.next()?;
    if !proto.to_ascii_uppercase().starts_with("RTSP/") {
        return None;
    }
    parts.next()?.parse::<u16>().ok()
}

fn extract_cseq(headers: &[(String, String)]) -> Option<u32> {
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("cseq") {
            return v.trim().parse::<u32>().ok();
        }
    }
    None
}

fn serialize_rtsp_message(
    start_line: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(start_line.len() + headers.len() * 40 + body.len() + 32);
    out.extend_from_slice(start_line.as_bytes());
    out.extend_from_slice(b"\r\n");
    for (k, v) in headers {
        let safe_k = digest::sanitize_header_token(k, 128);
        let safe_v = digest::sanitize_header_token(v, 4096);
        out.extend_from_slice(safe_k.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(safe_v.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

/// Extracts `(token, optional_suffix)` from `rtsp://127.0.0.1:<port>/<32-hex-token><optional_suffix>`.
fn extract_token_and_suffix(uri: &str) -> Option<(String, String)> {
    let rest = uri
        .strip_prefix("rtsp://")
        .or_else(|| uri.strip_prefix("RTSP://"))?;
    let slash_idx = rest.find('/')?;
    let after_slash = &rest[slash_idx + 1..];
    if !after_slash
        .as_bytes()
        .get(..32)
        .is_some_and(|b| b.iter().all(|c| c.is_ascii_hexdigit()))
    {
        return None;
    }
    let token_candidate = &after_slash[..32];
    let suffix = &after_slash[32..];
    if !suffix.is_empty()
        && !suffix.starts_with('/')
        && !suffix.starts_with('?')
        && !suffix.starts_with(';')
    {
        return None;
    }
    Some((token_candidate.to_ascii_lowercase(), suffix.to_string()))
}

fn extract_path_after_authority(uri: &str) -> Option<String> {
    if uri == "*" {
        return Some("*".to_string());
    }
    let rest = uri
        .strip_prefix("rtsp://")
        .or_else(|| uri.strip_prefix("RTSP://"))
        .or_else(|| uri.strip_prefix("rtsps://"))
        .or_else(|| uri.strip_prefix("RTSPS://"))?;
    let slash_idx = rest.find('/')?;
    Some(rest[slash_idx..].to_string())
}

fn normalize_upstream_content_base(
    raw_cb: &str,
    upstream_host: &str,
    upstream_port: u16,
) -> Option<String> {
    let trimmed = raw_cb.trim();
    if trimmed.is_empty() || trimmed.bytes().any(|b| b <= 0x20 || b == 0x7f) {
        return None;
    }
    if trimmed.starts_with('/') {
        return Some(format!("rtsp://{}:{}{}", upstream_host, upstream_port, trimmed));
    }
    let rest = trimmed
        .strip_prefix("rtsp://")
        .or_else(|| trimmed.strip_prefix("RTSP://"))
        .or_else(|| trimmed.strip_prefix("rtsps://"))
        .or_else(|| trimmed.strip_prefix("RTSPS://"))?;
    let path_part = match rest.find('/') {
        Some(idx) => &rest[idx..],
        None => "/",
    };
    Some(format!(
        "rtsp://{}:{}{}",
        upstream_host, upstream_port, path_part
    ))
}

fn rewrite_client_uri(
    client_uri: &str,
    grant: &TokenGrant,
    upstream_content_base: Option<&str>,
) -> Option<String> {
    let trimmed = client_uri.trim();
    if trimmed == "*" {
        return Some("*".to_string());
    }

    let Some((tok, suffix)) = extract_token_and_suffix(trimmed) else {
        // Client sent an absolute URL from SDP `a=control:rtsp://<nvr-ip>:554/...` on an
        // already-authenticated session: rewrite authority to upstream host:port directly!
        let raw_path = extract_path_after_authority(trimmed)?;
        if raw_path.bytes().any(|b| b <= 0x20 || b == 0x7f) {
            return None;
        }
        return Some(format!(
            "rtsp://{}:{}{}",
            grant.upstream_host, grant.upstream_port, raw_path
        ));
    };

    if tok != grant.token {
        return None;
    }
    if suffix.bytes().any(|b| b <= 0x20 || b == 0x7f) {
        return None;
    }

    if suffix.is_empty() {
        return Some(format!(
            "rtsp://{}:{}{}",
            grant.upstream_host, grant.upstream_port, grant.rtsp_path_and_query
        ));
    }

    if let Some(cb) = upstream_content_base
        .and_then(|raw_cb| {
            normalize_upstream_content_base(raw_cb, &grant.upstream_host, grant.upstream_port)
        })
    {
        if suffix == "/" {
            // Preserve upstream Content-Base verbatim without appending '/' onto query strings
            return Some(cb);
        }
        let rel = suffix.strip_prefix('/').unwrap_or(&suffix);
        if cb.ends_with('/') {
            return Some(format!("{}{}", cb, rel));
        } else {
            return Some(format!("{}/{}", cb, rel));
        }
    }

    if suffix == "/" {
        // Do not append '/' if rtsp_path_and_query did not end with '/' (especially with '?')
        return Some(format!(
            "rtsp://{}:{}{}",
            grant.upstream_host, grant.upstream_port, grant.rtsp_path_and_query
        ));
    }

    let base = grant.rtsp_path_and_query.trim_end_matches('/');
    if suffix.starts_with('/') || suffix.starts_with('?') || suffix.starts_with(';') {
        Some(format!(
            "rtsp://{}:{}{}{}",
            grant.upstream_host, grant.upstream_port, base, suffix
        ))
    } else {
        Some(format!(
            "rtsp://{}:{}{}/{}",
            grant.upstream_host, grant.upstream_port, base, suffix
        ))
    }
}

#[derive(Clone)]
struct PendingRequest {
    client_cseq: u32,
    outbound_cseq: u32,
    method: String,
    version: String,
    upstream_uri: String,
    filtered_headers: Vec<(String, String)>,
    body: Vec<u8>,
    had_initial_auth: bool,
    used_algorithm: Option<digest::DigestAlgorithm>,
    retries_done: u8,
}

struct SessionState {
    next_outbound_cseq: u32,
    cseq_offset: u32,
    digest_challenge: Option<digest::DigestChallenge>,
    md5_fallback_challenge: Option<digest::DigestChallenge>,
    sha256_rejected: bool,
    digest_nc: u32,
    use_basic: bool,
    upstream_content_base: Option<String>,
    pending_requests: HashMap<u32, PendingRequest>,
    pending_order: Vec<u32>,
}

impl SessionState {
    fn new() -> Self {
        Self {
            next_outbound_cseq: 1,
            cseq_offset: 0,
            digest_challenge: None,
            md5_fallback_challenge: None,
            sha256_rejected: false,
            digest_nc: 0,
            use_basic: false,
            upstream_content_base: None,
            pending_requests: HashMap::new(),
            pending_order: Vec::new(),
        }
    }

    fn alloc_outbound_cseq(&mut self, client_cseq_hint: u32) -> u32 {
        let mut cseq = self.next_outbound_cseq.max(client_cseq_hint.max(1));
        while self.pending_requests.contains_key(&cseq) {
            cseq = cseq.saturating_add(1);
        }
        self.next_outbound_cseq = cseq.saturating_add(1);
        if client_cseq_hint > 0 && cseq >= client_cseq_hint {
            self.cseq_offset = cseq - client_cseq_hint;
        }
        cseq
    }

    fn insert_pending(&mut self, req: PendingRequest) {
        if self.pending_order.len() >= MAX_IN_FLIGHT_REQUESTS {
            let oldest = self.pending_order.remove(0);
            self.pending_requests.remove(&oldest);
        }
        let key = req.outbound_cseq;
        self.pending_order.retain(|&k| k != key);
        self.pending_order.push(key);
        self.pending_requests.insert(key, req);
    }

    fn take_pending(&mut self, resp_cseq: Option<u32>) -> Option<PendingRequest> {
        if let Some(cseq) = resp_cseq {
            if let Some(req) = self.pending_requests.remove(&cseq) {
                self.pending_order.retain(|&k| k != cseq);
                return Some(req);
            }
        }
        // Fallback if buggy camera omitted CSeq on 401
        if self.pending_order.len() == 1 {
            let key = self.pending_order.remove(0);
            return self.pending_requests.remove(&key);
        }
        None
    }
}

fn build_outbound_request_bytes(
    pending: &PendingRequest,
    auth_header: Option<String>,
) -> Vec<u8> {
    let start_line = format!(
        "{} {} {}",
        pending.method, pending.upstream_uri, pending.version
    );
    let mut headers = Vec::with_capacity(pending.filtered_headers.len() + 2);
    headers.push(("CSeq".to_string(), pending.outbound_cseq.to_string()));
    for (k, v) in &pending.filtered_headers {
        headers.push((k.clone(), v.clone()));
    }
    if let Some(auth) = auth_header {
        headers.push(("Authorization".to_string(), auth));
    }
    serialize_rtsp_message(&start_line, &headers, &pending.body)
}

fn prepare_client_request(
    req: RtspMessage,
    grant: &TokenGrant,
    state: &mut SessionState,
) -> Option<Vec<u8>> {
    let (method, client_uri, version) = parse_rtsp_request_line(&req.start_line)?;
    let upstream_uri = rewrite_client_uri(
        &client_uri,
        grant,
        state.upstream_content_base.as_deref(),
    )?;
    let client_cseq = extract_cseq(&req.headers).unwrap_or(1);
    let outbound_cseq = state.alloc_outbound_cseq(client_cseq);

    let mut filtered_headers = Vec::with_capacity(req.headers.len() + 1);
    let mut has_scale = false;
    for (k, v) in req.headers {
        if k.eq_ignore_ascii_case("cseq") || k.eq_ignore_ascii_case("authorization") {
            continue;
        }
        if k.eq_ignore_ascii_case("scale") {
            has_scale = true;
        }
        if k.eq_ignore_ascii_case("host") {
            filtered_headers.push((
                "Host".to_string(),
                format!("{}:{}", grant.upstream_host, grant.upstream_port),
            ));
            continue;
        }
        filtered_headers.push((k, v));
    }

    if method.eq_ignore_ascii_case("PLAY") && !has_scale {
        if let Some(scale) = grant.playback_scale {
            if (scale - 1.0).abs() > 0.05 {
                filtered_headers.push(("Scale".to_string(), format!("{:.1}", scale)));
            }
        }
    }

    let (auth_header, had_initial_auth, used_algorithm) =
        if let Some(ref challenge) = state.digest_challenge {
            state.digest_nc = state.digest_nc.wrapping_add(1).max(1);
            let auth = digest::build_digest_authorization(
                challenge,
                &grant.username,
                &grant.password,
                &method,
                &upstream_uri,
                state.digest_nc,
                None,
            );
            (Some(auth), true, Some(challenge.algorithm))
        } else if state.use_basic && grant.allow_basic_auth {
            let auth = digest::build_basic_authorization(&grant.username, &grant.password);
            (Some(auth), true, None)
        } else {
            (None, false, None)
        };

    let pending = PendingRequest {
        client_cseq,
        outbound_cseq,
        method,
        version,
        upstream_uri,
        filtered_headers,
        body: req.body,
        had_initial_auth,
        used_algorithm,
        retries_done: 0,
    };

    let bytes = build_outbound_request_bytes(&pending, auth_header);
    state.insert_pending(pending);
    Some(bytes)
}

fn elapsed_since_ms(epoch: Instant) -> u64 {
    epoch.elapsed().as_millis() as u64
}

async fn read_wire_frame_with_shared_idle<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    rtp_scratch: &mut Vec<u8>,
    session_epoch: Instant,
    last_activity_ms: &AtomicU64,
    idle_timeout: Duration,
) -> Option<WireFrame> {
    let idle_limit_ms = idle_timeout.as_millis() as u64;
    loop {
        let last = last_activity_ms.load(Ordering::Acquire);
        let now = elapsed_since_ms(session_epoch);
        let idle_so_far = now.saturating_sub(last);
        if idle_so_far >= idle_limit_ms {
            return None;
        }
        let wait_dur = Duration::from_millis((idle_limit_ms - idle_so_far).max(50));
        match time::timeout(wait_dur, reader.fill_buf()).await {
            Ok(Ok(buf)) => {
                if buf.is_empty() {
                    return None;
                }
                break;
            }
            Ok(Err(_)) => return None,
            Err(_) => continue,
        }
    }

    match time::timeout(idle_timeout, read_wire_frame(reader, rtp_scratch)).await {
        Ok(Ok(Some(frame))) => {
            last_activity_ms.store(elapsed_since_ms(session_epoch), Ordering::Release);
            Some(frame)
        }
        _ => None,
    }
}

async fn handle_client_session(
    client_stream: TcpStream,
    local_port: u16,
    grants: Arc<Mutex<HashMap<String, TokenGrant>>>,
    circuit_breaker: Arc<AuthCircuitBreaker>,
) {
    let (client_read_half, mut client_write_half) = client_stream.into_split();
    let mut client_reader = BufReader::with_capacity(16 * 1024, client_read_half);
    let mut client_rtp_scratch = Vec::new();

    // Read the initial RTSP request and consume the single-use token before touching upstream.
    let first_frame = match time::timeout(
        FIRST_REQUEST_TIMEOUT,
        read_wire_frame(&mut client_reader, &mut client_rtp_scratch),
    )
    .await
    {
        Ok(Ok(Some(WireFrame::Rtsp(msg)))) => msg,
        _ => return,
    };

    let Some((_method, first_uri, _version)) = parse_rtsp_request_line(&first_frame.start_line)
    else {
        return;
    };
    let Some((token, _)) = extract_token_and_suffix(&first_uri) else {
        let cseq = extract_cseq(&first_frame.headers).unwrap_or(1);
        let resp = serialize_rtsp_message(
            "RTSP/1.0 401 Unauthorized",
            &[("CSeq".to_string(), cseq.to_string())],
            b"",
        );
        let _ = client_write_half.write_all(&resp).await;
        return;
    };

    let now = Instant::now();
    let grant_opt = {
        let Ok(mut map) = grants.lock() else {
            return;
        };
        map.retain(|_, g| g.expires_at > now);
        map.remove(&token)
            .filter(|g| g.expires_at >= now)
            .map(Arc::new)
    };
    let Some(grant) = grant_opt else {
        let cseq = extract_cseq(&first_frame.headers).unwrap_or(1);
        let resp = serialize_rtsp_message(
            "RTSP/1.0 401 Unauthorized",
            &[("CSeq".to_string(), cseq.to_string())],
            b"",
        );
        let _ = client_write_half.write_all(&resp).await;
        return;
    };

    let upstream_addr = format!("{}:{}", grant.upstream_host, grant.upstream_port);
    let upstream_stream = match time::timeout(
        UPSTREAM_CONNECT_TIMEOUT,
        TcpStream::connect(&upstream_addr),
    )
    .await
    {
        Ok(Ok(s)) => s,
        _ => {
            let cseq = extract_cseq(&first_frame.headers).unwrap_or(1);
            let resp = serialize_rtsp_message(
                "RTSP/1.0 503 Service Unavailable",
                &[("CSeq".to_string(), cseq.to_string())],
                b"",
            );
            let _ = client_write_half.write_all(&resp).await;
            return;
        }
    };
    let _ = upstream_stream.set_nodelay(true);

    let (upstream_read_half, upstream_write_half) = upstream_stream.into_split();
    let upstream_writer = Arc::new(AsyncMutex::new(upstream_write_half));
    let state = Arc::new(Mutex::new(SessionState::new()));

    let initial_bytes = {
        let Ok(mut st) = state.lock() else {
            return;
        };
        prepare_client_request(first_frame, &grant, &mut st)
    };
    let Some(initial_bytes) = initial_bytes else {
        return;
    };

    {
        let mut uw = upstream_writer.lock().await;
        if uw.write_all(&initial_bytes).await.is_err() {
            return;
        }
    }

    let session_epoch = Instant::now();
    let last_activity_ms = Arc::new(AtomicU64::new(0));

    let c2u_writer = Arc::clone(&upstream_writer);
    let c2u_state = Arc::clone(&state);
    let c2u_grant = Arc::clone(&grant);
    let c2u_activity = Arc::clone(&last_activity_ms);

    let u2c_writer = Arc::clone(&upstream_writer);
    let u2c_state = Arc::clone(&state);
    let u2c_grant = Arc::clone(&grant);
    let u2c_cb = Arc::clone(&circuit_breaker);
    let u2c_activity = Arc::clone(&last_activity_ms);

    let mut c2u_task = tokio::spawn(async move {
        pump_client_to_upstream(
            client_reader,
            client_rtp_scratch,
            c2u_writer,
            c2u_grant,
            c2u_state,
            session_epoch,
            c2u_activity,
        )
        .await;
    });

    let mut u2c_task = tokio::spawn(async move {
        pump_upstream_to_client(
            upstream_read_half,
            client_write_half,
            u2c_writer,
            u2c_grant,
            u2c_state,
            u2c_cb,
            local_port,
            session_epoch,
            u2c_activity,
        )
        .await;
    });

    tokio::select! {
        _ = &mut c2u_task => {
            u2c_task.abort();
        }
        _ = &mut u2c_task => {
            c2u_task.abort();
        }
    }
}

async fn pump_client_to_upstream(
    mut client_reader: BufReader<OwnedReadHalf>,
    mut rtp_scratch: Vec<u8>,
    upstream_writer: Arc<AsyncMutex<OwnedWriteHalf>>,
    grant: Arc<TokenGrant>,
    state: Arc<Mutex<SessionState>>,
    session_epoch: Instant,
    last_activity_ms: Arc<AtomicU64>,
) {
    loop {
        let Some(frame) = read_wire_frame_with_shared_idle(
            &mut client_reader,
            &mut rtp_scratch,
            session_epoch,
            &last_activity_ms,
            SESSION_IDLE_TIMEOUT,
        )
        .await
        else {
            break;
        };

        match frame {
            WireFrame::Rtp(total_len) => {
                let mut uw = upstream_writer.lock().await;
                if uw.write_all(&rtp_scratch[..total_len]).await.is_err() {
                    break;
                }
            }
            WireFrame::Rtsp(req) => {
                let out_bytes = {
                    let Ok(mut st) = state.lock() else {
                        break;
                    };
                    prepare_client_request(req, &grant, &mut st)
                };
                let Some(out_bytes) = out_bytes else {
                    break;
                };
                let mut uw = upstream_writer.lock().await;
                if uw.write_all(&out_bytes).await.is_err() {
                    break;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn pump_upstream_to_client(
    upstream_read_half: OwnedReadHalf,
    mut client_writer: OwnedWriteHalf,
    upstream_writer: Arc<AsyncMutex<OwnedWriteHalf>>,
    grant: Arc<TokenGrant>,
    state: Arc<Mutex<SessionState>>,
    circuit_breaker: Arc<AuthCircuitBreaker>,
    local_port: u16,
    session_epoch: Instant,
    last_activity_ms: Arc<AtomicU64>,
) {
    let mut upstream_reader = BufReader::with_capacity(64 * 1024, upstream_read_half);
    let mut rtp_scratch = Vec::with_capacity(4096);
    let local_content_base = format!("rtsp://127.0.0.1:{}/{}/", local_port, grant.token);

    loop {
        let Some(frame) = read_wire_frame_with_shared_idle(
            &mut upstream_reader,
            &mut rtp_scratch,
            session_epoch,
            &last_activity_ms,
            SESSION_IDLE_TIMEOUT,
        )
        .await
        else {
            break;
        };

        match frame {
            WireFrame::Rtp(total_len) => {
                if client_writer
                    .write_all(&rtp_scratch[..total_len])
                    .await
                    .is_err()
                {
                    break;
                }
            }
            WireFrame::Rtsp(resp) => {
                let status = parse_rtsp_status_code(&resp.start_line).unwrap_or(0);
                let resp_cseq = extract_cseq(&resp.headers);

                enum UpstreamAction {
                    Replay(Vec<u8>),
                    Forward(Vec<u8>),
                }

                let action = {
                    let Ok(mut st) = state.lock() else {
                        break;
                    };
                    let pending_opt = st.take_pending(resp_cseq);
                    let current_offset = st.cseq_offset;

                    if status == 401 {
                        let all_digest = digest::parse_www_authenticate_all(&resp.headers);
                        let (_, has_basic) =
                            digest::parse_www_authenticate_headers(&resp.headers);

                        if let Some(md5_cand) =
                            all_digest.iter().find(|c| c.algorithm.is_md5_family())
                        {
                            st.md5_fallback_challenge = Some(md5_cand.clone());
                        }

                        if let Some(mut pending) = pending_opt {
                            let selected_challenge = if pending.retries_done == 0 {
                                if st.sha256_rejected {
                                    all_digest
                                        .iter()
                                        .find(|c| c.algorithm.is_md5_family())
                                        .cloned()
                                        .or_else(|| all_digest.first().cloned())
                                } else {
                                    all_digest.first().cloned()
                                }
                            } else if pending.retries_done == 1 {
                                let stale_ch = all_digest
                                    .iter()
                                    .find(|c| {
                                        c.stale
                                            && (!st.sha256_rejected
                                                || c.algorithm.is_md5_family())
                                    })
                                    .cloned();
                                if stale_ch.is_some() {
                                    stale_ch
                                } else if pending
                                    .used_algorithm
                                    .is_some_and(|a| a.is_sha256_family())
                                {
                                    let md5_fb = all_digest
                                        .iter()
                                        .find(|c| c.algorithm.is_md5_family())
                                        .cloned()
                                        .or_else(|| st.md5_fallback_challenge.clone());
                                    if md5_fb.is_some() {
                                        st.sha256_rejected = true;
                                    }
                                    md5_fb
                                } else {
                                    None
                                }
                            } else {
                                None
                            };

                            if let Some(challenge) = selected_challenge {
                                let new_outbound_cseq = st.alloc_outbound_cseq(0);
                                pending.outbound_cseq = new_outbound_cseq;
                                pending.retries_done = pending.retries_done.saturating_add(1);
                                pending.used_algorithm = Some(challenge.algorithm);

                                st.digest_nc = 1;
                                let auth = digest::build_digest_authorization(
                                    &challenge,
                                    &grant.username,
                                    &grant.password,
                                    &pending.method,
                                    &pending.upstream_uri,
                                    st.digest_nc,
                                    None,
                                );
                                st.digest_challenge = Some(challenge);

                                let replay_bytes =
                                    build_outbound_request_bytes(&pending, Some(auth));
                                st.insert_pending(pending);
                                UpstreamAction::Replay(replay_bytes)
                            } else if all_digest.is_empty() && has_basic {
                                if !grant.allow_basic_auth {
                                    // Cloud P2P mode: NEVER downgrade to Basic credentials
                                    circuit_breaker.trip();
                                    let fwd = rewrite_response_for_client(
                                        &resp,
                                        Some(pending.client_cseq),
                                        current_offset,
                                        &local_content_base,
                                        false,
                                        &grant.upstream_host,
                                        grant.upstream_port,
                                        &mut st.upstream_content_base,
                                    );
                                    UpstreamAction::Forward(fwd)
                                } else if pending.retries_done == 0 && !pending.had_initial_auth {
                                    st.use_basic = true;
                                    let new_outbound_cseq = st.alloc_outbound_cseq(0);
                                    pending.outbound_cseq = new_outbound_cseq;
                                    pending.retries_done = 1;

                                    let auth = digest::build_basic_authorization(
                                        &grant.username,
                                        &grant.password,
                                    );
                                    let replay_bytes =
                                        build_outbound_request_bytes(&pending, Some(auth));
                                    st.insert_pending(pending);
                                    UpstreamAction::Replay(replay_bytes)
                                } else {
                                    circuit_breaker.trip();
                                    let fwd = rewrite_response_for_client(
                                        &resp,
                                        Some(pending.client_cseq),
                                        current_offset,
                                        &local_content_base,
                                        false,
                                        &grant.upstream_host,
                                        grant.upstream_port,
                                        &mut st.upstream_content_base,
                                    );
                                    UpstreamAction::Forward(fwd)
                                }
                            } else {
                                // Digest or Basic replay already tried and rejected (or no auth scheme)
                                circuit_breaker.trip();
                                let fwd = rewrite_response_for_client(
                                    &resp,
                                    Some(pending.client_cseq),
                                    current_offset,
                                    &local_content_base,
                                    false,
                                    &grant.upstream_host,
                                    grant.upstream_port,
                                    &mut st.upstream_content_base,
                                );
                                UpstreamAction::Forward(fwd)
                            }
                        } else {
                            circuit_breaker.trip();
                            let fwd = rewrite_response_for_client(
                                &resp,
                                None,
                                current_offset,
                                &local_content_base,
                                false,
                                &grant.upstream_host,
                                grant.upstream_port,
                                &mut st.upstream_content_base,
                            );
                            UpstreamAction::Forward(fwd)
                        }
                    } else {
                        // Note: 403 Forbidden indicates per-channel ACL restriction on valid
                        // credentials and MUST NOT trip the process-wide AuthCircuitBreaker.
                        let is_describe_ok = status == 200
                            && pending_opt
                                .as_ref()
                                .map(|p| p.method.eq_ignore_ascii_case("DESCRIBE"))
                                .unwrap_or(false);
                        let target_cseq = pending_opt.map(|p| p.client_cseq);
                        let fwd = rewrite_response_for_client(
                            &resp,
                            target_cseq,
                            current_offset,
                            &local_content_base,
                            is_describe_ok,
                            &grant.upstream_host,
                            grant.upstream_port,
                            &mut st.upstream_content_base,
                        );
                        UpstreamAction::Forward(fwd)
                    }
                };

                match action {
                    UpstreamAction::Replay(replay_bytes) => {
                        let mut uw = upstream_writer.lock().await;
                        if uw.write_all(&replay_bytes).await.is_err() {
                            break;
                        }
                    }
                    UpstreamAction::Forward(fwd_bytes) => {
                        if client_writer.write_all(&fwd_bytes).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn rewrite_response_for_client(
    resp: &RtspMessage,
    known_client_cseq: Option<u32>,
    cseq_offset: u32,
    local_content_base: &str,
    is_describe_ok: bool,
    upstream_host: &str,
    upstream_port: u16,
    saved_upstream_cb: &mut Option<String>,
) -> Vec<u8> {
    let mut new_headers = Vec::with_capacity(resp.headers.len() + 2);
    let mut saw_cseq = false;
    let mut saw_content_base = false;

    for (k, v) in &resp.headers {
        if k.eq_ignore_ascii_case("cseq") {
            saw_cseq = true;
            let adjusted = match known_client_cseq {
                Some(c) => c,
                None => v
                    .trim()
                    .parse::<u32>()
                    .map(|n| n.saturating_sub(cseq_offset))
                    .unwrap_or(1),
            };
            new_headers.push(("CSeq".to_string(), adjusted.to_string()));
        } else if k.eq_ignore_ascii_case("content-base")
            || k.eq_ignore_ascii_case("content-location")
        {
            saw_content_base = true;
            if is_describe_ok {
                let clean_cb = digest::sanitize_header_token(v.trim(), 2048);
                if let Some(normalized) =
                    normalize_upstream_content_base(&clean_cb, upstream_host, upstream_port)
                {
                    *saved_upstream_cb = Some(normalized);
                }
            }
            new_headers.push((k.clone(), local_content_base.to_string()));
        } else {
            new_headers.push((k.clone(), v.clone()));
        }
    }

    if !saw_cseq {
        if let Some(c) = known_client_cseq {
            new_headers.push(("CSeq".to_string(), c.to_string()));
        }
    }

    // Ensure ffmpeg always keeps `/<token>/` when resolving relative SDP `a=control:` tracks
    if is_describe_ok && !saw_content_base && !resp.body.is_empty() {
        new_headers.push(("Content-Base".to_string(), local_content_base.to_string()));
    }

    serialize_rtsp_message(&resp.start_line, &new_headers, &resp.body)
}

async fn probe_canary_auth_async(
    host: &str,
    port: u16,
    rtsp_path: &str,
    username: &str,
    password: &str,
    allow_basic: bool,
    circuit_breaker: &AuthCircuitBreaker,
) -> CanaryProbeOutcome {
    let addr = format!("{}:{}", host, port);
    let Ok(stream) = TcpStream::connect(&addr).await else {
        return CanaryProbeOutcome::Inconclusive;
    };
    let _ = stream.set_nodelay(true);
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::with_capacity(8 * 1024, read_half);
    let mut rtp_scratch = Vec::new();

    let uri = format!("rtsp://{}:{}{}", host, port, rtsp_path);
    let req1 = serialize_rtsp_message(
        &format!("DESCRIBE {} RTSP/1.0", uri),
        &[
            ("CSeq".to_string(), "1".to_string()),
            ("User-Agent".to_string(), "NVRCommandCenter/3.0".to_string()),
            ("Accept".to_string(), "application/sdp".to_string()),
        ],
        b"",
    );
    if write_half.write_all(&req1).await.is_err() {
        return CanaryProbeOutcome::Inconclusive;
    }

    let resp1 = match read_next_rtsp_only(&mut reader, &mut rtp_scratch).await {
        Ok(Some(msg)) => msg,
        _ => return CanaryProbeOutcome::Inconclusive,
    };
    let status1 = parse_rtsp_status_code(&resp1.start_line).unwrap_or(0);
    if status1 == 0 || status1 >= 500 {
        return CanaryProbeOutcome::Inconclusive;
    }
    if status1 != 401 {
        return CanaryProbeOutcome::AuthOk;
    }

    let challenges1 = digest::parse_www_authenticate_all(&resp1.headers);
    let (_, has_basic) = digest::parse_www_authenticate_headers(&resp1.headers);
    if let Some(challenge) = challenges1.first() {
        let auth = digest::build_digest_authorization(
            challenge, username, password, "DESCRIBE", &uri, 1, None,
        );
        let req2 = serialize_rtsp_message(
            &format!("DESCRIBE {} RTSP/1.0", uri),
            &[
                ("CSeq".to_string(), "2".to_string()),
                ("User-Agent".to_string(), "NVRCommandCenter/3.0".to_string()),
                ("Accept".to_string(), "application/sdp".to_string()),
                ("Authorization".to_string(), auth),
            ],
            b"",
        );
        if write_half.write_all(&req2).await.is_err() {
            return CanaryProbeOutcome::Inconclusive;
        }

        let resp2 = match read_next_rtsp_only(&mut reader, &mut rtp_scratch).await {
            Ok(Some(msg)) => msg,
            _ => return CanaryProbeOutcome::Inconclusive,
        };
        let status2 = parse_rtsp_status_code(&resp2.start_line).unwrap_or(0);
        if status2 == 0 || status2 >= 500 {
            return CanaryProbeOutcome::Inconclusive;
        }
        if status2 == 401 {
            let challenges2 = digest::parse_www_authenticate_all(&resp2.headers);
            let retry_ch = challenges2
                .iter()
                .find(|c| c.stale)
                .cloned()
                .or_else(|| {
                    if challenge.algorithm.is_sha256_family() {
                        challenges2
                            .iter()
                            .find(|c| c.algorithm.is_md5_family())
                            .cloned()
                            .or_else(|| {
                                challenges1
                                    .iter()
                                    .find(|c| c.algorithm.is_md5_family())
                                    .cloned()
                            })
                    } else {
                        None
                    }
                });

            if let Some(ch3) = retry_ch {
                let auth3 = digest::build_digest_authorization(
                    &ch3, username, password, "DESCRIBE", &uri, 1, None,
                );
                let req3 = serialize_rtsp_message(
                    &format!("DESCRIBE {} RTSP/1.0", uri),
                    &[
                        ("CSeq".to_string(), "3".to_string()),
                        ("User-Agent".to_string(), "NVRCommandCenter/3.0".to_string()),
                        ("Accept".to_string(), "application/sdp".to_string()),
                        ("Authorization".to_string(), auth3),
                    ],
                    b"",
                );
                if write_half.write_all(&req3).await.is_err() {
                    return CanaryProbeOutcome::Inconclusive;
                }
                let resp3 = match read_next_rtsp_only(&mut reader, &mut rtp_scratch).await {
                    Ok(Some(msg)) => msg,
                    _ => return CanaryProbeOutcome::Inconclusive,
                };
                let status3 = parse_rtsp_status_code(&resp3.start_line).unwrap_or(0);
                if status3 == 0 || status3 >= 500 {
                    return CanaryProbeOutcome::Inconclusive;
                }
                if status3 != 401 {
                    return CanaryProbeOutcome::AuthOk;
                }
            }
            circuit_breaker.trip();
            return CanaryProbeOutcome::AuthRejected(
                "401 Unauthorized: NVR rejected RTSP Digest credentials".to_string(),
            );
        }
        return CanaryProbeOutcome::AuthOk;
    }

    if has_basic {
        if !allow_basic {
            circuit_breaker.trip();
            return CanaryProbeOutcome::AuthRejected(
                "401 Unauthorized: Basic authentication downgrade blocked on Cloud P2P".to_string(),
            );
        }
        let auth = digest::build_basic_authorization(username, password);
        let req2 = serialize_rtsp_message(
            &format!("DESCRIBE {} RTSP/1.0", uri),
            &[
                ("CSeq".to_string(), "2".to_string()),
                ("User-Agent".to_string(), "NVRCommandCenter/3.0".to_string()),
                ("Accept".to_string(), "application/sdp".to_string()),
                ("Authorization".to_string(), auth),
            ],
            b"",
        );
        if write_half.write_all(&req2).await.is_err() {
            return CanaryProbeOutcome::Inconclusive;
        }
        let resp2 = match read_next_rtsp_only(&mut reader, &mut rtp_scratch).await {
            Ok(Some(msg)) => msg,
            _ => return CanaryProbeOutcome::Inconclusive,
        };
        let status2 = parse_rtsp_status_code(&resp2.start_line).unwrap_or(0);
        if status2 == 0 || status2 >= 500 {
            return CanaryProbeOutcome::Inconclusive;
        }
        if status2 == 401 {
            circuit_breaker.trip();
            return CanaryProbeOutcome::AuthRejected(
                "401 Unauthorized: NVR rejected RTSP Basic credentials".to_string(),
            );
        }
        return CanaryProbeOutcome::AuthOk;
    }

    circuit_breaker.trip();
    CanaryProbeOutcome::AuthRejected(
        "401 Unauthorized: NVR offered no supported authentication scheme".to_string(),
    )
}

async fn read_next_rtsp_only<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    rtp_scratch: &mut Vec<u8>,
) -> io::Result<Option<RtspMessage>> {
    for _ in 0..8 {
        match read_wire_frame(reader, rtp_scratch).await? {
            None => return Ok(None),
            Some(WireFrame::Rtp(_)) => continue,
            Some(WireFrame::Rtsp(msg)) => return Ok(Some(msg)),
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_gateway::digest::{
        build_digest_authorization, compute_digest_response, escape_quoted_header_val,
        parse_www_authenticate_headers, sanitize_header_token, DigestAlgorithm,
    };

    #[test]
    fn test_digest_rfc2617_md5_and_rfc7616_sha256_vectors() {
        // RFC 2617 Section 3.5 standard MD5 qop=auth test vector
        let headers = vec![(
            "WWW-Authenticate".to_string(),
            "Digest realm=\"testrealm@host.com\", qop=\"auth,auth-int\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", opaque=\"5ccc069c403ebaf9f0171e9517f40e41\"".to_string(),
        )];
        let (ch_opt, has_basic) = parse_www_authenticate_headers(&headers);
        assert!(!has_basic);
        let ch = ch_opt.expect("Expected MD5 DigestChallenge");
        assert_eq!(ch.algorithm, DigestAlgorithm::Md5);
        assert!(ch.qop_auth);
        assert_eq!(ch.realm, "testrealm@host.com");

        let resp = compute_digest_response(
            &ch,
            "Mufasa",
            "Circle Of Life",
            "GET",
            "/dir/index.html",
            1,
            "0a4f113b",
        );
        assert_eq!(resp, "6629fae49393a05397450978507c4ef1");

        // RFC 7616 Section 3.9.1 SHA-256 test vector
        let sha_headers = vec![
            (
                "WWW-Authenticate".to_string(),
                "Digest realm=\"http-auth@example.org\", qop=\"auth, auth-int\", algorithm=MD5, nonce=\"7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v\", opaque=\"FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS\"".to_string(),
            ),
            (
                "WWW-Authenticate".to_string(),
                "Digest realm=\"http-auth@example.org\", qop=\"auth, auth-int\", algorithm=SHA-256, nonce=\"7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v\", opaque=\"FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS\"".to_string(),
            ),
        ];
        let (sha_ch_opt, _) = parse_www_authenticate_headers(&sha_headers);
        let sha_ch = sha_ch_opt.expect("Expected SHA-256 DigestChallenge preferred over MD5");
        assert_eq!(sha_ch.algorithm, DigestAlgorithm::Sha256);
        let sha_resp = compute_digest_response(
            &sha_ch,
            "Mufasa",
            "Circle of Life",
            "GET",
            "/dir/index.html",
            1,
            "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ",
        );
        assert_eq!(
            sha_resp,
            "753927fa0e85d155564e2e272a28d1802ca10daf4496794697cf8db5856cb6c1"
        );

        // MD5-sess deterministic check
        let sess_headers = vec![(
            "WWW-Authenticate".to_string(),
            "Digest realm=\"Dahua\", nonce=\"abc12345\", qop=\"auth\", algorithm=MD5-sess"
                .to_string(),
        )];
        let (sess_ch, _) = parse_www_authenticate_headers(&sess_headers);
        let sess_ch = sess_ch.expect("MD5-sess challenge");
        assert_eq!(sess_ch.algorithm, DigestAlgorithm::Md5Sess);
        let hdr = build_digest_authorization(
            &sess_ch,
            "admin",
            "secret",
            "DESCRIBE",
            "rtsp://127.0.0.1:554/cam/realmonitor?channel=1&subtype=1",
            1,
            Some("deadbeefcafebabe"),
        );
        assert!(hdr.contains("algorithm=MD5-sess"));
        assert!(hdr.contains("nc=00000001"));
    }

    #[test]
    fn test_header_sanitization_and_length_cap() {
        let dirty = format!("realm\"with\\slash\r\nInjected: bad\x7f{}", "x".repeat(300));
        let clean_tok = sanitize_header_token(&dirty, 256);
        assert_eq!(clean_tok.chars().count(), 256);
        assert!(!clean_tok.contains('\r'));
        assert!(!clean_tok.contains('\n'));
        assert!(!clean_tok.contains('\x7f'));

        let escaped = escape_quoted_header_val("admin\"\\test\r\n", 256);
        assert_eq!(escaped, "admin\\\"\\\\test");
    }

    #[tokio::test]
    async fn test_rtsp_proxy_cseq_offset_content_base_scale_and_interleaved_rtp() {
        let mock_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_port = mock_listener.local_addr().unwrap().port();

        let server_task = tokio::spawn(async move {
            let (stream, _) = mock_listener.accept().await.unwrap();
            let (rh, mut wh) = stream.into_split();
            let mut reader = BufReader::new(rh);
            let mut scratch = Vec::new();

            // 1. OPTIONS CSeq 1 -> 200 OK
            let Some(WireFrame::Rtsp(req1)) = read_wire_frame(&mut reader, &mut scratch).await.unwrap() else {
                panic!("Expected OPTIONS");
            };
            assert!(req1.start_line.starts_with(
                &format!("OPTIONS rtsp://127.0.0.1:{}/cam/playback?channel=1&subtype=0", mock_port)
            ));
            assert_eq!(extract_cseq(&req1.headers), Some(1));
            wh.write_all(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nPublic: OPTIONS, DESCRIBE, SETUP, PLAY\r\n\r\n")
                .await
                .unwrap();

            // 2. DESCRIBE CSeq 2 (no auth) -> 401 Digest
            let Some(WireFrame::Rtsp(req2)) = read_wire_frame(&mut reader, &mut scratch).await.unwrap() else {
                panic!("Expected unauthenticated DESCRIBE");
            };
            assert_eq!(extract_cseq(&req2.headers), Some(2));
            wh.write_all(
                b"RTSP/1.0 401 Unauthorized\r\nCSeq: 2\r\nWWW-Authenticate: Digest realm=\"DahuaRtsp\", nonce=\"99887766\", qop=\"auth\"\r\n\r\n",
            )
            .await
            .unwrap();

            // 3. Replayed DESCRIBE must arrive with CSeq 3 and valid Digest Authorization
            let Some(WireFrame::Rtsp(req3)) = read_wire_frame(&mut reader, &mut scratch).await.unwrap() else {
                panic!("Expected replayed DESCRIBE");
            };
            assert_eq!(extract_cseq(&req3.headers), Some(3));
            let auth_val = req3
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                .map(|(_, v)| v.clone())
                .expect("Missing Authorization on replay");
            assert!(auth_val.starts_with("Digest username=\"admin\""));

            let sdp = b"v=0\r\nm=video 0 RTP/AVP 96\r\na=control:trackID=0\r\n";
            let cb_hdr = format!(
                "rtsp://127.0.0.1:{}/cam/playback?channel=1&subtype=0/",
                mock_port
            );
            let resp3 = format!(
                "RTSP/1.0 200 OK\r\nCSeq: 3\r\nContent-Base: {}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n",
                cb_hdr,
                sdp.len()
            );
            wh.write_all(resp3.as_bytes()).await.unwrap();
            wh.write_all(sdp).await.unwrap();

            // 4. Client sends SETUP CSeq 3 on /<token>/trackID=0 -> upstream must see CSeq 4 & Content-Base + trackID=0 + cached Digest
            let Some(WireFrame::Rtsp(req4)) = read_wire_frame(&mut reader, &mut scratch).await.unwrap() else {
                panic!("Expected SETUP");
            };
            assert_eq!(extract_cseq(&req4.headers), Some(4));
            assert!(req4.start_line.starts_with(&format!(
                "SETUP rtsp://127.0.0.1:{}/cam/playback?channel=1&subtype=0/trackID=0 RTSP/1.0",
                mock_port
            )));
            assert!(req4
                .headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("authorization") && v.contains("nc=00000002")));
            wh.write_all(
                b"RTSP/1.0 200 OK\r\nCSeq: 4\r\nSession: 771122\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n",
            )
            .await
            .unwrap();

            // 5. Client sends PLAY CSeq 4 -> upstream must see CSeq 5 + Scale: 2.0
            let Some(WireFrame::Rtsp(req5)) = read_wire_frame(&mut reader, &mut scratch).await.unwrap() else {
                panic!("Expected PLAY");
            };
            assert_eq!(extract_cseq(&req5.headers), Some(5));
            assert!(req5
                .headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("scale") && v == "2.0"));
            wh.write_all(b"RTSP/1.0 200 OK\r\nCSeq: 5\r\nSession: 771122\r\n\r\n")
                .await
                .unwrap();

            // 6. Send interleaved '$' RTP frame upstream -> client, then read '$' RTCP frame client -> upstream
            let rtp_pkt: [u8; 10] = [b'$', 0x00, 0x00, 0x06, 0x80, 0x60, 0x01, 0x02, 0xde, 0xad];
            wh.write_all(&rtp_pkt).await.unwrap();

            let Some(WireFrame::Rtp(rtcp_len)) = read_wire_frame(&mut reader, &mut scratch).await.unwrap() else {
                panic!("Expected client RTCP interleaved packet");
            };
            assert_eq!(&scratch[..rtcp_len], &[b'$', 0x01, 0x00, 0x04, 1, 2, 3, 4]);
        });

        let cb = AuthCircuitBreaker::new();
        let gw = AuthGateway::start(Arc::clone(&cb)).unwrap();
        let mut cfg = NvrEnvConfig::default();
        cfg.host = "127.0.0.1".to_string();
        cfg.port = mock_port;
        cfg.username = "admin".to_string();
        cfg.password = "pass123".to_string();

        let minted = gw
            .mint_rtsp_url(&cfg, "/cam/playback?channel=1&subtype=0", Some(2.0))
            .unwrap();

        let stream = TcpStream::connect(format!("127.0.0.1:{}", gw.local_port()))
            .await
            .unwrap();
        let (crh, mut cwh) = stream.into_split();
        let mut creader = BufReader::new(crh);
        let mut cscratch = Vec::new();

        // Client OPTIONS (CSeq 1)
        cwh.write_all(format!("OPTIONS {} RTSP/1.0\r\nCSeq: 1\r\n\r\n", minted).as_bytes())
            .await
            .unwrap();
        let Some(WireFrame::Rtsp(r1)) = read_wire_frame(&mut creader, &mut cscratch).await.unwrap() else {
            panic!("Expected OPTIONS resp");
        };
        assert_eq!(extract_cseq(&r1.headers), Some(1));

        // Client DESCRIBE (CSeq 2) -> proxy handles 401 transparently and returns 200 OK with CSeq 2!
        cwh.write_all(format!("DESCRIBE {} RTSP/1.0\r\nCSeq: 2\r\n\r\n", minted).as_bytes())
            .await
            .unwrap();
        let Some(WireFrame::Rtsp(r2)) = read_wire_frame(&mut creader, &mut cscratch).await.unwrap() else {
            panic!("Expected DESCRIBE resp");
        };
        assert_eq!(parse_rtsp_status_code(&r2.start_line), Some(200));
        assert_eq!(extract_cseq(&r2.headers), Some(2));
        let rewritten_cb = r2
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-base"))
            .map(|(_, v)| v.clone())
            .unwrap();
        assert_eq!(rewritten_cb, format!("{}/", minted));

        // Client SETUP (CSeq 3) using rewritten Content-Base + trackID=0
        cwh.write_all(
            format!(
                "SETUP {}trackID=0 RTSP/1.0\r\nCSeq: 3\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n",
                rewritten_cb
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let Some(WireFrame::Rtsp(r3)) = read_wire_frame(&mut creader, &mut cscratch).await.unwrap() else {
            panic!("Expected SETUP resp");
        };
        assert_eq!(extract_cseq(&r3.headers), Some(3));

        // Client PLAY (CSeq 4)
        cwh.write_all(
            format!(
                "PLAY {} RTSP/1.0\r\nCSeq: 4\r\nSession: 771122\r\n\r\n",
                rewritten_cb
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let Some(WireFrame::Rtsp(r4)) = read_wire_frame(&mut creader, &mut cscratch).await.unwrap() else {
            panic!("Expected PLAY resp");
        };
        assert_eq!(extract_cseq(&r4.headers), Some(4));

        // Receive '$' RTP packet untouched
        let Some(WireFrame::Rtp(rtp_len)) = read_wire_frame(&mut creader, &mut cscratch).await.unwrap() else {
            panic!("Expected RTP packet");
        };
        assert_eq!(
            &cscratch[..rtp_len],
            &[b'$', 0x00, 0x00, 0x06, 0x80, 0x60, 0x01, 0x02, 0xde, 0xad]
        );

        // Send '$' RTCP packet untouched
        cwh.write_all(&[b'$', 0x01, 0x00, 0x04, 1, 2, 3, 4])
            .await
            .unwrap();

        server_task.await.unwrap();
        assert!(!cb.is_tripped());

        // Single-use token check: a second TCP connection reusing `minted` must be rejected 401
        let reuse_stream = TcpStream::connect(format!("127.0.0.1:{}", gw.local_port()))
            .await
            .unwrap();
        let (rrh, mut rwh) = reuse_stream.into_split();
        let mut rreader = BufReader::new(rrh);
        rwh.write_all(format!("OPTIONS {} RTSP/1.0\r\nCSeq: 1\r\n\r\n", minted).as_bytes())
            .await
            .unwrap();
        let Some(WireFrame::Rtsp(reuse_resp)) = read_wire_frame(&mut rreader, &mut cscratch).await.unwrap() else {
            panic!("Expected 401 on reused token");
        };
        assert_eq!(parse_rtsp_status_code(&reuse_resp.start_line), Some(401));

        gw.shutdown();
    }

    #[tokio::test]
    async fn test_basic_auth_downgrade_blocked_on_cloud_p2p_and_canary_probe() {
        let mock_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_port = mock_listener.local_addr().unwrap().port();

        let server_task = tokio::spawn(async move {
            // Connection 1: proxy session where rogue P2P peer offers Basic only
            let (s1, _) = mock_listener.accept().await.unwrap();
            let (rh1, mut wh1) = s1.into_split();
            let mut r1 = BufReader::new(rh1);
            let mut scratch = Vec::new();
            let Some(WireFrame::Rtsp(req)) = read_wire_frame(&mut r1, &mut scratch).await.unwrap() else {
                panic!("Expected DESCRIBE");
            };
            assert!(req
                .headers
                .iter()
                .all(|(k, _)| !k.eq_ignore_ascii_case("authorization")));
            wh1.write_all(
                b"RTSP/1.0 401 Unauthorized\r\nCSeq: 7\r\nWWW-Authenticate: Basic realm=\"RogueRelay\"\r\n\r\n",
            )
            .await
            .unwrap();
            // Must NOT receive any Basic Authorization follow-up!
            assert!(read_wire_frame(&mut r1, &mut scratch).await.unwrap().is_none());

            // Connection 2: canary probe with wrong password on Digest -> 401 twice
            let (s2, _) = mock_listener.accept().await.unwrap();
            let (rh2, mut wh2) = s2.into_split();
            let mut r2 = BufReader::new(rh2);
            let _ = read_wire_frame(&mut r2, &mut scratch).await.unwrap();
            wh2.write_all(
                b"RTSP/1.0 401 Unauthorized\r\nCSeq: 1\r\nWWW-Authenticate: Digest realm=\"NVR\", nonce=\"aa11bb22\"\r\n\r\n",
            )
            .await
            .unwrap();
            let _ = read_wire_frame(&mut r2, &mut scratch).await.unwrap();
            wh2.write_all(b"RTSP/1.0 401 Unauthorized\r\nCSeq: 2\r\n\r\n")
                .await
                .unwrap();
        });

        let cb = AuthCircuitBreaker::new();
        let gw = AuthGateway::start(Arc::clone(&cb)).unwrap();

        let mut p2p_cfg = NvrEnvConfig::default();
        p2p_cfg.use_cloud_p2p = true;
        p2p_cfg.p2p_serial = "SN123456789".to_string();
        p2p_cfg.p2p_local_port = mock_port;
        p2p_cfg.username = "admin".to_string();
        p2p_cfg.password = "supersecret".to_string();

        let minted = gw
            .mint_rtsp_url(&p2p_cfg, "/cam/realmonitor?channel=1&subtype=1", None)
            .unwrap();
        let client = TcpStream::connect(format!("127.0.0.1:{}", gw.local_port()))
            .await
            .unwrap();
        let (crh, mut cwh) = client.into_split();
        let mut creader = BufReader::new(crh);
        let mut cscratch = Vec::new();

        cwh.write_all(format!("DESCRIBE {} RTSP/1.0\r\nCSeq: 7\r\n\r\n", minted).as_bytes())
            .await
            .unwrap();
        let Some(WireFrame::Rtsp(resp)) = read_wire_frame(&mut creader, &mut cscratch).await.unwrap() else {
            panic!("Expected 401 forwarded to client");
        };
        assert_eq!(parse_rtsp_status_code(&resp.start_line), Some(401));
        assert_eq!(extract_cseq(&resp.headers), Some(7));
        drop(cwh);
        drop(creader);
        assert!(cb.is_tripped());

        cb.reset();
        assert!(!cb.is_tripped());

        // Run synchronous canary probe on blocking thread to verify wrong-password tripping
        let gw_clone = Arc::clone(&gw);
        let canary_res = tokio::task::spawn_blocking(move || gw_clone.probe_canary_auth(&p2p_cfg))
            .await
            .unwrap();
        assert!(canary_res.is_err());
        assert!(canary_res.unwrap_err().starts_with("401 Unauthorized"));
        assert!(cb.is_tripped());

        server_task.await.unwrap();
        gw.shutdown();
    }

    #[test]
    fn test_crit05_utf8_boundary_and_crit06_hikvision_nat_uri_rewrites() {
        // [CRIT-05]: multibyte UTF-8 characters straddling byte index 32 MUST NOT panic!
        let multibyte_32 = "rtsp://127.0.0.1:8554/0123456789abcdef0123456789abcdeé/trackID=1";
        assert_eq!(multibyte_32.find("é").unwrap(), 22 + 31);
        assert!(extract_token_and_suffix(multibyte_32).is_none());
        assert!(extract_token_and_suffix("rtsp://127.0.0.1:8554/ééééééééééééééééé").is_none());

        let tok = "0123456789abcdef0123456789abcdef".to_string();
        let grant = TokenGrant {
            token: tok.clone(),
            upstream_host: "127.0.0.1".to_string(),
            upstream_port: 41234,
            rtsp_path_and_query:
                "/Streaming/tracks/101?starttime=20261118T080000Z&endtime=20261118T080500Z"
                    .to_string(),
            username: "admin".to_string(),
            password: Zeroizing::new("secret".to_string()),
            allow_basic_auth: false,
            playback_scale: None,
            expires_at: Instant::now() + Duration::from_secs(20),
        };

        // 1. [CRIT-06]: Hikvision absolute SDP control URL MUST NOT double path or prepend query!
        let hik_abs =
            "rtsp://192.168.1.64:554/Streaming/Channels/101/trackID=1?transportmode=unicast";
        assert_eq!(
            rewrite_client_uri(
                hik_abs,
                &grant,
                Some("rtsp://192.168.1.64:554/Streaming/Channels/101/")
            )
            .as_deref(),
            Some(
                "rtsp://127.0.0.1:41234/Streaming/Channels/101/trackID=1?transportmode=unicast"
            )
        );

        // 2. [CRIT-06]: NAT / Cloud P2P Content-Base host:port normalized to grant.upstream_host:port
        let lan_cb = "rtsp://192.168.1.108:554/cam/realmonitor?channel=1&subtype=1/";
        assert_eq!(
            rewrite_client_uri(
                &format!("rtsp://127.0.0.1:9999/{}/trackID=0", tok),
                &grant,
                Some(lan_cb)
            )
            .as_deref(),
            Some("rtsp://127.0.0.1:41234/cam/realmonitor?channel=1&subtype=1/trackID=0")
        );

        // 3. [CRIT-06]: suffix == "/" MUST NOT append '/' onto query-string playback URLs!
        assert_eq!(
            rewrite_client_uri(
                &format!("rtsp://127.0.0.1:9999/{}/", tok),
                &grant,
                None
            )
            .as_deref(),
            Some(
                "rtsp://127.0.0.1:41234/Streaming/tracks/101?starttime=20261118T080000Z&endtime=20261118T080500Z"
            )
        );
        assert_eq!(
            rewrite_client_uri(
                &format!("rtsp://127.0.0.1:9999/{}/", tok),
                &grant,
                Some("rtsp://192.168.1.64:554/Streaming/tracks/101?starttime=20261118T080000Z&endtime=20261118T080500Z")
            )
            .as_deref(),
            Some(
                "rtsp://127.0.0.1:41234/Streaming/tracks/101?starttime=20261118T080000Z&endtime=20261118T080500Z"
            )
        );
    }

    #[tokio::test]
    async fn test_sha256_to_md5_fallback_403_no_trip_and_leading_crlf_rtp() {
        let mock_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_port = mock_listener.local_addr().unwrap().port();

        let server_task = tokio::spawn(async move {
            // Session 1: Canary probe offers SHA-256 + MD5, rejects SHA-256 (stale=false), accepts MD5 with 403 ACL!
            let (s1, _) = mock_listener.accept().await.unwrap();
            let (rh1, mut wh1) = s1.into_split();
            let mut r1 = BufReader::new(rh1);
            let mut scratch = Vec::new();

            let _ = read_wire_frame(&mut r1, &mut scratch).await.unwrap();
            wh1.write_all(
                b"RTSP/1.0 401 Unauthorized\r\nCSeq: 1\r\n\
                  WWW-Authenticate: Digest realm=\"NVR\", nonce=\"n1\", qop=\"auth\", algorithm=SHA-256\r\n\
                  WWW-Authenticate: Digest realm=\"NVR\", nonce=\"n1\", qop=\"auth\", algorithm=MD5\r\n\r\n",
            )
            .await
            .unwrap();

            // Expect CSeq 2 with SHA-256 -> reject 401 (stale=false)
            let Some(WireFrame::Rtsp(req2)) = read_wire_frame(&mut r1, &mut scratch).await.unwrap() else {
                panic!("Expected SHA-256 attempt");
            };
            assert!(req2
                .headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("authorization") && v.contains("algorithm=SHA-256")));
            wh1.write_all(
                b"RTSP/1.0 401 Unauthorized\r\nCSeq: 2\r\n\
                  WWW-Authenticate: Digest realm=\"NVR\", nonce=\"n2\", qop=\"auth\", algorithm=MD5\r\n\r\n",
            )
            .await
            .unwrap();

            // Expect CSeq 3 with MD5 fallback -> return 403 Forbidden (per-channel ACL, valid creds!)
            let Some(WireFrame::Rtsp(req3)) = read_wire_frame(&mut r1, &mut scratch).await.unwrap() else {
                panic!("Expected MD5 fallback attempt");
            };
            assert!(req3
                .headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("authorization") && v.contains("algorithm=MD5")));
            wh1.write_all(b"RTSP/1.0 403 Forbidden\r\nCSeq: 3\r\n\r\n")
                .await
                .unwrap();

            // Session 2: Proxy session with SHA-256 -> MD5 fallback + stray \r\n before '$' RTP frame
            let (s2, _) = mock_listener.accept().await.unwrap();
            let (rh2, mut wh2) = s2.into_split();
            let mut r2 = BufReader::new(rh2);

            let _ = read_wire_frame(&mut r2, &mut scratch).await.unwrap();
            wh2.write_all(
                b"RTSP/1.0 401 Unauthorized\r\nCSeq: 1\r\n\
                  WWW-Authenticate: Digest realm=\"NVR\", nonce=\"n10\", qop=\"auth\", algorithm=SHA-256\r\n\
                  WWW-Authenticate: Digest realm=\"NVR\", nonce=\"n10\", qop=\"auth\", algorithm=MD5\r\n\r\n",
            )
            .await
            .unwrap();

            // Replay 1 (SHA-256, CSeq 2) -> 401
            let Some(WireFrame::Rtsp(p_req2)) = read_wire_frame(&mut r2, &mut scratch).await.unwrap() else {
                panic!("Expected proxy SHA-256");
            };
            assert_eq!(extract_cseq(&p_req2.headers), Some(2));
            wh2.write_all(b"RTSP/1.0 401 Unauthorized\r\nCSeq: 2\r\n\r\n")
                .await
                .unwrap();

            // Replay 2 (MD5 fallback from saved resp1 challenge, CSeq 3) -> 200 OK + leading CRLF before RTP
            let Some(WireFrame::Rtsp(p_req3)) = read_wire_frame(&mut r2, &mut scratch).await.unwrap() else {
                panic!("Expected proxy MD5 fallback");
            };
            assert_eq!(extract_cseq(&p_req3.headers), Some(3));
            assert!(p_req3
                .headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("authorization") && v.contains("algorithm=MD5")));
            wh2.write_all(b"RTSP/1.0 200 OK\r\nCSeq: 3\r\n\r\n\r\n$\x00\x00\x02\xaa\xbb")
                .await
                .unwrap();
        });

        let cb = AuthCircuitBreaker::new();
        let gw = AuthGateway::start(Arc::clone(&cb)).unwrap();
        let mut cfg = NvrEnvConfig::default();
        cfg.host = "127.0.0.1".to_string();
        cfg.port = mock_port;
        cfg.username = "admin".to_string();
        cfg.password = "pass".to_string();

        let gw_c = Arc::clone(&gw);
        let cfg_c = cfg.clone();
        let outcome = tokio::task::spawn_blocking(move || gw_c.probe_canary_auth_detailed(&cfg_c))
            .await
            .unwrap();
        assert_eq!(outcome, CanaryProbeOutcome::AuthOk);
        assert!(!cb.is_tripped());

        let minted = gw
            .mint_rtsp_url(&cfg, "/cam/realmonitor?channel=1&subtype=1", None)
            .unwrap();
        let client = TcpStream::connect(format!("127.0.0.1:{}", gw.local_port()))
            .await
            .unwrap();
        let (crh, mut cwh) = client.into_split();
        let mut creader = BufReader::new(crh);
        let mut cscratch = Vec::new();

        cwh.write_all(format!("DESCRIBE {} RTSP/1.0\r\nCSeq: 1\r\n\r\n", minted).as_bytes())
            .await
            .unwrap();
        let Some(WireFrame::Rtsp(ok_resp)) = read_wire_frame(&mut creader, &mut cscratch).await.unwrap() else {
            panic!("Expected 200 OK after SHA-256 -> MD5 fallback");
        };
        assert_eq!(parse_rtsp_status_code(&ok_resp.start_line), Some(200));
        assert_eq!(extract_cseq(&ok_resp.headers), Some(1));

        let Some(WireFrame::Rtp(rtp_len)) = read_wire_frame(&mut creader, &mut cscratch).await.unwrap() else {
            panic!("Expected RTP frame after stray CRLF");
        };
        assert_eq!(&cscratch[..rtp_len], b"$\x00\x00\x02\xaa\xbb");
        assert!(!cb.is_tripped());

        server_task.await.unwrap();
        gw.shutdown();
    }
}
