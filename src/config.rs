use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

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
            layout_mode: "3x4".to_string(),
            num_cameras: 12,
            screenshot_dir,
            recording_dir,
            fps_limit: 15,
            reconnect_interval: 3,
            connection_timeout: 5,
            fullscreen_auto_hd: false,
            patrol_interval_secs: 8,
            camera_names: BTreeMap::new(),
            channel_order: (1..=12).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvrEnvConfig {
    pub brand: String,
    pub host: String,
    pub p2p_serial: String,
    pub use_cloud_p2p: bool,
    pub p2p_local_port: u16,
    pub tailscale_host: String,
    pub use_tailscale: bool,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub num_cameras: usize,
    pub ip_camera_start_channel: usize,
    pub default_subtype: u8,
    pub hwaccel_mode: String,
    pub custom_url_template: String,
}

impl Default for NvrEnvConfig {
    fn default() -> Self {
        Self {
            brand: "dahua".to_string(),
            host: "192.168.1.100".to_string(),
            p2p_serial: String::new(),
            use_cloud_p2p: false,
            p2p_local_port: 15540,
            tailscale_host: String::new(),
            use_tailscale: false,
            port: 554,
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
    pub fn active_host(&self) -> &str {
        if self.use_cloud_p2p && !self.p2p_serial.trim().is_empty() {
            "127.0.0.1"
        } else if self.use_tailscale && !self.tailscale_host.trim().is_empty() {
            self.tailscale_host.trim()
        } else {
            self.host.trim()
        }
    }

    pub fn active_port(&self) -> u16 {
        if self.use_cloud_p2p && !self.p2p_serial.trim().is_empty() {
            if self.p2p_local_port > 0 {
                self.p2p_local_port
            } else {
                15540
            }
        } else {
            self.port
        }
    }

    pub fn build_rtsp_url(&self, channel: usize, subtype: u8) -> String {
        let user_enc = percent_encode_userinfo(&self.username);
        let pass_enc = percent_encode_userinfo(&self.password);
        let host_fmt = format_host_for_url(self.active_host());
        let port = self.active_port();

        match self.brand.to_lowercase().as_str() {
            "hikvision" => {
                let hik_stream = if subtype == 0 { 1 } else { 2 };
                format!(
                    "rtsp://{}:{}@{}:{}/Streaming/Channels/{}0{}",
                    user_enc, pass_enc, host_fmt, port, channel, hik_stream
                )
            }
            "custom" => self
                .custom_url_template
                .replace("{user}", &user_enc)
                .replace("{password}", &pass_enc)
                .replace("{host}", &host_fmt)
                .replace("{port}", &port.to_string())
                .replace("{channel}", &channel.to_string())
                .replace("{subtype}", &subtype.to_string()),
            _ => {
                // Default: Dahua / CP Plus / Amcrest XVR/NVR standard
                format!(
                    "rtsp://{}:{}@{}:{}/cam/realmonitor?channel={}&subtype={}",
                    user_enc, pass_enc, host_fmt, port, channel, subtype
                )
            }
        }
    }

    pub fn build_redacted_url(&self, channel: usize, subtype: u8) -> String {
        let mut clone = self.clone();
        clone.password = "***".to_string();
        clone.build_rtsp_url(channel, subtype)
    }
}

pub fn format_host_for_url(host: &str) -> String {
    let trimmed = host.trim();
    if trimmed.is_empty() || (trimmed.starts_with('[') && trimmed.ends_with(']')) {
        return trimmed.to_string();
    }
    let without_zone = trimmed.split('%').next().unwrap_or(trimmed);
    if without_zone.parse::<std::net::Ipv6Addr>().is_ok() || trimmed.matches(':').count() >= 2 {
        format!("[{}]", trimmed)
    } else {
        trimmed.to_string()
    }
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

pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(stripped);
        }
    }
    PathBuf::from(path)
}

pub fn config_file_path() -> PathBuf {
    if let Ok(snap_common) = std::env::var("SNAP_USER_COMMON") {
        if !snap_common.trim().is_empty() {
            return PathBuf::from(snap_common.trim()).join("config.json");
        }
    }
    expand_tilde("~/.rtsp_viewer/config.json")
}

