use chrono::{Local, NaiveDateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use zeroize::Zeroize;

const MAX_ENV_FILE_BYTES: u64 = 64 * 1024;
const MAX_CONFIG_JSON_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GridLayoutConfig {
    pub rows: usize,
    pub cols: usize,
}

impl Default for GridLayoutConfig {
    fn default() -> Self {
        Self { rows: 3, cols: 4 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub layout: GridLayoutConfig,
    pub layout_mode: String,
    pub num_cameras: usize,
    pub screenshot_dir: String,
    pub recording_dir: String,
    pub fps_limit: u32,
    pub reconnect_interval: u64,
    pub connection_timeout: u64,
    pub fullscreen_auto_hd: bool,
    pub aspect_fit_16_9: bool,
    pub patrol_interval_secs: u64,
    pub camera_names: BTreeMap<usize, String>,
    pub channel_order: Vec<usize>,
}

impl Default for AppConfig {
    fn default() -> Self {
        let (screenshot_dir, recording_dir) = if let Ok(real_home) = std::env::var("SNAP_REAL_HOME")
        {
            if !real_home.trim().is_empty() {
                (
                    format!("{}/Pictures/NVR-Snapshots", real_home.trim()),
                    format!("{}/Videos/NVR-Recordings", real_home.trim()),
                )
            } else {
                (
                    "~/rtsp_viewer/screenshots".to_string(),
                    "~/rtsp_viewer/recordings".to_string(),
                )
            }
        } else {
            (
                "~/rtsp_viewer/screenshots".to_string(),
                "~/rtsp_viewer/recordings".to_string(),
            )
        };

        Self {
            layout: GridLayoutConfig { rows: 3, cols: 4 },
            layout_mode: "auto".to_string(),
            num_cameras: 12,
            screenshot_dir,
            recording_dir,
            fps_limit: 15,
            reconnect_interval: 3,
            connection_timeout: 5,
            fullscreen_auto_hd: false,
            aspect_fit_16_9: false,
            patrol_interval_secs: 8,
            camera_names: BTreeMap::new(),
            channel_order: (1..=12).collect(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct NvrEnvConfig {
    pub brand: String,
    pub host: String,
    pub p2p_serial: String,
    pub use_cloud_p2p: bool,
    pub p2p_local_port: u16,
    pub p2p_local_http_port: u16,
    pub tailscale_host: String,
    pub use_tailscale: bool,
    pub port: u16,
    pub http_port: u16,
    pub username: String,
    pub password: String,
    pub num_cameras: usize,
    pub ip_camera_start_channel: usize,
    pub default_subtype: u8,
    pub hwaccel_mode: String,
    pub custom_url_template: String,
}

impl fmt::Debug for NvrEnvConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NvrEnvConfig")
            .field("brand", &self.brand)
            .field("host", &self.host)
            .field("p2p_serial", &self.p2p_serial)
            .field("use_cloud_p2p", &self.use_cloud_p2p)
            .field("p2p_local_port", &self.p2p_local_port)
            .field("p2p_local_http_port", &self.p2p_local_http_port)
            .field("tailscale_host", &self.tailscale_host)
            .field("use_tailscale", &self.use_tailscale)
            .field("port", &self.port)
            .field("http_port", &self.http_port)
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .field("num_cameras", &self.num_cameras)
            .field("ip_camera_start_channel", &self.ip_camera_start_channel)
            .field("default_subtype", &self.default_subtype)
            .field("hwaccel_mode", &self.hwaccel_mode)
            .field("custom_url_template", &self.custom_url_template)
            .finish()
    }
}

impl Drop for NvrEnvConfig {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

impl Default for NvrEnvConfig {
    fn default() -> Self {
        Self {
            brand: "dahua".to_string(),
            host: "192.168.1.100".to_string(),
            p2p_serial: String::new(),
            use_cloud_p2p: false,
            p2p_local_port: 0,
            p2p_local_http_port: 0,
            tailscale_host: String::new(),
            use_tailscale: false,
            port: 554,
            http_port: 80,
            username: "admin".to_string(),
            password: String::new(),
            num_cameras: 12,
            ip_camera_start_channel: 10,
            default_subtype: 1,
            hwaccel_mode: "auto".to_string(),
            custom_url_template:
                "rtsp://{user}:{password}@{host}:{port}/cam/realmonitor?channel={channel}&subtype={subtype}"
                    .to_string(),
        }
    }
}

impl NvrEnvConfig {
    pub fn is_cloud_p2p_active(&self) -> bool {
        self.use_cloud_p2p && !self.p2p_serial.trim().is_empty()
    }

    #[allow(dead_code)]
    pub fn is_tailscale_active(&self) -> bool {
        !self.is_cloud_p2p_active() && self.use_tailscale && !self.tailscale_host.trim().is_empty()
    }

    pub fn active_host(&self) -> &str {
        if self.is_cloud_p2p_active() {
            "127.0.0.1"
        } else if self.is_tailscale_active() {
            self.tailscale_host.trim()
        } else {
            self.host.trim()
        }
    }

    pub fn active_port(&self) -> u16 {
        if self.is_cloud_p2p_active() {
            self.p2p_local_port
        } else {
            self.port
        }
    }

    pub fn active_http_host(&self) -> &str {
        self.active_host()
    }

    pub fn active_http_port(&self) -> u16 {
        if self.is_cloud_p2p_active() {
            self.p2p_local_http_port
        } else {
            self.http_port
        }
    }

    /// Builds the RTSP path + query (without credentials) for live streaming on the active NVR.
    pub fn build_live_rtsp_path(&self, channel: usize, subtype: u8) -> String {
        let sub = if subtype == 0 { 0 } else { 1 };
        match self.brand.to_lowercase().as_str() {
            "hikvision" => {
                let hik_stream = if sub == 0 { 1 } else { 2 };
                format!("/Streaming/Channels/{}0{}", channel, hik_stream)
            }
            "custom" => {
                let candidate = self
                    .custom_url_template
                    .replace("{user}", "__u__")
                    .replace("{password}", "__p__")
                    .replace("{host}", "127.0.0.1")
                    .replace("{port}", &self.port.to_string())
                    .replace("{channel}", &channel.to_string())
                    .replace("{subtype}", &sub.to_string());
                if let Ok(parsed) = validate_custom_rtsp_url(&candidate) {
                    let mut pq = parsed.path().to_string();
                    if let Some(q) = parsed.query() {
                        pq.push('?');
                        pq.push_str(q);
                    }
                    if pq.starts_with('/') && !pq.contains("__u__") && !pq.contains("__p__") {
                        return pq;
                    }
                }
                format!("/cam/realmonitor?channel={}&subtype={}", channel, sub)
            }
            _ => format!("/cam/realmonitor?channel={}&subtype={}", channel, sub),
        }
    }

    /// Builds the RTSP path + query (without credentials) for historical playback.
    pub fn build_playback_rtsp_path(
        &self,
        channel: usize,
        subtype: u8,
        start_local: NaiveDateTime,
        end_local: NaiveDateTime,
    ) -> String {
        let sub = if subtype == 0 { 0 } else { 1 };
        match self.brand.to_lowercase().as_str() {
            "hikvision" => {
                let hik_stream = if sub == 0 { 1 } else { 2 };
                let start_utc = local_naive_to_utc(start_local);
                let end_utc = local_naive_to_utc(end_local);
                format!(
                    "/Streaming/tracks/{}0{}?starttime={}&endtime={}",
                    channel,
                    hik_stream,
                    start_utc.format("%Y%m%dT%H%M%SZ"),
                    end_utc.format("%Y%m%dT%H%M%SZ")
                )
            }
            _ => format!(
                "/cam/playback?channel={}&subtype={}&starttime={}&endtime={}",
                channel,
                sub,
                start_local.format("%Y_%m_%d_%H_%M_%S"),
                end_local.format("%Y_%m_%d_%H_%M_%S")
            ),
        }
    }

    /// Legacy/preview URL builder (with host validation; live workers use `AuthGateway` instead so
    /// credentials never appear in `/proc/<pid>/cmdline`).
    pub fn build_rtsp_url(&self, channel: usize, subtype: u8) -> String {
        let user_enc = percent_encode_userinfo(&self.username);
        let pass_enc = percent_encode_userinfo(&self.password);
        let raw_host = self.active_host();
        let host_fmt = validate_nvr_host(raw_host).unwrap_or_else(|_| "127.0.0.1".to_string());
        let port = self.active_port();
        let sub = if subtype == 0 { 0 } else { 1 };

        match self.brand.to_lowercase().as_str() {
            "hikvision" => {
                let hik_stream = if sub == 0 { 1 } else { 2 };
                format!(
                    "rtsp://{}:{}@{}:{}/Streaming/Channels/{}0{}",
                    user_enc, pass_enc, host_fmt, port, channel, hik_stream
                )
            }
            "custom" => {
                let built = self
                    .custom_url_template
                    .replace("{user}", &user_enc)
                    .replace("{password}", &pass_enc)
                    .replace("{host}", &host_fmt)
                    .replace("{port}", &port.to_string())
                    .replace("{channel}", &channel.to_string())
                    .replace("{subtype}", &sub.to_string());
                if validate_custom_rtsp_url(&built).is_ok() {
                    built
                } else {
                    format!(
                        "rtsp://{}:{}@{}:{}/cam/realmonitor?channel={}&subtype={}",
                        user_enc, pass_enc, host_fmt, port, channel, sub
                    )
                }
            }
            _ => format!(
                "rtsp://{}:{}@{}:{}/cam/realmonitor?channel={}&subtype={}",
                user_enc, pass_enc, host_fmt, port, channel, sub
            ),
        }
    }

    pub fn build_redacted_url(&self, channel: usize, subtype: u8) -> String {
        let mut clone = self.clone();
        clone.password = "***".to_string();
        let mut url_str = clone.build_rtsp_url(channel, subtype);
        if let Ok(mut parsed) = url::Url::parse(&url_str) {
            if parsed
                .password()
                .is_some_and(|p| p != "***" && p != "%2A%2A%2A")
            {
                let _ = parsed.set_password(Some("***"));
                url_str = parsed.to_string();
            }
        }
        redact_secret_in_text(&url_str, &self.password)
    }

    #[allow(dead_code)]
    pub fn validate_for_save(&self) -> Result<(), String> {
        let res: Result<(), String> = (|| {
            let has_ctl = |s: &str| s.bytes().any(|b| matches!(b, b'\r' | b'\n' | b'\0'));
            if has_ctl(&self.username) {
                return Err("NVR username contains forbidden newline or NUL characters".to_string());
            }
            if has_ctl(&self.password) {
                return Err("NVR password contains forbidden newline or NUL characters".to_string());
            }
            if has_ctl(&self.p2p_serial) {
                return Err(
                    "Cloud P2P serial contains forbidden newline or NUL characters".to_string(),
                );
            }
            if self.port < 1 {
                return Err("RTSP port must be at least 1".to_string());
            }
            if self.http_port < 1 {
                return Err("HTTP port must be at least 1".to_string());
            }
            if !(1..=32).contains(&self.num_cameras) {
                return Err("Number of cameras must be between 1 and 32".to_string());
            }
            if !(1..=32).contains(&self.ip_camera_start_channel) {
                return Err("IP camera start channel must be between 1 and 32".to_string());
            }

            if !((self.use_cloud_p2p || self.use_tailscale) && self.host.trim().is_empty()) {
                validate_nvr_host(&self.host)
                    .map_err(|e| format!("Invalid LAN NVR host: {}", e))?;
            }

            if self.use_cloud_p2p {
                if self.p2p_serial.is_empty() {
                    return Err(
                        "Cloud P2P serial is required when Cloud P2P is enabled".to_string()
                    );
                }
                if self.p2p_serial.len() > 64 {
                    return Err("Cloud P2P serial must be 64 characters or fewer".to_string());
                }
                if !self
                    .p2p_serial
                    .bytes()
                    .all(|b| b.is_ascii_graphic() && !matches!(b, b'/' | b'@' | b':' | b'\\'))
                {
                    return Err(
                        "Cloud P2P serial must contain only ASCII graphic characters without whitespace, '/', '@', or ':'"
                            .to_string(),
                    );
                }
                if self.password.is_empty() {
                    return Err("NVR password is required when Cloud P2P is enabled".to_string());
                }
            }

            if self.use_tailscale {
                if self.tailscale_host.trim().is_empty() {
                    return Err(
                        "Tailscale host is required when Tailscale mode is enabled".to_string()
                    );
                }
                validate_nvr_host(&self.tailscale_host)
                    .map_err(|e| format!("Invalid Tailscale host: {}", e))?;
            }

            if !self.use_cloud_p2p && !self.use_tailscale {
                validate_nvr_host(&self.host)
                    .map_err(|e| format!("Invalid NVR host: {}", e))?;
            }

            if self.brand.eq_ignore_ascii_case("custom") {
                if self.num_cameras > 1 && !self.custom_url_template.contains("{channel}") {
                    return Err(
                        "Custom RTSP URL template must contain '{channel}' when num_cameras > 1"
                            .to_string(),
                    );
                }
                if let Some((_scheme, after_scheme)) = self.custom_url_template.split_once("://") {
                    if let Some(pq_idx) = after_scheme.find(['/', '?', '#']) {
                        let pq_part = &after_scheme[pq_idx..];
                        if pq_part.contains("{user}") || pq_part.contains("{password}") {
                            return Err(
                                "Custom URL must not place {user} or {password} in path or query; AuthGateway handles RTSP authentication headers automatically"
                                    .to_string(),
                            );
                        }
                    }
                }
                let has_password_placeholder = self.custom_url_template.contains("{password}");
                let candidate = self
                    .custom_url_template
                    .replace("{user}", "__u__")
                    .replace("{password}", "__p__")
                    .replace("{host}", "127.0.0.1")
                    .replace("{port}", "554")
                    .replace("{channel}", "1")
                    .replace("{subtype}", "1");
                let parsed = validate_custom_rtsp_url(&candidate)?;
                if parsed.path().contains("__u__")
                    || parsed.path().contains("__p__")
                    || parsed
                        .query()
                        .is_some_and(|q| q.contains("__u__") || q.contains("__p__"))
                    || parsed
                        .fragment()
                        .is_some_and(|f| f.contains("__u__") || f.contains("__p__"))
                {
                    return Err(
                        "Custom URL must not place {user} or {password} in path or query; AuthGateway handles RTSP authentication headers automatically"
                            .to_string(),
                    );
                }
                if parsed
                    .password()
                    .is_some_and(|p| !has_password_placeholder || p != "__p__")
                {
                    return Err(
                        "Do not embed literal passwords in Custom URL Template; use {password} and the RTSP Password field"
                            .to_string(),
                    );
                }
            }

            Ok(())
        })();

        res.map_err(|e| redact_secret_in_text(&e, &self.password))
    }
}

pub(crate) fn redact_secret_in_text(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        return text.to_string();
    }
    let mut sanitized = text.replace(secret, "[REDACTED]");
    let encoded = percent_encode_userinfo(secret);
    if !encoded.is_empty() && encoded != secret && encoded != "***" {
        sanitized = sanitized.replace(&encoded, "[REDACTED]");
    }
    sanitized
}

pub fn local_naive_to_utc(local_dt: NaiveDateTime) -> NaiveDateTime {
    if let Some(dt) = Local.from_local_datetime(&local_dt).single() {
        return dt.with_timezone(&Utc).naive_utc();
    }
    if let Some(dt) = Local.from_local_datetime(&local_dt).earliest() {
        return dt.with_timezone(&Utc).naive_utc();
    }
    if let Some(shifted) = local_dt.checked_add_signed(chrono::Duration::hours(1)) {
        if let Some(dt) = Local.from_local_datetime(&shifted).earliest() {
            return dt.with_timezone(&Utc).naive_utc();
        }
    }
    local_dt
}

/// Prevents core dumps from writing decrypted video frames or credentials to `/var/crash`.
pub fn harden_process_security() {
    #[cfg(target_os = "linux")]
    unsafe {
        let _ = libc::prctl(libc::PR_SET_DUMPABLE, 0);
    }
}

pub fn format_host_for_url(host: &str) -> String {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        let without_zone = inner.split('%').next().unwrap_or(inner);
        if without_zone.parse::<std::net::Ipv6Addr>().is_ok() {
            return format!("[{}]", without_zone);
        }
        return trimmed.to_string();
    }
    let without_zone = trimmed.split('%').next().unwrap_or(trimmed);
    if without_zone.parse::<std::net::Ipv6Addr>().is_ok() || trimmed.matches(':').count() >= 2 {
        format!("[{}]", without_zone)
    } else {
        trimmed.to_string()
    }
}

pub fn validate_nvr_host(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Empty NVR host".to_string());
    }
    if trimmed
        .bytes()
        .any(|b| b <= 0x20 || b == 0x7f || b"@/?#\\".contains(&b))
    {
        return Err("NVR host contains forbidden URL authority characters".to_string());
    }
    if trimmed.starts_with('[') != trimmed.ends_with(']') {
        return Err("Mismatched IPv6 brackets in NVR host".to_string());
    }
    let unbracketed = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed);
    if let Some((addr_part, zone_part)) = unbracketed.split_once('%') {
        let valid_v6 = addr_part.parse::<std::net::Ipv6Addr>().is_ok();
        let valid_zone = !zone_part.is_empty()
            && zone_part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
        if !valid_v6 || !valid_zone {
            return Err("Malformed IPv6 zone identifier in NVR host".to_string());
        }
    }
    let formatted = format_host_for_url(trimmed);
    url::Host::parse(&formatted)
        .map(|h| h.to_string())
        .map_err(|e| format!("Malformed hostname or IP: {}", e))
}

pub fn validate_custom_rtsp_url(url_str: &str) -> Result<url::Url, String> {
    if url_str.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err("URL contains control characters".to_string());
    }
    let parsed = url::Url::parse(url_str).map_err(|e| format!("Invalid RTSP URL: {}", e))?;
    if !matches!(parsed.scheme(), "rtsp" | "rtsps") {
        return Err(format!("Unsupported URL scheme '{}': must be rtsp or rtsps", parsed.scheme()));
    }
    if parsed.host().is_none() {
        return Err("RTSP URL missing host".to_string());
    }
    Ok(parsed)
}

/// Resolves trusted system/Snap `ffmpeg` or `ffprobe` binaries before falling back to `$PATH`.
pub fn resolve_trusted_binary(name: &str) -> PathBuf {
    let mut candidates: Vec<PathBuf> = Vec::with_capacity(4);
    if let Ok(snap) = std::env::var("SNAP") {
        let s = snap.trim();
        if !s.is_empty() {
            candidates.push(PathBuf::from(s).join("usr/bin").join(name));
            candidates.push(PathBuf::from(s).join("bin").join(name));
        }
    }
    candidates.push(PathBuf::from("/usr/bin").join(name));
    candidates.push(PathBuf::from("/usr/local/bin").join(name));

    let my_uid = unsafe { libc::geteuid() };
    for cand in candidates {
        if let Ok(meta) = fs::symlink_metadata(&cand) {
            let real_meta = if meta.file_type().is_symlink() {
                fs::metadata(&cand).ok()
            } else {
                Some(meta)
            };
            if let Some(m) = real_meta {
                let mode = m.permissions().mode();
                let owner_ok = m.uid() == 0 || m.uid() == my_uid;
                let not_world_writable = (mode & 0o002) == 0;
                let is_exec = (mode & 0o111) != 0;
                if m.is_file() && owner_ok && not_world_writable && is_exec {
                    return cand;
                }
            }
        }
    }
    PathBuf::from(name)
}

fn select_rtsp_timeout_arg_from_help(help_stdout: &str) -> &'static str {
    if help_stdout.contains("-stimeout") && !help_stdout.contains("-timeout") {
        "-stimeout"
    } else {
        "-timeout"
    }
}

