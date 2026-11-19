use crate::auth_gateway::{AuthCircuitBreaker, AuthGateway, CanaryProbeOutcome};
use crate::config::{
    format_host_for_url, redact_secret_in_text, resolve_trusted_binary, rtsp_timeout_ffmpeg_arg,
    validate_custom_rtsp_url, validate_nvr_host, NvrEnvConfig,
};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream as StdTcpStream, ToSocketAddrs};
use std::os::unix::io::AsRawFd;
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const SD_WIDTH: u32 = 480;
pub const SD_HEIGHT: u32 = 270;
pub const HD_WIDTH: u32 = 1280;
pub const HD_HEIGHT: u32 = 720;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamStatus {
    Connecting = 0,
    Live = 1,
    Reconnecting = 2,
    Paused = 3,
    Error = 4,
    AuthError = 5,
}

impl StreamStatus {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Connecting,
            1 => Self::Live,
            2 => Self::Reconnecting,
            3 => Self::Paused,
            5 => Self::AuthError,
            _ => Self::Error,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Connecting => "Connecting",
            Self::Live => "Live",
            Self::Reconnecting => "Reconnecting",
            Self::Paused => "Paused",
            Self::Error => "Offline",
            Self::AuthError => "Auth Failed (401)",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FrameBuffer {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub subtype: u8,
}

pub struct FrameSlot {
    pub channel: usize,
    pub buffer: Mutex<FrameBuffer>,
    pub frame_seq: AtomicU64,
    pub status: AtomicU8,
    pub acl_denied: AtomicBool,
    pub target_subtype: AtomicU8,
    pub active_subtype: AtomicU8,
    pub auto_promoted_hd: AtomicBool,
    pub paused: AtomicBool,
    pub background_eco: AtomicBool,
    pub force_resync: AtomicBool,
    pub stop_flag: AtomicBool,
    pub active_child: Mutex<Option<Child>>,
    pub wake_pair: (Mutex<bool>, Condvar),
    pub last_frame_epoch_ms: AtomicU64,
    pub connect_start_epoch_ms: AtomicU64,
    pub fps_x10: AtomicU32,
    pub drained_burst_frames: AtomicU64,
    pub reconnect_count: AtomicU32,
}

impl FrameSlot {
    pub fn new(channel: usize, default_subtype: u8) -> Arc<Self> {
        let (w, h) = dimensions_for_subtype(default_subtype);
        Arc::new(Self {
            channel,
            buffer: Mutex::new(FrameBuffer {
                rgba: Vec::with_capacity((w * h * 4) as usize),
                width: w,
                height: h,
                subtype: default_subtype,
            }),
            frame_seq: AtomicU64::new(0),
            status: AtomicU8::new(StreamStatus::Connecting as u8),
            acl_denied: AtomicBool::new(false),
            target_subtype: AtomicU8::new(default_subtype),
            active_subtype: AtomicU8::new(default_subtype),
            auto_promoted_hd: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            background_eco: AtomicBool::new(false),
            force_resync: AtomicBool::new(false),
            stop_flag: AtomicBool::new(false),
            active_child: Mutex::new(None),
            wake_pair: (Mutex::new(false), Condvar::new()),
            last_frame_epoch_ms: AtomicU64::new(0),
            connect_start_epoch_ms: AtomicU64::new(now_epoch_ms()),
            fps_x10: AtomicU32::new(0),
            drained_burst_frames: AtomicU64::new(0),
            reconnect_count: AtomicU32::new(0),
        })
    }

    pub fn status(&self) -> StreamStatus {
        StreamStatus::from_u8(self.status.load(Ordering::Acquire))
    }

    #[allow(dead_code)]
    pub fn status_label(&self) -> &'static str {
        if self.status() == StreamStatus::AuthError && self.acl_denied.load(Ordering::Relaxed) {
            "Access Denied (403)"
        } else {
            self.status().label()
        }
    }

    pub fn set_status(&self, st: StreamStatus) {
        self.status.store(st as u8, Ordering::Release);
    }

    pub fn notify_wake(&self) {
        let (lock, cvar) = &self.wake_pair;
        if let Ok(mut woken) = lock.lock() {
            *woken = true;
            cvar.notify_all();
        }
    }

    pub fn sleep_interruptible(&self, dur: Duration) {
        if dur.is_zero() || self.stop_flag.load(Ordering::Acquire) {
            return;
        }
        let (lock, cvar) = &self.wake_pair;
        let Ok(mut woken) = lock.lock() else {
            return;
        };
        if self.stop_flag.load(Ordering::Acquire) {
            return;
        }
        if *woken {
            *woken = false;
            return;
        }
        if let Ok((mut guard, _)) = cvar.wait_timeout_while(woken, dur, |w| {
            !*w && !self.stop_flag.load(Ordering::Acquire)
        }) {
            if !self.stop_flag.load(Ordering::Acquire) {
                *guard = false;
            }
        }
    }

    pub fn request_subtype(&self, subtype: u8) {
        let clamped = if subtype == 0 { 0 } else { 1 };
        let prev = self.target_subtype.swap(clamped, Ordering::AcqRel);
        if prev != clamped {
            self.force_resync.store(true, Ordering::Release);
            self.kill_active_child();
            self.notify_wake();
        }
    }

    pub fn set_manual_subtype(&self, subtype: u8) {
        self.auto_promoted_hd.store(false, Ordering::Release);
        self.request_subtype(subtype);
    }

    pub fn trigger_resync(&self) {
        self.acl_denied.store(false, Ordering::Release);
        self.force_resync.store(true, Ordering::Release);
        self.kill_active_child();
        self.notify_wake();
    }

    pub fn set_paused(&self, pause: bool) {
        let was_paused = self.paused.swap(pause, Ordering::AcqRel);
        if pause && !was_paused {
            self.set_status(StreamStatus::Paused);
            self.fps_x10.store(0, Ordering::Release);
            self.kill_active_child();
            self.notify_wake();
        } else if !pause && was_paused {
            self.set_status(StreamStatus::Connecting);
            self.force_resync.store(true, Ordering::Release);
            self.notify_wake();
        }
    }

    pub fn has_active_child(&self) -> bool {
        self.active_child
            .lock()
            .map(|guard| guard.is_some())
            .unwrap_or(false)
    }

    pub fn kill_active_child(&self) {
        if let Ok(mut guard) = self.active_child.lock() {
            if let Some(ref mut child) = *guard {
                let _ = child.kill();
            }
        }
    }
}

pub fn dimensions_for_subtype(subtype: u8) -> (u32, u32) {
    if subtype == 0 {
        (HD_WIDTH, HD_HEIGHT)
    } else {
        (SD_WIDTH, SD_HEIGHT)
    }
}

pub fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn pipe_available_bytes(fd: i32) -> usize {
    let mut avail: libc::c_int = 0;
    let rc = unsafe { libc::ioctl(fd, libc::FIONREAD, &mut avail) };
    if rc == 0 && avail > 0 {
        avail as usize
    } else {
        0
    }
}