pub fn is_rtsp_env_file(path: &Path) -> bool {
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    let map = parse_env_content(&content);
    map.keys()
        .any(|k| k.starts_with("RTSP_") || k.starts_with("DVR_") || k.starts_with("NVR_"))
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
    if let Some(cwd_dir) = cwd {
        let candidate = cwd_dir.join(".env");
        if candidate.is_file() && is_rtsp_env_file(&candidate) {
            return candidate;
        }
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
    user_env
}

pub fn locate_env_file() -> PathBuf {
    let explicit = std::env::var("RTSP_ENV_FILE").ok();
    let snap_common = std::env::var("SNAP_USER_COMMON").ok();
    let user_env = expand_tilde("~/.rtsp_viewer/.env");
    if let Some(parent) = user_env.parent() {
        if parent.exists() {
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
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
    let mut cfg = NvrEnvConfig::default();
    let file_map = fs::read_to_string(path)
        .map(|s| parse_env_content(&s))
        .unwrap_or_default();

    let get_val = |key: &str| -> Option<String> {
        std::env::var(key)
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| file_map.get(key).cloned())
    };

    if let Some(v) = get_val("RTSP_BRAND") {
        cfg.brand = v;
    }
    if let Some(v) = get_val("RTSP_HOST") {
        cfg.host = v;
    }
    if let Some(v) = get_val("RTSP_P2P_SERIAL") {
        cfg.p2p_serial = v;
    }
    if let Some(v) = get_val("RTSP_USE_CLOUD_P2P") {
        cfg.use_cloud_p2p = matches!(v.to_lowercase().as_str(), "1" | "true" | "yes");
    }
    if let Some(v) = get_val("RTSP_TAILSCALE_HOST") {
        cfg.tailscale_host = v;
    }
    if let Some(v) = get_val("RTSP_USE_TAILSCALE") {
        cfg.use_tailscale = matches!(v.to_lowercase().as_str(), "1" | "true" | "yes");
    }
    if let Some(v) = get_val("RTSP_PORT").and_then(|s| s.parse::<u16>().ok()) {
        cfg.port = v;
    }
    if let Some(v) = get_val("RTSP_USER") {
        cfg.username = v;
    }
    if let Some(v) = get_val("RTSP_PASSWORD") {
        cfg.password = v;
    }
    if let Some(v) = get_val("NUM_CAMERAS").and_then(|s| s.parse::<usize>().ok()) {
        cfg.num_cameras = v.clamp(1, 32);
    }
    if let Some(v) = get_val("IP_CAMERA_START_CHANNEL").and_then(|s| s.parse::<usize>().ok()) {
        cfg.ip_camera_start_channel = v.max(1);
    }
    if let Some(v) = get_val("DEFAULT_SUBTYPE").and_then(|s| s.parse::<u8>().ok()) {
        cfg.default_subtype = if v == 0 { 0 } else { 1 };
    }
    if let Some(v) = get_val("HWACCEL_MODE") {
        cfg.hwaccel_mode = v;
    }
    if let Some(v) = get_val("RTSP_CUSTOM_URL_TEMPLATE") {
        if !v.is_empty() {
            cfg.custom_url_template = v;
        }
    }

    cfg
}

pub fn write_private_file(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    if !parent.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    if parent.file_name().is_some_and(|n| n == ".rtsp_viewer") {
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
            .mode(0o600)
            .open(&tmp_path)?;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600))?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp_path, path)?;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        Ok(())
    })();

    if write_res.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_res
}

pub fn save_env_config(path: &Path, cfg: &NvrEnvConfig) -> std::io::Result<()> {
    let content = format!(
        "# Local RTSP Camera Viewer Configuration (chmod 0600, gitignored)\n\
         RTSP_BRAND=\"{}\"\n\
         RTSP_HOST=\"{}\"\n\
         RTSP_P2P_SERIAL=\"{}\"\n\
         RTSP_USE_CLOUD_P2P=\"{}\"\n\
         RTSP_TAILSCALE_HOST=\"{}\"\n\
         RTSP_USE_TAILSCALE=\"{}\"\n\
         RTSP_PORT=\"{}\"\n\
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
        escape_env_val(&cfg.username),
        escape_env_val(&cfg.password),
        cfg.num_cameras.clamp(1, 32),
        cfg.ip_camera_start_channel,
        cfg.default_subtype,
        escape_env_val(&cfg.hwaccel_mode),
        escape_env_val(&cfg.custom_url_template)
    );
    write_private_file(path, &content)
}