/// Detects whether the installed `ffmpeg` uses `-stimeout` (FFmpeg 4.x) or `-timeout` (FFmpeg 5.0+)
/// for RTSP TCP socket I/O timeouts, caching the result process-wide.
pub fn rtsp_timeout_ffmpeg_arg() -> &'static str {
    static RTSP_TIMEOUT_ARG: OnceLock<&'static str> = OnceLock::new();
    RTSP_TIMEOUT_ARG.get_or_init(|| {
        let ffmpeg_bin = resolve_trusted_binary("ffmpeg");
        let Ok(mut child) = Command::new(ffmpeg_bin)
            .args(["-hide_banner", "-h", "demuxer=rtsp"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        else {
            return "-timeout";
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return "-timeout";
        };
        let reader_handle = std::thread::spawn(move || {
            let mut out_bytes = Vec::with_capacity(4096);
            let _ = stdout.take(65_536).read_to_end(&mut out_bytes);
            out_bytes
        });
        let deadline = Instant::now() + Duration::from_millis(1500);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return "-timeout";
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return "-timeout";
                }
            }
        }
        let Ok(bytes) = reader_handle.join() else {
            return "-timeout";
        };
        let text = String::from_utf8_lossy(&bytes);
        select_rtsp_timeout_arg_from_help(&text)
    })
}

