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
        Self {
            layout: GridLayoutConfig { rows: 3, cols: 4 },
            layout_mode: "3x4".to_string(),
            num_cameras: 12,
            screenshot_dir: "~/rtsp_viewer/screenshots".to_string(),
            recording_dir: "~/rtsp_viewer/recordings".to_string(),
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
        let host = self.active_host();
        let port = self.active_port();

        match self.brand.to_lowercase().as_str() {
            "hikvision" => {
                let hik_stream = if subtype == 0 { 1 } else { 2 };
                format!(
                    "rtsp://{}:{}@{}:{}/Streaming/Channels/{}0{}",
                    user_enc, pass_enc, host, port, channel, hik_stream
                )
            }
            "custom" => self
                .custom_url_template
                .replace("{user}", &user_enc)
                .replace("{password}", &pass_enc)
                .replace("{host}", host)
                .replace("{port}", &port.to_string())
                .replace("{channel}", &channel.to_string())
                .replace("{subtype}", &subtype.to_string()),
            _ => {
                // Default: Dahua / CP Plus / Amcrest XVR/NVR standard
                format!(
                    "rtsp://{}:{}@{}:{}/cam/realmonitor?channel={}&subtype={}",
                    user_enc, pass_enc, host, port, channel, subtype
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
    expand_tilde("~/.rtsp_viewer/config.json")
}

pub fn locate_env_file() -> PathBuf {
    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join(".env");
        if candidate.exists() {
            return candidate;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let direct = exe_dir.join(".env");
            if direct.exists() {
                return direct;
            }
            if let Some(parent) = exe_dir.parent().and_then(|p| p.parent()) {
                let root_env = parent.join(".env");
                if root_env.exists() {
                    return root_env;
                }
            }
        }
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".env")
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

pub fn unescape_env_val(val: &str) -> String {
    let mut out = String::with_capacity(val.len());
    let mut chars = val.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next_ch) = chars.next() {
                out.push(next_ch);
            } else {
                out.push('\\');
            }
        } else {
            out.push(ch);
        }
    }
    out
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
            let key = k.trim().to_string();
            let mut val = v.trim().to_string();
            if val.starts_with('"') && val.ends_with('"') && val.len() >= 2 {
                val = unescape_env_val(&val[1..val.len() - 1]);
            } else if val.starts_with('\'') && val.ends_with('\'') && val.len() >= 2 {
                val = val[1..val.len() - 1].to_string();
            }
            map.insert(key, val);
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
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    file.write_all(content.as_bytes())?;
    file.sync_all()
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
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
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
                let rows = (n + cols - 1) / cols;
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
    fn test_dahua_p2p_and_hikvision_url_generation() {
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
    }

    #[test]
    fn test_dynamic_channel_normalization_and_swapping() {
        let mut app_cfg = AppConfig::default();
        app_cfg.num_cameras = 12;
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
