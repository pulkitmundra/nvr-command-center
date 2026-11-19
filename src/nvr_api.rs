use crate::auth_gateway::{
    digest::{
        build_digest_authorization, generate_random_hex_16, md5_upper_hex,
        parse_www_authenticate_all, parse_www_authenticate_headers, sanitize_header_token,
        DigestChallenge,
    },
    AuthCircuitBreaker, AuthGateway, CanaryProbeOutcome,
};
use crate::config::{local_naive_to_utc, redact_secret_in_text, validate_nvr_host, NvrEnvConfig};
use chrono::{Datelike, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use quick_xml::{events::Event, Reader};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time;
use zeroize::Zeroizing;

const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(15);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_HTTP_HEADER_BYTES: usize = 32 * 1024;
const MAX_API_BODY_BYTES: usize = 2 * 1024 * 1024;
#[allow(dead_code)]
const DEFAULT_MAX_DHAV_STREAM_BYTES: u64 = 160 * 1024 * 1024;
const DHAV_CHUNK_SIZE: usize = 32 * 1024;
const MAX_SEARCH_ITEMS: usize = 4000;
const PAGE_BATCH_SIZE: usize = 100;
const COVERAGE_MERGE_GAP_MS: i64 = 2000;
const EVENT_MERGE_GAP_MS: i64 = 1000;
const MAX_ISAPI_XML_DEPTH: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NvrEventKind {
    Continuous,
    Motion,
    Human,
    Vehicle,
    Intrusion,
    Alarm,
}

impl NvrEventKind {
    pub fn specificity_rank(self) -> u8 {
        match self {
            Self::Continuous => 0,
            Self::Motion => 1,
            Self::Alarm => 2,
            Self::Intrusion => 3,
            Self::Vehicle => 4,
            Self::Human => 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedSpan {
    pub channel: usize,
    pub start_ms: i64,
    pub end_ms: i64,
    pub kind: NvrEventKind,
    pub file_path: Option<String>,
    pub byte_len: u64,
    pub main_stream: bool,
}

#[derive(Debug, Clone)]
pub struct DayTimelineIndex {
    #[allow(dead_code)]
    pub channel: usize,
    pub date: NaiveDate,
    pub coverage: Vec<RecordedSpan>,
    pub events: Vec<RecordedSpan>,
    pub api_supported: bool,
    pub note: Option<String>,
}

pub fn local_naive_to_epoch_ms(dt: NaiveDateTime) -> i64 {
    if let Some(ldt) = Local.from_local_datetime(&dt).single() {
        return ldt.timestamp_millis();
    }
    if let Some(ldt) = Local.from_local_datetime(&dt).earliest() {
        return ldt.timestamp_millis();
    }
    if let Some(shifted) = dt.checked_add_signed(chrono::Duration::hours(1)) {
        if let Some(ldt) = Local.from_local_datetime(&shifted).earliest() {
            return ldt.timestamp_millis();
        }
    }
    dt.and_utc().timestamp_millis()
}

pub fn epoch_ms_to_local_naive(epoch_ms: i64) -> NaiveDateTime {
    match Local.timestamp_millis_opt(epoch_ms).single() {
        Some(ldt) => ldt.naive_local(),
        None => Utc
            .timestamp_millis_opt(epoch_ms)
            .single()
            .map(|u| u.with_timezone(&Local).naive_local())
            .unwrap_or_default(),
    }
}

struct CachedDigestEntry {
    challenge: DigestChallenge,
    nc: AtomicU32,
}

#[derive(Clone)]
struct HttpTarget {
    host: String,
    port: u16,
    username: String,
    password: Zeroizing<String>,
    is_cloud_p2p: bool,
}

impl HttpTarget {
    fn from_env(env_cfg: &NvrEnvConfig) -> Result<Self, String> {
        let host = validate_nvr_host(env_cfg.active_http_host())?;
        let port = env_cfg.active_http_port();
        if port == 0 {
            return Err("Active HTTP port is 0 (P2P HTTP tunnel not ready)".to_string());
        }
        Ok(Self {
            host,
            port,
            username: env_cfg.username.clone(),
            password: Zeroizing::new(env_cfg.password.clone()),
            is_cloud_p2p: env_cfg.is_cloud_p2p_active(),
        })
    }

    fn cache_key(&self) -> (String, u16, String) {
        (self.host.clone(), self.port, self.username.clone())
    }
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyFraming {
    Chunked,
    ContentLength(usize),
    UntilEof,
}

type DigestCacheMap = HashMap<(String, u16, String), Arc<CachedDigestEntry>>;

fn build_cached_auth_from_map(
    cache: &Mutex<DigestCacheMap>,
    target: &HttpTarget,
    method: &str,
    uri: &str,
) -> Option<String> {
    let key = target.cache_key();
    let entry = {
        let guard = cache.lock().ok()?;
        guard.get(&key).cloned()?
    };
    let nc = entry
        .nc
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
        .max(1);
    Some(build_digest_authorization(
        &entry.challenge,
        &target.username,
        &target.password,
        method,
        uri,
        nc,
        None,
    ))
}

async fn await_with_abort<Fut, T>(
    fut: Fut,
    total_timeout: Duration,
    abort_flag: Option<&AtomicBool>,
    timeout_msg: &str,
) -> Result<T, String>
where
    Fut: std::future::Future<Output = Result<T, String>>,
{
    if abort_flag.is_some_and(|f| f.load(Ordering::Acquire)) {
        return Err("DHAV stream aborted".to_string());
    }
    tokio::pin!(fut);
    let start = time::Instant::now();
    loop {
        if abort_flag.is_some_and(|f| f.load(Ordering::Acquire)) {
            return Err("DHAV stream aborted".to_string());
        }
        let elapsed = start.elapsed();
        if elapsed >= total_timeout {
            return Err(timeout_msg.to_string());
        }
        let rem = total_timeout - elapsed;
        let step = if abort_flag.is_some() {
            rem.min(Duration::from_millis(100))
        } else {
            rem
        };
        match time::timeout(step, &mut fut).await {
            Ok(res) => return res,
            Err(_) => continue,
        }
    }
}

type DigestColdLockMap = HashMap<(String, u16, String), Arc<AsyncMutex<()>>>;

#[allow(dead_code)]
pub fn test_nvr_connection_sync(env_cfg: &NvrEnvConfig) -> Result<String, String> {
    env_cfg.validate_for_save()?;

    let is_cloud = env_cfg.is_cloud_p2p_active();
    let tcp_timeout = Duration::from_millis(if is_cloud { 4_500 } else { 2_500 });
    if let Err(msg) = crate::stream::connect_rtsp_probe_socket(env_cfg, tcp_timeout) {
        return Err(redact_secret_in_text(
            &format!("RTSP unreachable: {}", msg),
            &env_cfg.password,
        ));
    }

    let gw = AuthGateway::start(AuthCircuitBreaker::new())
        .map_err(|e| format!("Failed to start isolated AuthGateway: {}", e))?;

    let rtsp_summary = match gw.probe_canary_auth_detailed(env_cfg) {
        CanaryProbeOutcome::AuthRejected(_) => {
            return Err(
                "RTSP authentication failed (401) - check username & password".to_string(),
            );
        }
        CanaryProbeOutcome::Inconclusive => {
            crate::stream::probe_rtsp_options_alive(env_cfg, Duration::from_millis(1_800))
                .map_err(|msg| {
                    redact_secret_in_text(
                        &format!("RTSP handshake failed: {}", msg),
                        &env_cfg.password,
                    )
                })?;
            "RTSP OK"
        }
        CanaryProbeOutcome::AuthOk => "RTSP Auth OK",
    };

    let cfg_for_http = env_cfg.clone();
    let run_http_probe = move || -> Result<String, String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("Failed to initialize HTTP probe runtime: {}", e))?;
        let client = NvrHttpClient::new(AuthCircuitBreaker::new());
        rt.block_on(client.probe_http_api_raw(&cfg_for_http))
    };

    let http_res = if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::spawn(run_http_probe)
            .join()
            .unwrap_or_else(|_| Err("HTTP probe worker panicked".to_string()))
    } else {
        run_http_probe()
    };

    match http_res {
        Ok(http_msg) => Ok(redact_secret_in_text(
            &format!("{} | {}", rtsp_summary, http_msg),
            &env_cfg.password,
        )),
        Err(http_err) => {
            let lower_raw = http_err.to_ascii_lowercase();
            let is_auth_err = lower_raw.contains("401") || lower_raw.contains("unauthorized");
            let clean_err = redact_secret_in_text(&http_err, &env_cfg.password);
            if is_auth_err {
                Err(format!(
                    "{} | HTTP Auth Failed (401) - {}",
                    rtsp_summary, clean_err
                ))
            } else {
                Ok(format!(
                    "{} | HTTP API unavailable ({}) - RTSP playback fallback",
                    rtsp_summary, clean_err
                ))
            }
        }
    }
}

pub struct NvrHttpClient {
    circuit_breaker: Arc<AuthCircuitBreaker>,
    digest_cache: Arc<Mutex<DigestCacheMap>>,
    digest_locks: Mutex<DigestColdLockMap>,
    dahua_channel_base: Mutex<HashMap<(String, u16), u8>>,
    rpc2_preferred: Mutex<HashSet<(String, u16)>>,
    rpc2_sessions: Mutex<HashMap<(String, u16, String), String>>,
}

impl NvrHttpClient {
    pub fn new(circuit_breaker: Arc<AuthCircuitBreaker>) -> Arc<Self> {
        Arc::new(Self {
            circuit_breaker,
            digest_cache: Arc::new(Mutex::new(HashMap::new())),
            digest_locks: Mutex::new(HashMap::new()),
            dahua_channel_base: Mutex::new(HashMap::new()),
            rpc2_preferred: Mutex::new(HashSet::new()),
            rpc2_sessions: Mutex::new(HashMap::new()),
        })
    }

    async fn probe_http_api_raw(&self, env_cfg: &NvrEnvConfig) -> Result<String, String> {
        let isolated = Self::new(AuthCircuitBreaker::new());
        let today = Local::now().date_naive();
        match isolated.query_day_timeline(env_cfg, 1, today).await {
            Ok(idx) if idx.api_supported => Ok(format!(
                "HTTP Playback API OK ({} spans today on Ch 1)",
                idx.coverage.len()
            )),
            Ok(idx) => {
                let raw_note = idx.note.unwrap_or_else(|| {
                    "HTTP Timeline API unavailable (RTSP playback fallback)".to_string()
                });
                Ok(redact_secret_in_text(&raw_note, &env_cfg.password))
            }
            Err(e) => Err(e),
        }
    }

    #[allow(dead_code)]
    pub async fn probe_http_api(&self, env_cfg: &NvrEnvConfig) -> Result<String, String> {
        self.probe_http_api_raw(env_cfg)
            .await
            .map_err(|e| redact_secret_in_text(&e, &env_cfg.password))
    }

    pub async fn query_day_timeline(
        &self,
        env_cfg: &NvrEnvConfig,
        channel: usize,
        date: NaiveDate,
    ) -> Result<DayTimelineIndex, String> {
        if self.circuit_breaker.is_tripped() {
            return Err("401 Unauthorized: Auth circuit breaker is tripped".to_string());
        }
        let ch = channel.clamp(1, 64);
        let target = HttpTarget::from_env(env_cfg)?;

        if env_cfg.brand.eq_ignore_ascii_case("hikvision") {
            return self.query_hikvision_day(&target, ch, date).await;
        }

        self.query_dahua_day(&target, ch, date).await
    }

    pub async fn query_month_recorded_days(
        &self,
        env_cfg: &NvrEnvConfig,
        channel: usize,
        year: i32,
        month: u32,
    ) -> Result<Vec<u32>, String> {
        if self.circuit_breaker.is_tripped() {
            return Err("401 Unauthorized: Auth circuit breaker is tripped".to_string());
        }
        if !(1..=12).contains(&month) {
            return Err(format!("Invalid month: {}", month));
        }
        let first_date = NaiveDate::from_ymd_opt(year, month, 1)
            .ok_or_else(|| format!("Invalid calendar year/month: {}-{}", year, month))?;
        let days_in_month = last_day_of_month(year, month);
        let last_date = NaiveDate::from_ymd_opt(year, month, days_in_month)
            .ok_or_else(|| "Invalid end of month".to_string())?;

        let ch = channel.clamp(1, 64);
        let target = HttpTarget::from_env(env_cfg)?;

        if env_cfg.brand.eq_ignore_ascii_case("hikvision") {
            return self
                .query_hikvision_month_days(&target, ch, year, month, first_date, last_date)
                .await;
        }

        self.query_dahua_month_days(&target, ch, year, month, first_date, last_date)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn stream_dhav_time_slice_cancellable<F>(
        &self,
        env_cfg: &NvrEnvConfig,
        channel: usize,
        subtype: u8,
        start_local: NaiveDateTime,
        end_local: NaiveDateTime,
        max_bytes: u64,
        abort_flag: Option<&AtomicBool>,
        mut on_chunk: F,
    ) -> Result<u64, String>
    where
        F: FnMut(&[u8]) -> bool,
    {
        if abort_flag.is_some_and(|f| f.load(Ordering::Acquire)) {
            return Err("DHAV stream aborted".to_string());
        }
        if self.circuit_breaker.is_tripped() {
            return Err("401 Unauthorized: Auth circuit breaker is tripped".to_string());
        }
        if end_local <= start_local {
            return Err("Invalid time slice: end_local must be greater than start_local".to_string());
        }

        let target = HttpTarget::from_env(env_cfg)?;
        // Dahua loadfile.cgi is ALWAYS 1-based (channel=1..32)
        let ch_1based = channel.clamp(1, 32);
        let sub = if subtype == 0 { 0 } else { 1 };
        let path_and_query = format!(
            "/cgi-bin/loadfile.cgi?action=startLoad&channel={}&startTime={}%20{}&endTime={}%20{}&subtype={}",
            ch_1based,
            start_local.format("%Y-%m-%d"),
            start_local.format("%H:%M:%S"),
            end_local.format("%Y-%m-%d"),
            end_local.format("%H:%M:%S"),
            sub
        );

        let stream_opt = match self
            .open_authenticated_stream(&target, "GET", &path_and_query, abort_flag)
            .await
        {
            Ok(opt) => opt,
            Err(e) if is_dahua_no_record_text(&e) => return Ok(0),
            Err(e) => return Err(e),
        };

        let Some((mut reader, framing)) = stream_opt else {
            return Ok(0);
        };

        match stream_http_body_chunks(&mut reader, framing, max_bytes, abort_flag, &mut on_chunk)
            .await
        {
            Ok(n) => Ok(n),
            Err(e) if is_dahua_no_record_text(&e) => Ok(0),
            Err(e) => Err(e),
        }
    }

    #[allow(dead_code)]
    pub async fn stream_dhav_time_slice<F>(
        &self,
        env_cfg: &NvrEnvConfig,
        channel: usize,
        subtype: u8,
        start_local: NaiveDateTime,
        end_local: NaiveDateTime,
        on_chunk: F,
    ) -> Result<usize, String>
    where
        F: FnMut(&[u8]) -> bool,
    {
        self.stream_dhav_time_slice_cancellable(
            env_cfg,
            channel,
            subtype,
            start_local,
            end_local,
            DEFAULT_MAX_DHAV_STREAM_BYTES,
            None,
            on_chunk,
        )
        .await
        .map(|n| n as usize)
    }

    fn get_cached_auth_header(
        &self,
        target: &HttpTarget,
        method: &str,
        uri: &str,
    ) -> Option<String> {
        build_cached_auth_from_map(&self.digest_cache, target, method, uri)
    }

    fn get_digest_lock(&self, target: &HttpTarget) -> Arc<AsyncMutex<()>> {
        let key = target.cache_key();
        if let Ok(mut guard) = self.digest_locks.lock() {
            Arc::clone(
                guard
                    .entry(key)
                    .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
            )
        } else {
            Arc::new(AsyncMutex::new(()))
        }
    }

    fn store_digest_challenge(
        &self,
        target: &HttpTarget,
        challenge: DigestChallenge,
        initial_nc: u32,
    ) {
        if let Ok(mut guard) = self.digest_cache.lock() {
            guard.insert(
                target.cache_key(),
                Arc::new(CachedDigestEntry {
                    challenge,
                    nc: AtomicU32::new(initial_nc),
                }),
            );
        }
    }

    fn evict_digest_challenge(&self, target: &HttpTarget) {
        if let Ok(mut guard) = self.digest_cache.lock() {
            guard.remove(&target.cache_key());
        }
    }

    async fn send_digest_request(
        &self,
        target: &HttpTarget,
        method: &str,
        path_and_query: &str,
        extra_headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<HttpResponse, String> {
        if self.circuit_breaker.is_tripped() {
            return Err("401 Unauthorized: Auth circuit breaker is tripped".to_string());
        }

        let mut initial_auth = self.get_cached_auth_header(target, method, path_and_query);
        let cold_lock = self.get_digest_lock(target);
        let mut cold_guard = None;
        if initial_auth.is_none() {
            let g = cold_lock.lock().await;
            initial_auth = self.get_cached_auth_header(target, method, path_and_query);
            if initial_auth.is_none() {
                cold_guard = Some(g);
            }
        }

        let resp1 = self
            .send_single_http_request(
                target,
                method,
                path_and_query,
                initial_auth.as_deref(),
                extra_headers,
                body,
            )
            .await?;

        if (300..=399).contains(&resp1.status) {
            return Err(format!(
                "HTTP redirect {} refused by security policy",
                resp1.status
            ));
        }
        if resp1.status != 401 {
            return Ok(resp1);
        }

        let challenges1 = parse_www_authenticate_all(&resp1.headers);
        let (_, has_basic) = parse_www_authenticate_headers(&resp1.headers);
        let Some(challenge) = challenges1.first().cloned() else {
            self.evict_digest_challenge(target);
            self.circuit_breaker.trip();
            if has_basic {
                let ctx = if target.is_cloud_p2p {
                    "over Cloud P2P"
                } else {
                    "over unencrypted HTTP"
                };
                return Err(format!(
                    "401 Unauthorized: Basic authentication downgrade refused {}",
                    ctx
                ));
            }
            return Err(
                "401 Unauthorized: NVR offered no supported Digest authentication challenge"
                    .to_string(),
            );
        };

        let auth2 = build_digest_authorization(
            &challenge,
            &target.username,
            &target.password,
            method,
            path_and_query,
            1,
            None,
        );
        self.store_digest_challenge(target, challenge.clone(), 1);

        let resp2 = self
            .send_single_http_request(
                target,
                method,
                path_and_query,
                Some(&auth2),
                extra_headers,
                body,
            )
            .await?;

        if (300..=399).contains(&resp2.status) {
            return Err(format!(
                "HTTP redirect {} refused by security policy",
                resp2.status
            ));
        }

        if resp2.status == 401 {
            let challenges2 = parse_www_authenticate_all(&resp2.headers);
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
                let auth3 = build_digest_authorization(
                    &ch3,
                    &target.username,
                    &target.password,
                    method,
                    path_and_query,
                    1,
                    None,
                );
                self.store_digest_challenge(target, ch3, 1);
                let resp3 = self
                    .send_single_http_request(
                        target,
                        method,
                        path_and_query,
                        Some(&auth3),
                        extra_headers,
                        body,
                    )
                    .await?;
                drop(cold_guard);
                if (300..=399).contains(&resp3.status) {
                    return Err(format!(
                        "HTTP redirect {} refused by security policy",
                        resp3.status
                    ));
                }
                if resp3.status != 401 {
                    return Ok(resp3);
                }
            }

            self.evict_digest_challenge(target);
            self.circuit_breaker.trip();
            return Err("401 Unauthorized: NVR rejected HTTP Digest credentials".to_string());
        }

        drop(cold_guard);
        Ok(resp2)
    }

    async fn send_single_http_request(
        &self,
        target: &HttpTarget,
        method: &str,
        path_and_query: &str,
        auth_header: Option<&str>,
        extra_headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<HttpResponse, String> {
        let addr = format!("{}:{}", target.host, target.port);
        let mut stream = time::timeout(HTTP_CONNECT_TIMEOUT, TcpStream::connect(&addr))
            .await
            .map_err(|_| format!("HTTP connect timeout to {}", addr))?
            .map_err(|e| format!("HTTP connect failed to {}: {}", addr, e))?;
        let _ = stream.set_nodelay(true);

        let req_bytes = build_http_request_bytes(
            method,
            path_and_query,
            &target.host,
            target.port,
            auth_header,
            extra_headers,
            body,
        );

        time::timeout(HTTP_READ_TIMEOUT, stream.write_all(&req_bytes))
            .await
            .map_err(|_| "HTTP request write timeout".to_string())?
            .map_err(|e| format!("HTTP request write failed: {}", e))?;

        let mut reader = BufReader::with_capacity(16 * 1024, stream);
        let (status, headers, framing) =
            time::timeout(HTTP_READ_TIMEOUT, read_http_response_head(&mut reader))
                .await
                .map_err(|_| "HTTP response header timeout".to_string())?
                .map_err(|e| format!("HTTP response header error: {}", e))?;

        let resp_body = time::timeout(
            HTTP_READ_TIMEOUT,
            read_bounded_http_body(&mut reader, framing, MAX_API_BODY_BYTES),
        )
        .await
        .map_err(|_| "HTTP response body timeout".to_string())?
        .map_err(|e| format!("HTTP response body error: {}", e))?;

        Ok(HttpResponse {
            status,
            headers,
            body: resp_body,
        })
    }

    async fn open_authenticated_stream(
        &self,
        target: &HttpTarget,
        method: &str,
        path_and_query: &str,
        abort_flag: Option<&AtomicBool>,
    ) -> Result<Option<(BufReader<TcpStream>, BodyFraming)>, String> {
        let mut initial_auth = self.get_cached_auth_header(target, method, path_and_query);
        let cold_lock = self.get_digest_lock(target);
        let mut cold_guard = None;
        if initial_auth.is_none() {
            let g = cold_lock.lock().await;
            initial_auth = self.get_cached_auth_header(target, method, path_and_query);
            if initial_auth.is_none() {
                cold_guard = Some(g);
            }
        }

        let (mut reader1, status1, headers1, framing1) = self
            .open_single_http_stream(
                target,
                method,
                path_and_query,
                initial_auth.as_deref(),
                abort_flag,
            )
            .await?;

        if (300..=399).contains(&status1) {
            return Err(format!(
                "HTTP redirect {} refused by security policy",
                status1
            ));
        }

        if status1 == 200 {
            return Ok(Some((reader1, framing1)));
        }

        if status1 != 401 {
            if status1 == 403 {
                // Per-channel ACL restriction with valid credentials: never trip circuit breaker!
                return Err("403 Forbidden: loadfile.cgi denied access to channel".to_string());
            }
            let err_preview =
                read_bounded_http_body(&mut reader1, framing1, 4096)
                    .await
                    .unwrap_or_default();
            let preview_str = String::from_utf8_lossy(&err_preview);
            if matches!(status1, 400 | 404) && is_dahua_no_record_text(&preview_str) {
                return Ok(None);
            }
            return Err(format!(
                "loadfile.cgi returned HTTP {}: {}",
                status1,
                preview_str.trim()
            ));
        }

        drop(reader1);

        let challenges1 = parse_www_authenticate_all(&headers1);
        let (_, has_basic) = parse_www_authenticate_headers(&headers1);
        let Some(challenge) = challenges1.first().cloned() else {
            self.evict_digest_challenge(target);
            self.circuit_breaker.trip();
            if has_basic {
                return Err(
                    "401 Unauthorized: Basic authentication downgrade refused on loadfile.cgi"
                        .to_string(),
                );
            }
            return Err(
                "401 Unauthorized: NVR offered no Digest challenge on loadfile.cgi".to_string(),
            );
        };

        let auth2 = build_digest_authorization(
            &challenge,
            &target.username,
            &target.password,
            method,
            path_and_query,
            1,
            None,
        );
        self.store_digest_challenge(target, challenge.clone(), 1);

        let (mut reader2, status2, headers2, framing2) = self
            .open_single_http_stream(target, method, path_and_query, Some(&auth2), abort_flag)
            .await?;

        if (300..=399).contains(&status2) {
            return Err(format!(
                "HTTP redirect {} refused by security policy",
                status2
            ));
        }

        if status2 == 401 {
            drop(reader2);
            let challenges2 = parse_www_authenticate_all(&headers2);
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
                let auth3 = build_digest_authorization(
                    &ch3,
                    &target.username,
                    &target.password,
                    method,
                    path_and_query,
                    1,
                    None,
                );
                self.store_digest_challenge(target, ch3, 1);
                let (mut reader3, status3, _, framing3) = self
                    .open_single_http_stream(
                        target,
                        method,
                        path_and_query,
                        Some(&auth3),
                        abort_flag,
                    )
                    .await?;
                drop(cold_guard);
                if (300..=399).contains(&status3) {
                    return Err(format!(
                        "HTTP redirect {} refused by security policy",
                        status3
                    ));
                }
                if status3 == 200 {
                    return Ok(Some((reader3, framing3)));
                }
                if status3 == 403 {
                    return Err("403 Forbidden: loadfile.cgi denied access to channel".to_string());
                }
                if status3 != 401 {
                    let err_preview =
                        read_bounded_http_body(&mut reader3, framing3, 4096)
                            .await
                            .unwrap_or_default();
                    let preview_str = String::from_utf8_lossy(&err_preview);
                    if matches!(status3, 400 | 404) && is_dahua_no_record_text(&preview_str) {
                        return Ok(None);
                    }
                    return Err(format!(
                        "loadfile.cgi returned HTTP {}: {}",
                        status3,
                        preview_str.trim()
                    ));
                }
            }

            self.evict_digest_challenge(target);
            self.circuit_breaker.trip();
            return Err("401 Unauthorized: loadfile.cgi rejected credentials (HTTP 401)".to_string());
        }

        drop(cold_guard);

        if status2 == 403 {
            // Do NOT trip AuthCircuitBreaker on 403 Forbidden!
            return Err("403 Forbidden: loadfile.cgi denied access to channel".to_string());
        }
        if status2 != 200 {
            let err_preview =
                read_bounded_http_body(&mut reader2, framing2, 4096)
                    .await
                    .unwrap_or_default();
            let preview_str = String::from_utf8_lossy(&err_preview);
            if matches!(status2, 400 | 404) && is_dahua_no_record_text(&preview_str) {
                return Ok(None);
            }
            return Err(format!(
                "loadfile.cgi returned HTTP {}: {}",
                status2,
                preview_str.trim()
            ));
        }

        Ok(Some((reader2, framing2)))
    }

    async fn open_single_http_stream(
        &self,
        target: &HttpTarget,
        method: &str,
        path_and_query: &str,
        auth_header: Option<&str>,
        abort_flag: Option<&AtomicBool>,
    ) -> Result<(BufReader<TcpStream>, u16, Vec<(String, String)>, BodyFraming), String> {
        let addr = format!("{}:{}", target.host, target.port);
        let mut stream = await_with_abort(
            async {
                TcpStream::connect(&addr)
                    .await
                    .map_err(|e| format!("HTTP connect failed to {}: {}", addr, e))
            },
            HTTP_CONNECT_TIMEOUT,
            abort_flag,
            &format!("HTTP connect timeout to {}", addr),
        )
        .await?;
        let _ = stream.set_nodelay(true);

        // [CRIT-01]: Write directly to `stream: TcpStream` and wrap the full `TcpStream` in
        // `BufReader` WITHOUT `into_split()`, so `OwnedWriteHalf::drop` NEVER sends `SHUT_WR`
        // (`FIN`) before DHAV body streaming completes!
        let req_bytes = build_http_request_bytes(
            method,
            path_and_query,
            &target.host,
            target.port,
            auth_header,
            &[],
            b"",
        );

        await_with_abort(
            async {
                stream
                    .write_all(&req_bytes)
                    .await
                    .map_err(|e| format!("HTTP write failed: {}", e))
            },
            HTTP_READ_TIMEOUT,
            abort_flag,
            "HTTP write timeout",
        )
        .await?;

        let mut reader = BufReader::with_capacity(64 * 1024, stream);
        let (status, headers, framing) = await_with_abort(
            async {
                read_http_response_head(&mut reader)
                    .await
                    .map_err(|e| format!("HTTP header read error: {}", e))
            },
            HTTP_READ_TIMEOUT,
            abort_flag,
            "HTTP header read timeout",
        )
        .await?;

        Ok((reader, status, headers, framing))
    }

    // =========================================================================
    // Dahua / CP Plus / Amcrest CGI + RPC2 Fallback
    // =========================================================================

    fn get_dahua_channel_base(&self, target: &HttpTarget) -> Option<u8> {
        self.dahua_channel_base
            .lock()
            .ok()
            .and_then(|m| m.get(&(target.host.clone(), target.port)).copied())
    }

    fn set_dahua_channel_base(&self, target: &HttpTarget, base: u8) {
        if let Ok(mut guard) = self.dahua_channel_base.lock() {
            guard.insert((target.host.clone(), target.port), base);
        }
    }

    fn is_rpc2_preferred(&self, target: &HttpTarget) -> bool {
        self.rpc2_preferred
            .lock()
            .ok()
            .map(|s| s.contains(&(target.host.clone(), target.port)))
            .unwrap_or(false)
    }

    fn mark_rpc2_preferred(&self, target: &HttpTarget) {
        if let Ok(mut guard) = self.rpc2_preferred.lock() {
            guard.insert((target.host.clone(), target.port));
        }
    }

    async fn query_dahua_day(
        &self,
        target: &HttpTarget,
        channel: usize,
        date: NaiveDate,
    ) -> Result<DayTimelineIndex, String> {
        let start_dt = date
            .and_hms_opt(0, 0, 0)
            .unwrap_or_else(|| NaiveDateTime::new(date, NaiveTime::MIN));
        let end_dt = date
            .and_hms_opt(23, 59, 59)
            .unwrap_or_else(|| NaiveDateTime::new(date, NaiveTime::MIN));

        if !self.is_rpc2_preferred(target) {
            match self
                .query_dahua_cgi_range(target, channel, date, start_dt, end_dt, true)
                .await
            {
                Ok(Some((coverage, events, _hit_limit))) => {
                    return Ok(DayTimelineIndex {
                        channel,
                        date,
                        coverage,
                        events,
                        api_supported: true,
                        note: None,
                    });
                }
                Ok(None) => {
                    // 404/403/405/501 on mediaFileFind.cgi -> fall back to Dahua /RPC2_Login + /RPC2
                    self.mark_rpc2_preferred(target);
                }
                Err(e) => return Err(e),
            }
        }

        match self
            .query_dahua_rpc2_range(target, channel, date, start_dt, end_dt, true)
            .await
        {
            Ok(Some((coverage, events, _hit_limit))) => Ok(DayTimelineIndex {
                channel,
                date,
                coverage,
                events,
                api_supported: true,
                note: None,
            }),
            Ok(None) => Ok(DayTimelineIndex {
                channel,
                date,
                coverage: Vec::new(),
                events: Vec::new(),
                api_supported: false,
                note: Some(
                    "NVR HTTP timeline search API unavailable; direct RTSP playback remains active"
                        .to_string(),
                ),
            }),
            Err(e) => Err(e),
        }
    }

    async fn query_dahua_month_days(
        &self,
        target: &HttpTarget,
        channel: usize,
        year: i32,
        month: u32,
        first_date: NaiveDate,
        last_date: NaiveDate,
    ) -> Result<Vec<u32>, String> {
        let mut recorded_days = HashSet::new();
        let mut cursor_start = first_date
            .and_hms_opt(0, 0, 0)
            .unwrap_or_else(|| NaiveDateTime::new(first_date, NaiveTime::MIN));
        let month_end = last_date
            .and_hms_opt(23, 59, 59)
            .unwrap_or_else(|| NaiveDateTime::new(last_date, NaiveTime::MIN));

        // Up to 16 range slices in case a motion-heavy month exceeds MAX_SEARCH_ITEMS = 4000 clips
        for _ in 0..16 {
            if cursor_start > month_end {
                break;
            }
            let batch_opt = if !self.is_rpc2_preferred(target) {
                match self
                    .query_dahua_cgi_range(
                        target,
                        channel,
                        first_date,
                        cursor_start,
                        month_end,
                        false,
                    )
                    .await?
                {
                    Some((cov, _, hit_limit)) => Some((cov, hit_limit)),
                    None => {
                        self.mark_rpc2_preferred(target);
                        self.query_dahua_rpc2_range(
                            target,
                            channel,
                            first_date,
                            cursor_start,
                            month_end,
                            false,
                        )
                        .await?
                        .map(|(cov, _, hit_limit)| (cov, hit_limit))
                    }
                }
            } else {
                self.query_dahua_rpc2_range(
                    target,
                    channel,
                    first_date,
                    cursor_start,
                    month_end,
                    false,
                )
                .await?
                .map(|(cov, _, hit_limit)| (cov, hit_limit))
            };

            let Some((spans, hit_limit)) = batch_opt else {
                break;
            };
            if spans.is_empty() {
                break;
            }

            let mut max_end_ms = local_naive_to_epoch_ms(cursor_start);
            for sp in &spans {
                collect_days_touched_by_span(sp.start_ms, sp.end_ms, year, month, &mut recorded_days);
                if sp.end_ms > max_end_ms {
                    max_end_ms = sp.end_ms;
                }
            }

            if !hit_limit {
                break;
            }

            let mut next_dt = epoch_ms_to_local_naive(max_end_ms);
            if next_dt <= cursor_start {
                next_dt += chrono::Duration::seconds(1);
            }
            cursor_start = next_dt;
        }

        let mut days: Vec<u32> = recorded_days.into_iter().collect();
        days.sort_unstable();
        Ok(days)
    }

    /// Returns `Ok(None)` when CGI returns 404/403/405/501 so caller can seamlessly fall back to RPC2.
    async fn query_dahua_cgi_range(
        &self,
        target: &HttpTarget,
        channel: usize,
        fallback_date: NaiveDate,
        start_dt: NaiveDateTime,
        end_dt: NaiveDateTime,
        run_event_pass: bool,
    ) -> Result<Option<(Vec<RecordedSpan>, Vec<RecordedSpan>, bool)>, String> {
        let create_resp = self
            .send_digest_request(
                target,
                "GET",
                "/cgi-bin/mediaFileFind.cgi?action=factory.create",
                &[],
                b"",
            )
            .await?;

        if matches!(create_resp.status, 403 | 404 | 405 | 501) {
            return Ok(None);
        }
        if create_resp.status != 200 {
            return Err(format!(
                "mediaFileFind factory.create failed with HTTP {}",
                create_resp.status
            ));
        }

        let body_str = String::from_utf8_lossy(&create_resp.body);
        let Some(object_id) = parse_factory_create_object_id(&body_str) else {
            if body_str.contains("404") || body_str.contains("Not Found") {
                return Ok(None);
            }
            return Err(format!(
                "Failed to parse Dahua mediaFileFind object handle from '{}'",
                body_str.trim()
            ));
        };

        let mut guard = CgiFinderGuard::new(self, target.clone(), object_id.clone());

        let result = self
            .execute_dahua_cgi_passes(
                target,
                &object_id,
                channel,
                fallback_date,
                start_dt,
                end_dt,
                run_event_pass,
            )
            .await;

        guard.cleanup().await;
        result.map(Some)
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_dahua_cgi_passes(
        &self,
        target: &HttpTarget,
        object_id: &str,
        channel: usize,
        fallback_date: NaiveDate,
        start_dt: NaiveDateTime,
        end_dt: NaiveDateTime,
        run_event_pass: bool,
    ) -> Result<(Vec<RecordedSpan>, Vec<RecordedSpan>, bool), String> {
        let start_enc = format!(
            "{}%20{}",
            start_dt.format("%Y-%m-%d"),
            start_dt.format("%H:%M:%S")
        );
        let end_enc = format!(
            "{}%20{}",
            end_dt.format("%Y-%m-%d"),
            end_dt.format("%H:%M:%S")
        );

        let dahua_ch = self
            .open_cgi_find_file_with_calibration(
                target,
                object_id,
                channel,
                start_dt,
                &start_enc,
                &end_enc,
                "condition.Types[0]=dav&condition.Types[1]=mp4",
            )
            .await?;

        let mut pass1_items = Vec::new();
        if let Some(active_dahua_ch) = dahua_ch {
            pass1_items = self
                .paginate_cgi_find_next_file(target, object_id, channel, fallback_date)
                .await?;
            let hit_limit = pass1_items.len() >= MAX_SEARCH_ITEMS;

            if run_event_pass {
                // Close Pass 1 cursor before starting Pass 2 on the same finder handle
                let close_path = format!(
                    "/cgi-bin/mediaFileFind.cgi?action=close&object={}",
                    object_id
                );
                let _ = self
                    .send_digest_request(target, "GET", &close_path, &[], b"")
                    .await;

                let find_ev_path = format!(
                    "/cgi-bin/mediaFileFind.cgi?action=findFile&object={}&condition.Channel={}&condition.StartTime={}&condition.EndTime={}&condition.Flags[0]=Event",
                    object_id, active_dahua_ch, start_enc, end_enc
                );
                let ev_resp = self
                    .send_digest_request(target, "GET", &find_ev_path, &[], b"")
                    .await?;
                let mut pass2_items = Vec::new();
                if ev_resp.status == 200 && is_dahua_find_file_positive(&ev_resp.body) {
                    pass2_items = self
                        .paginate_cgi_find_next_file(target, object_id, channel, fallback_date)
                        .await?;
                }
                let (cov, evs) = assemble_timeline_from_passes(channel, pass1_items, pass2_items);
                return Ok((cov, evs, hit_limit));
            }

            let (cov, evs) = assemble_timeline_from_passes(channel, pass1_items, Vec::new());
            return Ok((cov, evs, hit_limit));
        }

        let (cov, evs) = assemble_timeline_from_passes(channel, pass1_items, Vec::new());
        Ok((cov, evs, false))
    }

    /// Probes `condition.Channel=0` first when uncalibrated (including via a dedicated 1s finder
    /// when `channel > 1` is queried first); caches 0-based vs 1-based indexing per `(host, port)`.
    /// Returns `Ok(Some(calibrated_dahua_ch))` if files may exist, or `Ok(None)` if 0 files matched.
    #[allow(clippy::too_many_arguments)]
    async fn open_cgi_find_file_with_calibration(
        &self,
        target: &HttpTarget,
        object_id: &str,
        channel: usize,
        start_dt: NaiveDateTime,
        start_enc: &str,
        end_enc: &str,
        extra_query: &str,
    ) -> Result<Option<usize>, String> {
        if self.get_dahua_channel_base(target).is_none() && channel > 1 {
            let cal_create = self
                .send_digest_request(
                    target,
                    "GET",
                    "/cgi-bin/mediaFileFind.cgi?action=factory.create",
                    &[],
                    b"",
                )
                .await?;
            if cal_create.status == 200 {
                if let Some(cal_obj) =
                    parse_factory_create_object_id(&String::from_utf8_lossy(&cal_create.body))
                {
                    let mut cal_guard = CgiFinderGuard::new(self, target.clone(), cal_obj.clone());
                    let cal_end_dt = start_dt + chrono::Duration::seconds(1);
                    let cal_start_enc = format!(
                        "{}%20{}",
                        start_dt.format("%Y-%m-%d"),
                        start_dt.format("%H:%M:%S")
                    );
                    let cal_end_enc = format!(
                        "{}%20{}",
                        cal_end_dt.format("%Y-%m-%d"),
                        cal_end_dt.format("%H:%M:%S")
                    );
                    let cal_path = format!(
                        "/cgi-bin/mediaFileFind.cgi?action=findFile&object={}&condition.Channel=0&condition.StartTime={}&condition.EndTime={}&{}",
                        cal_obj, cal_start_enc, cal_end_enc, extra_query
                    );
                    let cal_resp = self
                        .send_digest_request(target, "GET", &cal_path, &[], b"")
                        .await;
                    cal_guard.cleanup().await;
                    if let Ok(r) = cal_resp {
                        if r.status == 400 || is_dahua_body_error(&r.body) {
                            self.set_dahua_channel_base(target, 1);
                        } else if r.status == 200 {
                            self.set_dahua_channel_base(target, 0);
                        }
                    }
                }
            }
        }

        if let Some(base) = self.get_dahua_channel_base(target) {
            let dahua_ch = if base == 1 {
                channel
            } else {
                channel.saturating_sub(1)
            };
            let path = format!(
                "/cgi-bin/mediaFileFind.cgi?action=findFile&object={}&condition.Channel={}&condition.StartTime={}&condition.EndTime={}&{}",
                object_id, dahua_ch, start_enc, end_enc, extra_query
            );
            let resp = self
                .send_digest_request(target, "GET", &path, &[], b"")
                .await?;
            if base == 0 && channel == 1 && (resp.status == 400 || is_dahua_body_error(&resp.body))
            {
                self.set_dahua_channel_base(target, 1);
                let retry_path = format!(
                    "/cgi-bin/mediaFileFind.cgi?action=findFile&object={}&condition.Channel=1&condition.StartTime={}&condition.EndTime={}&{}",
                    object_id, start_enc, end_enc, extra_query
                );
                let retry_resp = self
                    .send_digest_request(target, "GET", &retry_path, &[], b"")
                    .await?;
                if retry_resp.status == 200 && !is_dahua_body_error(&retry_resp.body) {
                    if is_dahua_find_file_positive(&retry_resp.body) {
                        return Ok(Some(1));
                    }
                    return Ok(None);
                }
                return Err(format!(
                    "Dahua findFile failed after 1-based recalibration (HTTP {}): {}",
                    retry_resp.status,
                    String::from_utf8_lossy(&retry_resp.body).trim()
                ));
            }
            if resp.status == 200 && is_dahua_find_file_positive(&resp.body) {
                return Ok(Some(dahua_ch));
            }
            if resp.status == 200 && !is_dahua_body_error(&resp.body) {
                return Ok(None);
            }
            return Err(format!(
                "Dahua findFile failed (HTTP {}): {}",
                resp.status,
                String::from_utf8_lossy(&resp.body).trim()
            ));
        }

        let ch_zero_based = channel.saturating_sub(1);
        let path0 = format!(
            "/cgi-bin/mediaFileFind.cgi?action=findFile&object={}&condition.Channel={}&condition.StartTime={}&condition.EndTime={}&{}",
            object_id, ch_zero_based, start_enc, end_enc, extra_query
        );
        let resp0 = self
            .send_digest_request(target, "GET", &path0, &[], b"")
            .await?;

        if resp0.status == 400 || is_dahua_body_error(&resp0.body) {
            // Retry with 1-based channel indexing
            let ch_one_based = channel;
            let path1 = format!(
                "/cgi-bin/mediaFileFind.cgi?action=findFile&object={}&condition.Channel={}&condition.StartTime={}&condition.EndTime={}&{}",
                object_id, ch_one_based, start_enc, end_enc, extra_query
            );
            let resp1 = self
                .send_digest_request(target, "GET", &path1, &[], b"")
                .await?;
            if resp1.status == 200 && !is_dahua_body_error(&resp1.body) {
                self.set_dahua_channel_base(target, 1);
                if is_dahua_find_file_positive(&resp1.body) {
                    return Ok(Some(ch_one_based));
                }
                return Ok(None);
            }
            return Err(format!(
                "Dahua findFile rejected both 0-based and 1-based channel {} (HTTP {})",
                channel, resp1.status
            ));
        }

        if resp0.status == 200 {
            self.set_dahua_channel_base(target, 0);
            if is_dahua_find_file_positive(&resp0.body) {
                return Ok(Some(ch_zero_based));
            }
            return Ok(None);
        }

        Err(format!(
            "Dahua findFile failed with HTTP {}",
            resp0.status
        ))
    }

    async fn paginate_cgi_find_next_file(
        &self,
        target: &HttpTarget,
        object_id: &str,
        channel: usize,
        fallback_date: NaiveDate,
    ) -> Result<Vec<RawDahuaFileItem>, String> {
        let mut all_items = Vec::new();
        while all_items.len() < MAX_SEARCH_ITEMS {
            let batch_count = PAGE_BATCH_SIZE.min(MAX_SEARCH_ITEMS - all_items.len());
            let next_path = format!(
                "/cgi-bin/mediaFileFind.cgi?action=findNextFile&object={}&count={}",
                object_id, batch_count
            );
            let resp = self
                .send_digest_request(target, "GET", &next_path, &[], b"")
                .await?;
            if resp.status != 200 {
                return Err(format!(
                    "mediaFileFind findNextFile failed with HTTP {}",
                    resp.status
                ));
            }
            let text = String::from_utf8_lossy(&resp.body);
            if is_dahua_body_error(resp.body.as_slice()) {
                return Err(format!(
                    "mediaFileFind findNextFile returned error: {}",
                    text.trim()
                ));
            }
            let (found, mut batch) = parse_dahua_find_next_file_kv(&text, channel, fallback_date);
            if found == 0 || batch.is_empty() {
                break;
            }
            all_items.append(&mut batch);
        }
        if all_items.len() > MAX_SEARCH_ITEMS {
            all_items.truncate(MAX_SEARCH_ITEMS);
        }
        Ok(all_items)
    }

    // =========================================================================
    // Dahua NVR 4.0 JSON-RPC (`/RPC2_Login` + `/RPC2`) Fallback
    // =========================================================================

    async fn ensure_rpc2_session(&self, target: &HttpTarget) -> Result<Option<String>, String> {
        if let Ok(guard) = self.rpc2_sessions.lock() {
            if let Some(existing) = guard.get(&target.cache_key()) {
                return Ok(Some(existing.clone()));
            }
        }
        self.rpc2_login(target).await
    }

    async fn rpc2_login(&self, target: &HttpTarget) -> Result<Option<String>, String> {
        let step1_payload = json!({
            "method": "global.login",
            "params": {
                "userName": target.username,
                "password": "",
                "clientType": "Web3.0",
                "loginType": "Direct"
            },
            "id": 1,
            "session": 0
        });
        let step1_bytes = serde_json::to_vec(&step1_payload)
            .map_err(|e| format!("RPC2 JSON encode error: {}", e))?;

        let resp1 = self
            .send_single_http_request(
                target,
                "POST",
                "/RPC2_Login",
                None,
                &[("Content-Type", "application/json")],
                &step1_bytes,
            )
            .await?;

        if resp1.status == 404 || resp1.status == 405 || resp1.status == 501 {
            return Ok(None);
        }
        if resp1.status != 200 {
            return Err(format!("RPC2_Login step 1 returned HTTP {}", resp1.status));
        }

        let v1: Value = serde_json::from_slice(&resp1.body)
            .map_err(|e| format!("Invalid JSON from /RPC2_Login step 1: {}", e))?;

        let session1 = json_value_to_id_string(&v1["session"])
            .ok_or_else(|| "Missing session in /RPC2_Login challenge".to_string())?;
        let realm = v1["params"]["realm"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let random = v1["params"]["random"]
            .as_str()
            .map(|s| s.to_string())
            .or_else(|| v1["params"]["random"].as_u64().map(|n| n.to_string()))
            .ok_or_else(|| "Missing random nonce in /RPC2_Login challenge".to_string())?;

        // Dahua RPC2 Default hash: MD5_UPPER("{user}:{realm}:{pass}") -> MD5_UPPER("{user}:{random}:{ha1}")
        let ha1_in = Zeroizing::new(format!(
            "{}:{}:{}",
            target.username,
            realm,
            target.password.as_str()
        ));
        let ha1 = Zeroizing::new(md5_upper_hex(ha1_in.as_bytes()));
        let pw_in = Zeroizing::new(format!("{}:{}:{}", target.username, random, ha1.as_str()));
        let pw_hash = md5_upper_hex(pw_in.as_bytes());

        let step2_payload = json!({
            "method": "global.login",
            "params": {
                "userName": target.username,
                "password": pw_hash,
                "clientType": "Web3.0",
                "loginType": "Direct",
                "authorityType": "Default",
                "passwordType": "Default"
            },
            "id": 2,
            "session": session1
        });
        let step2_bytes = serde_json::to_vec(&step2_payload)
            .map_err(|e| format!("RPC2 JSON encode error: {}", e))?;
        let cookie_hdr = format!("DhWebClientSessionID={}", session1);

        let resp2 = self
            .send_single_http_request(
                target,
                "POST",
                "/RPC2_Login",
                None,
                &[
                    ("Content-Type", "application/json"),
                    ("Cookie", &cookie_hdr),
                ],
                &step2_bytes,
            )
            .await?;

        if resp2.status == 401 {
            self.circuit_breaker.trip();
            return Err("401 Unauthorized: /RPC2_Login rejected HTTP credentials".to_string());
        }
        if resp2.status == 403 {
            // Do NOT trip AuthCircuitBreaker on 403 Forbidden!
            return Err("403 Forbidden: /RPC2_Login denied access".to_string());
        }
        if resp2.status != 200 {
            return Err(format!("RPC2_Login step 2 returned HTTP {}", resp2.status));
        }

        let v2: Value = serde_json::from_slice(&resp2.body)
            .map_err(|e| format!("Invalid JSON from /RPC2_Login step 2: {}", e))?;

        if v2["result"].as_bool() != Some(true) {
            if is_rpc2_credential_error(&v2) {
                self.circuit_breaker.trip();
                return Err("401 Unauthorized: Dahua /RPC2_Login rejected credentials".to_string());
            }
            let detail = v2["error"]["message"]
                .as_str()
                .unwrap_or("non-credential RPC2 login error");
            return Err(format!("Dahua /RPC2_Login failed: {}", detail));
        }

        let final_session = json_value_to_id_string(&v2["session"]).unwrap_or(session1);
        let safe_session = sanitize_header_token(&final_session, 128);
        if let Ok(mut guard) = self.rpc2_sessions.lock() {
            guard.insert(target.cache_key(), safe_session.clone());
        }
        Ok(Some(safe_session))
    }

    async fn rpc2_call(&self, target: &HttpTarget, session: &str, payload: &Value) -> Result<Value, String> {
        let body =
            serde_json::to_vec(payload).map_err(|e| format!("RPC2 serialize error: {}", e))?;
        let cookie = format!("DhWebClientSessionID={}", session);
        let resp = self
            .send_single_http_request(
                target,
                "POST",
                "/RPC2",
                None,
                &[("Content-Type", "application/json"), ("Cookie", &cookie)],
                &body,
            )
            .await?;
        if resp.status != 200 {
            return Err(format!("POST /RPC2 returned HTTP {}", resp.status));
        }
        serde_json::from_slice(&resp.body).map_err(|e| format!("Invalid /RPC2 JSON: {}", e))
    }

    async fn query_dahua_rpc2_range(
        &self,
        target: &HttpTarget,
        channel: usize,
        fallback_date: NaiveDate,
        start_dt: NaiveDateTime,
        end_dt: NaiveDateTime,
        run_event_pass: bool,
    ) -> Result<Option<(Vec<RecordedSpan>, Vec<RecordedSpan>, bool)>, String> {
        let Some(mut session) = self.ensure_rpc2_session(target).await? else {
            return Ok(None);
        };

        let create_req = json!({
            "method": "mediaFileFind.factory.create",
            "params": Value::Null,
            "id": 10,
            "session": session
        });

        let mut create_val = self.rpc2_call(target, &session, &create_req).await?;
        if create_val.get("result").is_none() || create_val["result"].is_null() || create_val["result"] == Value::Bool(false) {
            // Session may have expired on NVR; re-authenticate once
            if let Ok(mut guard) = self.rpc2_sessions.lock() {
                guard.remove(&target.cache_key());
            }
            let Some(fresh_sess) = self.rpc2_login(target).await? else {
                return Ok(None);
            };
            session = fresh_sess;
            let retry_req = json!({
                "method": "mediaFileFind.factory.create",
                "params": Value::Null,
                "id": 11,
                "session": session
            });
            create_val = self.rpc2_call(target, &session, &retry_req).await?;
        }

        let object_val = create_val["result"].clone();
        if object_val.is_null() || object_val == Value::Bool(false) {
            return Err("Dahua RPC2 mediaFileFind.factory.create failed".to_string());
        }

        let mut guard = Rpc2FinderGuard::new(self, target.clone(), session.clone(), object_val.clone());

        let res = self
            .execute_dahua_rpc2_passes(
                target,
                &session,
                &object_val,
                channel,
                fallback_date,
                start_dt,
                end_dt,
                run_event_pass,
            )
            .await;

        guard.cleanup().await;
        res.map(Some)
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_dahua_rpc2_passes(
        &self,
        target: &HttpTarget,
        session: &str,
        object_val: &Value,
        channel: usize,
        fallback_date: NaiveDate,
        start_dt: NaiveDateTime,
        end_dt: NaiveDateTime,
        run_event_pass: bool,
    ) -> Result<(Vec<RecordedSpan>, Vec<RecordedSpan>, bool), String> {
        let start_str = start_dt.format("%Y-%m-%d %H:%M:%S").to_string();
        let end_str = end_dt.format("%Y-%m-%d %H:%M:%S").to_string();

        let (active_ch_opt, calibrated_ch) = self
            .rpc2_find_file_with_calibration(
                target,
                session,
                object_val,
                channel,
                start_dt,
                &start_str,
                &end_str,
            )
            .await?;

        let mut pass1_items = Vec::new();
        if active_ch_opt {
            pass1_items = self
                .paginate_rpc2_find_next_file(target, session, object_val, channel, fallback_date)
                .await?;
        }
        let hit_limit = pass1_items.len() >= MAX_SEARCH_ITEMS;

        let mut pass2_items = Vec::new();
        if run_event_pass {
            let close_req = json!({
                "method": "mediaFileFind.close",
                "params": Value::Null,
                "object": object_val,
                "id": 20,
                "session": session
            });
            let _ = self.rpc2_call(target, session, &close_req).await;

            let ev_req = json!({
                "method": "mediaFileFind.findFile",
                "params": {
                    "condition": {
                        "Channel": calibrated_ch,
                        "StartTime": start_str,
                        "EndTime": end_str,
                        "Flags": ["Event"],
                        "Dirs": Value::Null
                    }
                },
                "object": object_val,
                "id": 21,
                "session": session
            });
            if let Ok(ev_val) = self.rpc2_call(target, session, &ev_req).await {
                if ev_val["result"].as_bool() == Some(true) {
                    pass2_items = self
                        .paginate_rpc2_find_next_file(
                            target,
                            session,
                            object_val,
                            channel,
                            fallback_date,
                        )
                        .await?;
                }
            }
        }

        let (cov, evs) = assemble_timeline_from_passes(channel, pass1_items, pass2_items);
        Ok((cov, evs, hit_limit))
    }

    #[allow(clippy::too_many_arguments)]
    async fn rpc2_find_file_with_calibration(
        &self,
        target: &HttpTarget,
        session: &str,
        object_val: &Value,
        channel: usize,
        start_dt: NaiveDateTime,
        start_str: &str,
        end_str: &str,
    ) -> Result<(bool, usize), String> {
        if self.get_dahua_channel_base(target).is_none() && channel > 1 {
            let cal_create_req = json!({
                "method": "mediaFileFind.factory.create",
                "params": Value::Null,
                "id": 110,
                "session": session
            });
            if let Ok(cal_create_val) = self.rpc2_call(target, session, &cal_create_req).await {
                let cal_obj = cal_create_val["result"].clone();
                if !cal_obj.is_null() && cal_obj != Value::Bool(false) {
                    let mut cal_guard = Rpc2FinderGuard::new(
                        self,
                        target.clone(),
                        session.to_string(),
                        cal_obj.clone(),
                    );
                    let cal_end_dt = start_dt + chrono::Duration::seconds(1);
                    let cal_start_str = start_dt.format("%Y-%m-%d %H:%M:%S").to_string();
                    let cal_end_str = cal_end_dt.format("%Y-%m-%d %H:%M:%S").to_string();
                    let cal_req = json!({
                        "method": "mediaFileFind.findFile",
                        "params": {
                            "condition": {
                                "Channel": 0,
                                "StartTime": cal_start_str,
                                "EndTime": cal_end_str,
                                "Types": ["dav", "mp4"],
                                "Dirs": Value::Null
                            }
                        },
                        "object": cal_obj,
                        "id": 111,
                        "session": session
                    });
                    let cal_res = self.rpc2_call(target, session, &cal_req).await;
                    cal_guard.cleanup().await;
                    if let Ok(cal_val) = cal_res {
                        let has_err = cal_val.get("error").is_some_and(|e| !e.is_null());
                        if !has_err && cal_val["result"].as_bool() == Some(true) {
                            self.set_dahua_channel_base(target, 0);
                        } else if has_err || cal_val["result"].as_bool() == Some(false) {
                            self.set_dahua_channel_base(target, 1);
                        }
                    }
                }
            }
        }

        if let Some(base) = self.get_dahua_channel_base(target) {
            let ch = if base == 1 {
                channel
            } else {
                channel.saturating_sub(1)
            };
            let req = json!({
                "method": "mediaFileFind.findFile",
                "params": {
                    "condition": {
                        "Channel": ch,
                        "StartTime": start_str,
                        "EndTime": end_str,
                        "Types": ["dav", "mp4"],
                        "Dirs": Value::Null
                    }
                },
                "object": object_val,
                "id": 12,
                "session": session
            });
            let val = self.rpc2_call(target, session, &req).await?;
            if base == 0 && channel == 1 && val.get("error").is_some_and(|e| !e.is_null()) {
                self.set_dahua_channel_base(target, 1);
                let retry_req = json!({
                    "method": "mediaFileFind.findFile",
                    "params": {
                        "condition": {
                            "Channel": 1,
                            "StartTime": start_str,
                            "EndTime": end_str,
                            "Types": ["dav", "mp4"],
                            "Dirs": Value::Null
                        }
                    },
                    "object": object_val,
                    "id": 13,
                    "session": session
                });
                let retry_val = self.rpc2_call(target, session, &retry_req).await?;
                return Ok((retry_val["result"].as_bool() == Some(true), 1));
            }
            return Ok((val["result"].as_bool() == Some(true), ch));
        }

        let ch0 = channel.saturating_sub(1);
        let req0 = json!({
            "method": "mediaFileFind.findFile",
            "params": {
                "condition": {
                    "Channel": ch0,
                    "StartTime": start_str,
                    "EndTime": end_str,
                    "Types": ["dav", "mp4"],
                    "Dirs": Value::Null
                }
            },
            "object": object_val,
            "id": 12,
            "session": session
        });
        let val0 = self.rpc2_call(target, session, &req0).await?;
        if val0.get("error").is_some_and(|e| !e.is_null()) {
            let ch1 = channel;
            let req1 = json!({
                "method": "mediaFileFind.findFile",
                "params": {
                    "condition": {
                        "Channel": ch1,
                        "StartTime": start_str,
                        "EndTime": end_str,
                        "Types": ["dav", "mp4"],
                        "Dirs": Value::Null
                    }
                },
                "object": object_val,
                "id": 13,
                "session": session
            });
            let val1 = self.rpc2_call(target, session, &req1).await?;
            if val1.get("error").is_none() || val1["error"].is_null() {
                self.set_dahua_channel_base(target, 1);
                return Ok((val1["result"].as_bool() == Some(true), ch1));
            }
        }

        self.set_dahua_channel_base(target, 0);
        Ok((val0["result"].as_bool() == Some(true), ch0))
    }

    async fn paginate_rpc2_find_next_file(
        &self,
        target: &HttpTarget,
        session: &str,
        object_val: &Value,
        channel: usize,
        fallback_date: NaiveDate,
    ) -> Result<Vec<RawDahuaFileItem>, String> {
        let mut items = Vec::new();
        while items.len() < MAX_SEARCH_ITEMS {
            let count = PAGE_BATCH_SIZE.min(MAX_SEARCH_ITEMS - items.len());
            let req = json!({
                "method": "mediaFileFind.findNextFile",
                "params": { "count": count },
                "object": object_val,
                "id": 15,
                "session": session
            });
            let val = self.rpc2_call(target, session, &req).await?;
            let found = val["params"]["found"].as_u64().unwrap_or(0) as usize;
            if found == 0 {
                break;
            }
            let Some(infos) = val["params"]["infos"].as_array() else {
                break;
            };
            if infos.is_empty() {
                break;
            }
            for info in infos {
                if let Some(parsed) = parse_rpc2_file_info(info, channel, fallback_date) {
                    items.push(parsed);
                }
            }
        }
        if items.len() > MAX_SEARCH_ITEMS {
            items.truncate(MAX_SEARCH_ITEMS);
        }
        Ok(items)
    }

    // =========================================================================
    // Hikvision ISAPI (`/ISAPI/ContentMgmt/search`)
    // =========================================================================

    async fn query_hikvision_day(
        &self,
        target: &HttpTarget,
        channel: usize,
        date: NaiveDate,
    ) -> Result<DayTimelineIndex, String> {
        let start_local = date
            .and_hms_opt(0, 0, 0)
            .unwrap_or_else(|| NaiveDateTime::new(date, NaiveTime::MIN));
        let end_local = date
            .and_hms_opt(23, 59, 59)
            .unwrap_or_else(|| NaiveDateTime::new(date, NaiveTime::MIN));

        match self
            .query_hikvision_isapi_range(target, channel, start_local, end_local)
            .await?
        {
            Some((coverage, events)) => Ok(DayTimelineIndex {
                channel,
                date,
                coverage,
                events,
                api_supported: true,
                note: None,
            }),
            None => Ok(DayTimelineIndex {
                channel,
                date,
                coverage: Vec::new(),
                events: Vec::new(),
                api_supported: false,
                note: Some(
                    "Hikvision ISAPI ContentMgmt search endpoint unsupported; RTSP scrub active"
                        .to_string(),
                ),
            }),
        }
    }

    async fn query_hikvision_month_days(
        &self,
        target: &HttpTarget,
        channel: usize,
        year: i32,
        month: u32,
        first_date: NaiveDate,
        last_date: NaiveDate,
    ) -> Result<Vec<u32>, String> {
        let daily_path = format!(
            "/ISAPI/ContentMgmt/record/tracks/{}01/dailyDistribution",
            channel
        );
        let daily_xml = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><trackDailyParam><year>{}</year><monthOfYear>{}</monthOfYear></trackDailyParam>",
            year, month
        );
        let daily_resp = self
            .send_digest_request(
                target,
                "POST",
                &daily_path,
                &[("Content-Type", "application/xml")],
                daily_xml.as_bytes(),
            )
            .await?;

        if daily_resp.status == 200 {
            let xml_str = String::from_utf8_lossy(&daily_resp.body);
            if let Ok(days) = parse_hikvision_daily_distribution_xml(&xml_str) {
                return Ok(days);
            }
        }

        let start_local = first_date
            .and_hms_opt(0, 0, 0)
            .unwrap_or_else(|| NaiveDateTime::new(first_date, NaiveTime::MIN));
        let end_local = last_date
            .and_hms_opt(23, 59, 59)
            .unwrap_or_else(|| NaiveDateTime::new(last_date, NaiveTime::MIN));

        let mut days_set = HashSet::new();
        if let Some((coverage, _)) = self
            .query_hikvision_isapi_range(target, channel, start_local, end_local)
            .await?
        {
            for sp in coverage {
                collect_days_touched_by_span(sp.start_ms, sp.end_ms, year, month, &mut days_set);
            }
        }
        let mut out: Vec<u32> = days_set.into_iter().collect();
        out.sort_unstable();
        Ok(out)
    }

    async fn query_hikvision_isapi_range(
        &self,
        target: &HttpTarget,
        channel: usize,
        start_local: NaiveDateTime,
        end_local: NaiveDateTime,
    ) -> Result<Option<(Vec<RecordedSpan>, Vec<RecordedSpan>)>, String> {
        let start_utc = local_naive_to_utc(start_local);
        let end_utc = local_naive_to_utc(end_local);
        let start_str = start_utc.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let end_str = end_utc.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let search_id = format_pseudo_uuid(&generate_random_hex_16());
        let track_id = format!("{}01", channel);

        let mut raw_spans = Vec::new();
        let mut position: usize = 0;
        let mut include_metadata = true;

        while raw_spans.len() < MAX_SEARCH_ITEMS {
            let max_res = PAGE_BATCH_SIZE.min(MAX_SEARCH_ITEMS - raw_spans.len());
            let metadata_block = if include_metadata {
                "<metadataList><metadataDescriptor>//recordType.meta.std-cgi.com</metadataDescriptor></metadataList>"
            } else {
                ""
            };
            let xml_req = format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
                 <CMSearchDescription>\
                   <searchID>{}</searchID>\
                   <trackList><trackID>{}</trackID></trackList>\
                   <timeSpanList><timeSpan><startTime>{}</startTime><endTime>{}</endTime></timeSpan></timeSpanList>\
                   <maxResults>{}</maxResults>\
                   <searchResultPostion>{}</searchResultPostion>\
                   {}\
                 </CMSearchDescription>",
                search_id, track_id, start_str, end_str, max_res, position, metadata_block
            );

            let resp = self
                .send_digest_request(
                    target,
                    "POST",
                    "/ISAPI/ContentMgmt/search",
                    &[("Content-Type", "application/xml")],
                    xml_req.as_bytes(),
                )
                .await?;

            if resp.status == 400 && include_metadata && position == 0 {
                include_metadata = false;
                continue;
            }
            if resp.status == 404 || resp.status == 405 || resp.status == 501 {
                return Ok(None);
            }
            if resp.status == 403 {
                // Per-channel ACL restriction: do NOT trip AuthCircuitBreaker!
                return Err("403 Forbidden: Hikvision ISAPI returned 403 Forbidden".to_string());
            }
            if resp.status != 200 {
                return Err(format!(
                    "Hikvision ISAPI search returned HTTP {}",
                    resp.status
                ));
            }

            let xml_body = String::from_utf8_lossy(&resp.body);
            let (has_more, page_spans) = parse_hikvision_search_xml(&xml_body, channel)?;
            if page_spans.is_empty() {
                break;
            }
            position = position.saturating_add(page_spans.len());
            raw_spans.extend(page_spans);
            if !has_more {
                break;
            }
        }

        if raw_spans.len() > MAX_SEARCH_ITEMS {
            raw_spans.truncate(MAX_SEARCH_ITEMS);
        }

        let mut events = Vec::new();
        for sp in &raw_spans {
            if sp.kind != NvrEventKind::Continuous {
                events.push(sp.clone());
            }
        }
        let coverage = merge_coverage_spans(raw_spans, channel);
        let merged_events = merge_event_spans(events);
        Ok(Some((coverage, merged_events)))
    }
}

// =============================================================================
// RAII / Async Cleanup Guards (`PB-02`: Zero Leaked NVR Finder Slots)
// =============================================================================

struct CgiFinderGuard<'a> {
    client: &'a NvrHttpClient,
    digest_cache: Arc<Mutex<DigestCacheMap>>,
    target: HttpTarget,
    object_id: String,
    cleaned_up: bool,
}

impl<'a> CgiFinderGuard<'a> {
    fn new(client: &'a NvrHttpClient, target: HttpTarget, object_id: String) -> Self {
        Self {
            client,
            digest_cache: Arc::clone(&client.digest_cache),
            target,
            object_id,
            cleaned_up: false,
        }
    }

    async fn cleanup(&mut self) {
        if self.cleaned_up {
            return;
        }
        self.cleaned_up = true;
        let close_uri = format!(
            "/cgi-bin/mediaFileFind.cgi?action=close&object={}",
            self.object_id
        );
        let destroy_uri = format!(
            "/cgi-bin/mediaFileFind.cgi?action=destroy&object={}",
            self.object_id
        );
        let _ = time::timeout(CLEANUP_TIMEOUT, async {
            let _ = self
                .client
                .send_digest_request(&self.target, "GET", &close_uri, &[], b"")
                .await;
            let _ = self
                .client
                .send_digest_request(&self.target, "GET", &destroy_uri, &[], b"")
                .await;
        })
        .await;
    }
}

impl Drop for CgiFinderGuard<'_> {
    fn drop(&mut self) {
        if self.cleaned_up {
            return;
        }
        self.cleaned_up = true;
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let digest_cache = Arc::clone(&self.digest_cache);
        let target = self.target.clone();
        let object_id = self.object_id.clone();

        handle.spawn(async move {
            let close_uri = format!("/cgi-bin/mediaFileFind.cgi?action=close&object={}", object_id);
            let destroy_uri = format!(
                "/cgi-bin/mediaFileFind.cgi?action=destroy&object={}",
                object_id
            );
            let _ = time::timeout(CLEANUP_TIMEOUT, async {
                let auth_close =
                    build_cached_auth_from_map(&digest_cache, &target, "GET", &close_uri);
                let _ = fire_and_forget_http(
                    &target.host,
                    target.port,
                    "GET",
                    &close_uri,
                    auth_close.as_deref(),
                    &[],
                    b"",
                )
                .await;
                let auth_destroy =
                    build_cached_auth_from_map(&digest_cache, &target, "GET", &destroy_uri);
                let _ = fire_and_forget_http(
                    &target.host,
                    target.port,
                    "GET",
                    &destroy_uri,
                    auth_destroy.as_deref(),
                    &[],
                    b"",
                )
                .await;
            })
            .await;
        });
    }
}

struct Rpc2FinderGuard<'a> {
    client: &'a NvrHttpClient,
    target: HttpTarget,
    session: String,
    object_val: Value,
    cleaned_up: bool,
}

impl<'a> Rpc2FinderGuard<'a> {
    fn new(
        client: &'a NvrHttpClient,
        target: HttpTarget,
        session: String,
        object_val: Value,
    ) -> Self {
        Self {
            client,
            target,
            session,
            object_val,
            cleaned_up: false,
        }
    }

    async fn cleanup(&mut self) {
        if self.cleaned_up {
            return;
        }
        self.cleaned_up = true;
        let close_req = json!({
            "method": "mediaFileFind.close",
            "params": Value::Null,
            "object": self.object_val,
            "id": 98,
            "session": self.session
        });
        let destroy_req = json!({
            "method": "mediaFileFind.destroy",
            "params": Value::Null,
            "object": self.object_val,
            "id": 99,
            "session": self.session
        });
        let _ = time::timeout(CLEANUP_TIMEOUT, async {
            let _ = self
                .client
                .rpc2_call(&self.target, &self.session, &close_req)
                .await;
            let _ = self
                .client
                .rpc2_call(&self.target, &self.session, &destroy_req)
                .await;
        })
        .await;
    }
}

impl Drop for Rpc2FinderGuard<'_> {
    fn drop(&mut self) {
        if self.cleaned_up {
            return;
        }
        self.cleaned_up = true;
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let target = self.target.clone();
        let session = self.session.clone();
        let object_val = self.object_val.clone();

        handle.spawn(async move {
            let cookie = format!("DhWebClientSessionID={}", session);
            let close_body = serde_json::to_vec(&json!({
                "method": "mediaFileFind.close",
                "params": Value::Null,
                "object": object_val,
                "id": 98,
                "session": session
            }))
            .unwrap_or_default();
            let destroy_body = serde_json::to_vec(&json!({
                "method": "mediaFileFind.destroy",
                "params": Value::Null,
                "object": object_val,
                "id": 99,
                "session": session
            }))
            .unwrap_or_default();

            let _ = time::timeout(CLEANUP_TIMEOUT, async {
                let _ = fire_and_forget_http(
                    &target.host,
                    target.port,
                    "POST",
                    "/RPC2",
                    None,
                    &[("Content-Type", "application/json"), ("Cookie", &cookie)],
                    &close_body,
                )
                .await;
                let _ = fire_and_forget_http(
                    &target.host,
                    target.port,
                    "POST",
                    "/RPC2",
                    None,
                    &[("Content-Type", "application/json"), ("Cookie", &cookie)],
                    &destroy_body,
                )
                .await;
            })
            .await;
        });
    }
}