pub fn sanitize_export_label(raw: &str) -> String {
    let mut s: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    while s.starts_with('-') || s.starts_with('.') || s.starts_with('_') {
        s.remove(0);
    }
    while s.ends_with('_') {
        s.pop();
    }
    if s.is_empty() {
        "Camera".to_string()
    } else {
        s
    }
}

fn is_protected_shared_user_dir_with_homes(dir: &Path, homes: &[PathBuf]) -> bool {
    const SHARED_XDG_DIRS: [&str; 5] = [
        "Pictures",
        "Videos",
        "Downloads",
        "Documents",
        "Desktop",
    ];
    let dir_canon = fs::canonicalize(dir).ok();
    for home in homes {
        if home.as_os_str().is_empty() {
            continue;
        }
        let home_canon = fs::canonicalize(home).ok();
        for h_cand in [Some(home), home_canon.as_ref()].into_iter().flatten() {
            for d_cand in [Some(dir), dir_canon.as_deref()].into_iter().flatten() {
                if d_cand == h_cand.as_path() {
                    return true;
                }
                for xdg in SHARED_XDG_DIRS {
                    if d_cand == h_cand.join(xdg).as_path() {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn is_protected_shared_user_dir(dir: &Path) -> bool {
    let mut homes = Vec::with_capacity(2);
    if let Ok(h) = std::env::var("HOME") {
        let t = h.trim();
        if !t.is_empty() {
            homes.push(PathBuf::from(t));
        }
    }
    if let Ok(sh) = std::env::var("SNAP_REAL_HOME") {
        let t = sh.trim();
        if !t.is_empty() {
            homes.push(PathBuf::from(t));
        }
    }
    is_protected_shared_user_dir_with_homes(dir, &homes)
}

fn read_regular_file_bounded(path: &Path, max_bytes: u64) -> Option<String> {
    use std::os::unix::fs::OpenOptionsExt;

    let my_uid = unsafe { libc::geteuid() };
    let meta = fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink()
        || !meta.is_file()
        || meta.len() > max_bytes
        || (meta.uid() != my_uid && meta.uid() != 0)
    {
        return None;
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let fmeta = file.metadata().ok()?;
    if !fmeta.is_file() || fmeta.len() > max_bytes || (fmeta.uid() != my_uid && fmeta.uid() != 0) {
        return None;
    }
    let mut buf = Vec::with_capacity((fmeta.len() as usize).min(max_bytes as usize));
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut buf)
        .ok()?;
    if (buf.len() as u64) > max_bytes {
        buf.zeroize();
        return None;
    }
    match String::from_utf8(buf) {
        Ok(s) => Some(s),
        Err(e) => {
            let mut b = e.into_bytes();
            b.zeroize();
            None
        }
    }
}

pub fn reject_any_symlink_component(path: &Path) -> std::io::Result<()> {
    let mut home_roots: Vec<PathBuf> = Vec::with_capacity(2);
    if let Ok(h) = std::env::var("HOME") {
        let t = h.trim();
        if !t.is_empty() {
            home_roots.push(PathBuf::from(t));
        }
    }
    if let Ok(sh) = std::env::var("SNAP_REAL_HOME") {
        let t = sh.trim();
        if !t.is_empty() {
            home_roots.push(PathBuf::from(t));
        }
    }
    let temp_root = std::env::temp_dir();

    let check_chain = |target: &Path| -> std::io::Result<()> {
        let inside_home = home_roots.iter().any(|h| target.starts_with(h));
        let inside_temp = !temp_root.as_os_str().is_empty() && target.starts_with(&temp_root);

        for (depth, anc) in target.ancestors().enumerate() {
            if anc.as_os_str().is_empty() || anc == Path::new("/") {
                break;
            }
            if inside_temp && depth > 0 && anc == temp_root.as_path() {
                break;
            }
            if let Ok(meta) = fs::symlink_metadata(anc) {
                if meta.file_type().is_symlink() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "Refusing symlinked directory component",
                    ));
                }
            }
            if inside_home && home_roots.iter().any(|h| anc == h.as_path()) {
                break;
            }
            if !inside_home && !inside_temp && depth >= 4 {
                break;
            }
        }
        Ok(())
    };

    check_chain(path)?;
    if !path.is_absolute() {
        if let Ok(cwd) = std::env::current_dir() {
            check_chain(&cwd.join(path))?;
        }
    }
    Ok(())
}

pub fn ensure_private_dir(dir: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;

    reject_any_symlink_component(dir)?;

    let existed_before = fs::symlink_metadata(dir).is_ok();
    if !existed_before {
        let init_mode = if is_protected_shared_user_dir(dir) {
            0o755
        } else {
            0o700
        };
        fs::DirBuilder::new()
            .recursive(true)
            .mode(init_mode)
            .create(dir)?;
    }
    reject_any_symlink_component(dir)?;
    let meta = fs::symlink_metadata(dir)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Refusing symlinked output directory",
        ));
    }
    if !meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Target path is not a directory",
        ));
    }
    let my_uid = unsafe { libc::geteuid() };
    if meta.uid() != my_uid && meta.uid() != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Refusing directory owned by foreign UID",
        ));
    }
    let canon = fs::canonicalize(dir)?;
    let is_protected_shared =
        is_protected_shared_user_dir(dir) || is_protected_shared_user_dir(&canon);
    let is_dedicated_config_dir = dir.file_name().and_then(|n| n.to_str()) == Some(".rtsp_viewer")
        || canon.file_name().and_then(|n| n.to_str()) == Some(".rtsp_viewer");
    if meta.uid() == my_uid && ((!existed_before && !is_protected_shared) || is_dedicated_config_dir)
    {
        let _ = fs::set_permissions(&canon, fs::Permissions::from_mode(0o700));
    }
    Ok(canon)
}