impl AppConfig {
    pub fn load_and_sync(env_cfg: &NvrEnvConfig) -> Self {
        let path = config_file_path();
        let mut cfg: AppConfig = fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        cfg.num_cameras = env_cfg.num_cameras.clamp(1, 32);
        cfg.normalize_channels();
        cfg
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

    pub fn grid_dimensions(&self) -> (usize, usize) {
        let n = self.num_cameras.max(1);
        match self.layout_mode.as_str() {
            "3x4" if n <= 12 => (3, 4),
            "4x3" if n <= 12 => (4, 3),
            "2x6" if n <= 12 => (2, 6),
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
    fn test_readme_env_snippet_with_inline_comments() {
        let readme_snippet = r##"
RTSP_BRAND="dahua"             # "dahua" (Dahua/CP Plus/Amcrest), "hikvision", or "custom"
RTSP_HOST="192.168.1.100"      # Primary LAN NVR IP
RTSP_P2P_SERIAL="YOUR_NVR_SN"  # Optional Cloud P2P Serial Number (works on any internet connection!)
RTSP_USE_CLOUD_P2P="false"     # Auto-activates when LAN NVR IP is unreachable
RTSP_TAILSCALE_HOST="100.x.y.z"# Optional Tailscale remote IP for 1-click switching
RTSP_USE_TAILSCALE="false"
RTSP_PORT="554"
RTSP_USER="admin"
RTSP_PASSWORD='p@ss#word_with_hash' # single-quoted value with # inside
NUM_CAMERAS="12"               # Dynamic 1-32 channels (default: 12 for 3x4 grid)
IP_CAMERA_START_CHANNEL=10     # Unquoted integer with trailing comment
DEFAULT_SUBTYPE="1"            # 1 = Sub-Stream (SD), 0 = Main-Stream (HD)
HWACCEL_MODE="auto"            # "auto" (VAAPI/NVDEC with CPU fallback) or "software"
"##;
        let parsed = parse_env_content(readme_snippet);
        assert_eq!(parsed.get("RTSP_BRAND").map(String::as_str), Some("dahua"));
        assert_eq!(
            parsed.get("RTSP_HOST").map(String::as_str),
            Some("192.168.1.100")
        );
        assert_eq!(
            parsed.get("RTSP_P2P_SERIAL").map(String::as_str),
            Some("YOUR_NVR_SN")
        );
        assert_eq!(
            parsed.get("RTSP_USE_CLOUD_P2P").map(String::as_str),
            Some("false")
        );
        assert_eq!(
            parsed.get("RTSP_TAILSCALE_HOST").map(String::as_str),
            Some("100.x.y.z")
        );
        assert_eq!(parsed.get("RTSP_PORT").map(String::as_str), Some("554"));
        assert_eq!(
            parsed.get("RTSP_PASSWORD").map(String::as_str),
            Some("p@ss#word_with_hash")
        );
        assert_eq!(parsed.get("NUM_CAMERAS").map(String::as_str), Some("12"));
        assert_eq!(
            parsed.get("IP_CAMERA_START_CHANNEL").map(String::as_str),
            Some("10")
        );
        assert_eq!(parsed.get("DEFAULT_SUBTYPE").map(String::as_str), Some("1"));
        assert_eq!(parsed.get("HWACCEL_MODE").map(String::as_str), Some("auto"));
    }

    #[test]
    fn test_cwd_env_hijack_prevention_and_atomic_private_write() {
        let base = std::env::temp_dir().join(format!(
            "rtsp_viewer_cfg_test_{}_{:08x}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let unrelated_cwd = base.join("some_webapp");
        let rtsp_cwd = base.join("nvr_checkout");
        let home_rtsp_dir = base.join(".rtsp_viewer");
        let user_env = home_rtsp_dir.join(".env");

        fs::create_dir_all(&unrelated_cwd).unwrap();
        fs::create_dir_all(&rtsp_cwd).unwrap();

        // 1. Unrelated project's .env MUST NOT be hijacked or returned
        let unrelated_env = unrelated_cwd.join(".env");
        fs::write(&unrelated_env, "DATABASE_URL=postgres://localhost/db\nSECRET_KEY=xyz\n").unwrap();
        let resolved = locate_env_file_from(None, None, user_env.clone(), Some(&unrelated_cwd), None);
        assert_eq!(resolved, user_env);

        // 2. Legitimate repo .env with RTSP_ key is used only when ~/.rtsp_viewer/.env doesn't exist yet
        let repo_env = rtsp_cwd.join(".env");
        fs::write(&repo_env, "RTSP_HOST=\"192.168.1.200\"\n").unwrap();
        let resolved_repo = locate_env_file_from(None, None, user_env.clone(), Some(&rtsp_cwd), None);
        assert_eq!(resolved_repo, repo_env);

        // 3. Once ~/.rtsp_viewer/.env exists, it takes priority over CWD .env
        write_private_file(&user_env, "RTSP_HOST=\"10.0.0.5\"\n").unwrap();
        let dir_mode = fs::metadata(&home_rtsp_dir).unwrap().permissions().mode() & 0o777;
        let file_mode = fs::metadata(&user_env).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);

        let resolved_user = locate_env_file_from(None, None, user_env.clone(), Some(&rtsp_cwd), None);
        assert_eq!(resolved_user, user_env);

        // 4. SNAP_USER_COMMON and RTSP_ENV_FILE take top priority
        let snap_dir = base.join("snap_common");
        let resolved_snap = locate_env_file_from(
            None,
            Some(snap_dir.to_str().unwrap()),
            user_env.clone(),
            Some(&rtsp_cwd),
            None,
        );
        assert_eq!(resolved_snap, snap_dir.join(".env"));

        let explicit_file = base.join("custom.env");
        let resolved_explicit = locate_env_file_from(
            Some(explicit_file.to_str().unwrap()),
            Some(snap_dir.to_str().unwrap()),
            user_env,
            Some(&rtsp_cwd),
            None,
        );
        assert_eq!(resolved_explicit, explicit_file);

        // Verify unrelated .env was completely untouched
        assert_eq!(
            fs::read_to_string(&unrelated_env).unwrap(),
            "DATABASE_URL=postgres://localhost/db\nSECRET_KEY=xyz\n"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_dahua_p2p_hikvision_and_ipv6_url_generation() {
        let mut cfg = NvrEnvConfig {
            brand: "dahua".to_string(),
            host: "192.168.1.50".to_string(),
            p2p_serial: "SN123456".to_string(),
            use_cloud_p2p: false,
            p2p_local_port: 15540,
            tailscale_host: "100.64.0.10".to_string(),
            use_tailscale: false,
            port: 554,
            username: "admin".to_string(),
            password: "my@pass".to_string(),
            num_cameras: 12,
            ip_camera_start_channel: 10,
            default_subtype: 1,
            hwaccel_mode: "auto".to_string(),
            custom_url_template: String::new(),
        };

        assert_eq!(
            cfg.build_rtsp_url(12, 1),
            "rtsp://admin:my%40pass@192.168.1.50:554/cam/realmonitor?channel=12&subtype=1"
        );

        // Switch to Cloud P2P tunnel
        cfg.use_cloud_p2p = true;
        assert_eq!(
            cfg.build_rtsp_url(1, 1),
            "rtsp://admin:my%40pass@127.0.0.1:15540/cam/realmonitor?channel=1&subtype=1"
        );
        cfg.use_cloud_p2p = false;

        // Switch to Tailscale host
        cfg.use_tailscale = true;
        assert_eq!(
            cfg.build_rtsp_url(1, 0),
            "rtsp://admin:my%40pass@100.64.0.10:554/cam/realmonitor?channel=1&subtype=0"
        );

        // Switch to Hikvision template
        cfg.brand = "hikvision".to_string();
        assert_eq!(
            cfg.build_rtsp_url(3, 1),
            "rtsp://admin:my%40pass@100.64.0.10:554/Streaming/Channels/302"
        );

        // Raw IPv6 literal bracketing (and idempotence when already bracketed)
        cfg.use_tailscale = false;
        cfg.brand = "dahua".to_string();
        cfg.host = "2001:db8::108".to_string();
        assert_eq!(
            cfg.build_rtsp_url(1, 1),
            "rtsp://admin:my%40pass@[2001:db8::108]:554/cam/realmonitor?channel=1&subtype=1"
        );
        cfg.host = "[2001:db8::108]".to_string();
        assert_eq!(
            cfg.build_rtsp_url(1, 1),
            "rtsp://admin:my%40pass@[2001:db8::108]:554/cam/realmonitor?channel=1&subtype=1"
        );
    }

    #[test]
    fn test_dynamic_channel_normalization_and_swapping() {
        let mut app_cfg = AppConfig {
            num_cameras: 12,
            ..Default::default()
        };
        app_cfg.normalize_channels();
        assert_eq!(app_cfg.channel_order.len(), 12);
        assert_eq!(app_cfg.channel_order[11], 12);

        app_cfg.swap_channels(1, 12);
        assert_eq!(app_cfg.channel_order[0], 12);
        assert_eq!(app_cfg.channel_order[11], 1);

        app_cfg.num_cameras = 16;
        app_cfg.normalize_channels();
        assert_eq!(app_cfg.channel_order.len(), 16);
        assert_eq!(app_cfg.grid_dimensions(), (4, 4));
    }
}