async fn fire_and_forget_http(
    host: &str,
    port: u16,
    method: &str,
    path_and_query: &str,
    auth: Option<&str>,
    extra_headers: &[(&str, &str)],
    body: &[u8],
) -> io::Result<()> {
    let mut stream = TcpStream::connect(format!("{}:{}", host, port)).await?;
    let _ = stream.set_nodelay(true);
    let req = build_http_request_bytes(method, path_and_query, host, port, auth, extra_headers, body);
    stream.write_all(&req).await?;
    let mut drain = [0u8; 512];
    let _ = stream.read(&mut drain).await;
    Ok(())
}

// =============================================================================
// Low-Level HTTP/1.1 Wire Framing & Chunked Transfer-Encoding Decoder
// =============================================================================

fn build_http_request_bytes(
    method: &str,
    path_and_query: &str,
    host: &str,
    port: u16,
    auth_header: Option<&str>,
    extra_headers: &[(&str, &str)],
    body: &[u8],
) -> Vec<u8> {
    let safe_method = sanitize_header_token(method, 16);
    let safe_pq = sanitize_header_token(path_and_query, 4096);
    let host_hdr = if port == 80 {
        host.to_string()
    } else {
        format!("{}:{}", host, port)
    };

    let mut out = Vec::with_capacity(256 + body.len());
    out.extend_from_slice(
        format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: NVRCommandCenter/3.0\r\nConnection: close\r\n",
            safe_method, safe_pq, host_hdr
        )
        .as_bytes(),
    );

    if let Some(auth) = auth_header {
        let safe_auth = sanitize_header_token(auth, 4096);
        out.extend_from_slice(b"Authorization: ");
        out.extend_from_slice(safe_auth.as_bytes());
        out.extend_from_slice(b"\r\n");
    }

    for &(k, v) in extra_headers {
        let sk = sanitize_header_token(k, 128);
        let sv = sanitize_header_token(v, 2048);
        out.extend_from_slice(sk.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(sv.as_bytes());
        out.extend_from_slice(b"\r\n");
    }

    if !body.is_empty() {
        out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

async fn read_http_response_head<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> io::Result<(u16, Vec<(String, String)>, BodyFraming)> {
    let mut raw = Vec::with_capacity(512);
    loop {
        if raw.ends_with(b"\r\n\r\n") {
            break;
        }
        if raw.len() >= MAX_HTTP_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP header exceeds 32 KiB limit",
            ));
        }
        let avail = reader.fill_buf().await?;
        if avail.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "EOF before HTTP response headers completed",
            ));
        }
        let mut consumed = 0;
        for &b in avail {
            raw.push(b);
            consumed += 1;
            if raw.ends_with(b"\r\n\r\n") || raw.len() >= MAX_HTTP_HEADER_BYTES {
                break;
            }
        }
        reader.consume(consumed);
    }

    let head_str = std::str::from_utf8(&raw[..raw.len() - 4])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Non-UTF8 HTTP response header"))?;
    let mut lines = head_str.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Empty HTTP status line"))?;

    let mut status_parts = status_line.split_whitespace();
    let proto = status_parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Malformed HTTP status line"))?;
    if !proto.to_ascii_uppercase().starts_with("HTTP/") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Response does not start with HTTP/",
        ));
    }
    let status = status_parts
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid HTTP status code"))?;

    let mut headers = Vec::with_capacity(12);
    let mut is_chunked = false;
    let mut content_len: Option<usize> = None;

    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let name = k.trim().to_string();
            let val = v.trim().to_string();
            if name.eq_ignore_ascii_case("transfer-encoding")
                && val.to_ascii_lowercase().contains("chunked")
            {
                is_chunked = true;
            } else if name.eq_ignore_ascii_case("content-length") {
                let parsed = val.parse::<usize>().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "Invalid HTTP Content-Length")
                })?;
                content_len = Some(parsed);
            }
            headers.push((name, val));
        }
    }

    let framing = if status == 204 || status == 304 {
        BodyFraming::ContentLength(0)
    } else if is_chunked {
        BodyFraming::Chunked
    } else if let Some(len) = content_len {
        BodyFraming::ContentLength(len)
    } else {
        BodyFraming::UntilEof
    };

    Ok((status, headers, framing))
}