pub fn create_private_file_exclusive(dir: &Path, filename: &str) -> std::io::Result<(fs::File, PathBuf)> {
    use std::os::unix::fs::OpenOptionsExt;

    if filename.is_empty()
        || filename.contains('/')
        || filename.contains('\\')
        || filename.contains('\0')
        || filename == "."
        || filename == ".."
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Invalid output filename",
        ));
    }
    let canon_dir = ensure_private_dir(dir)?;
    let target = canon_dir.join(filename);
    if target.parent() != Some(canon_dir.as_path()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Output path escaped target directory",
        ));
    }
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(&target)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok((file, target))
}

pub fn percent_encode_userinfo(input: &str) -> String {
    if input == "***" {
        return "***".to_string();
    }
    let mut out = String::with_capacity(input.len() + 8);
    for b in input.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn expand_tilde_with_env(
    path: &str,
    snap_real_home: Option<&str>,
    home: Option<&str>,
) -> PathBuf {
    let stripped_opt = if path == "~" {
        Some("")
    } else {
        path.strip_prefix("~/")
    };
    if let Some(stripped) = stripped_opt {
        let preferred_home = if !stripped.starts_with('.') {
            snap_real_home
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .or_else(|| home.map(str::trim).filter(|s| !s.is_empty()))
        } else {
            home.map(str::trim).filter(|s| !s.is_empty())
                .or_else(|| snap_real_home.map(str::trim).filter(|s| !s.is_empty()))
        };
        if let Some(base) = preferred_home {
            if stripped.is_empty() {
                return PathBuf::from(base);
            }
            return PathBuf::from(base).join(stripped);
        }
    }
    PathBuf::from(path)
}

pub fn expand_tilde(path: &str) -> PathBuf {
    let snap_real_home = std::env::var("SNAP_REAL_HOME").ok();
    let home = std::env::var("HOME").ok();
    expand_tilde_with_env(path, snap_real_home.as_deref(), home.as_deref())
}

pub fn config_file_path() -> PathBuf {
    if let Ok(snap_common) = std::env::var("SNAP_USER_COMMON") {
        if !snap_common.trim().is_empty() {
            return PathBuf::from(snap_common.trim()).join("config.json");
        }
    }
    expand_tilde("~/.rtsp_viewer/config.json")
}

fn zeroize_env_map(map: &mut BTreeMap<String, String>) {
    for v in map.values_mut() {
        v.zeroize();
    }
    while let Some((mut k, mut v)) = map.pop_first() {
        k.zeroize();
        v.zeroize();
    }
}

pub fn is_rtsp_env_file(path: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > MAX_ENV_FILE_BYTES {
        return false;
    }
    let my_uid = unsafe { libc::geteuid() };
    if meta.uid() != my_uid {
        return false;
    }
    let Some(mut content) = read_regular_file_bounded(path, MAX_ENV_FILE_BYTES) else {
        return false;
    };
    let mut map = parse_env_content(&content);
    content.zeroize();
    let matches_rtsp = map
        .keys()
        .any(|k| k.starts_with("RTSP_") || k.starts_with("DVR_") || k.starts_with("NVR_"));
    zeroize_env_map(&mut map);
    matches_rtsp
}

fn locate_env_file_from(
    explicit_override: Option<&str>,
    snap_common: Option<&str>,
    user_env: PathBuf,
    cwd: Option<&Path>,
    exe: Option<&Path>,
) -> PathBuf {
    if let Some(explicit) = explicit_override {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            return expand_tilde(trimmed);
        }
    }
    if let Some(snap) = snap_common {
        let trimmed = snap.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed).join(".env");
        }
    }
    if user_env.exists() {
        return user_env;
    }
    if let Some(exe_path) = exe {
        if let Some(exe_dir) = exe_path.parent() {
            let direct = exe_dir.join(".env");
            if direct.is_file() && is_rtsp_env_file(&direct) {
                return direct;
            }
            if let Some(parent) = exe_dir.parent().and_then(|p| p.parent()) {
                let root_env = parent.join(".env");
                if root_env.is_file() && is_rtsp_env_file(&root_env) {
                    return root_env;
                }
            }
        }
    }
    if let Some(cwd_dir) = cwd {
        // Only adopt a CWD .env when running from a legitimate git checkout (e.g. local dev)
        // owned by the current UID, never from arbitrary ~/Downloads folders.
        let candidate = cwd_dir.join(".env");
        if cwd_dir.join("Cargo.toml").is_file() && candidate.is_file() && is_rtsp_env_file(&candidate)
        {
            return candidate;
        }
    }
    user_env
}