fn configure_child_lifecycle(cmd: &mut Command) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let parent_pid = unsafe { libc::getpid() };
        unsafe {
            cmd.pre_exec(move || {
                let _ = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                if libc::getppid() != parent_pid {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
    }
}

fn is_rtsp_auth_error(stderr_text: &str) -> bool {
    let lower = stderr_text.to_ascii_lowercase();
    lower.contains("401 unauthorized")
        || lower.contains("403 forbidden")
        || lower.contains("authorization failed")
}

fn is_rtsp_credential_rejection(stderr_text: &str) -> bool {
    let lower = stderr_text.to_ascii_lowercase();
    lower.contains("401 unauthorized") || lower.contains("authorization failed")
}

/// Continuously drains child stderr in a lightweight background thread so the 64 KiB OS pipe
/// can never fill up and deadlock FFmpeg before Frame 1, while retaining up to 2 KiB for
/// `401 Unauthorized` / `403 Forbidden` inspection (`SEC-13`).
fn spawn_stderr_drain(mut stderr: ChildStderr) -> (Arc<Mutex<Vec<u8>>>, JoinHandle<()>) {
    let captured = Arc::new(Mutex::new(Vec::with_capacity(512)));
    let cap_ref = Arc::clone(&captured);
    let handle = thread::Builder::new()
        .name("ffmpeg-stderr-drain".to_string())
        .spawn(move || {
            let mut scratch = [0u8; 1024];
            loop {
                match stderr.read(&mut scratch) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut buf) = cap_ref.lock() {
                            if buf.len() < 2048 {
                                let space = 2048 - buf.len();
                                buf.extend_from_slice(&scratch[..n.min(space)]);
                            }
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })
        .expect("Failed to spawn stderr drain thread");
    (captured, handle)
}

/// Builds a credential-free loopback RTSP URL via `AuthGateway` (`SEC-01`).
/// Supports Dahua, Hikvision, and Custom RTSP templates (including custom per-camera hosts/ports).
fn mint_live_gateway_url(
    auth_gw: &AuthGateway,
    cfg: &NvrEnvConfig,
    channel: usize,
    subtype: u8,
) -> Result<String, String> {
    let mut eff_cfg = cfg.clone();
    let sub = if subtype == 0 { 0 } else { 1 };

    if cfg.brand.eq_ignore_ascii_case("custom")
        && !cfg.is_cloud_p2p_active()
        && !cfg.is_tailscale_active()
    {
        let host_fmt = format_host_for_url(cfg.active_host());
        let candidate = cfg
            .custom_url_template
            .replace("{user}", "__u__")
            .replace("{password}", "__p__")
            .replace("{host}", &host_fmt)
            .replace("{port}", &cfg.port.to_string())
            .replace("{channel}", &channel.to_string())
            .replace("{subtype}", &sub.to_string());
        if let Ok(parsed) = validate_custom_rtsp_url(&candidate) {
            if let Some(h) = parsed.host_str() {
                eff_cfg.host = h.to_string();
            }
            if !cfg.custom_url_template.contains("{port}") {
                eff_cfg.port = parsed.port().unwrap_or(554);
            } else if let Some(p) = parsed.port() {
                eff_cfg.port = p;
            }
        }
    }

    let rtsp_path = eff_cfg.build_live_rtsp_path(channel, sub);
    auth_gw.mint_rtsp_url(&eff_cfg, &rtsp_path, None)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CanaryGateState {
    Pending,
    Ready,
    AuthFailed,
}

fn sleep_watchdog_stop(stop_pair: &(Mutex<bool>, Condvar), dur: Duration) -> bool {
    let (lock, cvar) = stop_pair;
    let Ok(stopped) = lock.lock() else {
        return true;
    };
    if *stopped {
        return true;
    }
    match cvar.wait_timeout_while(stopped, dur, |s| !*s) {
        Ok((guard, _)) => *guard,
        Err(_) => true,
    }
}

pub struct StreamManager {
    env_cfg: Arc<RwLock<NvrEnvConfig>>,
    p2p_ready: Arc<AtomicBool>,
    auth_gw: Arc<AuthGateway>,
    auth_lockout_guard: Arc<AtomicBool>,
    last_recovery_probe_ms: Arc<AtomicU64>,
    canary_gate: Arc<(Mutex<CanaryGateState>, Condvar)>,
    canary_handle: Option<JoinHandle<()>>,
    slots: BTreeMap<usize, Arc<FrameSlot>>,
    workers: Vec<JoinHandle<()>>,
    watchdog_stop: Arc<(Mutex<bool>, Condvar)>,
    watchdog_handle: Option<JoinHandle<()>>,
}

impl StreamManager {
    pub fn new(
        env_cfg: NvrEnvConfig,
        p2p_ready: Arc<AtomicBool>,
        auth_gw: Arc<AuthGateway>,
    ) -> Self {
        let shared_env = Arc::new(RwLock::new(env_cfg));
        Self {
            env_cfg: shared_env,
            p2p_ready,
            auth_gw,
            auth_lockout_guard: Arc::new(AtomicBool::new(false)),
            last_recovery_probe_ms: Arc::new(AtomicU64::new(0)),
            canary_gate: Arc::new((Mutex::new(CanaryGateState::Ready), Condvar::new())),
            canary_handle: None,
            slots: BTreeMap::new(),
            workers: Vec::new(),
            watchdog_stop: Arc::new((Mutex::new(false), Condvar::new())),
            watchdog_handle: None,
        }
    }

    pub fn start_streams(&mut self, num_cameras: usize) {
        self.stop_all();
        if let Ok(mut stopped) = self.watchdog_stop.0.lock() {
            *stopped = false;
        }
        self.auth_lockout_guard.store(false, Ordering::Release);
        self.last_recovery_probe_ms.store(0, Ordering::Release);
        self.auth_gw.circuit_breaker().reset();

        let env_snapshot = self.env_cfg.read().unwrap().clone();
        let default_sub = env_snapshot.default_subtype;
        let ip_start = env_snapshot.ip_camera_start_channel.min(num_cameras + 1);

        let needs_canary = num_cameras > 1
            && (!env_snapshot.username.trim().is_empty()
                || !env_snapshot.password.trim().is_empty());
        let initial_gate = if needs_canary {
            CanaryGateState::Pending
        } else {
            CanaryGateState::Ready
        };
        self.canary_gate = Arc::new((Mutex::new(initial_gate), Condvar::new()));

        for ch in 1..=num_cameras {
            let slot = FrameSlot::new(ch, default_sub);
            self.slots.insert(ch, slot);
        }

        // Channel 1 Canary Auth Probe (`SEC-12`): verifies credentials with a single handshake
        // (~15ms on LAN) before waking Channels 2..=N, preventing 12-attempt NVR account lockouts.
        if needs_canary {
            let gate_ref = Arc::clone(&self.canary_gate);
            let stop_ref = Arc::clone(&self.watchdog_stop);
            let p2p_ref = Arc::clone(&self.p2p_ready);
            let gw_ref = Arc::clone(&self.auth_gw);
            let lockout_ref = Arc::clone(&self.auth_lockout_guard);
            let slots_for_canary: Vec<Arc<FrameSlot>> = self.slots.values().cloned().collect();
            let canary_cfg = env_snapshot.clone();

            self.canary_handle = Some(
                thread::Builder::new()
                    .name("rtsp-canary".to_string())
                    .spawn(move || {
                        if canary_cfg.is_cloud_p2p_active() {
                            while !p2p_ref.load(Ordering::Acquire) {
                                if sleep_watchdog_stop(&stop_ref, Duration::from_millis(80)) {
                                    if let Ok(mut g) = gate_ref.0.lock() {
                                        *g = CanaryGateState::Ready;
                                        gate_ref.1.notify_all();
                                    }
                                    return;
                                }
                            }
                        }

                        let next_state = match gw_ref.probe_canary_auth_detailed(&canary_cfg) {
                            CanaryProbeOutcome::AuthRejected(_) => {
                                lockout_ref.store(true, Ordering::Release);
                                for s in &slots_for_canary {
                                    s.acl_denied.store(false, Ordering::Release);
                                    s.set_status(StreamStatus::AuthError);
                                }
                                CanaryGateState::AuthFailed
                            }
                            CanaryProbeOutcome::AuthOk | CanaryProbeOutcome::Inconclusive => {
                                // Either credentials succeeded (`200 OK`) or NVR had a transient
                                // network error without `401` rejection; let workers proceed normally.
                                CanaryGateState::Ready
                            }
                        };

                        if let Ok(mut g) = gate_ref.0.lock() {
                            *g = next_state;
                            gate_ref.1.notify_all();
                        }
                        for s in &slots_for_canary {
                            s.notify_wake();
                        }
                    })
                    .expect("Failed to spawn canary auth thread"),
            );
        }

        // Launch IP camera channels (and channel 9+) first so they begin RTSP PLAY
        // handshakes immediately while BNC channels 1..8 lock in <900ms.
        let mut launch_order: Vec<usize> = Vec::with_capacity(num_cameras);
        let priority_start = ip_start.saturating_sub(1).max(1);
        for ch in priority_start..=num_cameras {
            launch_order.push(ch);
        }
        for ch in 1..priority_start {
            launch_order.push(ch);
        }

        for ch in launch_order {
            if let Some(slot) = self.slots.get(&ch).cloned() {
                let env_ref = Arc::clone(&self.env_cfg);
                let p2p_ready_ref = Arc::clone(&self.p2p_ready);
                let auth_gw_ref = Arc::clone(&self.auth_gw);
                let auth_guard_ref = Arc::clone(&self.auth_lockout_guard);
                let probe_ms_ref = Arc::clone(&self.last_recovery_probe_ms);
                let canary_ref = Arc::clone(&self.canary_gate);
                let handle = thread::Builder::new()
                    .name(format!("rtsp-cam-{}", ch))
                    .spawn(move || {
                        camera_worker_loop(
                            slot,
                            env_ref,
                            p2p_ready_ref,
                            auth_gw_ref,
                            auth_guard_ref,
                            probe_ms_ref,
                            canary_ref,
                        )
                    })
                    .expect("Failed to spawn camera worker thread");
                self.workers.push(handle);
            }
        }

        // Spawn lightweight socket stall watchdog thread
        let slots_vec: Vec<Arc<FrameSlot>> = self.slots.values().cloned().collect();
        let wd_stop = Arc::clone(&self.watchdog_stop);
        let wd_env = Arc::clone(&self.env_cfg);
        let wd_p2p = Arc::clone(&self.p2p_ready);
        self.watchdog_handle = Some(
            thread::Builder::new()
                .name("rtsp-watchdog".to_string())
                .spawn(move || watchdog_loop(slots_vec, wd_stop, wd_env, wd_p2p))
                .expect("Failed to spawn watchdog thread"),
        );
    }

    pub fn update_env_and_restart(&mut self, new_env: NvrEnvConfig) {
        let num_cams = new_env.num_cameras.clamp(1, 32);
        {
            let mut guard = self.env_cfg.write().unwrap();
            *guard = new_env;
        }
        self.start_streams(num_cams);
    }

    pub fn slot(&self, channel: usize) -> Option<Arc<FrameSlot>> {
        self.slots.get(&channel).cloned()
    }

    pub fn resync_all(&self) {
        self.auth_lockout_guard.store(false, Ordering::Release);
        self.last_recovery_probe_ms.store(0, Ordering::Release);
        self.auth_gw.circuit_breaker().reset();
        if let Ok(mut g) = self.canary_gate.0.lock() {
            *g = CanaryGateState::Ready;
            self.canary_gate.1.notify_all();
        }
        for slot in self.slots.values() {
            slot.trigger_resync();
        }
    }

    pub fn set_all_paused(&self, paused: bool) {
        for slot in self.slots.values() {
            slot.set_paused(paused);
        }
    }

    pub fn set_all_subtype(&self, subtype: u8) {
        for slot in self.slots.values() {
            slot.set_manual_subtype(subtype);
        }
    }

    pub fn set_fullscreen_focus(&self, focused_channel: Option<usize>, auto_hd: bool) {
        for (&ch, slot) in &self.slots {
            match focused_channel {
                Some(target_ch) if ch == target_ch => {
                    slot.background_eco.store(false, Ordering::Release);
                    if auto_hd {
                        if slot.target_subtype.load(Ordering::Acquire) != 0 {
                            slot.auto_promoted_hd.store(true, Ordering::Release);
                            slot.request_subtype(0);
                        }
                    } else if slot.auto_promoted_hd.swap(false, Ordering::AcqRel) {
                        slot.request_subtype(1);
                    }
                }
                Some(_) => {
                    // Background camera while another camera is fullscreen
                    slot.background_eco.store(true, Ordering::Release);
                    if slot.auto_promoted_hd.swap(false, Ordering::AcqRel) {
                        slot.request_subtype(1);
                    }
                }
                None => {
                    slot.background_eco.store(false, Ordering::Release);
                    if slot.auto_promoted_hd.swap(false, Ordering::AcqRel) {
                        slot.request_subtype(1);
                    }
                }
            }
        }
    }

    pub fn set_window_minimized_eco(&self, minimized: bool) {
        for slot in self.slots.values() {
            slot.background_eco.store(minimized, Ordering::Release);
        }
    }

    pub fn stop_all(&mut self) {
        if let Ok(mut stopped) = self.watchdog_stop.0.lock() {
            *stopped = true;
            self.watchdog_stop.1.notify_all();
        }
        if let Ok(mut g) = self.canary_gate.0.lock() {
            *g = CanaryGateState::Ready;
            self.canary_gate.1.notify_all();
        }
        for slot in self.slots.values() {
            slot.stop_flag.store(true, Ordering::Release);
            slot.kill_active_child();
            slot.notify_wake();
        }
        if let Some(h) = self.canary_handle.take() {
            let _ = h.join();
        }
        if let Some(h) = self.watchdog_handle.take() {
            let _ = h.join();
        }
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
        self.slots.clear();
    }
}

impl Drop for StreamManager {
    fn drop(&mut self) {
        self.stop_all();
    }
}

fn watchdog_loop(
    slots: Vec<Arc<FrameSlot>>,
    stop_pair: Arc<(Mutex<bool>, Condvar)>,
    env_cfg: Arc<RwLock<NvrEnvConfig>>,
    p2p_ready: Arc<AtomicBool>,
) {
    while !sleep_watchdog_stop(&stop_pair, Duration::from_millis(500)) {
        let is_cloud = env_cfg
            .read()
            .map(|c| c.is_cloud_p2p_active())
            .unwrap_or(false);
        if is_cloud && !p2p_ready.load(Ordering::Acquire) {
            continue;
        }
        let live_stall_ms = if is_cloud { 12000 } else { 4500 };
        let connect_timeout_ms = if is_cloud { 22000 } else { 7500 };

        let now = now_epoch_ms();
        for slot in &slots {
            if slot.paused.load(Ordering::Acquire) || slot.stop_flag.load(Ordering::Acquire) {
                continue;
            }
            if !slot.has_active_child() {
                continue;
            }
            let st = slot.status();
            let last_frame = slot.last_frame_epoch_ms.load(Ordering::Acquire);
            let conn_start = slot.connect_start_epoch_ms.load(Ordering::Acquire);

            let live_stalled = st == StreamStatus::Live
                && last_frame > 0
                && now.saturating_sub(last_frame) > live_stall_ms;
            let connect_stalled = (st == StreamStatus::Connecting
                || st == StreamStatus::Reconnecting)
                && conn_start > 0
                && now.saturating_sub(conn_start) > connect_timeout_ms;

            if live_stalled || connect_stalled {
                slot.kill_active_child();
            }
        }
    }
}

fn camera_worker_loop(
    slot: Arc<FrameSlot>,
    env_cfg: Arc<RwLock<NvrEnvConfig>>,
    p2p_ready: Arc<AtomicBool>,
    auth_gw: Arc<AuthGateway>,
    auth_lockout_guard: Arc<AtomicBool>,
    last_recovery_probe_ms: Arc<AtomicU64>,
    canary_gate: Arc<(Mutex<CanaryGateState>, Condvar)>,
) {
    let ffmpeg_bin = resolve_trusted_binary("ffmpeg");
    let rtsp_timeout_flag = rtsp_timeout_ffmpeg_arg();
    let circuit_breaker = auth_gw.circuit_breaker();
    let mut back_buf: Vec<u8> = Vec::new();
    let mut consecutive_failures: u32 = 0;

    while !slot.stop_flag.load(Ordering::Acquire) {
        if slot.paused.load(Ordering::Acquire) {
            slot.set_status(StreamStatus::Paused);
            slot.sleep_interruptible(Duration::from_millis(120));
            continue;
        }

        // Wait for initial Channel 1 Canary Auth Probe before connecting in parallel (`SEC-12`)
        {
            let gate_state = canary_gate.0.lock().map(|g| *g).unwrap_or(CanaryGateState::Ready);
            if gate_state == CanaryGateState::Pending {
                slot.set_status(StreamStatus::Connecting);
                if let Ok(guard) = canary_gate.0.lock() {
                    let _ = canary_gate.1.wait_timeout_while(
                        guard,
                        Duration::from_millis(150),
                        |st| *st == CanaryGateState::Pending && !slot.stop_flag.load(Ordering::Acquire),
                    );
                }
                continue;
            }
        }

        if (auth_lockout_guard.load(Ordering::Acquire) || circuit_breaker.is_tripped())
            && !slot.force_resync.load(Ordering::Acquire)
        {
            slot.acl_denied.store(false, Ordering::Release);
            slot.set_status(StreamStatus::AuthError);
            slot.sleep_interruptible(Duration::from_secs(30));
            if slot.stop_flag.load(Ordering::Acquire) {
                break;
            }
            if slot.paused.load(Ordering::Acquire) || slot.force_resync.load(Ordering::Acquire) {
                continue;
            }
            if auth_lockout_guard.load(Ordering::Acquire) || circuit_breaker.is_tripped() {
                // Allow ANY unpaused worker (CAS-gated once per 30s) to gently probe recovery
                // even when Channel 1 is paused, and ONLY clear lockout on explicit AuthOk!
                let now = now_epoch_ms();
                let prev = last_recovery_probe_ms.load(Ordering::Acquire);
                if now.saturating_sub(prev) >= 29_000
                    && last_recovery_probe_ms
                        .compare_exchange(prev, now, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    let cfg_probe = env_cfg.read().unwrap().clone();
                    match auth_gw.probe_canary_auth_detailed(&cfg_probe) {
                        CanaryProbeOutcome::AuthOk => {
                            circuit_breaker.reset();
                            auth_lockout_guard.store(false, Ordering::Release);
                        }
                        CanaryProbeOutcome::AuthRejected(_) | CanaryProbeOutcome::Inconclusive => {
                            auth_lockout_guard.store(true, Ordering::Release);
                            continue;
                        }
                    }
                } else {
                    continue;
                }
            }
        }

        // Clear force_resync BEFORE reading target_subtype so any concurrent request_subtype
        // during spawn is never overwritten
        slot.force_resync.store(false, Ordering::Release);
        let subtype = slot.target_subtype.load(Ordering::Acquire);
        let (width, height) = dimensions_for_subtype(subtype);
        let frame_bytes = (width * height * 4) as usize;
        if back_buf.len() != frame_bytes {
            back_buf.resize(frame_bytes, 0);
        }

        let cfg_snapshot = env_cfg.read().unwrap().clone();
        if cfg_snapshot.is_cloud_p2p_active() {
            if !p2p_ready.load(Ordering::Acquire) {
                if slot.frame_seq.load(Ordering::Acquire) == 0 {
                    slot.set_status(StreamStatus::Connecting);
                } else {
                    slot.set_status(StreamStatus::Reconnecting);
                }
                slot.sleep_interruptible(Duration::from_millis(120));
                continue;
            }
            if slot.frame_seq.load(Ordering::Acquire) == 0 && consecutive_failures == 0 {
                // Launch IP cameras (channel >= ip_camera_start_channel) first with minimal stagger
                // so their RTSP handshakes complete before BNC video streams saturate the relay
                let stagger_ms = if slot.channel >= cfg_snapshot.ip_camera_start_channel {
                    (slot.channel.saturating_sub(cfg_snapshot.ip_camera_start_channel) as u64) * 25
                } else {
                    90 + (slot.channel.saturating_sub(1) as u64) * 25
                };
                if stagger_ms > 0 {
                    slot.sleep_interruptible(Duration::from_millis(stagger_ms));
                    if slot.stop_flag.load(Ordering::Acquire) {
                        break;
                    }
                }
            }
        }

        let rtsp_url = match mint_live_gateway_url(&auth_gw, &cfg_snapshot, slot.channel, subtype) {
            Ok(url) => url,
            Err(_) => {
                if circuit_breaker.is_tripped() {
                    auth_lockout_guard.store(true, Ordering::Release);
                    slot.acl_denied.store(false, Ordering::Release);
                    slot.set_status(StreamStatus::AuthError);
                    slot.sleep_interruptible(Duration::from_secs(10));
                } else {
                    slot.set_status(StreamStatus::Error);
                    slot.sleep_interruptible(Duration::from_secs(2));
                }
                continue;
            }
        };

        slot.connect_start_epoch_ms
            .store(now_epoch_ms(), Ordering::Release);

        if slot.frame_seq.load(Ordering::Acquire) == 0 {
            slot.set_status(StreamStatus::Connecting);
        } else {
            slot.set_status(StreamStatus::Reconnecting);
        }

        // SD uses 32KB / 0us / fpsprobesize 0 so streams (including IP cams with silent pcm_alaw tracks)
        // never block waiting on audio packets; HD uses 500KB / 500ms.
        let (probe_size, analyze_dur) = if subtype == 0 {
            ("500000", "500000")
        } else {
            ("32768", "0")
        };

        let mut cmd = Command::new(&ffmpeg_bin);
        configure_child_lifecycle(&mut cmd);
        cmd.arg("-nostdin")
            .arg("-v")
            .arg("error")
            .arg("-protocol_whitelist")
            .arg("rtsp,tcp,rtp")
            .arg("-max_alloc")
            .arg("134217728")
            .arg("-rtsp_transport")
            .arg("tcp")
            .arg(rtsp_timeout_flag)
            .arg("5000000")
            .arg("-fflags")
            .arg("nobuffer+discardcorrupt")
            .arg("-flags")
            .arg("low_delay")
            .arg("-probesize")
            .arg(probe_size)
            .arg("-analyzeduration")
            .arg(analyze_dur)
            .arg("-fpsprobesize")
            .arg("0")
            .arg("-max_delay")
            .arg("0");

        // Consumer NVIDIA GPUs cap concurrent NVDEC sessions at 8; use hwaccel on HD main-stream,
        // while 12+ SD sub-streams decode in <1ms via CPU SIMD without hitting the 8-session NVDEC limit.
        if cfg_snapshot.hwaccel_mode.eq_ignore_ascii_case("auto") && subtype == 0 {
            cmd.arg("-hwaccel").arg("auto");
        }

        cmd.arg("-i")
            .arg(&rtsp_url)
            .arg("-an")
            .arg("-sn")
            .arg("-dn")
            .arg("-fps_mode")
            .arg("passthrough")
            .arg("-vf")
            .arg(format!("scale={}:{}:flags=fast_bilinear", width, height))
            .arg("-f")
            .arg("rawvideo")
            .arg("-pix_fmt")
            .arg("rgba")
            .arg("-")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => {
                slot.set_status(StreamStatus::Error);
                slot.sleep_interruptible(Duration::from_secs(2));
                continue;
            }
        };

        let Some(mut stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            continue;
        };
        let stderr_drain = child.stderr.take().map(spawn_stderr_drain);

        {
            if let Ok(mut guard) = slot.active_child.lock() {
                *guard = Some(child);
            }
        }

        // If a resync, pause, stop, or subtype change arrived while cmd.spawn() was executing,
        // abort this child immediately
        if slot.stop_flag.load(Ordering::Acquire)
            || slot.paused.load(Ordering::Acquire)
            || slot.force_resync.load(Ordering::Acquire)
            || slot.target_subtype.load(Ordering::Acquire) != subtype
        {
            let taken = slot.active_child.lock().ok().and_then(|mut g| g.take());
            if let Some(mut c) = taken {
                let _ = c.kill();
                let _ = c.wait();
            }
            if let Some((_, drain_h)) = stderr_drain {
                let _ = drain_h.join();
            }
            continue;
        }

        let pipe_fd = stdout.as_raw_fd();
        #[cfg(target_os = "linux")]
        unsafe {
            // Raise Linux anonymous pipe capacity from 64 KiB default to 1 MiB (~2 full SD RGBA frames)
            // so ioctl(FIONREAD) >= frame_bytes genuinely activates zero-lag burst frame draining.
            if libc::fcntl(pipe_fd, libc::F_SETPIPE_SZ, 1_048_576) < 0 {
                let _ = libc::fcntl(pipe_fd, libc::F_SETPIPE_SZ, 524_288);
            }
        }

        let mut fps_window_start = Instant::now();
        let mut fps_frames: u32 = 0;
        let mut eco_frame_counter: u64 = 0;
        let mut received_any_frame = false;

        'frame_loop: loop {
            if slot.stop_flag.load(Ordering::Acquire)
                || slot.paused.load(Ordering::Acquire)
                || slot.force_resync.load(Ordering::Acquire)
            {
                break 'frame_loop;
            }

            match stdout.read_exact(&mut back_buf) {
                Ok(()) => {
                    received_any_frame = true;
                    consecutive_failures = 0;
                    auth_lockout_guard.store(false, Ordering::Release);
                    slot.acl_denied.store(false, Ordering::Release);

                    // Zero-Lag Kernel Pipe Burst Drain:
                    // If 1 or more newer frames are already waiting in the OS pipe buffer,
                    // drain them immediately into back_buf so the UI only ever receives
                    // the newest real-time frame (0 buffered frames!).
                    let mut drained = 0u64;
                    while pipe_available_bytes(pipe_fd) >= frame_bytes && drained < 16 {
                        if stdout.read_exact(&mut back_buf).is_err() {
                            // Do NOT publish a partially overwritten/torn frame
                            break 'frame_loop;
                        }
                        drained += 1;
                    }
                    if drained > 0 {
                        slot.drained_burst_frames
                            .fetch_add(drained, Ordering::Relaxed);
                    }

                    let now_ms = now_epoch_ms();
                    slot.last_frame_epoch_ms.store(now_ms, Ordering::Release);
                    slot.active_subtype.store(subtype, Ordering::Release);
                    slot.set_status(StreamStatus::Live);

                    fps_frames += 1;
                    eco_frame_counter = eco_frame_counter.wrapping_add(1);
                    let elapsed = fps_window_start.elapsed().as_secs_f32();
                    if elapsed >= 1.0 {
                        let fps10 = ((fps_frames as f32 / elapsed) * 10.0).round() as u32;
                        slot.fps_x10.store(fps10, Ordering::Release);
                        fps_frames = 0;
                        fps_window_start = Instant::now();
                    }

                    // In background eco mode (e.g. minimized or another camera is Fullscreen),
                    // only publish 1 frame every 4 frames while still draining the socket at 100% wire speed!
                    let should_publish = if slot.background_eco.load(Ordering::Acquire) {
                        eco_frame_counter.is_multiple_of(4)
                    } else {
                        true
                    };

                    if should_publish {
                        if let Ok(mut guard) = slot.buffer.lock() {
                            guard.width = width;
                            guard.height = height;
                            guard.subtype = subtype;
                            if guard.rgba.len() != frame_bytes {
                                guard.rgba.resize(frame_bytes, 0);
                            }
                            std::mem::swap(&mut guard.rgba, &mut back_buf);
                        }
                        slot.frame_seq.fetch_add(1, Ordering::Release);
                    }
                }
                Err(_) => {
                    break 'frame_loop;
                }
            }
        }

        let exited_child = slot.active_child.lock().ok().and_then(|mut g| g.take());
        if let Some(mut c) = exited_child {
            let _ = c.kill();
            let _ = c.wait();
        }
        let stderr_bytes = if let Some((cap_buf, drain_h)) = stderr_drain {
            let _ = drain_h.join();
            cap_buf.lock().map(|b| b.clone()).unwrap_or_default()
        } else {
            Vec::new()
        };

        if slot.stop_flag.load(Ordering::Acquire) {
            break;
        }
        if slot.paused.load(Ordering::Acquire) {
            slot.set_status(StreamStatus::Paused);
            continue;
        }
        if slot.force_resync.load(Ordering::Acquire) {
            // Immediate reconnect on user quality switch or Re-Sync button
            continue;
        }

        let mut auth_rejected = circuit_breaker.is_tripped();
        let mut per_channel_acl_denied = false;
        if !received_any_frame {
            consecutive_failures = consecutive_failures.saturating_add(1);
            if !stderr_bytes.is_empty() {
                let err_text = String::from_utf8_lossy(&stderr_bytes);
                if is_rtsp_credential_rejection(&err_text) {
                    auth_rejected = true;
                    circuit_breaker.trip();
                } else if is_rtsp_auth_error(&err_text) {
                    // 403 Forbidden: per-channel ACL restriction, do NOT trip global circuit_breaker!
                    per_channel_acl_denied = true;
                }
            }
        }

        slot.reconnect_count.fetch_add(1, Ordering::Relaxed);
        slot.fps_x10.store(0, Ordering::Release);

        let backoff_ms = if auth_rejected {
            auth_lockout_guard.store(true, Ordering::Release);
            slot.acl_denied.store(false, Ordering::Release);
            slot.set_status(StreamStatus::AuthError);
            match consecutive_failures {
                0 | 1 => 10_000,
                2 => 20_000,
                _ => 30_000,
            }
        } else if per_channel_acl_denied {
            slot.acl_denied.store(true, Ordering::Release);
            slot.set_status(StreamStatus::AuthError);
            match consecutive_failures {
                0 | 1 => 10_000,
                2 => 20_000,
                _ => 30_000,
            }
        } else {
            if consecutive_failures >= 3 {
                slot.set_status(StreamStatus::Error);
            } else {
                slot.set_status(StreamStatus::Reconnecting);
            }
            match consecutive_failures {
                0 | 1 => 300,
                2 => 800,
                3 => 1_500,
                4 => 3_000,
                5 => 6_000,
                6 => 12_000,
                7 => 20_000,
                _ => 30_000,
            }
        };

        slot.sleep_interruptible(Duration::from_millis(backoff_ms));
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct ChannelScanReport {
    pub channels: Vec<usize>,
    pub auth_rejected: bool,
    pub error: Option<String>,
}

pub(crate) fn connect_rtsp_probe_socket(
    env_cfg: &NvrEnvConfig,
    timeout: Duration,
) -> Result<(StdTcpStream, String, u16), String> {
    let mut eff_cfg = env_cfg.clone();
    if env_cfg.brand.eq_ignore_ascii_case("custom")
        && !env_cfg.is_cloud_p2p_active()
        && !env_cfg.is_tailscale_active()
    {
        let host_fmt = format_host_for_url(env_cfg.active_host());
        let candidate = env_cfg
            .custom_url_template
            .replace("{user}", "__u__")
            .replace("{password}", "__p__")
            .replace("{host}", &host_fmt)
            .replace("{port}", &env_cfg.port.to_string())
            .replace("{channel}", "1")
            .replace("{subtype}", "1");
        if let Ok(parsed) = validate_custom_rtsp_url(&candidate) {
            if let Some(h) = parsed.host_str() {
                eff_cfg.host = h.to_string();
            }
            if !env_cfg.custom_url_template.contains("{port}") {
                eff_cfg.port = parsed.port().unwrap_or(554);
            } else if let Some(p) = parsed.port() {
                eff_cfg.port = p;
            }
        }
    }

    let raw_active = eff_cfg.active_host().trim().to_string();
    let formatted_host = validate_nvr_host(&raw_active)
        .map_err(|e| redact_secret_in_text(&e, &env_cfg.password))?;
    let port = eff_cfg.active_port();
    if port == 0 {
        return Err("active RTSP port is 0 (Cloud P2P tunnel not connected)".to_string());
    }

    let connect_budget = timeout.max(Duration::from_millis(100));
    let raw_unbracketed = raw_active
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(&raw_active);

    if !raw_unbracketed.contains('%') {
        if let Ok(ip) = raw_unbracketed.parse::<IpAddr>() {
            return StdTcpStream::connect_timeout(&SocketAddr::new(ip, port), connect_budget)
                .map(|s| (s, formatted_host.clone(), port))
                .map_err(|e| {
                    redact_secret_in_text(
                        &format!("{} ({}:{})", e, formatted_host, port),
                        &env_cfg.password,
                    )
                });
        }
    }

    let addr_str = if raw_unbracketed.contains('%') {
        format!("[{}]:{}", raw_unbracketed, port)
    } else {
        format!("{}:{}", formatted_host, port)
    };
    let lookup_str = addr_str.clone();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    thread::Builder::new()
        .name("nvr-rtsp-dns".to_string())
        .spawn(move || {
            let resolved = lookup_str
                .to_socket_addrs()
                .map(|it| it.collect::<Vec<SocketAddr>>());
            let _ = tx.send(resolved);
        })
        .map_err(|e| format!("DNS thread spawn failed: {}", e))?;

    let addrs = match rx.recv_timeout(connect_budget) {
        Ok(Ok(list)) if !list.is_empty() => list,
        Ok(Ok(_)) => {
            return Err(redact_secret_in_text(
                &format!("could not resolve host {}", formatted_host),
                &env_cfg.password,
            ));
        }
        Ok(Err(e)) => {
            return Err(redact_secret_in_text(
                &format!("DNS lookup failed for {}: {}", formatted_host, e),
                &env_cfg.password,
            ));
        }
        Err(_) => {
            return Err(redact_secret_in_text(
                &format!("DNS lookup timed out for {}", formatted_host),
                &env_cfg.password,
            ));
        }
    };

    let deadline = Instant::now() + connect_budget;
    let mut last_err = String::from("connection timed out");
    for (idx, addr) in addrs.iter().enumerate() {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let rem = deadline - now;
        let rem_count = (addrs.len() - idx) as u32;
        let per_addr = if rem_count > 1 {
            (rem / rem_count).max(Duration::from_millis(100)).min(rem)
        } else {
            rem
        };
        match StdTcpStream::connect_timeout(addr, per_addr) {
            Ok(s) => return Ok((s, formatted_host, port)),
            Err(e) => last_err = e.to_string(),
        }
    }

    Err(redact_secret_in_text(
        &format!("{} ({}:{})", last_err, formatted_host, port),
        &env_cfg.password,
    ))
}

pub(crate) fn probe_rtsp_options_alive(
    env_cfg: &NvrEnvConfig,
    timeout: Duration,
) -> Result<(), String> {
    let (mut sock, host, port) = connect_rtsp_probe_socket(env_cfg, timeout)?;
    let _ = sock.set_nodelay(true);
    let _ = sock.set_write_timeout(Some(timeout));
    let _ = sock.set_read_timeout(Some(timeout));

    let req = b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nUser-Agent: NVRCommandCenter/3.0\r\n\r\n";
    sock.write_all(req).map_err(|e| {
        redact_secret_in_text(
            &format!("RTSP write failed to {}:{}: {}", host, port, e),
            &env_cfg.password,
        )
    })?;

    let mut buf = [0u8; 256];
    let n = sock.read(&mut buf).map_err(|e| {
        redact_secret_in_text(
            &format!("no RTSP response from {}:{}: {}", host, port, e),
            &env_cfg.password,
        )
    })?;
    if n == 0 {
        return Err(redact_secret_in_text(
            &format!("peer {}:{} closed RTSP connection immediately", host, port),
            &env_cfg.password,
        ));
    }
    let head = String::from_utf8_lossy(&buf[..n]);
    let first_line = head.lines().next().unwrap_or("").trim();
    if !first_line.to_ascii_uppercase().starts_with("RTSP/") {
        return Err(redact_secret_in_text(
            &format!("{}:{} did not speak RTSP", host, port),
            &env_cfg.password,
        ));
    }
    if first_line.split_whitespace().nth(1) == Some("503") {
        return Err(redact_secret_in_text(
            &format!("RTSP service unavailable (503) at {}:{}", host, port),
            &env_cfg.password,
        ));
    }
    Ok(())
}

/// Bounded-concurrency cancellable NVR Channel Auto-Scanner.
/// Fails closed with an isolated `AuthGateway` + `AuthCircuitBreaker` so scanning unsaved
/// Settings credentials never touches or trips live camera streams.
#[allow(dead_code)]
pub fn scan_active_nvr_channels_cancellable(
    env_cfg: &NvrEnvConfig,
    max_channels: usize,
    cancel_flag: Option<&AtomicBool>,
) -> ChannelScanReport {
    let is_cancelled = || cancel_flag.is_some_and(|f| f.load(Ordering::Acquire));
    if is_cancelled() {
        return ChannelScanReport {
            channels: Vec::new(),
            auth_rejected: false,
            error: Some("Channel scan cancelled".to_string()),
        };
    }

    let max_ch = max_channels.clamp(1, 32);
    let is_cloud = env_cfg.is_cloud_p2p_active();
    let tcp_timeout = Duration::from_millis(if is_cloud { 4_500 } else { 2_500 });

    if let Err(msg) = connect_rtsp_probe_socket(env_cfg, tcp_timeout) {
        let safe_msg = redact_secret_in_text(&msg, &env_cfg.password);
        return ChannelScanReport {
            channels: Vec::new(),
            auth_rejected: false,
            error: Some(format!("NVR RTSP unreachable: {}", safe_msg)),
        };
    }

    if is_cancelled() {
        return ChannelScanReport {
            channels: Vec::new(),
            auth_rejected: false,
            error: Some("Channel scan cancelled".to_string()),
        };
    }

    let Ok(scan_gw) = AuthGateway::start(AuthCircuitBreaker::new()) else {
        return ChannelScanReport {
            channels: Vec::new(),
            auth_rejected: false,
            error: Some("Failed to start isolated local AuthGateway for scan".to_string()),
        };
    };

    // Probe Channel 1 auth first so a mistyped password never triggers 16 failed login attempts,
    // and verify RTSP handshake reachability before spawning ffprobe workers.
    match scan_gw.probe_canary_auth_detailed(env_cfg) {
        CanaryProbeOutcome::AuthRejected(_) => {
            return ChannelScanReport {
                channels: Vec::new(),
                auth_rejected: true,
                error: Some(
                    "Authentication rejected (401) - check NVR username & password".to_string(),
                ),
            };
        }
        CanaryProbeOutcome::Inconclusive => {
            if is_cancelled() {
                return ChannelScanReport {
                    channels: Vec::new(),
                    auth_rejected: false,
                    error: Some("Channel scan cancelled".to_string()),
                };
            }
            if let Err(msg) = probe_rtsp_options_alive(env_cfg, Duration::from_millis(1_800)) {
                let safe_msg = redact_secret_in_text(&msg, &env_cfg.password);
                return ChannelScanReport {
                    channels: Vec::new(),
                    auth_rejected: false,
                    error: Some(format!("NVR RTSP unreachable: {}", safe_msg)),
                };
            }
        }
        CanaryProbeOutcome::AuthOk => {}
    }

    if is_cancelled() {
        return ChannelScanReport {
            channels: Vec::new(),
            auth_rejected: false,
            error: Some("Channel scan cancelled".to_string()),
        };
    }

    let ffprobe_bin = Arc::new(resolve_trusted_binary("ffprobe"));
    let rtsp_timeout_flag = rtsp_timeout_ffmpeg_arg();
    let rtsp_timeout_us = if is_cloud { "5000000" } else { "2500000" };
    let deadline_ms = if is_cloud { 8000 } else { 3200 };

    let next_ch = Arc::new(AtomicUsize::new(1));
    let worker_count = max_ch.min(4);
    let mut active = Vec::new();

    thread::scope(|s| {
        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let next_ch_ref = Arc::clone(&next_ch);
            let probe_bin = Arc::clone(&ffprobe_bin);
            let gw_ref = Arc::clone(&scan_gw);
            let cfg_clone = env_cfg.clone();

            handles.push(s.spawn(move || {
                let mut found = Vec::new();
                loop {
                    if cancel_flag.is_some_and(|f| f.load(Ordering::Acquire)) {
                        break;
                    }
                    let ch = next_ch_ref.fetch_add(1, Ordering::Relaxed);
                    if ch > max_ch || gw_ref.circuit_breaker().is_tripped() {
                        break;
                    }
                    let Ok(url) = mint_live_gateway_url(&gw_ref, &cfg_clone, ch, 1) else {
                        continue;
                    };

                    let mut cmd = Command::new(probe_bin.as_ref());
                    configure_child_lifecycle(&mut cmd);
                    cmd.arg("-v")
                        .arg("error")
                        .arg("-protocol_whitelist")
                        .arg("rtsp,tcp,rtp")
                        .arg("-max_alloc")
                        .arg("67108864")
                        .arg("-rtsp_transport")
                        .arg("tcp")
                        .arg(rtsp_timeout_flag)
                        .arg(rtsp_timeout_us)
                        .arg("-probesize")
                        .arg("32768")
                        .arg("-analyzeduration")
                        .arg("0")
                        .arg("-fpsprobesize")
                        .arg("0")
                        .arg("-select_streams")
                        .arg("v:0")
                        .arg("-show_entries")
                        .arg("stream=codec_name")
                        .arg("-of")
                        .arg("default=noprint_wrappers=1:nokey=1")
                        .arg("-i")
                        .arg(&url)
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null());

                    let Ok(mut child) = cmd.spawn() else {
                        continue;
                    };
                    let Some(stdout) = child.stdout.take() else {
                        let _ = child.kill();
                        let _ = child.wait();
                        continue;
                    };

                    let deadline = Instant::now() + Duration::from_millis(deadline_ms);
                    let status = loop {
                        if cancel_flag.is_some_and(|f| f.load(Ordering::Acquire)) {
                            let _ = child.kill();
                            let _ = child.wait();
                            return found;
                        }
                        match child.try_wait() {
                            Ok(Some(st)) => break Some(st),
                            Ok(None) => {
                                if Instant::now() >= deadline {
                                    let _ = child.kill();
                                    let _ = child.wait();
                                    break None;
                                }
                                thread::sleep(Duration::from_millis(25));
                            }
                            Err(_) => {
                                let _ = child.kill();
                                let _ = child.wait();
                                break None;
                            }
                        }
                    };

                    if let Some(st) = status {
                        if st.success() {
                            let mut out_bytes = Vec::with_capacity(64);
                            let _ = stdout.take(512).read_to_end(&mut out_bytes);
                            let s = String::from_utf8_lossy(&out_bytes);
                            if !s.trim().is_empty() {
                                found.push(ch);
                            }
                        }
                    }
                }
                found
            }));
        }

        for h in handles {
            if let Ok(mut chs) = h.join() {
                active.append(&mut chs);
            }
        }
    });

    if is_cancelled() {
        return ChannelScanReport {
            channels: Vec::new(),
            auth_rejected: false,
            error: Some("Channel scan cancelled".to_string()),
        };
    }

    active.sort_unstable();

    if active.is_empty() && scan_gw.circuit_breaker().is_tripped() {
        return ChannelScanReport {
            channels: Vec::new(),
            auth_rejected: true,
            error: Some(
                "Authentication rejected (401) - check NVR username & password".to_string(),
            ),
        };
    }

    ChannelScanReport {
        channels: active,
        auth_rejected: false,
        error: None,
    }
}

/// Bounded-concurrency 1-Click NVR Channel Auto-Scanner returning detailed auth/reachability status.
/// Uses an isolated `AuthGateway` + `AuthCircuitBreaker` so scanning unsaved Settings credentials
/// never trips live camera streams (`[MED-07]`).
#[allow(dead_code)]
pub fn scan_active_nvr_channels_detailed(
    env_cfg: &NvrEnvConfig,
    _auth_gw: &Arc<AuthGateway>,
    max_channels: usize,
) -> ChannelScanReport {
    scan_active_nvr_channels_cancellable(env_cfg, max_channels, None)
}

/// Bounded-concurrency 1-Click NVR Channel Auto-Scanner (probes channels 1..=max_channels
/// with at most 4 concurrent `ffprobe` processes).
pub fn scan_active_nvr_channels(
    env_cfg: &NvrEnvConfig,
    auth_gw: &Arc<AuthGateway>,
    max_channels: usize,
) -> Vec<usize> {
    scan_active_nvr_channels_detailed(env_cfg, auth_gw, max_channels).channels
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_gateway::AuthCircuitBreaker;

    #[test]
    fn test_patrol_auto_hd_downgrades_only_auto_promoted_slots() {
        let env_cfg = NvrEnvConfig::default();
        let p2p_ready = Arc::new(AtomicBool::new(false));
        let auth_gw = AuthGateway::start(AuthCircuitBreaker::new()).expect("start auth_gw");
        let mut mgr = StreamManager::new(env_cfg, p2p_ready, auth_gw);
        for ch in 1..=4 {
            mgr.slots.insert(ch, FrameSlot::new(ch, 1));
        }

        // User manually promotes Camera 3 to HD in Grid View
        mgr.slot(3).unwrap().set_manual_subtype(0);
        assert_eq!(mgr.slot(3).unwrap().target_subtype.load(Ordering::Acquire), 0);
        assert!(!mgr.slot(3).unwrap().auto_promoted_hd.load(Ordering::Acquire));

        // Patrol focuses Camera 1 with auto_hd = true -> Camera 1 promoted, Camera 3 stays HD
        mgr.set_fullscreen_focus(Some(1), true);
        assert_eq!(mgr.slot(1).unwrap().target_subtype.load(Ordering::Acquire), 0);
        assert!(mgr.slot(1).unwrap().auto_promoted_hd.load(Ordering::Acquire));
        assert_eq!(mgr.slot(3).unwrap().target_subtype.load(Ordering::Acquire), 0);

        // [HIGH-19]: Entering Playback on Camera 1 calls set_fullscreen_focus(Some(1), false)
        // -> Camera 1 Live PiP MUST downgrade back to SD (1)!
        mgr.set_fullscreen_focus(Some(1), false);
        assert_eq!(mgr.slot(1).unwrap().target_subtype.load(Ordering::Acquire), 1);
        assert!(!mgr.slot(1).unwrap().auto_promoted_hd.load(Ordering::Acquire));

        // Re-promote Camera 1 in Fullscreen, then Patrol rotates to Camera 2
        mgr.set_fullscreen_focus(Some(1), true);
        mgr.set_fullscreen_focus(Some(2), true);
        assert_eq!(mgr.slot(1).unwrap().target_subtype.load(Ordering::Acquire), 1);
        assert!(!mgr.slot(1).unwrap().auto_promoted_hd.load(Ordering::Acquire));
        assert_eq!(mgr.slot(2).unwrap().target_subtype.load(Ordering::Acquire), 0);
        assert_eq!(mgr.slot(3).unwrap().target_subtype.load(Ordering::Acquire), 0);

        // Patrol rotates to Camera 3 (already user-selected HD) -> Camera 2 downgrades to SD, Camera 3 stays non-auto HD
        mgr.set_fullscreen_focus(Some(3), true);
        assert_eq!(mgr.slot(2).unwrap().target_subtype.load(Ordering::Acquire), 1);
        assert_eq!(mgr.slot(3).unwrap().target_subtype.load(Ordering::Acquire), 0);
        assert!(!mgr.slot(3).unwrap().auto_promoted_hd.load(Ordering::Acquire));

        // Entering Playback on Camera 3 (manual HD) -> preserves manual HD (0)!
        mgr.set_fullscreen_focus(Some(3), false);
        assert_eq!(mgr.slot(3).unwrap().target_subtype.load(Ordering::Acquire), 0);

        // Exit Fullscreen -> Camera 3 remains user-selected HD (0), all others are SD (1)
        mgr.set_fullscreen_focus(None, true);
        assert_eq!(mgr.slot(1).unwrap().target_subtype.load(Ordering::Acquire), 1);
        assert_eq!(mgr.slot(2).unwrap().target_subtype.load(Ordering::Acquire), 1);
        assert_eq!(mgr.slot(3).unwrap().target_subtype.load(Ordering::Acquire), 0);
        assert_eq!(mgr.slot(4).unwrap().target_subtype.load(Ordering::Acquire), 1);
    }

    #[test]
    fn test_mint_live_gateway_url_never_leaks_credentials() {
        let mut cfg = NvrEnvConfig::default();
        cfg.host = "192.168.1.108".to_string();
        cfg.username = "admin".to_string();
        cfg.password = "SuperSecret!Pass#2026".to_string();

        let auth_gw = AuthGateway::start(AuthCircuitBreaker::new()).expect("start auth_gw");
        let url = mint_live_gateway_url(&auth_gw, &cfg, 5, 1).expect("mint url");
        assert!(url.starts_with("rtsp://127.0.0.1:"));
        assert!(!url.contains("SuperSecret"));
        assert!(!url.contains("admin"));
        assert!(!url.contains('@'));

        // [MED-07]: Custom template with bare IPv6 + zone ID formats host cleanly
        cfg.brand = "custom".to_string();
        cfg.host = "fe80::1%eth0".to_string();
        cfg.custom_url_template =
            "rtsp://{user}:{password}@{host}:{port}/ch{channel}_{subtype}".to_string();
        let url_v6 = mint_live_gateway_url(&auth_gw, &cfg, 2, 1).expect("mint custom ipv6 url");
        assert!(url_v6.starts_with("rtsp://127.0.0.1:"));
    }

    #[test]
    fn test_condvar_sleep_interruptible_wakes_immediately() {
        let slot = FrameSlot::new(1, 1);
        let slot_bg = Arc::clone(&slot);
        let start = Instant::now();

        let handle = thread::spawn(move || {
            slot_bg.sleep_interruptible(Duration::from_secs(30));
        });

        thread::sleep(Duration::from_millis(10));
        slot.stop_flag.store(true, Ordering::Release);
        slot.notify_wake();
        handle.join().unwrap();

        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn test_rtsp_auth_error_detection_and_acl_status_label() {
        assert!(is_rtsp_auth_error(
            "[rtsp @ 0x55] method DESCRIBE failed: 401 Unauthorized\n"
        ));
        assert!(is_rtsp_auth_error(
            "Server returned 403 Forbidden (access denied)"
        ));
        assert!(!is_rtsp_auth_error("Connection refused"));

        assert!(is_rtsp_credential_rejection(
            "[rtsp @ 0x55] method DESCRIBE failed: 401 Unauthorized\n"
        ));
        assert!(!is_rtsp_credential_rejection(
            "Server returned 403 Forbidden (access denied)"
        ));

        let slot = FrameSlot::new(2, 1);
        slot.set_status(StreamStatus::AuthError);
        assert_eq!(slot.status_label(), "Auth Failed (401)");
        slot.acl_denied.store(true, Ordering::Relaxed);
        assert_eq!(slot.status_label(), "Access Denied (403)");
        slot.trigger_resync();
        assert!(!slot.acl_denied.load(Ordering::Relaxed));
        assert_eq!(slot.status_label(), "Auth Failed (401)");
    }

    #[test]
    fn test_scan_active_nvr_channels_detailed_fast_unreachable_and_redaction() {
        let tmp_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed_port = tmp_listener.local_addr().unwrap().port();
        drop(tmp_listener);

        let mut cfg = NvrEnvConfig::default();
        cfg.host = "127.0.0.1".to_string();
        cfg.port = closed_port;
        cfg.password = "NeverLeakThisScanSecret!".to_string();

        let auth_gw = AuthGateway::start(AuthCircuitBreaker::new()).expect("start auth_gw");
        let t0 = Instant::now();
        let report = scan_active_nvr_channels_detailed(&cfg, &auth_gw, 16);
        assert!(t0.elapsed() < Duration::from_secs(2));
        assert!(report.channels.is_empty());
        assert!(!report.auth_rejected);
        let err = report.error.expect("unreachable error");
        assert!(err.starts_with("NVR RTSP unreachable:"));
        assert!(!err.contains("NeverLeakThisScanSecret!"));
    }
}