async fn read_crlf_line_bounded<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    max_len: usize,
) -> io::Result<String> {
    let mut line = Vec::with_capacity(64);
    loop {
        if line.ends_with(b"\r\n") {
            line.truncate(line.len() - 2);
            return String::from_utf8(line).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "Non-UTF8 chunk header line")
            });
        }
        if line.len() >= max_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Chunk header line exceeded limit",
            ));
        }
        let avail = reader.fill_buf().await?;
        if avail.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Unexpected EOF in chunked framing",
            ));
        }
        let mut consumed = 0;
        for &b in avail {
            line.push(b);
            consumed += 1;
            if line.ends_with(b"\r\n") || line.len() >= max_len {
                break;
            }
        }
        reader.consume(consumed);
    }
}

async fn decode_chunked_body_bounded<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    max_bytes: usize,
) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let size_line = read_crlf_line_bounded(reader, 256).await?;
        let hex_part = size_line
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        let chunk_size = usize::from_str_radix(hex_part, 16).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "Invalid hex chunk size")
        })?;

        if chunk_size == 0 {
            // Drain optional chunk trailers until empty CRLF line
            loop {
                let trailer = read_crlf_line_bounded(reader, 1024).await?;
                if trailer.is_empty() {
                    break;
                }
            }
            return Ok(out);
        }

        if out.len().saturating_add(chunk_size) > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Chunked HTTP response exceeded 2 MiB API limit",
            ));
        }

        let start = out.len();
        out.resize(start + chunk_size, 0);
        reader.read_exact(&mut out[start..start + chunk_size]).await?;

        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf).await?;
        if &crlf != b"\r\n" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Missing CRLF after HTTP chunk payload",
            ));
        }
    }
}