pub fn locate_env_file() -> PathBuf {
    let explicit = std::env::var("RTSP_ENV_FILE").ok();
    let snap_common = std::env::var("SNAP_USER_COMMON").ok();
    let user_env = expand_tilde("~/.rtsp_viewer/.env");
    if let Some(parent) = user_env.parent() {
        if let Ok(meta) = fs::symlink_metadata(parent) {
            let my_uid = unsafe { libc::geteuid() };
            if !meta.file_type().is_symlink()
                && meta.is_dir()
                && meta.uid() == my_uid
                && !is_protected_shared_user_dir(parent)
            {
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
            }
        }
    }
    let cwd = std::env::current_dir().ok();
    let exe = std::env::current_exe().ok();
    locate_env_file_from(
        explicit.as_deref(),
        snap_common.as_deref(),
        user_env,
        cwd.as_deref(),
        exe.as_deref(),
    )
}

pub fn escape_env_val(val: &str) -> String {
    let mut out = String::with_capacity(val.len() + 4);
    for ch in val.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\r' | '\n' => {}
            _ => out.push(ch),
        }
    }
    out
}

fn parse_env_value(raw_val: &str) -> String {
    let trimmed = raw_val.trim_start();
    if let Some(rest) = trimmed.strip_prefix('"') {
        let mut out = String::with_capacity(rest.len());
        let mut chars = rest.chars();
        while let Some(ch) = chars.next() {
            match ch {
                '\\' => {
                    if let Some(next_ch) = chars.next() {
                        out.push(next_ch);
                    } else {
                        out.push('\\');
                    }
                }
                '"' => break,
                _ => out.push(ch),
            }
        }
        return out;
    }

    if let Some(rest) = trimmed.strip_prefix('\'') {
        if let Some(end_idx) = rest.find('\'') {
            return rest[..end_idx].to_string();
        }
        return rest.trim_end().to_string();
    }

    // Unquoted value: strip inline `# ...` comments (at start or preceded by whitespace)
    let mut cut_idx = None;
    let mut prev_ws = true;
    for (idx, ch) in trimmed.char_indices() {
        if ch == '#' && prev_ws {
            cut_idx = Some(idx);
            break;
        }
        prev_ws = ch.is_whitespace();
    }
    let slice = match cut_idx {
        Some(idx) => &trimmed[..idx],
        None => trimmed,
    };
    slice.trim().to_string()
}

pub fn parse_env_content(content: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        if let Some((k, v)) = line.split_once('=') {
            let key = k.trim();
            if !key.is_empty() {
                map.insert(key.to_string(), parse_env_value(v));
            }
        }
    }
    map
}

pub fn load_env_config(path: &Path) -> NvrEnvConfig {
    use std::os::unix::fs::OpenOptionsExt;

    let mut cfg = NvrEnvConfig::default();
    let my_uid = unsafe { libc::geteuid() };

    // Verify ownership BEFORE reading and tighten world/group-readable .env file permissions if owned by current UID
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.uid() != my_uid && meta.uid() != 0 {
            return NvrEnvConfig::default();
        }
        if meta.is_file()
            && !meta.file_type().is_symlink()
            && meta.len() <= MAX_ENV_FILE_BYTES
            && meta.uid() == my_uid
        {
            let mode = meta.permissions().mode() & 0o777;
            if (mode & 0o077) != 0 {
                if let Ok(f) = fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(path)
                {
                    let _ = f.set_permissions(fs::Permissions::from_mode(0o600));
                }
            }
        }
    }

    let mut file_map = read_regular_file_bounded(path, MAX_ENV_FILE_BYTES)
        .map(|mut raw| {
            let parsed = parse_env_content(&raw);
            raw.zeroize();
            parsed
        })
        .unwrap_or_default();

    let get_val = |key: &str, fmap: &BTreeMap<String, String>| -> Option<String> {
        std::env::var(key)
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| fmap.get(key).cloned())
    };

    if let Some(v) = get_val("RTSP_BRAND", &file_map) {
        cfg.brand = v;
    }
    if let Some(v) = get_val("RTSP_HOST", &file_map) {
        cfg.host = v;
    }
    if let Some(v) = get_val("RTSP_P2P_SERIAL", &file_map) {
        cfg.p2p_serial = v;
    }
    if let Some(v) = get_val("RTSP_USE_CLOUD_P2P", &file_map) {
        cfg.use_cloud_p2p = matches!(v.to_lowercase().as_str(), "1" | "true" | "yes");
    }
    if let Some(v) = get_val("RTSP_TAILSCALE_HOST", &file_map) {
        cfg.tailscale_host = v;
    }
    if let Some(v) = get_val("RTSP_USE_TAILSCALE", &file_map) {
        cfg.use_tailscale = matches!(v.to_lowercase().as_str(), "1" | "true" | "yes");
    }
    if let Some(v) = get_val("RTSP_PORT", &file_map).and_then(|s| s.parse::<u16>().ok()) {
        cfg.port = v.max(1);
    }
    if let Some(v) = get_val("RTSP_HTTP_PORT", &file_map)
        .or_else(|| get_val("NVR_HTTP_PORT", &file_map))
        .and_then(|s| s.parse::<u16>().ok())
    {
        cfg.http_port = v.max(1);
    }
    if let Some(v) = get_val("RTSP_USER", &file_map) {
        cfg.username = v;
    }
    if let Some(v) = get_val("RTSP_PASSWORD", &file_map) {
        cfg.password.zeroize();
        cfg.password = v;
    }
    if let Some(v) = get_val("NUM_CAMERAS", &file_map).and_then(|s| s.parse::<usize>().ok()) {
        cfg.num_cameras = v.clamp(1, 32);
    }
    if let Some(v) =
        get_val("IP_CAMERA_START_CHANNEL", &file_map).and_then(|s| s.parse::<usize>().ok())
    {
        cfg.ip_camera_start_channel = v.max(1);
    }
    if let Some(v) = get_val("DEFAULT_SUBTYPE", &file_map).and_then(|s| s.parse::<u8>().ok()) {
        cfg.default_subtype = if v == 0 { 0 } else { 1 };
    }
    if let Some(v) = get_val("HWACCEL_MODE", &file_map) {
        cfg.hwaccel_mode = v;
    }
    if let Some(v) = get_val("RTSP_CUSTOM_URL_TEMPLATE", &file_map) {
        if !v.is_empty() {
            cfg.custom_url_template = v;
        }
    }

    zeroize_env_map(&mut file_map);
    cfg
}

