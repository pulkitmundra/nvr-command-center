use crate::config::NvrEnvConfig;
use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
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
}

impl StreamStatus {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Connecting,
            1 => Self::Live,
            2 => Self::Reconnecting,
            3 => Self::Paused,
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
        }
    }
}

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
    pub target_subtype: AtomicU8,
    pub active_subtype: AtomicU8,
    pub paused: AtomicBool,
    pub background_eco: AtomicBool,
    pub force_resync: AtomicBool,
    pub stop_flag: AtomicBool,
    pub active_child_pid: AtomicU32,
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
            target_subtype: AtomicU8::new(default_subtype),
            active_subtype: AtomicU8::new(default_subtype),
            paused: AtomicBool::new(false),
            background_eco: AtomicBool::new(false),
            force_resync: AtomicBool::new(false),
            stop_flag: AtomicBool::new(false),
            active_child_pid: AtomicU32::new(0),
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

    pub fn set_status(&self, st: StreamStatus) {
        self.status.store(st as u8, Ordering::Release);
    }

    pub fn request_subtype(&self, subtype: u8) {
        let clamped = if subtype == 0 { 0 } else { 1 };
        let prev = self.target_subtype.swap(clamped, Ordering::AcqRel);
        if prev != clamped {
            self.force_resync.store(true, Ordering::Release);
            self.kill_active_child();
        }
    }

    pub fn trigger_resync(&self) {
        self.force_resync.store(true, Ordering::Release);
        self.kill_active_child();
    }

    pub fn set_paused(&self, pause: bool) {
        let was_paused = self.paused.swap(pause, Ordering::AcqRel);
        if pause && !was_paused {
            self.set_status(StreamStatus::Paused);
            self.fps_x10.store(0, Ordering::Release);
            self.kill_active_child();
        } else if !pause && was_paused {
            self.set_status(StreamStatus::Connecting);
            self.force_resync.store(true, Ordering::Release);
        }
    }

    pub fn kill_active_child(&self) {
        let pid = self.active_child_pid.swap(0, Ordering::AcqRel);
        if pid > 0 {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
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

pub struct StreamManager {
    env_cfg: Arc<RwLock<NvrEnvConfig>>,
    p2p_ready: Arc<AtomicBool>,
    slots: BTreeMap<usize, Arc<FrameSlot>>,
    workers: Vec<JoinHandle<()>>,
    watchdog_stop: Arc<AtomicBool>,
    watchdog_handle: Option<JoinHandle<()>>,
}

impl StreamManager {
    pub fn new(env_cfg: NvrEnvConfig, p2p_ready: Arc<AtomicBool>) -> Self {
        let shared_env = Arc::new(RwLock::new(env_cfg));
        Self {
            env_cfg: shared_env,
            p2p_ready,
            slots: BTreeMap::new(),
            workers: Vec::new(),
            watchdog_stop: Arc::new(AtomicBool::new(false)),
            watchdog_handle: None,
        }
    }

    pub fn start_streams(&mut self, num_cameras: usize) {
        self.stop_all();
        self.watchdog_stop.store(false, Ordering::Release);

        let env_snapshot = self.env_cfg.read().unwrap().clone();
        let default_sub = env_snapshot.default_subtype;
        let ip_start = env_snapshot.ip_camera_start_channel.min(num_cameras + 1);

        for ch in 1..=num_cameras {
            let slot = FrameSlot::new(ch, default_sub);
            self.slots.insert(ch, slot);
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
                let handle = thread::Builder::new()
                    .name(format!("rtsp-cam-{}", ch))
                    .spawn(move || camera_worker_loop(slot, env_ref, p2p_ready_ref))
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
            slot.request_subtype(subtype);
        }
    }

    pub fn set_fullscreen_focus(&self, focused_channel: Option<usize>, auto_hd: bool, default_sub: u8) {
        for (&ch, slot) in &self.slots {
            match focused_channel {
                Some(target_ch) if ch == target_ch => {
                    slot.background_eco.store(false, Ordering::Release);
                    if auto_hd {
                        slot.request_subtype(0);
                    }
                }
                Some(_) => {
                    // Background camera while another camera is fullscreen
                    slot.background_eco.store(true, Ordering::Release);
                }
                None => {
                    slot.background_eco.store(false, Ordering::Release);
                    if auto_hd && slot.target_subtype.load(Ordering::Acquire) == 0 && default_sub == 1 {
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
        self.watchdog_stop.store(true, Ordering::Release);
        for slot in self.slots.values() {
            slot.stop_flag.store(true, Ordering::Release);
            slot.kill_active_child();
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
    stop_flag: Arc<AtomicBool>,
    env_cfg: Arc<RwLock<NvrEnvConfig>>,
    p2p_ready: Arc<AtomicBool>,
) {
    while !stop_flag.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(500));
        if stop_flag.load(Ordering::Acquire) {
            break;
        }
        let is_cloud = env_cfg
            .read()
            .map(|c| c.use_cloud_p2p && !c.p2p_serial.trim().is_empty())
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
            let pid = slot.active_child_pid.load(Ordering::Acquire);
            if pid == 0 {
                continue;
            }
            let st = slot.status();
            let last_frame = slot.last_frame_epoch_ms.load(Ordering::Acquire);
            let conn_start = slot.connect_start_epoch_ms.load(Ordering::Acquire);

            if st == StreamStatus::Live && last_frame > 0 && now.saturating_sub(last_frame) > live_stall_ms {
                slot.kill_active_child();
            } else if (st == StreamStatus::Connecting || st == StreamStatus::Reconnecting)
                && conn_start > 0
                && now.saturating_sub(conn_start) > connect_timeout_ms
            {
                slot.kill_active_child();
            }
        }
    }
}

fn camera_worker_loop(
    slot: Arc<FrameSlot>,
    env_cfg: Arc<RwLock<NvrEnvConfig>>,
    p2p_ready: Arc<AtomicBool>,
) {
    let mut back_buf: Vec<u8> = Vec::new();
    let mut consecutive_failures: u32 = 0;

    while !slot.stop_flag.load(Ordering::Acquire) {
        if slot.paused.load(Ordering::Acquire) {
            slot.set_status(StreamStatus::Paused);
            thread::sleep(Duration::from_millis(120));
            continue;
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
        if cfg_snapshot.use_cloud_p2p && !cfg_snapshot.p2p_serial.trim().is_empty() {
            if !p2p_ready.load(Ordering::Acquire) {
                if slot.frame_seq.load(Ordering::Acquire) == 0 {
                    slot.set_status(StreamStatus::Connecting);
                } else {
                    slot.set_status(StreamStatus::Reconnecting);
                }
                thread::sleep(Duration::from_millis(120));
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
                    thread::sleep(Duration::from_millis(stagger_ms));
                }
            }
        }
        let rtsp_url = cfg_snapshot.build_rtsp_url(slot.channel, subtype);

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

        let mut cmd = Command::new("ffmpeg");
        cmd.arg("-nostdin")
            .arg("-v")
            .arg("error")
            .arg("-rtsp_transport")
            .arg("tcp")
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
            .stderr(Stdio::null());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => {
                slot.set_status(StreamStatus::Error);
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };

        slot.active_child_pid.store(child.id(), Ordering::Release);

        // If a resync, pause, stop, or subtype change arrived while cmd.spawn() was executing,
        // abort this child immediately
        if slot.stop_flag.load(Ordering::Acquire)
            || slot.paused.load(Ordering::Acquire)
            || slot.force_resync.load(Ordering::Acquire)
            || slot.target_subtype.load(Ordering::Acquire) != subtype
        {
            slot.active_child_pid.store(0, Ordering::Release);
            let _ = child.kill();
            let _ = child.wait();
            continue;
        }

        let mut stdout = match child.stdout.take() {
            Some(out) => out,
            None => {
                slot.active_child_pid.store(0, Ordering::Release);
                let _ = child.kill();
                let _ = child.wait();
                continue;
            }
        };

        let pipe_fd = stdout.as_raw_fd();
        let mut fps_window_start = Instant::now();
        let mut fps_frames: u32 = 0;
        let mut eco_frame_counter: u64 = 0;
        let mut received_any_frame = false;

        loop {
            if slot.stop_flag.load(Ordering::Acquire)
                || slot.paused.load(Ordering::Acquire)
                || slot.force_resync.load(Ordering::Acquire)
            {
                break;
            }

            match stdout.read_exact(&mut back_buf) {
                Ok(()) => {
                    received_any_frame = true;
                    consecutive_failures = 0;

                    // Zero-Lag Kernel Pipe Burst Drain:
                    // If 1 or more newer frames are already waiting in the OS pipe buffer,
                    // drain them immediately into back_buf so the UI only ever receives
                    // the newest real-time frame (0 buffered frames!).
                    let mut drained = 0u64;
                    while pipe_available_bytes(pipe_fd) >= frame_bytes && drained < 16 {
                        if stdout.read_exact(&mut back_buf).is_err() {
                            break;
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
                        eco_frame_counter % 4 == 0
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
                    break;
                }
            }
        }

        slot.active_child_pid.store(0, Ordering::Release);
        let _ = child.kill();
        let _ = child.wait();

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

        if !received_any_frame {
            consecutive_failures = consecutive_failures.saturating_add(1);
        }
        slot.reconnect_count.fetch_add(1, Ordering::Relaxed);
        slot.fps_x10.store(0, Ordering::Release);

        if consecutive_failures >= 3 {
            slot.set_status(StreamStatus::Error);
        } else {
            slot.set_status(StreamStatus::Reconnecting);
        }

        let backoff_ms = match consecutive_failures {
            0 | 1 => 300,
            2 => 800,
            3 => 1500,
            _ => 2500,
        };
        thread::sleep(Duration::from_millis(backoff_ms));
    }
}

/// Parallel 1-Click NVR Channel Auto-Scanner (probes channels 1..=max_channels in parallel)
pub fn scan_active_nvr_channels(env_cfg: &NvrEnvConfig, max_channels: usize) -> Vec<usize> {
    let max_ch = max_channels.clamp(1, 32);
    let mut handles = Vec::with_capacity(max_ch);

    for ch in 1..=max_ch {
        let url = env_cfg.build_rtsp_url(ch, 1);
        handles.push(thread::spawn(move || {
            let output = Command::new("timeout")
                .arg("3")
                .arg("ffprobe")
                .arg("-v")
                .arg("error")
                .arg("-rtsp_transport")
                .arg("tcp")
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
                .arg(&url)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output();

            match output {
                Ok(out) if out.status.success() => {
                    let s = String::from_utf8_lossy(&out.stdout);
                    if !s.trim().is_empty() {
                        Some(ch)
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }));
    }

    let mut active = Vec::new();
    for h in handles {
        if let Ok(Some(ch)) = h.join() {
            active.push(ch);
        }
    }
    active.sort_unstable();
    active
}