async fn read_bounded_http_body<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    framing: BodyFraming,
    max_bytes: usize,
) -> io::Result<Vec<u8>> {
    match framing {
        BodyFraming::ContentLength(len) => {
            if len > max_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "HTTP Content-Length exceeded 2 MiB API limit",
                ));
            }
            let mut buf = vec![0u8; len];
            if len > 0 {
                reader.read_exact(&mut buf).await?;
            }
            Ok(buf)
        }
        BodyFraming::Chunked => decode_chunked_body_bounded(reader, max_bytes).await,
        BodyFraming::UntilEof => {
            let mut out = Vec::new();
            let mut scratch = [0u8; 8192];
            loop {
                let n = reader.read(&mut scratch).await?;
                if n == 0 {
                    break;
                }
                if out.len() + n > max_bytes {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "EOF-framed HTTP response exceeded 2 MiB API limit",
                    ));
                }
                out.extend_from_slice(&scratch[..n]);
            }
            Ok(out)
        }
    }
}

async fn stream_http_body_chunks<R, F>(
    reader: &mut BufReader<R>,
    framing: BodyFraming,
    max_bytes: u64,
    abort_flag: Option<&AtomicBool>,
    on_chunk: &mut F,
) -> Result<u64, String>
where
    R: tokio::io::AsyncRead + Unpin,
    F: FnMut(&[u8]) -> bool,
{
    let mut total_bytes: u64 = 0;
    let mut buf = vec![0u8; DHAV_CHUNK_SIZE];
    let mut checked_first_chunk = false;

    match framing {
        BodyFraming::ContentLength(content_len) => {
            if (content_len as u64) > max_bytes {
                return Err(format!(
                    "DHAV Content-Length exceeds {} byte safety cap",
                    max_bytes
                ));
            }
            let mut remaining = content_len;
            while remaining > 0 {
                if abort_flag.is_some_and(|f| f.load(Ordering::Acquire)) {
                    return Err("DHAV stream aborted".to_string());
                }
                let to_read = remaining.min(DHAV_CHUNK_SIZE);
                let n = await_with_abort(
                    async {
                        reader
                            .read(&mut buf[..to_read])
                            .await
                            .map_err(|e| format!("DHAV stream read error: {}", e))
                    },
                    HTTP_READ_TIMEOUT,
                    abort_flag,
                    "DHAV stream read timeout",
                )
                .await?;
                if n == 0 {
                    return Err("Unexpected EOF during Content-Length DHAV stream".to_string());
                }
                if !checked_first_chunk {
                    checked_first_chunk = true;
                    if content_len < 256 && is_dahua_body_error(&buf[..n]) {
                        let msg = String::from_utf8_lossy(&buf[..n]);
                        if is_dahua_no_record_text(&msg) {
                            return Ok(0);
                        }
                        return Err(format!("loadfile.cgi returned error: {}", msg.trim()));
                    }
                }
                if total_bytes.saturating_add(n as u64) > max_bytes {
                    return Err(format!(
                        "DHAV stream exceeded {} byte safety cap",
                        max_bytes
                    ));
                }
                total_bytes += n as u64;
                remaining -= n;
                if !on_chunk(&buf[..n]) {
                    return Ok(total_bytes);
                }
            }
        }
        BodyFraming::Chunked => {
            loop {
                if abort_flag.is_some_and(|f| f.load(Ordering::Acquire)) {
                    return Err("DHAV stream aborted".to_string());
                }
                let size_line = await_with_abort(
                    async {
                        read_crlf_line_bounded(reader, 256)
                            .await
                            .map_err(|e| format!("DHAV chunk header error: {}", e))
                    },
                    HTTP_READ_TIMEOUT,
                    abort_flag,
                    "DHAV chunk header timeout",
                )
                .await?;

                let hex_part = size_line.split(';').next().unwrap_or("").trim();
                let mut chunk_rem = usize::from_str_radix(hex_part, 16)
                    .map_err(|_| "Invalid hex chunk size in DHAV stream".to_string())?;

                if chunk_rem == 0 {
                    while let Ok(line) = await_with_abort(
                        async {
                            read_crlf_line_bounded(reader, 1024)
                                .await
                                .map_err(|e| e.to_string())
                        },
                        Duration::from_secs(2),
                        abort_flag,
                        "DHAV trailer timeout",
                    )
                    .await
                    {
                        if line.is_empty() {
                            break;
                        }
                    }
                    break;
                }

                while chunk_rem > 0 {
                    if abort_flag.is_some_and(|f| f.load(Ordering::Acquire)) {
                        return Err("DHAV stream aborted".to_string());
                    }
                    let to_read = chunk_rem.min(DHAV_CHUNK_SIZE);
                    let n = await_with_abort(
                        async {
                            reader
                                .read(&mut buf[..to_read])
                                .await
                                .map_err(|e| format!("DHAV chunk payload error: {}", e))
                        },
                        HTTP_READ_TIMEOUT,
                        abort_flag,
                        "DHAV chunk payload timeout",
                    )
                    .await?;
                    if n == 0 {
                        return Err("Unexpected EOF inside DHAV chunk".to_string());
                    }
                    if !checked_first_chunk {
                        checked_first_chunk = true;
                        if chunk_rem < 256 && is_dahua_body_error(&buf[..n]) {
                            let msg = String::from_utf8_lossy(&buf[..n]);
                            if is_dahua_no_record_text(&msg) {
                                return Ok(0);
                            }
                            return Err(format!("loadfile.cgi returned error: {}", msg.trim()));
                        }
                    }
                    if total_bytes.saturating_add(n as u64) > max_bytes {
                        return Err(format!(
                            "DHAV stream exceeded {} byte safety cap",
                            max_bytes
                        ));
                    }
                    total_bytes += n as u64;
                    chunk_rem -= n;
                    if !on_chunk(&buf[..n]) {
                        return Ok(total_bytes);
                    }
                }

                let mut crlf = [0u8; 2];
                await_with_abort(
                    async {
                        reader
                            .read_exact(&mut crlf)
                            .await
                            .map_err(|e| format!("DHAV chunk CRLF error: {}", e))
                    },
                    HTTP_READ_TIMEOUT,
                    abort_flag,
                    "DHAV chunk CRLF timeout",
                )
                .await?;
                if &crlf != b"\r\n" {
                    return Err("Malformed CRLF in chunked DHAV stream".to_string());
                }
            }
        }
        BodyFraming::UntilEof => {
            loop {
                if abort_flag.is_some_and(|f| f.load(Ordering::Acquire)) {
                    return Err("DHAV stream aborted".to_string());
                }
                let n = await_with_abort(
                    async {
                        reader
                            .read(&mut buf)
                            .await
                            .map_err(|e| format!("DHAV stream read error: {}", e))
                    },
                    HTTP_READ_TIMEOUT,
                    abort_flag,
                    "DHAV stream read timeout",
                )
                .await?;
                if n == 0 {
                    break;
                }
                if !checked_first_chunk {
                    checked_first_chunk = true;
                    if n < 256 && is_dahua_body_error(&buf[..n]) {
                        let msg = String::from_utf8_lossy(&buf[..n]);
                        if is_dahua_no_record_text(&msg) {
                            return Ok(0);
                        }
                        return Err(format!("loadfile.cgi returned error: {}", msg.trim()));
                    }
                }
                if total_bytes.saturating_add(n as u64) > max_bytes {
                    return Err(format!(
                        "DHAV stream exceeded {} byte safety cap",
                        max_bytes
                    ));
                }
                total_bytes += n as u64;
                if !on_chunk(&buf[..n]) {
                    return Ok(total_bytes);
                }
            }
        }
    }

    Ok(total_bytes)
}