pub fn write_private_file(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    if let Ok(pmeta) = fs::symlink_metadata(path) {
        if pmeta.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Refusing symlinked config file target",
            ));
        }
    }

    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    reject_any_symlink_component(parent)?;

    if let Ok(meta) = fs::symlink_metadata(parent) {
        if meta.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Refusing symlinked config directory",
            ));
        }
        if !meta.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Parent config path is not a directory",
            ));
        }
    } else {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }

    let meta = fs::symlink_metadata(parent)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Refusing symlinked config directory",
        ));
    }

    // Enforce 0700 on dedicated config directories owned by current UID (including $SNAP_USER_COMMON
    // and ~/.rtsp_viewer), while never chmodding shared repo roots, $HOME, or top-level XDG folders.
    let my_uid = unsafe { libc::geteuid() };
    let is_repo_root = parent.join("Cargo.toml").is_file();
    if meta.uid() == my_uid && !is_protected_shared_user_dir(parent) && !is_repo_root {
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");
    let tmp_path = parent.join(format!(
        ".{}.tmp.{}.{:016x}",
        file_name,
        std::process::id(),
        rand::random::<u64>()
    ));

    let write_res = (|| -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(0o600)
            .open(&tmp_path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp_path, path)?;
        Ok(())
    })();

    if write_res.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_res
}

pub fn save_env_config(path: &Path, cfg: &NvrEnvConfig) -> std::io::Result<()> {
    let mut content = format!(
        "# Local RTSP Camera Viewer Configuration (chmod 0600, gitignored)\n\
         RTSP_BRAND=\"{}\"\n\
         RTSP_HOST=\"{}\"\n\
         RTSP_P2P_SERIAL=\"{}\"\n\
         RTSP_USE_CLOUD_P2P=\"{}\"\n\
         RTSP_TAILSCALE_HOST=\"{}\"\n\
         RTSP_USE_TAILSCALE=\"{}\"\n\
         RTSP_PORT=\"{}\"\n\
         RTSP_HTTP_PORT=\"{}\"\n\
         RTSP_USER=\"{}\"\n\
         RTSP_PASSWORD=\"{}\"\n\
         NUM_CAMERAS=\"{}\"\n\
         IP_CAMERA_START_CHANNEL=\"{}\"\n\
         DEFAULT_SUBTYPE=\"{}\"\n\
         HWACCEL_MODE=\"{}\"\n\
         RTSP_CUSTOM_URL_TEMPLATE=\"{}\"\n",
        escape_env_val(&cfg.brand),
        escape_env_val(&cfg.host),
        escape_env_val(&cfg.p2p_serial),
        cfg.use_cloud_p2p,
        escape_env_val(&cfg.tailscale_host),
        cfg.use_tailscale,
        cfg.port,
        cfg.http_port,
        escape_env_val(&cfg.username),
        escape_env_val(&cfg.password),
        cfg.num_cameras.clamp(1, 32),
        cfg.ip_camera_start_channel,
        cfg.default_subtype,
        escape_env_val(&cfg.hwaccel_mode),
        escape_env_val(&cfg.custom_url_template)
    );
    let res = write_private_file(path, &content);
    content.zeroize();
    res
}

impl AppConfig {
    fn load_from_path_and_sync(path: &Path, env_cfg: &NvrEnvConfig) -> Self {
        let mut cfg: AppConfig = read_regular_file_bounded(path, MAX_CONFIG_JSON_BYTES)
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        cfg.num_cameras = env_cfg.num_cameras.clamp(1, 32);
        cfg.normalize_channels();
        cfg
    }

    pub fn load_and_sync(env_cfg: &NvrEnvConfig) -> Self {
        let path = config_file_path();
        Self::load_from_path_and_sync(&path, env_cfg)
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = config_file_path();
        let json = serde_json::to_string_pretty(self)?;
        write_private_file(&path, &json)
    }

    pub fn normalize_channels(&mut self) {
        let n = self.num_cameras.clamp(1, 32);
        self.num_cameras = n;
        let mut ordered: Vec<usize> = self
            .channel_order
            .iter()
            .copied()
            .filter(|&ch| ch >= 1 && ch <= n)
            .collect();
        for ch in 1..=n {
            if !ordered.contains(&ch) {
                ordered.push(ch);
            }
        }
        self.channel_order = ordered;
    }

    pub fn camera_display_name(&self, channel: usize) -> String {
        if let Some(custom) = self.camera_names.get(&channel) {
            let trimmed = custom.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        format!("Camera {}", channel)
    }

    pub fn swap_channels(&mut self, ch_a: usize, ch_b: usize) {
        if let (Some(idx_a), Some(idx_b)) = (
            self.channel_order.iter().position(|&c| c == ch_a),
            self.channel_order.iter().position(|&c| c == ch_b),
        ) {
            self.channel_order.swap(idx_a, idx_b);
        }
    }

    #[allow(dead_code)]
    pub fn reset_channel_order(&mut self) {
        self.channel_order = (1..=self.num_cameras.clamp(1, 32)).collect();
    }

    pub fn effective_layout_mode(&self) -> &'static str {
        let n = self.num_cameras.max(1);
        match self.layout_mode.trim().to_ascii_lowercase().as_str() {
            "2x2" if n <= 4 => "2x2",
            "3x3" if n <= 9 => "3x3",
            "3x4" if n <= 12 => "3x4",
            "4x3" if n <= 12 => "4x3",
            "2x6" if n <= 12 => "2x6",
            _ => "auto",
        }
    }