// =============================================================================
// Dahua / Hikvision Response Parsers & Span Mergers
// =============================================================================

#[derive(Debug, Clone)]
struct RawDahuaFileItem {
    channel: usize,
    start_ms: i64,
    end_ms: i64,
    file_path: Option<String>,
    byte_len: u64,
    main_stream: bool,
    file_type: String,
    has_timing_flag: bool,
    has_event_flag: bool,
    events: Vec<String>,
}

fn parse_factory_create_object_id(body: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("result=") {
            let candidate: String = rest
                .trim()
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .take(64)
                .collect();
            if !candidate.is_empty() && !candidate.eq_ignore_ascii_case("false") {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_dahua_body_error(body: &[u8]) -> bool {
    let s = String::from_utf8_lossy(body);
    let trimmed = s.trim_start();
    trimmed.starts_with("Error")
        || trimmed.starts_with("error")
        || trimmed.contains("ErrorID=")
        || trimmed.contains("Bad Request")
}

fn is_dahua_no_record_text(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.contains("no record")
        || lower.contains("no file")
        || lower.contains("can not find")
        || lower.contains("cannot find")
        || lower.contains("errorid=3")
}

fn is_rpc2_credential_error(v2: &Value) -> bool {
    let err_code = v2["error"]["code"].as_i64();
    let err_msg = v2["error"]["message"]
        .as_str()
        .unwrap_or("")
        .to_ascii_lowercase();
    if err_msg.contains("max")
        || err_msg.contains("busy")
        || err_msg.contains("connect")
        || err_msg.contains("encrypt")
        || err_msg.contains("support")
        || err_msg.contains("timeout")
        || err_msg.contains("resource")
        || err_msg.contains("logged in")
    {
        return false;
    }
    if matches!(
        err_code,
        Some(268632079 | 268632080 | 268632081 | 268632085 | 401)
    ) {
        return true;
    }
    if v2["params"].get("remainLoginTimes").is_some()
        || v2["params"].get("remainLockSecond").is_some()
    {
        return true;
    }
    err_msg.contains("password")
        || err_msg.contains("user")
        || err_msg.contains("account")
        || err_msg.contains("locked")
        || err_msg.contains("auth")
        || err_msg.contains("credential")
        || err_msg.contains("invalid")
}

fn is_dahua_find_file_positive(body: &[u8]) -> bool {
    let s = String::from_utf8_lossy(body);
    let trimmed = s.trim();
    if trimmed.eq_ignore_ascii_case("ok") {
        return true;
    }
    for line in trimmed.lines() {
        let l = line.trim();
        if l.eq_ignore_ascii_case("ok") || l.eq_ignore_ascii_case("result=true") {
            return true;
        }
        if l.eq_ignore_ascii_case("result=false") {
            return false;
        }
    }
    false
}

fn json_value_to_id_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn parse_dahua_datetime_ms(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    for fmt in [
        "%Y-%m-%d %H:%M:%S",
        "%Y/%m/%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y_%m_%d_%H_%M_%S",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(trimmed, fmt) {
            return Some(local_naive_to_epoch_ms(dt));
        }
    }
    None
}

fn parse_times_from_dahua_filepath(file_path: &str, fallback_date: NaiveDate) -> Option<(i64, i64)> {
    // Matches `.../HH.MM.SS-HH.MM.SS[...`
    let base_name = file_path.rsplit('/').next().unwrap_or(file_path);
    let time_part = base_name.split('[').next().unwrap_or(base_name);
    let (s_str, e_str) = time_part.split_once('-')?;

    let date = extract_date_from_filepath(file_path).unwrap_or(fallback_date);
    let s_time = NaiveTime::parse_from_str(s_str.trim(), "%H.%M.%S").ok()?;
    let e_time = NaiveTime::parse_from_str(e_str.trim(), "%H.%M.%S").ok()?;

    let s_ms = local_naive_to_epoch_ms(NaiveDateTime::new(date, s_time));
    let mut e_ms = local_naive_to_epoch_ms(NaiveDateTime::new(date, e_time));
    if e_ms < s_ms {
        // Crossed midnight
        if let Some(next_d) = date.succ_opt() {
            e_ms = local_naive_to_epoch_ms(NaiveDateTime::new(next_d, e_time));
        }
    }
    Some((s_ms, e_ms))
}

fn extract_date_from_filepath(file_path: &str) -> Option<NaiveDate> {
    for seg in file_path.split('/') {
        if seg.len() == 10 && seg.as_bytes().get(4) == Some(&b'-') {
            if let Ok(d) = NaiveDate::parse_from_str(seg, "%Y-%m-%d") {
                return Some(d);
            }
        }
    }
    None
}

fn classify_event_token(tok: &str) -> Option<NvrEventKind> {
    let lower = tok.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    if lower.contains("smartmotionhuman")
        || lower.contains("facedetection")
        || lower.contains("humantrait")
        || lower == "human"
    {
        Some(NvrEventKind::Human)
    } else if lower.contains("smartmotionvehicle")
        || lower.contains("trafficjunction")
        || lower.contains("platedetection")
        || lower == "vehicle"
    {
        Some(NvrEventKind::Vehicle)
    } else if lower.contains("crosslinedetection")
        || lower.contains("crossregiondetection")
        || lower.contains("intrusion")
        || lower.contains("wanderdetection")
        || lower.contains("tripwire")
    {
        Some(NvrEventKind::Intrusion)
    } else if lower.contains("alarmlocal")
        || lower.contains("videoloss")
        || lower.contains("videoblind")
        || lower.contains("audioanomaly")
        || lower == "alarm"
    {
        Some(NvrEventKind::Alarm)
    } else {
        Some(NvrEventKind::Motion)
    }
}

fn classify_filepath_bracket_tag(file_path: &str) -> Option<NvrEventKind> {
    if file_path.contains("[M]") {
        Some(NvrEventKind::Motion)
    } else if file_path.contains("[A]") {
        Some(NvrEventKind::Alarm)
    } else if file_path.contains("[I]") {
        Some(NvrEventKind::Intrusion)
    } else {
        None
    }
}

fn determine_item_event_kind(item: &RawDahuaFileItem) -> Option<NvrEventKind> {
    let mut best: Option<NvrEventKind> = None;
    for ev in &item.events {
        if let Some(kind) = classify_event_token(ev) {
            best = match best {
                None => Some(kind),
                Some(prev) if kind.specificity_rank() > prev.specificity_rank() => Some(kind),
                Some(prev) => Some(prev),
            };
        }
    }
    if best.is_some() {
        return best;
    }
    if let Some(ref fp) = item.file_path {
        if let Some(tag_kind) = classify_filepath_bracket_tag(fp) {
            return Some(tag_kind);
        }
    }
    if item.has_event_flag {
        return Some(NvrEventKind::Motion);
    }
    None
}

fn parse_dahua_find_next_file_kv(
    body: &str,
    channel: usize,
    fallback_date: NaiveDate,
) -> (usize, Vec<RawDahuaFileItem>) {
    #[derive(Default)]
    struct PartialItem {
        start_time: Option<String>,
        end_time: Option<String>,
        file_path: Option<String>,
        byte_len: u64,
        video_stream: Option<String>,
        file_type: Option<String>,
        flags: Vec<String>,
        events: Vec<String>,
    }

    let mut found: Option<usize> = None;
    let mut map: HashMap<usize, PartialItem> = HashMap::new();
    let mut indices: Vec<usize> = Vec::new();

    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim();

        if key.eq_ignore_ascii_case("found") {
            found = val.parse::<usize>().ok();
            continue;
        }

        // Parse `<prefix>[<idx>].<field>` (handles `items[0].StartTime` and `table.file_info[0].StartTime`)
        let Some(open_br) = key.find('[') else {
            continue;
        };
        let after_open = &key[open_br + 1..];
        let Some(close_br) = after_open.find(']') else {
            continue;
        };
        let Ok(idx) = after_open[..close_br].parse::<usize>() else {
            continue;
        };
        let rest = after_open[close_br + 1..].strip_prefix('.').unwrap_or("");
        if rest.is_empty() {
            continue;
        }

        let entry = map.entry(idx).or_insert_with(|| {
            indices.push(idx);
            PartialItem::default()
        });

        let field_base = rest.split('[').next().unwrap_or(rest);
        match field_base {
            "StartTime" => entry.start_time = Some(val.to_string()),
            "EndTime" => entry.end_time = Some(val.to_string()),
            "FilePath" => entry.file_path = Some(val.to_string()),
            "Length" | "Size" => {
                if let Some(hex_s) = val.strip_prefix("0x").or_else(|| val.strip_prefix("0X")) {
                    if let Ok(v) = u64::from_str_radix(hex_s, 16) {
                        entry.byte_len = v;
                    }
                } else if let Ok(v) = val.parse::<u64>() {
                    entry.byte_len = v;
                }
            }
            "VideoStream" => entry.video_stream = Some(val.to_string()),
            "Type" => entry.file_type = Some(val.to_string()),
            "Flags" if !val.is_empty() => entry.flags.push(val.to_string()),
            "Events" if !val.is_empty() => entry.events.push(val.to_string()),
            _ => {}
        }
    }

    indices.sort_unstable();
    let mut out = Vec::with_capacity(indices.len());

    for idx in indices {
        let Some(p) = map.remove(&idx) else {
            continue;
        };
        let ftype = p.file_type.unwrap_or_else(|| "dav".to_string());
        if ftype.eq_ignore_ascii_case("jpg") || ftype.eq_ignore_ascii_case("bmp") {
            continue;
        }

        let times = match (&p.start_time, &p.end_time) {
            (Some(s), Some(e)) => match (parse_dahua_datetime_ms(s), parse_dahua_datetime_ms(e)) {
                (Some(sm), Some(em)) => Some((sm, em)),
                _ => p
                    .file_path
                    .as_deref()
                    .and_then(|fp| parse_times_from_dahua_filepath(fp, fallback_date)),
            },
            _ => p
                .file_path
                .as_deref()
                .and_then(|fp| parse_times_from_dahua_filepath(fp, fallback_date)),
        };

        let Some((start_ms, mut end_ms)) = times else {
            continue;
        };
        if end_ms < start_ms {
            continue;
        }
        if end_ms == start_ms {
            end_ms = start_ms + 1000;
        }

        let main_stream = match p.video_stream.as_deref() {
            Some(vs) => {
                !vs.eq_ignore_ascii_case("extra1")
                    && !vs.eq_ignore_ascii_case("extra2")
                    && !vs.eq_ignore_ascii_case("sub")
            }
            None => true,
        };

        let has_timing_flag = p
            .flags
            .iter()
            .any(|f| f.eq_ignore_ascii_case("timing"));
        let has_event_flag = p
            .flags
            .iter()
            .any(|f| f.eq_ignore_ascii_case("event"));

        out.push(RawDahuaFileItem {
            channel,
            start_ms,
            end_ms,
            file_path: p.file_path,
            byte_len: p.byte_len,
            main_stream,
            file_type: ftype,
            has_timing_flag,
            has_event_flag,
            events: p.events,
        });
    }

    let reported_found = found.unwrap_or(out.len());
    (reported_found, out)
}

fn parse_rpc2_file_info(
    info: &Value,
    channel: usize,
    fallback_date: NaiveDate,
) -> Option<RawDahuaFileItem> {
    let ftype = info["Type"].as_str().unwrap_or("dav").to_string();
    if ftype.eq_ignore_ascii_case("jpg") || ftype.eq_ignore_ascii_case("bmp") {
        return None;
    }
    let file_path = info["FilePath"].as_str().map(|s| s.to_string());
    let times = match (info["StartTime"].as_str(), info["EndTime"].as_str()) {
        (Some(s), Some(e)) => match (parse_dahua_datetime_ms(s), parse_dahua_datetime_ms(e)) {
            (Some(sm), Some(em)) => Some((sm, em)),
            _ => file_path
                .as_deref()
                .and_then(|fp| parse_times_from_dahua_filepath(fp, fallback_date)),
        },
        _ => file_path
            .as_deref()
            .and_then(|fp| parse_times_from_dahua_filepath(fp, fallback_date)),
    }?;

    let (start_ms, mut end_ms) = times;
    if end_ms < start_ms {
        return None;
    }
    if end_ms == start_ms {
        end_ms = start_ms + 1000;
    }

    let byte_len = info["Length"]
        .as_u64()
        .or_else(|| info["Length"].as_str().and_then(|s| s.parse::<u64>().ok()))
        .unwrap_or(0);

    let main_stream = match info["VideoStream"].as_str() {
        Some(vs) => {
            !vs.eq_ignore_ascii_case("extra1")
                && !vs.eq_ignore_ascii_case("extra2")
                && !vs.eq_ignore_ascii_case("sub")
        }
        None => true,
    };

    let mut has_timing_flag = false;
    let mut has_event_flag = false;
    if let Some(flags) = info["Flags"].as_array() {
        for f in flags {
            if let Some(fs) = f.as_str() {
                if fs.eq_ignore_ascii_case("timing") {
                    has_timing_flag = true;
                } else if fs.eq_ignore_ascii_case("event") {
                    has_event_flag = true;
                }
            }
        }
    }

    let mut events = Vec::new();
    if let Some(ev_arr) = info["Events"].as_array() {
        for ev in ev_arr {
            if let Some(es) = ev.as_str() {
                events.push(es.to_string());
            }
        }
    }

    Some(RawDahuaFileItem {
        channel,
        start_ms,
        end_ms,
        file_path,
        byte_len,
        main_stream,
        file_type: ftype,
        has_timing_flag,
        has_event_flag,
        events,
    })
}

fn assemble_timeline_from_passes(
    channel: usize,
    pass1_items: Vec<RawDahuaFileItem>,
    pass2_items: Vec<RawDahuaFileItem>,
) -> (Vec<RecordedSpan>, Vec<RecordedSpan>) {
    let mut raw_coverage = Vec::with_capacity(pass1_items.len() + pass2_items.len());
    let mut pass2_events = Vec::with_capacity(pass2_items.len());

    for item in &pass2_items {
        let _ = &item.file_type;
        let kind = determine_item_event_kind(item).unwrap_or(NvrEventKind::Motion);
        let span = RecordedSpan {
            channel: item.channel,
            start_ms: item.start_ms,
            end_ms: item.end_ms,
            kind,
            file_path: item.file_path.clone(),
            byte_len: item.byte_len,
            main_stream: item.main_stream,
        };
        raw_coverage.push(span.clone());
        pass2_events.push(span);
    }

    let mut pass1_events = Vec::new();
    for item in pass1_items {
        if let Some(ev_kind) = determine_item_event_kind(&item) {
            // In Dahua 60-min pack mode, a file can have both `Timing` and `Event` flags while Pass 2
            // provides the exact second-accurate sub-spans inside it. Keep Pass 1's event span unless
            // it is a `Timing` pack container already refined by overlapping Pass 2 sub-spans.
            let refined_by_pass2 = item.has_timing_flag
                && pass2_events
                    .iter()
                    .any(|p2| p2.start_ms < item.end_ms && p2.end_ms > item.start_ms);
            if !refined_by_pass2 {
                pass1_events.push(RecordedSpan {
                    channel: item.channel,
                    start_ms: item.start_ms,
                    end_ms: item.end_ms,
                    kind: ev_kind,
                    file_path: item.file_path.clone(),
                    byte_len: item.byte_len,
                    main_stream: item.main_stream,
                });
            }
        }

        raw_coverage.push(RecordedSpan {
            channel: item.channel,
            start_ms: item.start_ms,
            end_ms: item.end_ms,
            kind: NvrEventKind::Continuous,
            file_path: item.file_path,
            byte_len: item.byte_len,
            main_stream: item.main_stream,
        });
    }

    pass2_events.extend(pass1_events);
    let coverage = merge_coverage_spans(raw_coverage, channel);
    let events = merge_event_spans(pass2_events);
    (coverage, events)
}

fn merge_coverage_spans(mut spans: Vec<RecordedSpan>, channel: usize) -> Vec<RecordedSpan> {
    spans.retain(|s| s.end_ms > s.start_ms);
    if spans.is_empty() {
        return Vec::new();
    }
    spans.sort_by(|a, b| a.start_ms.cmp(&b.start_ms).then(a.end_ms.cmp(&b.end_ms)));

    let mut merged: Vec<RecordedSpan> = Vec::with_capacity(spans.len());
    for mut sp in spans {
        sp.channel = channel;
        sp.kind = NvrEventKind::Continuous;
        if let Some(last) = merged.last_mut() {
            if sp.start_ms <= last.end_ms.saturating_add(COVERAGE_MERGE_GAP_MS) {
                if sp.end_ms > last.end_ms {
                    last.end_ms = sp.end_ms;
                }
                last.byte_len = last.byte_len.saturating_add(sp.byte_len);
                last.main_stream |= sp.main_stream;
                if last.file_path.is_none() {
                    last.file_path = sp.file_path;
                }
                continue;
            }
        }
        merged.push(sp);
    }
    merged
}

fn merge_event_spans(mut spans: Vec<RecordedSpan>) -> Vec<RecordedSpan> {
    spans.retain(|s| s.end_ms > s.start_ms);
    if spans.is_empty() {
        return Vec::new();
    }
    spans.sort_by(|a, b| {
        a.start_ms
            .cmp(&b.start_ms)
            .then(a.end_ms.cmp(&b.end_ms))
            .then(b.kind.specificity_rank().cmp(&a.kind.specificity_rank()))
    });

    let mut merged: Vec<RecordedSpan> = Vec::with_capacity(spans.len());
    for sp in spans {
        if let Some(last) = merged.last_mut() {
            if last.kind == sp.kind
                && sp.start_ms <= last.end_ms.saturating_add(EVENT_MERGE_GAP_MS)
            {
                if sp.end_ms > last.end_ms {
                    last.end_ms = sp.end_ms;
                }
                last.byte_len = last.byte_len.max(sp.byte_len);
                last.main_stream |= sp.main_stream;
                continue;
            }
        }
        merged.push(sp);
    }
    merged
}

fn last_day_of_month(year: i32, month: u32) -> u32 {
    let (next_y, next_m) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    NaiveDate::from_ymd_opt(next_y, next_m, 1)
        .and_then(|d| d.pred_opt())
        .map(|d| d.day())
        .unwrap_or(28)
}

fn collect_days_touched_by_span(
    start_ms: i64,
    end_ms: i64,
    year: i32,
    month: u32,
    days_out: &mut HashSet<u32>,
) {
    let start_date = epoch_ms_to_local_naive(start_ms).date();
    let effective_end_ms = if end_ms > start_ms {
        end_ms - 1
    } else {
        start_ms
    };
    let end_date = epoch_ms_to_local_naive(effective_end_ms).date();

    let mut d = start_date;
    for _ in 0..366 {
        if d > end_date {
            break;
        }
        if d.year() == year && d.month() == month {
            days_out.insert(d.day());
        }
        match d.succ_opt() {
            Some(next) => d = next,
            None => break,
        }
    }
}

fn format_pseudo_uuid(hex32: &str) -> String {
    if hex32.len() >= 32 {
        format!(
            "{}-{}-4{}-a{}-{}",
            &hex32[0..8],
            &hex32[8..12],
            &hex32[13..16],
            &hex32[17..20],
            &hex32[20..32]
        )
    } else {
        "00000000-0000-4000-a000-000000000001".to_string()
    }
}

// =============================================================================
// Hikvision ISAPI Safe XML Parsers (`quick_xml`, zero custom entities, depth <= 24)
// =============================================================================

fn parse_hikvision_utc_ms(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return Some(dt.timestamp_millis());
    }
    let no_z = trimmed
        .strip_suffix('Z')
        .or_else(|| trimmed.strip_suffix('z'))
        .unwrap_or(trimmed);
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y%m%dT%H%M%S%.f",
        "%Y%m%dT%H%M%S",
    ] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(no_z, fmt) {
            return Some(ndt.and_utc().timestamp_millis());
        }
    }
    None
}

fn classify_hikvision_descriptor(desc: &str) -> NvrEventKind {
    let lower = desc.to_ascii_lowercase();
    if lower.contains("human") || lower.contains("face") {
        NvrEventKind::Human
    } else if lower.contains("vehicle") || lower.contains("plate") {
        NvrEventKind::Vehicle
    } else if lower.contains("linedetection")
        || lower.contains("fielddetection")
        || lower.contains("intrusion")
        || lower.contains("region")
    {
        NvrEventKind::Intrusion
    } else if lower.contains("alarm") || lower.contains("videoloss") || lower.contains("tamper") {
        NvrEventKind::Alarm
    } else if lower.contains("motion") || lower.contains("vmd") {
        NvrEventKind::Motion
    } else {
        NvrEventKind::Continuous
    }
}

fn resolve_safe_xml_general_ref(raw_ref: &[u8]) -> Result<char, String> {
    match raw_ref {
        b"amp" => Ok('&'),
        b"lt" => Ok('<'),
        b"gt" => Ok('>'),
        b"apos" => Ok('\''),
        b"quot" => Ok('"'),
        _ if raw_ref.starts_with(b"#x") || raw_ref.starts_with(b"#X") => {
            let hex_s = std::str::from_utf8(&raw_ref[2..])
                .map_err(|_| "Invalid XML hex character reference".to_string())?;
            let code = u32::from_str_radix(hex_s, 16)
                .map_err(|_| "Malformed XML hex character reference".to_string())?;
            char::from_u32(code).ok_or_else(|| "Invalid Unicode scalar in XML".to_string())
        }
        _ if raw_ref.starts_with(b"#") => {
            let dec_s = std::str::from_utf8(&raw_ref[1..])
                .map_err(|_| "Invalid XML decimal character reference".to_string())?;
            let code = dec_s
                .parse::<u32>()
                .map_err(|_| "Malformed XML decimal character reference".to_string())?;
            char::from_u32(code).ok_or_else(|| "Invalid Unicode scalar in XML".to_string())
        }
        _ => Err("Custom XML entity expansion refused by security policy".to_string()),
    }
}