    pub fn grid_dimensions(&self) -> (usize, usize) {
        let n = self.num_cameras.max(1);
        match self.effective_layout_mode() {
            "2x2" => (2, 2),
            "3x3" => (3, 3),
            "3x4" => (3, 4),
            "4x3" => (4, 3),
            "2x6" => (2, 6),
            _ => {
                let cols = (n as f64).sqrt().ceil() as usize;
                let rows = n.div_ceil(cols);
                (rows.max(1), cols.max(1))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_percent_encode_special_chars() {
        assert_eq!(percent_encode_userinfo("demo@123"), "demo%40123");
        assert_eq!(percent_encode_userinfo("p@ss:word#1"), "p%40ss%3Aword%231");
        assert_eq!(percent_encode_userinfo("admin"), "admin");
    }

    #[test]
    fn test_validate_nvr_host_blocks_authority_injection() {
        assert_eq!(validate_nvr_host("192.168.1.108").unwrap(), "192.168.1.108");
        assert_eq!(validate_nvr_host("2001:db8::1").unwrap(), "[2001:db8::1]");
        assert!(validate_nvr_host("evil.com:554/capture#").is_err());
        assert!(validate_nvr_host("user@evil.com").is_err());
        assert!(validate_nvr_host("192.168.1.1\r\nInjected: 1").is_err());
    }

    #[test]
    fn test_validate_custom_rtsp_url_blocks_protocol_smuggling() {
        assert!(validate_custom_rtsp_url("rtsp://192.168.1.10:554/live").is_ok());
        assert!(validate_custom_rtsp_url("file:///etc/passwd").is_err());
        assert!(validate_custom_rtsp_url("http://169.254.169.254/latest").is_err());
    }

    #[test]
    fn test_sanitize_export_label_prevents_traversal_and_flag_injection() {
        assert_eq!(sanitize_export_label("../../etc/passwd"), "etc_passwd");
        assert_eq!(sanitize_export_label("-rf --no-preserve-root"), "rf_--no-preserve-root");
        assert_eq!(sanitize_export_label("Front Door: Gate #1"), "Front_Door__Gate__1");
        assert_eq!(sanitize_export_label("---...___"), "Camera");
    }

    #[test]
    fn test_debug_redacts_password() {
        let mut cfg = NvrEnvConfig::default();
        cfg.password = "SuperSecretPassword123!".to_string();
        let dbg = format!("{:?}", cfg);
        assert!(!dbg.contains("SuperSecretPassword123!"));
        assert!(dbg.contains("[REDACTED]"));
    }

    #[test]
    fn test_env_escape_and_parse_roundtrip() {
        let raw_pw = "my\"complex\\pass@123\ninjected=1";
        let escaped = escape_env_val(raw_pw);
        let env_line = format!("RTSP_PASSWORD=\"{}\"\n", escaped);
        let parsed = parse_env_content(&env_line);
        assert_eq!(
            parsed.get("RTSP_PASSWORD").map(String::as_str),
            Some("my\"complex\\pass@123injected=1")
        );
        assert!(!parsed.contains_key("injected"));
    }

    #[test]
    fn test_cwd_env_hijack_prevention_and_atomic_private_write() {
        let base = std::env::temp_dir().join(format!(
            "rtsp_viewer_cfg_test_{}_{:08x}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let unrelated_cwd = base.join("some_webapp");
        let untrusted_dl = base.join("Downloads");
        let rtsp_cwd = base.join("nvr_checkout");
        let home_rtsp_dir = base.join(".rtsp_viewer");
        let user_env = home_rtsp_dir.join(".env");

        fs::create_dir_all(&unrelated_cwd).unwrap();
        fs::create_dir_all(&untrusted_dl).unwrap();
        fs::create_dir_all(&rtsp_cwd).unwrap();

        // 1. Unrelated project's .env MUST NOT be hijacked
        let unrelated_env = unrelated_cwd.join(".env");
        fs::write(&unrelated_env, "DATABASE_URL=postgres://localhost/db\n").unwrap();
        let resolved = locate_env_file_from(None, None, user_env.clone(), Some(&unrelated_cwd), None);
        assert_eq!(resolved, user_env);

        // 2. Untrusted Downloads .env (even with RTSP_HOST) without Cargo.toml MUST NOT be auto-loaded!
        fs::write(untrusted_dl.join(".env"), "RTSP_HOST=\"evil.example.com\"\n").unwrap();
        let resolved_dl =
            locate_env_file_from(None, None, user_env.clone(), Some(&untrusted_dl), None);
        assert_eq!(resolved_dl, user_env);

        // 3. Legitimate repo checkout with Cargo.toml + .env IS adopted when ~/.rtsp_viewer/.env doesn't exist yet
        fs::write(rtsp_cwd.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let repo_env = rtsp_cwd.join(".env");
        fs::write(&repo_env, "RTSP_HOST=\"192.168.1.200\"\n").unwrap();
        let resolved_repo = locate_env_file_from(None, None, user_env.clone(), Some(&rtsp_cwd), None);
        assert_eq!(resolved_repo, repo_env);

        // 4. SNAP_USER_COMMON directory gets hardened to 0700 on write_private_file
        let snap_dir = base.join("snap_common");
        fs::create_dir_all(&snap_dir).unwrap();
        fs::set_permissions(&snap_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let snap_env = snap_dir.join(".env");
        write_private_file(&snap_env, "RTSP_HOST=\"10.0.0.5\"\n").unwrap();
        assert_eq!(
            fs::metadata(&snap_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&snap_env).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_dahua_p2p_hikvision_and_playback_paths() {
        let mut cfg = NvrEnvConfig {
            brand: "dahua".to_string(),
            host: "192.168.1.50".to_string(),
            p2p_serial: "SN123456".to_string(),
            use_cloud_p2p: false,
            p2p_local_port: 41234,
            p2p_local_http_port: 41235,
            tailscale_host: "100.64.0.10".to_string(),
            use_tailscale: false,
            port: 554,
            http_port: 80,
            username: "admin".to_string(),
            password: "my@pass".to_string(),
            num_cameras: 12,
            ip_camera_start_channel: 10,
            default_subtype: 1,
            hwaccel_mode: "auto".to_string(),
            custom_url_template: String::new(),
        };

        assert_eq!(
            cfg.build_live_rtsp_path(12, 1),
            "/cam/realmonitor?channel=12&subtype=1"
        );
        let t0 = NaiveDateTime::parse_from_str("2026-11-18 14:03:12", "%Y-%m-%d %H:%M:%S").unwrap();
        let t1 = NaiveDateTime::parse_from_str("2026-11-18 14:03:47", "%Y-%m-%d %H:%M:%S").unwrap();
        assert_eq!(
            cfg.build_playback_rtsp_path(1, 0, t0, t1),
            "/cam/playback?channel=1&subtype=0&starttime=2026_11_18_14_03_12&endtime=2026_11_18_14_03_47"
        );

        cfg.use_cloud_p2p = true;
        assert_eq!(cfg.active_port(), 41234);
        assert_eq!(cfg.active_http_port(), 41235);
    }

    #[test]
    fn test_ipv6_zone_ids_and_rtsp_timeout_flag_selection() {
        assert_eq!(validate_nvr_host("fe80::1%eth0").unwrap(), "[fe80::1]");
        assert_eq!(
            validate_nvr_host("[fe80::1%enp3s0.10_a-1]").unwrap(),
            "[fe80::1]"
        );
        assert_eq!(format_host_for_url("fe80::1%wlan0"), "[fe80::1]");
        assert!(validate_nvr_host("fe80::1%").is_err());
        assert!(validate_nvr_host("fe80::1%eth0%bad").is_err());
        assert!(validate_nvr_host("192.168.1.10%eth0").is_err());
        assert!(validate_nvr_host("evil.com%00").is_err());

        assert_eq!(
            select_rtsp_timeout_arg_from_help("  -stimeout          <int64>  set timeout\n"),
            "-stimeout"
        );
        assert_eq!(
            select_rtsp_timeout_arg_from_help(
                "  -timeout           <int64>  set timeout\n  -stimeout          <int64>  deprecated\n"
            ),
            "-timeout"
        );
        assert_eq!(select_rtsp_timeout_arg_from_help(""), "-timeout");
        assert!(matches!(rtsp_timeout_ffmpeg_arg(), "-timeout" | "-stimeout"));
    }

    #[test]
    fn test_expand_tilde_and_protected_xdg_dirs_and_symlink_caps() {
        assert_eq!(
            expand_tilde_with_env(
                "~",
                Some("/home/alice"),
                Some("/home/alice/snap/nvr/current")
            ),
            PathBuf::from("/home/alice")
        );
        assert_eq!(
            expand_tilde_with_env(
                "~/Pictures/NVR-Snapshots",
                Some("/home/alice"),
                Some("/home/alice/snap/nvr/current")
            ),
            PathBuf::from("/home/alice/Pictures/NVR-Snapshots")
        );
        assert_eq!(
            expand_tilde_with_env(
                "~/.rtsp_viewer/config.json",
                Some("/home/alice"),
                Some("/home/alice/snap/nvr/current")
            ),
            PathBuf::from("/home/alice/snap/nvr/current/.rtsp_viewer/config.json")
        );

        let base = std::env::temp_dir().join(format!(
            "rtsp_viewer_hardening_{}_{:08x}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let fake_home = base.join("home");
        let pics = fake_home.join("Pictures");
        let snaps = pics.join("NVR-Snapshots");
        fs::create_dir_all(&snaps).unwrap();

        let homes = vec![fake_home.clone()];
        assert!(is_protected_shared_user_dir_with_homes(&fake_home, &homes));
        assert!(is_protected_shared_user_dir_with_homes(&pics, &homes));
        assert!(!is_protected_shared_user_dir_with_homes(&snaps, &homes));

        // Symlinked parent directory rejected by write_private_file without chmodding target
        let target_dir = base.join("victim_dir");
        fs::create_dir_all(&target_dir).unwrap();
        fs::set_permissions(&target_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let sym_dir = base.join("sym_parent");
        std::os::unix::fs::symlink(&target_dir, &sym_dir).unwrap();
        assert!(write_private_file(&sym_dir.join(".env"), "RTSP_HOST=\"1.2.3.4\"\n").is_err());
        assert_eq!(
            fs::metadata(&target_dir).unwrap().permissions().mode() & 0o777,
            0o755
        );

        // Symlinked .env and oversized .env rejected by load_env_config
        let real_env = base.join("real.env");
        fs::write(&real_env, "RTSP_HOST=\"10.20.30.40\"\n").unwrap();
        let sym_env = base.join("sym.env");
        std::os::unix::fs::symlink(&real_env, &sym_env).unwrap();
        assert!(read_regular_file_bounded(&sym_env, MAX_ENV_FILE_BYTES).is_none());

        let huge_env = base.join("huge.env");
        fs::write(&huge_env, format!("RTSP_HOST=\"10.0.0.9\"\n# {}", "x".repeat(70_000))).unwrap();
        assert!(read_regular_file_bounded(&huge_env, MAX_ENV_FILE_BYTES).is_none());

        // Symlinked and oversized config.json rejected by AppConfig::load_from_path_and_sync
        let real_json = base.join("real_config.json");
        fs::write(&real_json, "{\"fps_limit\": 29}").unwrap();
        let sym_json = base.join("sym_config.json");
        std::os::unix::fs::symlink(&real_json, &sym_json).unwrap();
        let env_default = NvrEnvConfig::default();
        assert_eq!(
            AppConfig::load_from_path_and_sync(&sym_json, &env_default).fps_limit,
            AppConfig::default().fps_limit
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_app_config_default_auto_grid_modes_and_reset_channel_order() {
        let mut cfg = AppConfig::default();
        assert_eq!(cfg.layout_mode, "auto");
        assert_eq!(cfg.effective_layout_mode(), "auto");
        assert_eq!(cfg.grid_dimensions(), (3, 4)); // 12 cameras -> 3x4 via sqrt

        cfg.num_cameras = 4;
        assert_eq!(cfg.grid_dimensions(), (2, 2));

        cfg.layout_mode = "2X2".to_string();
        assert_eq!(cfg.effective_layout_mode(), "2x2");
        assert_eq!(cfg.grid_dimensions(), (2, 2));

        // Exceeding 2x2 capacity falls back to auto cleanly
        cfg.num_cameras = 6;
        assert_eq!(cfg.effective_layout_mode(), "auto");
        assert_eq!(cfg.grid_dimensions(), (2, 3));

        cfg.layout_mode = "3x3".to_string();
        cfg.num_cameras = 8;
        assert_eq!(cfg.effective_layout_mode(), "3x3");
        assert_eq!(cfg.grid_dimensions(), (3, 3));

        cfg.num_cameras = 10;
        assert_eq!(cfg.effective_layout_mode(), "auto");
        assert_eq!(cfg.grid_dimensions(), (3, 4));

        cfg.layout_mode = "3x4".to_string();
        cfg.num_cameras = 16;
        assert_eq!(cfg.effective_layout_mode(), "auto");
        assert_eq!(cfg.grid_dimensions(), (4, 4));

        cfg.num_cameras = 5;
        cfg.channel_order = vec![5, 3, 1, 4, 2];
        cfg.reset_channel_order();
        assert_eq!(cfg.channel_order, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_nvr_env_config_validate_for_save_and_password_redaction() {
        // 1. Valid LAN
        let mut lan = NvrEnvConfig::default();
        lan.host = "192.168.1.108".to_string();
        lan.password = "Secret@Pass#99".to_string();
        assert!(lan.validate_for_save().is_ok());

        // 2. Valid Cloud P2P (including empty LAN host)
        let mut p2p = NvrEnvConfig::default();
        p2p.host.clear();
        p2p.use_cloud_p2p = true;
        p2p.p2p_serial = "SN987654321ABCD".to_string();
        p2p.password = "CloudPass!2026".to_string();
        assert!(p2p.validate_for_save().is_ok());

        // Empty P2P serial rejected
        let mut p2p_empty_serial = p2p.clone();
        p2p_empty_serial.p2p_serial.clear();
        assert!(p2p_empty_serial.validate_for_save().is_err());

        // P2P serial with forbidden characters rejected
        let mut p2p_bad_serial = p2p.clone();
        p2p_bad_serial.p2p_serial = "SN123@evil:80/x".to_string();
        assert!(p2p_bad_serial.validate_for_save().is_err());

        // 3. Valid Tailscale + is_tailscale_active()
        let mut ts = NvrEnvConfig::default();
        ts.use_tailscale = true;
        ts.tailscale_host = "100.64.10.25".to_string();
        assert!(ts.is_tailscale_active());
        assert_eq!(ts.active_host(), "100.64.10.25");
        assert!(ts.validate_for_save().is_ok());

        // Cloud P2P takes precedence over Tailscale in is_tailscale_active()
        let mut both = ts.clone();
        both.use_cloud_p2p = true;
        both.p2p_serial = "SN112233".to_string();
        both.password = "pw".to_string();
        assert!(!both.is_tailscale_active());
        assert_eq!(both.active_host(), "127.0.0.1");

        // 4. Invalid host injection rejected even when P2P is active
        let mut injected = p2p.clone();
        injected.host = "attacker@10.0.0.1/path".to_string();
        assert!(injected.validate_for_save().is_err());

        // 5. Custom URL without {channel} rejected for multi-camera, allowed for single-camera
        let mut custom = lan.clone();
        custom.brand = "custom".to_string();
        custom.num_cameras = 4;
        custom.custom_url_template =
            "rtsp://{user}:{password}@{host}:{port}/live/main".to_string();
        assert!(custom.validate_for_save().is_err());

        custom.num_cameras = 1;
        assert!(custom.validate_for_save().is_ok());

        // 6. Zero password leakage even if user typed password into invalid URL/host
        let mut leak_attempt = lan.clone();
        leak_attempt.password = "UltraSecretPass2026!".to_string();
        leak_attempt.brand = "custom".to_string();
        leak_attempt.num_cameras = 1;
        leak_attempt.custom_url_template =
            "http://admin:UltraSecretPass2026!@127.0.0.1:554/stream".to_string();
        let err_msg = leak_attempt.validate_for_save().unwrap_err();
        assert!(!err_msg.contains("UltraSecretPass2026!"));
    }
}