fn parse_hikvision_search_xml(
    xml: &str,
    channel: usize,
) -> Result<(bool, Vec<RecordedSpan>), String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut depth: usize = 0;
    let mut tag_stack: Vec<String> = Vec::with_capacity(MAX_ISAPI_XML_DEPTH);
    let mut current_text = String::new();

    let mut has_more = false;
    let mut spans = Vec::new();

    let mut in_match_item = false;
    let mut item_start_ms: Option<i64> = None;
    let mut item_end_ms: Option<i64> = None;
    let mut item_uri: Option<String> = None;
    let mut item_size: u64 = 0;
    let mut item_kind = NvrEventKind::Continuous;

    loop {
        match reader
            .read_event()
            .map_err(|e| format!("Invalid Hikvision ISAPI XML: {}", e))?
        {
            Event::DocType(_) => {
                return Err("XML DOCTYPE/DTD refused by security policy".to_string());
            }
            Event::Start(e) => {
                depth += 1;
                if depth > MAX_ISAPI_XML_DEPTH {
                    return Err("Hikvision ISAPI XML exceeded maximum depth of 24".to_string());
                }
                let local = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                if local.eq_ignore_ascii_case("searchMatchItem") {
                    in_match_item = true;
                    item_start_ms = None;
                    item_end_ms = None;
                    item_uri = None;
                    item_size = 0;
                    item_kind = NvrEventKind::Continuous;
                }
                current_text.clear();
                tag_stack.push(local);
            }
            Event::Empty(_) => {
                if depth + 1 > MAX_ISAPI_XML_DEPTH {
                    return Err("Hikvision ISAPI XML exceeded maximum depth of 24".to_string());
                }
            }
            Event::Text(t) => {
                let s = std::str::from_utf8(t.as_ref())
                    .map_err(|_| "Non-UTF8 text in Hikvision ISAPI XML".to_string())?;
                current_text.push_str(s);
            }
            Event::CData(c) => {
                let s = std::str::from_utf8(c.as_ref())
                    .map_err(|_| "Non-UTF8 CDATA in Hikvision ISAPI XML".to_string())?;
                current_text.push_str(s);
            }
            Event::GeneralRef(r) => {
                let ch = resolve_safe_xml_general_ref(r.as_ref())?;
                current_text.push(ch);
            }
            Event::End(_) => {
                let ended = tag_stack.pop().unwrap_or_default();
                depth = depth.saturating_sub(1);
                let val = current_text.trim();

                if !in_match_item && ended.eq_ignore_ascii_case("responseStatusStrg") {
                    if val.eq_ignore_ascii_case("MORE") {
                        has_more = true;
                    }
                } else if in_match_item {
                    if ended.eq_ignore_ascii_case("startTime") {
                        if item_start_ms.is_none() {
                            item_start_ms = parse_hikvision_utc_ms(val);
                        }
                    } else if ended.eq_ignore_ascii_case("endTime") {
                        if item_end_ms.is_none() {
                            item_end_ms = parse_hikvision_utc_ms(val);
                        }
                    } else if ended.eq_ignore_ascii_case("playbackURI") {
                        item_uri = Some(val.to_string());
                        if let Some(sz) = extract_size_param_from_uri(val) {
                            item_size = sz;
                        }
                    } else if ended.eq_ignore_ascii_case("metadataDescriptor") {
                        let k = classify_hikvision_descriptor(val);
                        if k.specificity_rank() > item_kind.specificity_rank() {
                            item_kind = k;
                        }
                    } else if ended.eq_ignore_ascii_case("searchMatchItem") {
                        in_match_item = false;
                        if let (Some(sm), Some(mut em)) = (item_start_ms, item_end_ms) {
                            if em == sm {
                                em = sm + 1000;
                            }
                            if em > sm {
                                spans.push(RecordedSpan {
                                    channel,
                                    start_ms: sm,
                                    end_ms: em,
                                    kind: item_kind,
                                    file_path: item_uri.take(),
                                    byte_len: item_size,
                                    main_stream: true,
                                });
                            }
                        }
                    }
                }
                current_text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok((has_more, spans))
}

fn parse_hikvision_daily_distribution_xml(xml: &str) -> Result<Vec<u32>, String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut depth: usize = 0;
    let mut tag_stack: Vec<String> = Vec::new();
    let mut current_text = String::new();

    let mut in_day = false;
    let mut day_num: Option<u32> = None;
    let mut has_record = false;
    let mut days = Vec::new();

    loop {
        match reader
            .read_event()
            .map_err(|e| format!("Invalid ISAPI dailyDistribution XML: {}", e))?
        {
            Event::DocType(_) => {
                return Err("XML DOCTYPE refused by security policy".to_string());
            }
            Event::Start(e) => {
                depth += 1;
                if depth > MAX_ISAPI_XML_DEPTH {
                    return Err("ISAPI XML exceeded maximum depth of 24".to_string());
                }
                let local = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                if local.eq_ignore_ascii_case("day") {
                    in_day = true;
                    day_num = None;
                    has_record = false;
                }
                current_text.clear();
                tag_stack.push(local);
            }
            Event::Text(t) => {
                if let Ok(s) = std::str::from_utf8(t.as_ref()) {
                    current_text.push_str(s);
                }
            }
            Event::GeneralRef(r) => {
                let ch = resolve_safe_xml_general_ref(r.as_ref())?;
                current_text.push(ch);
            }
            Event::End(_) => {
                let ended = tag_stack.pop().unwrap_or_default();
                depth = depth.saturating_sub(1);
                let val = current_text.trim();
                if in_day {
                    if ended.eq_ignore_ascii_case("dayOfMonth") {
                        day_num = val.parse::<u32>().ok();
                    } else if ended.eq_ignore_ascii_case("record") {
                        has_record = val.eq_ignore_ascii_case("true") || val == "1";
                    } else if ended.eq_ignore_ascii_case("day") {
                        in_day = false;
                        if has_record {
                            if let Some(d) = day_num.filter(|&d| (1..=31).contains(&d)) {
                                days.push(d);
                            }
                        }
                    }
                }
                current_text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }

    days.sort_unstable();
    days.dedup();
    Ok(days)
}

fn extract_size_param_from_uri(uri: &str) -> Option<u64> {
    let (_, query) = uri.split_once('?')?;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k.eq_ignore_ascii_case("size") {
                if let Ok(sz) = v.parse::<u64>() {
                    return Some(sz);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    #[test]
    fn test_local_epoch_roundtrip() {
        let dt = NaiveDate::from_ymd_opt(2026, 11, 18)
            .unwrap()
            .and_hms_opt(14, 30, 45)
            .unwrap();
        let ms = local_naive_to_epoch_ms(dt);
        assert_eq!(epoch_ms_to_local_naive(ms), dt);
    }

    #[test]
    fn test_dahua_find_next_file_kv_parser_filepath_fallback_and_merging() {
        let date = NaiveDate::from_ymd_opt(2026, 11, 18).unwrap();

        let pass1_kv = "\
found=3\r\n\
items[0].Channel=0\r\n\
items[0].StartTime=2026-11-18 08:00:00\r\n\
items[0].EndTime=2026-11-18 09:00:00\r\n\
items[0].Type=dav\r\n\
items[0].FilePath=/mnt/dvr/2026-11-18/001/dav/08/08.00.00-09.00.00[R][0@0][0].dav\r\n\
items[0].Length=104857600\r\n\
items[0].VideoStream=Main\r\n\
items[0].Flags[0]=Timing\r\n\
items[1].Channel=0\r\n\
items[1].StartTime=2026-11-18 09:00:01\r\n\
items[1].EndTime=2026-11-18 10:00:00\r\n\
items[1].Type=dav\r\n\
items[1].FilePath=/mnt/dvr/2026-11-18/001/dav/09/09.00.01-10.00.00[R][0@0][0].dav\r\n\
items[1].Length=104857600\r\n\
items[1].VideoStream=Main\r\n\
items[1].Flags[0]=Timing\r\n\
items[2].Channel=0\r\n\
items[2].Type=dav\r\n\
items[2].FilePath=/mnt/dvr/2026-11-18/001/dav/12/12.10.00-12.11.15[M][0@0][0].dav\r\n\
items[2].Length=8388608\r\n\
";
        let (found1, items1) = parse_dahua_find_next_file_kv(pass1_kv, 1, date);
        assert_eq!(found1, 3);
        assert_eq!(items1.len(), 3);

        let pass2_kv = "\
found=4\r\n\
items[0].StartTime=2026-11-18 08:15:00\r\n\
items[0].EndTime=2026-11-18 08:15:25\r\n\
items[0].Flags[0]=Event\r\n\
items[0].Events[0]=VideoMotion\r\n\
items[0].Events[1]=SmartMotionHuman\r\n\
items[1].StartTime=2026-11-18 08:40:10\r\n\
items[1].EndTime=2026-11-18 08:40:30\r\n\
items[1].Flags[0]=Event\r\n\
items[1].Events[0]=SmartMotionVehicle\r\n\
items[2].StartTime=2026-11-18 09:12:00\r\n\
items[2].EndTime=2026-11-18 09:12:18\r\n\
items[2].Flags[0]=Event\r\n\
items[2].Events[0]=CrossLineDetection\r\n\
items[3].StartTime=2026-11-18 09:50:00\r\n\
items[3].EndTime=2026-11-18 09:50:12\r\n\
items[3].Flags[0]=Event\r\n\
items[3].Events[0]=AlarmLocal\r\n\
";
        let (found2, items2) = parse_dahua_find_next_file_kv(pass2_kv, 1, date);
        assert_eq!(found2, 4);

        let (coverage, events) = assemble_timeline_from_passes(1, items1, items2);
        // 08:00..09:00 and 09:00:01..10:00 are within 2000ms -> merged into 1 span, plus 12:10..12:11:15
        assert_eq!(coverage.len(), 2);
        assert_eq!(coverage[0].byte_len, 209_715_200);
        assert_eq!(events.len(), 5);
        assert_eq!(events[0].kind, NvrEventKind::Human);
        assert_eq!(events[1].kind, NvrEventKind::Vehicle);
        assert_eq!(events[2].kind, NvrEventKind::Intrusion);
        assert_eq!(events[3].kind, NvrEventKind::Alarm);
        assert_eq!(events[4].kind, NvrEventKind::Motion); // From [M] FilePath fallback!
    }

    #[tokio::test]
    async fn test_chunked_http_body_decoder() {
        let raw_chunked = b"7;ext= ignored\r\nHello, \r\n6\r\nWorld!\r\n0\r\nX-Trailer: ok\r\n\r\n";
        let mut reader = BufReader::with_capacity(3, &raw_chunked[..]);
        let decoded = decode_chunked_body_bounded(&mut reader, 1024)
            .await
            .unwrap();
        assert_eq!(&decoded, b"Hello, World!");

        let oversized = b"400000\r\n0000";
        let mut reader2 = BufReader::new(&oversized[..]);
        assert!(decode_chunked_body_bounded(&mut reader2, MAX_API_BODY_BYTES)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_cgi_digest_cache_channel_calibration_and_raii_destroy() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let destroy_calls = Arc::new(AtomicUsize::new(0));
        let close_calls = Arc::new(AtomicUsize::new(0));
        let unauth_401_count = Arc::new(AtomicUsize::new(0));

        let destroy_clone = Arc::clone(&destroy_calls);
        let close_clone = Arc::clone(&close_calls);
        let unauth_clone = Arc::clone(&unauth_401_count);

        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let destroy_c = Arc::clone(&destroy_clone);
                let close_c = Arc::clone(&close_clone);
                let unauth_c = Arc::clone(&unauth_clone);

                tokio::spawn(async move {
                    let (rh, mut wh) = stream.into_split();
                    let mut reader = BufReader::new(rh);
                    let mut req_head = Vec::new();
                    loop {
                        let avail = reader.fill_buf().await.unwrap();
                        if avail.is_empty() {
                            return;
                        }
                        let mut c = 0;
                        for &b in avail {
                            req_head.push(b);
                            c += 1;
                            if req_head.ends_with(b"\r\n\r\n") {
                                break;
                            }
                        }
                        reader.consume(c);
                        if req_head.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }
                    let req_str = String::from_utf8_lossy(&req_head);
                    let first_line = req_str.lines().next().unwrap_or("");
                    let has_digest = req_str
                        .lines()
                        .any(|l| l.to_ascii_lowercase().starts_with("authorization: digest"));

                    if !has_digest {
                        unauth_c.fetch_add(1, Ordering::SeqCst);
                        wh.write_all(
                            b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"DahuaHTTP\", nonce=\"11223344\", qop=\"auth\"\r\nContent-Length: 0\r\n\r\n",
                        )
                        .await
                        .unwrap();
                        return;
                    }

                    if first_line.contains("action=factory.create") {
                        wh.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nresult=998811\r\n")
                            .await
                            .unwrap();
                    } else if first_line.contains("action=findFile")
                        && first_line.contains("condition.Channel=0")
                    {
                        // Simulate 1-based firmware rejecting Channel=0
                        wh.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 5\r\n\r\nError")
                            .await
                            .unwrap();
                    } else if first_line.contains("action=findFile")
                        && first_line.contains("condition.Channel=1")
                    {
                        wh.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nresult=true")
                            .await
                            .unwrap();
                    } else if first_line.contains("action=findNextFile") {
                        if first_line.contains("object=998811") && close_c.load(Ordering::SeqCst) == 0
                        {
                            let body = "found=1\r\nitems[0].StartTime=2026-11-18 10:00:00\r\nitems[0].EndTime=2026-11-18 10:30:00\r\nitems[0].Length=4096\r\nitems[0].Flags[0]=Timing\r\n";
                            // After first call, return found=0 on second page
                            static SENT_PAGE1: AtomicUsize = AtomicUsize::new(0);
                            if SENT_PAGE1.fetch_add(1, Ordering::SeqCst) == 0 {
                                let resp = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                                    body.len(),
                                    body
                                );
                                wh.write_all(resp.as_bytes()).await.unwrap();
                            } else {
                                wh.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nfound=0")
                                    .await
                                    .unwrap();
                            }
                        } else {
                            wh.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nfound=0")
                                .await
                                .unwrap();
                        }
                    } else if first_line.contains("action=close") {
                        close_c.fetch_add(1, Ordering::SeqCst);
                        wh.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
                            .await
                            .unwrap();
                    } else if first_line.contains("action=destroy") {
                        destroy_c.fetch_add(1, Ordering::SeqCst);
                        wh.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
                            .await
                            .unwrap();
                    }
                });
            }
        });

        let cb = AuthCircuitBreaker::new();
        let client = NvrHttpClient::new(Arc::clone(&cb));

        let mut cfg = NvrEnvConfig::default();
        cfg.host = "127.0.0.1".to_string();
        cfg.http_port = port;
        cfg.username = "admin".to_string();
        cfg.password = "admin123".to_string();

        let date = NaiveDate::from_ymd_opt(2026, 11, 18).unwrap();
        let idx = client.query_day_timeline(&cfg, 1, date).await.unwrap();
        assert!(idx.api_supported);
        assert_eq!(idx.coverage.len(), 1);
        // Only the VERY FIRST HTTP request saw 401; all subsequent paginated calls hit 1-RTT Digest cache!
        assert_eq!(unauth_401_count.load(Ordering::SeqCst), 1);
        assert!(close_calls.load(Ordering::SeqCst) >= 2);
        assert_eq!(destroy_calls.load(Ordering::SeqCst), 1);

        server.abort();
    }

    #[tokio::test]
    async fn test_rpc2_fallback_when_cgi_returns_404_and_raii_cleanup() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let rpc_destroyed = Arc::new(AtomicUsize::new(0));
        let rpc_destroyed_clone = Arc::clone(&rpc_destroyed);

        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let destroyed = Arc::clone(&rpc_destroyed_clone);
                tokio::spawn(async move {
                    let (rh, mut wh) = stream.into_split();
                    let mut reader = BufReader::new(rh);
                    let (_, headers, framing) = read_http_response_or_req_for_test(&mut reader).await;
                    let body = read_bounded_http_body(&mut reader, framing, 65536)
                        .await
                        .unwrap();
                    let path = headers
                        .iter()
                        .find(|(k, _)| k == ":path")
                        .map(|(_, v)| v.as_str())
                        .unwrap_or("");

                    if path.starts_with("/cgi-bin/mediaFileFind.cgi") {
                        wh.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                            .await
                            .unwrap();
                        return;
                    }
                    if path == "/RPC2_Login" {
                        let req_json: Value = serde_json::from_slice(&body).unwrap();
                        let pw = req_json["params"]["password"].as_str().unwrap_or("");
                        if pw.is_empty() {
                            let ch = json!({
                                "result": false,
                                "params": { "realm": "Login to NVR4", "random": "987654321", "encryption": "Default" },
                                "session": "sess_abc_123"
                            });
                            let b = serde_json::to_vec(&ch).unwrap();
                            wh.write_all(
                                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", b.len())
                                    .as_bytes(),
                            )
                            .await
                            .unwrap();
                            wh.write_all(&b).await.unwrap();
                        } else {
                            let expected_ha1 = md5_upper_hex(b"admin:Login to NVR4:secret99");
                            let expected_pw = md5_upper_hex(
                                format!("admin:987654321:{}", expected_ha1).as_bytes(),
                            );
                            assert_eq!(pw, expected_pw);
                            let ok = json!({ "result": true, "session": "sess_abc_123" });
                            let b = serde_json::to_vec(&ok).unwrap();
                            wh.write_all(
                                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", b.len())
                                    .as_bytes(),
                            )
                            .await
                            .unwrap();
                            wh.write_all(&b).await.unwrap();
                        }
                        return;
                    }
                    if path == "/RPC2" {
                        let req_json: Value = serde_json::from_slice(&body).unwrap();
                        let method = req_json["method"].as_str().unwrap_or("");
                        let resp_val = match method {
                            "mediaFileFind.factory.create" => json!({ "result": 554433, "session": "sess_abc_123" }),
                            "mediaFileFind.findFile" => json!({ "result": true, "session": "sess_abc_123" }),
                            "mediaFileFind.findNextFile" => {
                                static RPC_CALLS: AtomicUsize = AtomicUsize::new(0);
                                if RPC_CALLS.fetch_add(1, Ordering::SeqCst) == 0 {
                                    json!({
                                        "result": true,
                                        "params": {
                                            "found": 1,
                                            "infos": [{
                                                "StartTime": "2026-11-18 15:00:00",
                                                "EndTime": "2026-11-18 15:02:00",
                                                "Length": 20480,
                                                "Flags": ["Event"],
                                                "Events": ["SmartMotionHuman"]
                                            }]
                                        }
                                    })
                                } else {
                                    json!({ "result": true, "params": { "found": 0, "infos": [] } })
                                }
                            }
                            "mediaFileFind.destroy" => {
                                destroyed.fetch_add(1, Ordering::SeqCst);
                                json!({ "result": true })
                            }
                            _ => json!({ "result": true }),
                        };
                        let b = serde_json::to_vec(&resp_val).unwrap();
                        wh.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", b.len())
                                .as_bytes(),
                        )
                        .await
                        .unwrap();
                        wh.write_all(&b).await.unwrap();
                    }
                });
            }
        });

        let cb = AuthCircuitBreaker::new();
        let client = NvrHttpClient::new(Arc::clone(&cb));
        let mut cfg = NvrEnvConfig::default();
        cfg.host = "127.0.0.1".to_string();
        cfg.http_port = port;
        cfg.username = "admin".to_string();
        cfg.password = "secret99".to_string();

        let date = NaiveDate::from_ymd_opt(2026, 11, 18).unwrap();
        let timeline = client.query_day_timeline(&cfg, 1, date).await.unwrap();
        assert!(timeline.api_supported);
        assert_eq!(timeline.coverage.len(), 1);
        assert_eq!(timeline.events.len(), 1);
        assert_eq!(timeline.events[0].kind, NvrEventKind::Human);
        assert_eq!(rpc_destroyed.load(Ordering::SeqCst), 1);
        assert!(!cb.is_tripped());

        server.abort();
    }

    #[test]
    fn test_hikvision_isapi_xml_parser_and_xxe_depth_guards() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<CMSearchResult version="2.0" xmlns="http://www.hikvision.com/ver20/XMLSchema">
  <responseStatus>true</responseStatus>
  <responseStatusStrg>OK</responseStatusStrg>
  <numOfMatches>1</numOfMatches>
  <matchList>
    <searchMatchItem>
      <trackID>101</trackID>
      <timeSpan>
        <startTime>2026-11-18T08:00:00Z</startTime>
        <endTime>2026-11-18T08:05:00Z</endTime>
      </timeSpan>
      <mediaSegmentDescriptor>
        <contentType>video</contentType>
        <playbackURI>rtsp://192.168.1.64/Streaming/tracks/101?starttime=20261118T080000Z&amp;endtime=20261118T080500Z&amp;size=654321</playbackURI>
      </mediaSegmentDescriptor>
      <metadataMatches>
        <metadataDescriptor>//recordType.meta.std-cgi.com/motion</metadataDescriptor>
      </metadataMatches>
    </searchMatchItem>
  </matchList>
</CMSearchResult>"#;

        let (more, spans) = parse_hikvision_search_xml(xml, 1).unwrap();
        assert!(!more);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].kind, NvrEventKind::Motion);
        assert_eq!(spans[0].byte_len, 654_321);
        assert!(spans[0]
            .file_path
            .as_deref()
            .unwrap()
            .contains("&endtime=20261118T080500Z"));

        // Reject DOCTYPE / XXE
        let xxe_xml = "<?xml version=\"1.0\"?><!DOCTYPE lolz [<!ENTITY lol \"lol\">]><CMSearchResult/>";
        assert!(parse_hikvision_search_xml(xxe_xml, 1).is_err());

        // Reject depth > 24
        let deep_open: String = (0..26).map(|_| "<a>").collect();
        let deep_close: String = (0..26).map(|_| "</a>").collect();
        assert!(parse_hikvision_search_xml(&format!("{}{}", deep_open, deep_close), 1).is_err());
    }

    #[tokio::test]
    async fn test_stream_dhav_time_slice_chunked_and_early_stop() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            // 1st connection: 401 Digest challenge
            let (s1, _) = listener.accept().await.unwrap();
            let (rh1, mut wh1) = s1.into_split();
            let mut r1 = BufReader::new(rh1);
            let _ = read_http_response_or_req_for_test(&mut r1).await;
            wh1.write_all(
                b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"Dahua\", nonce=\"abcdef12\"\r\nContent-Length: 0\r\n\r\n",
            )
            .await
            .unwrap();

            // 2nd connection: verify 1-based channel=1 and stream chunked DHAV bytes
            let (s2, _) = listener.accept().await.unwrap();
            let (rh2, mut wh2) = s2.into_split();
            let mut r2 = BufReader::new(rh2);
            let (_, hdrs, _) = read_http_response_or_req_for_test(&mut r2).await;
            let path = hdrs
                .iter()
                .find(|(k, _)| k == ":path")
                .map(|(_, v)| v.clone())
                .unwrap();
            assert!(path.starts_with("/cgi-bin/loadfile.cgi?action=startLoad&channel=1&"));

            wh2.write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: application/octet-stream\r\n\r\n8\r\nDHAV0001\r\n8\r\nDHAV0002\r\n0\r\n\r\n",
            )
            .await
            .unwrap();
        });

        let cb = AuthCircuitBreaker::new();
        let client = NvrHttpClient::new(cb);
        let mut cfg = NvrEnvConfig::default();
        cfg.host = "127.0.0.1".to_string();
        cfg.http_port = port;
        cfg.username = "admin".to_string();
        cfg.password = "pass".to_string();

        let s = NaiveDate::from_ymd_opt(2026, 11, 18)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let e = NaiveDate::from_ymd_opt(2026, 11, 18)
            .unwrap()
            .and_hms_opt(12, 5, 0)
            .unwrap();

        let mut collected = Vec::new();
        let total = client
            .stream_dhav_time_slice(&cfg, 1, 0, s, e, |chunk| {
                collected.extend_from_slice(chunk);
                false // Stop immediately after first chunk!
            })
            .await
            .unwrap();

        assert_eq!(total, 8);
        assert_eq!(&collected, b"DHAV0001");
        server.await.unwrap();
    }

    async fn read_http_response_or_req_for_test<R: tokio::io::AsyncRead + Unpin>(
        reader: &mut BufReader<R>,
    ) -> (String, Vec<(String, String)>, BodyFraming) {
        let mut raw = Vec::new();
        loop {
            let avail = reader.fill_buf().await.unwrap();
            if avail.is_empty() {
                break;
            }
            let mut c = 0;
            for &b in avail {
                raw.push(b);
                c += 1;
                if raw.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            reader.consume(c);
            if raw.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let s = String::from_utf8_lossy(&raw[..raw.len() - 4]);
        let mut lines = s.split("\r\n");
        let start = lines.next().unwrap_or("").to_string();
        let mut headers = Vec::new();
        if let Some(p) = start.split_whitespace().nth(1) {
            headers.push((":path".to_string(), p.to_string()));
        }
        let mut content_len = 0usize;
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                if k.trim().eq_ignore_ascii_case("content-length") {
                    content_len = v.trim().parse().unwrap_or(0);
                }
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        (start, headers, BodyFraming::ContentLength(content_len))
    }

    #[tokio::test]
    async fn test_month_recorded_days_and_mid_error_raii_destroy() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let destroyed = Arc::new(AtomicUsize::new(0));
        let destroyed_clone = Arc::clone(&destroyed);
        let fail_mode = Arc::new(AtomicUsize::new(0));
        let fail_clone = Arc::clone(&fail_mode);

        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let dc = Arc::clone(&destroyed_clone);
                let fc = Arc::clone(&fail_clone);
                tokio::spawn(async move {
                    let (rh, mut wh) = stream.into_split();
                    let mut reader = BufReader::new(rh);
                    let (_, hdrs, _) = read_http_response_or_req_for_test(&mut reader).await;
                    let path = hdrs
                        .iter()
                        .find(|(k, _)| k == ":path")
                        .map(|(_, v)| v.as_str())
                        .unwrap_or("");

                    if path.contains("action=factory.create") {
                        wh.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\nresult=776655")
                            .await
                            .unwrap();
                    } else if path.contains("action=findFile") {
                        wh.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
                            .await
                            .unwrap();
                    } else if path.contains("action=findNextFile") {
                        if fc.load(Ordering::SeqCst) == 1 {
                            wh.write_all(
                                b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\n\r\nError",
                            )
                            .await
                            .unwrap();
                        } else {
                            static MONTH_PAGE: AtomicUsize = AtomicUsize::new(0);
                            if MONTH_PAGE.fetch_add(1, Ordering::SeqCst) == 0 {
                                let body = "found=2\r\n\
items[0].StartTime=2026-11-05 10:00:00\r\nitems[0].EndTime=2026-11-05 11:00:00\r\n\
items[1].StartTime=2026-11-18 23:55:00\r\nitems[1].EndTime=2026-11-19 00:10:00\r\n";
                                wh.write_all(
                                    format!(
                                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                                        body.len(),
                                        body
                                    )
                                    .as_bytes(),
                                )
                                .await
                                .unwrap();
                            } else {
                                wh.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nfound=0")
                                    .await
                                    .unwrap();
                            }
                        }
                    } else if path.contains("action=destroy") {
                        dc.fetch_add(1, Ordering::SeqCst);
                        wh.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
                            .await
                            .unwrap();
                    } else {
                        wh.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
                            .await
                            .unwrap();
                    }
                });
            }
        });

        let cb = AuthCircuitBreaker::new();
        let client = NvrHttpClient::new(cb);
        let mut cfg = NvrEnvConfig::default();
        cfg.host = "127.0.0.1".to_string();
        cfg.http_port = port;

        let days = client
            .query_month_recorded_days(&cfg, 1, 2026, 11)
            .await
            .unwrap();
        assert_eq!(days, vec![5, 18, 19]);
        assert_eq!(destroyed.load(Ordering::SeqCst), 1);

        // Trigger mid-query HTTP 500 error and verify RAII destroy STILL executes!
        fail_mode.store(1, Ordering::SeqCst);
        let date = NaiveDate::from_ymd_opt(2026, 11, 18).unwrap();
        assert!(client.query_day_timeline(&cfg, 1, date).await.is_err());
        assert_eq!(destroyed.load(Ordering::SeqCst), 2);

        server.abort();
    }

    #[tokio::test]
    async fn test_crit01_no_premature_tcp_fin_during_dhav_stream() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let saw_premature_fin = Arc::new(AtomicBool::new(false));
        let saw_fin_clone = Arc::clone(&saw_premature_fin);

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut req_buf = Vec::new();
            let mut tmp = [0u8; 512];
            while !req_buf.ends_with(b"\r\n\r\n") {
                let n = stream.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                req_buf.extend_from_slice(&tmp[..n]);
            }

            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: application/octet-stream\r\n\r\n8\r\nDHAV_AA1\r\n",
                )
                .await
                .unwrap();

            // Give client time to finish open_single_http_stream and read the first chunk.
            // If open_single_http_stream used into_split() and dropped OwnedWriteHalf, the kernel
            // would have sent TCP FIN (SHUT_WR) and try_read would return Ok(0) here!
            time::sleep(Duration::from_millis(40)).await;
            let mut probe = [0u8; 1];
            match stream.try_read(&mut probe) {
                Ok(0) => {
                    saw_fin_clone.store(true, Ordering::SeqCst);
                    return;
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                other => panic!("Unexpected client read state during DHAV stream: {:?}", other),
            }

            stream.write_all(b"8\r\nDHAV_BB2\r\n0\r\n\r\n").await.unwrap();
        });

        let cb = AuthCircuitBreaker::new();
        let client = NvrHttpClient::new(cb);
        let mut cfg = NvrEnvConfig::default();
        cfg.host = "127.0.0.1".to_string();
        cfg.http_port = port;

        let s = NaiveDate::from_ymd_opt(2026, 11, 18)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let e = NaiveDate::from_ymd_opt(2026, 11, 18)
            .unwrap()
            .and_hms_opt(12, 1, 0)
            .unwrap();

        let mut out = Vec::new();
        let n = client
            .stream_dhav_time_slice(&cfg, 1, 0, s, e, |chunk| {
                out.extend_from_slice(chunk);
                true
            })
            .await
            .unwrap();

        server.await.unwrap();
        assert!(!saw_premature_fin.load(Ordering::SeqCst));
        assert_eq!(n, 16);
        assert_eq!(&out, b"DHAV_AA1DHAV_BB2");
    }

    #[tokio::test]
    async fn test_high03_dahua_channel_gt_1_calibration_and_cleanup() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let next_obj = Arc::new(AtomicUsize::new(100));
        let destroyed_count = Arc::new(AtomicUsize::new(0));
        let queried_wrong_ch4 = Arc::new(AtomicBool::new(false));
        let sent_page = Arc::new(AtomicUsize::new(0));

        let next_obj_c = Arc::clone(&next_obj);
        let destroyed_c = Arc::clone(&destroyed_count);
        let wrong_ch4_c = Arc::clone(&queried_wrong_ch4);
        let sent_page_c = Arc::clone(&sent_page);

        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let no = Arc::clone(&next_obj_c);
                let dc = Arc::clone(&destroyed_c);
                let wc = Arc::clone(&wrong_ch4_c);
                let sp = Arc::clone(&sent_page_c);
                tokio::spawn(async move {
                    let (rh, mut wh) = stream.into_split();
                    let mut reader = BufReader::new(rh);
                    let (_, hdrs, _) = read_http_response_or_req_for_test(&mut reader).await;
                    let path = hdrs
                        .iter()
                        .find(|(k, _)| k == ":path")
                        .map(|(_, v)| v.as_str())
                        .unwrap_or("");

                    if path.contains("action=factory.create") {
                        let id = no.fetch_add(1, Ordering::SeqCst);
                        let body = format!("result={}\r\n", id);
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        wh.write_all(resp.as_bytes()).await.unwrap();
                    } else if path.contains("action=findFile") {
                        if path.contains("condition.Channel=0") {
                            // 1-based Dahua NVR rejects Channel=0
                            wh.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 5\r\n\r\nError")
                                .await
                                .unwrap();
                        } else if path.contains("condition.Channel=4") {
                            wc.store(true, Ordering::SeqCst);
                            wh.write_all(
                                b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\n\r\nError",
                            )
                            .await
                            .unwrap();
                        } else if path.contains("condition.Channel=5") {
                            wh.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nresult=true")
                                .await
                                .unwrap();
                        }
                    } else if path.contains("action=findNextFile") {
                        if sp.fetch_add(1, Ordering::SeqCst) == 0 {
                            let body = "found=1\r\nitems[0].StartTime=2026-11-18 11:00:00\r\nitems[0].EndTime=2026-11-18 11:15:00\r\nitems[0].Length=8192\r\nitems[0].Flags[0]=Timing\r\n";
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            wh.write_all(resp.as_bytes()).await.unwrap();
                        } else {
                            wh.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nfound=0")
                                .await
                                .unwrap();
                        }
                    } else if path.contains("action=destroy") {
                        dc.fetch_add(1, Ordering::SeqCst);
                        wh.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
                            .await
                            .unwrap();
                    } else {
                        wh.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
                            .await
                            .unwrap();
                    }
                });
            }
        });

        let cb = AuthCircuitBreaker::new();
        let client = NvrHttpClient::new(cb);
        let mut cfg = NvrEnvConfig::default();
        cfg.host = "127.0.0.1".to_string();
        cfg.http_port = port;

        // Query channel 5 FIRST on an uncalibrated client!
        let date = NaiveDate::from_ymd_opt(2026, 11, 18).unwrap();
        let idx = client.query_day_timeline(&cfg, 5, date).await.unwrap();
        assert!(idx.api_supported);
        assert_eq!(idx.coverage.len(), 1);
        assert!(!queried_wrong_ch4.load(Ordering::SeqCst));
        // Both temporary calibration finder and main query finder were destroyed cleanly
        assert_eq!(destroyed_count.load(Ordering::SeqCst), 2);

        server.abort();
    }

    #[tokio::test]
    async fn test_high04_loadfile_no_record_returns_ok_zero_and_cancellable_abort() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mode = Arc::new(AtomicUsize::new(0));
        let mode_c = Arc::clone(&mode);

        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let m = mode_c.load(Ordering::SeqCst);
                tokio::spawn(async move {
                    let (rh, mut wh) = stream.into_split();
                    let mut reader = BufReader::new(rh);
                    let _ = read_http_response_or_req_for_test(&mut reader).await;
                    if m == 0 {
                        let body = b"Error\r\nErrorID=3, Detail=No Record!\r\n";
                        let hdr = format!(
                            "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\n\r\n",
                            body.len()
                        );
                        wh.write_all(hdr.as_bytes()).await.unwrap();
                        wh.write_all(body).await.unwrap();
                    } else if m == 1 {
                        let body = b"Error\r\nErrorID=3, Detail=Can not find matched file!\r\n";
                        let hdr = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                            body.len()
                        );
                        wh.write_all(hdr.as_bytes()).await.unwrap();
                        wh.write_all(body).await.unwrap();
                    } else {
                        wh.write_all(
                            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
                        )
                        .await
                        .unwrap();
                        // Stall for 5 seconds so abort_flag must interrupt it promptly
                        time::sleep(Duration::from_secs(5)).await;
                    }
                });
            }
        });

        let cb = AuthCircuitBreaker::new();
        let client = NvrHttpClient::new(cb);
        let mut cfg = NvrEnvConfig::default();
        cfg.host = "127.0.0.1".to_string();
        cfg.http_port = port;

        let s = NaiveDate::from_ymd_opt(2026, 11, 18)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let e = NaiveDate::from_ymd_opt(2026, 11, 18)
            .unwrap()
            .and_hms_opt(12, 1, 0)
            .unwrap();

        // Mode 0: HTTP 404 with No Record -> Ok(0)
        assert_eq!(
            client
                .stream_dhav_time_slice(&cfg, 1, 0, s, e, |_| true)
                .await
                .unwrap(),
            0
        );

        // Mode 1: HTTP 200 with ErrorID=3 Can not find -> Ok(0)
        mode.store(1, Ordering::SeqCst);
        assert_eq!(
            client
                .stream_dhav_time_slice(&cfg, 1, 0, s, e, |_| true)
                .await
                .unwrap(),
            0
        );

        // Mode 2: Stalled DHAV stream aborts promptly when abort_flag is set
        mode.store(2, Ordering::SeqCst);
        let abort_flag = Arc::new(AtomicBool::new(false));
        let abort_setter = Arc::clone(&abort_flag);
        tokio::spawn(async move {
            time::sleep(Duration::from_millis(80)).await;
            abort_setter.store(true, Ordering::Release);
        });
        let t0 = time::Instant::now();
        let res = client
            .stream_dhav_time_slice_cancellable(
                &cfg,
                1,
                0,
                s,
                e,
                1024 * 1024,
                Some(&abort_flag),
                |_| true,
            )
            .await;
        assert!(res.is_err());
        assert!(t0.elapsed() < Duration::from_millis(600));

        server.abort();
    }

    #[test]
    fn test_med04_hikvision_rfc3339_offsets_and_med05_rpc2_error_classification() {
        let base_utc = parse_hikvision_utc_ms("2026-11-18T08:00:00Z").unwrap();
        assert_eq!(
            parse_hikvision_utc_ms("2026-11-18T13:30:00+05:30"),
            Some(base_utc)
        );
        assert_eq!(
            parse_hikvision_utc_ms("2026-11-18T08:00:00.250z"),
            Some(base_utc + 250)
        );
        assert_eq!(
            parse_hikvision_utc_ms("20261118T080000z"),
            Some(base_utc)
        );

        assert!(!is_rpc2_credential_error(&json!({
            "result": false,
            "error": { "code": 268632084, "message": "Over max connect num" }
        })));
        assert!(!is_rpc2_credential_error(&json!({
            "result": false,
            "error": { "code": 268632099, "message": "Not support Default encryption" }
        })));
        assert!(is_rpc2_credential_error(&json!({
            "result": false,
            "error": { "code": 268632079, "message": "User or password not valid!" }
        })));
    }

    #[tokio::test]
    async fn test_probe_http_api_isolates_live_circuit_breaker_and_redacts_password() {
        assert!(NvrEventKind::Human.specificity_rank() > NvrEventKind::Vehicle.specificity_rank());
        assert!(NvrEventKind::Vehicle.specificity_rank() > NvrEventKind::Motion.specificity_rank());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let (rh, mut wh) = stream.into_split();
                    let mut reader = BufReader::new(rh);
                    let _ = read_http_response_or_req_for_test(&mut reader).await;
                    wh.write_all(
                        b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"NVR\", nonce=\"abc12345\"\r\nContent-Length: 0\r\n\r\n",
                    )
                    .await
                    .unwrap();
                });
            }
        });

        let live_cb = AuthCircuitBreaker::new();
        let live_client = NvrHttpClient::new(Arc::clone(&live_cb));

        let mut cfg = NvrEnvConfig::default();
        cfg.host = "127.0.0.1".to_string();
        cfg.http_port = port;
        cfg.password = "NeverLeakProbePass2026!".to_string();

        let err = live_client.probe_http_api(&cfg).await.unwrap_err();
        assert!(!err.contains("NeverLeakProbePass2026!"));
        // Live circuit breaker MUST NOT be tripped by probing unsaved settings
        assert!(!live_cb.is_tripped());

        // Also verify test_nvr_connection_sync on closed RTSP port never leaks password
        let tmp_rtsp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        cfg.port = tmp_rtsp.local_addr().unwrap().port();
        drop(tmp_rtsp);
        let sync_err = test_nvr_connection_sync(&cfg).unwrap_err();
        assert!(!sync_err.contains("NeverLeakProbePass2026!"));

        server.abort();
    }
}
