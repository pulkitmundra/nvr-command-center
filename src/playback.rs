use crate::auth_gateway::AuthGateway;
use crate::config::{
    create_private_file_exclusive, ensure_private_dir, redact_secret_in_text,
    resolve_trusted_binary, sanitize_export_label, NvrEnvConfig,
};
#[allow(unused_imports)]
pub use crate::nvr_api::test_nvr_connection_sync;
use crate::nvr_api::{
    epoch_ms_to_local_naive, local_naive_to_epoch_ms, DayTimelineIndex, NvrEventKind,
    NvrHttpClient, RecordedSpan,
};
use crate::stream::FrameBuffer;
use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::{CString, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::os::unix::io::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn block_on_nvr_io<F: std::future::Future>(fut: F) -> F::Output {
    static PLAYBACK_IO_RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    let rt = PLAYBACK_IO_RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("nvr-pb-io")
            .enable_all()
            .build()
            .expect("Failed to initialize playback Tokio I/O runtime")
    });
    rt.block_on(fut)
}

pub const MAX_CACHE_BYTES: usize = 128 * 1024 * 1024;
pub const SEGMENT_DURATION_MS: i64 = 60_000;
pub const SEEK_DEBOUNCE_MS: u64 = 200;
pub const MIN_EXPORT_FREE_BYTES: u64 = 512 * 1024 * 1024;
#[allow(dead_code)]
pub const MAX_CLIP_EXPORT_MS: i64 = 7_200_000;
const MAX_EXPORT_FED_BYTES: u64 = 4 * 1024 * 1024 * 1024;

pub const SINGLE_DECODE_WIDTH: u32 = 1920;
pub const SINGLE_DECODE_HEIGHT: u32 = 1080;
pub const GRID_DECODE_WIDTH: u32 = 960;
pub const GRID_DECODE_HEIGHT: u32 = 540;

pub const SINGLE_RING_FRAMES: usize = 16;
pub const GRID_RING_FRAMES: usize = 24;

pub const ALLOWED_SPEEDS: [f32; 9] = [-4.0, -2.0, -1.0, 0.5, 1.0, 2.0, 4.0, 8.0, 16.0];

const DHAV_MIN_PACKET_LEN: usize = 24;
const DHAV_MAX_PACKET_LEN: usize = 8_388_608;
const PKT_VIDEO_IDR: u8 = 0xFD;
const PKT_VIDEO_P: u8 = 0xFC;
const PKT_FILE_HEADER: u8 = 0xFE;
const PKT_METADATA_F1: u8 = 0xF1;

static NEXT_FRAME_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmartSkipMode {
    Off,
    FastGlide8x,
    InstantJump,
}

impl SmartSkipMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::FastGlide8x => "8x Glide",
            Self::InstantJump => "Jump",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::FastGlide8x,
            Self::FastGlide8x => Self::InstantJump,
            Self::InstantJump => Self::Off,
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackStatus {
    Idle = 0,
    Buffering = 1,
    Playing = 2,
    Paused = 3,
    EndOfRecording = 4,
    Error = 5,
    AuthError = 6,
}

impl PlaybackStatus {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Idle,
            1 => Self::Buffering,
            2 => Self::Playing,
            3 => Self::Paused,
            4 => Self::EndOfRecording,
            6 => Self::AuthError,
            _ => Self::Error,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Buffering => "Buffering",
            Self::Playing => "Playing",
            Self::Paused => "Paused",
            Self::EndOfRecording => "End of Recording",
            Self::Error => "Playback Error",
            Self::AuthError => "Auth Locked (401)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvrResourceGovernor {
    pub max_channels: usize,
    pub decode_width: u32,
    pub decode_height: u32,
    pub ring_capacity: usize,
    pub allow_prefetch: bool,
}

impl NvrResourceGovernor {
    pub fn compute(is_cloud_p2p: bool, requested_channels: usize) -> Self {
        let max_channels = if is_cloud_p2p { 1 } else { 4 };
        let active_count = requested_channels.clamp(1, max_channels);
        if active_count > 1 {
            Self {
                max_channels,
                decode_width: GRID_DECODE_WIDTH,
                decode_height: GRID_DECODE_HEIGHT,
                ring_capacity: GRID_RING_FRAMES,
                allow_prefetch: !is_cloud_p2p,
            }
        } else {
            Self {
                max_channels,
                decode_width: SINGLE_DECODE_WIDTH,
                decode_height: SINGLE_DECODE_HEIGHT,
                ring_capacity: SINGLE_RING_FRAMES,
                allow_prefetch: !is_cloud_p2p,
            }
        }
    }

    pub fn clamp_channels(is_cloud_p2p: bool, channels: &[usize]) -> Vec<usize> {
        let max_ch = if is_cloud_p2p { 1 } else { 4 };
        let mut out = Vec::with_capacity(max_ch);
        for &ch in channels {
            if ch > 0 && !out.contains(&ch) {
                out.push(ch);
                if out.len() >= max_ch {
                    break;
                }
            }
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DhavPacketEntry {
    pub pkt_type: u8,
    pub is_keyframe: bool,
    pub epoch_ms: i64,
    pub byte_offset: u64,
    pub frame_len: u32,
}

pub struct DhavPacketScanner {
    carry: Vec<u8>,
    stream_offset: u64,
    fallback_epoch_ms: i64,
    prev_u16_ts: Option<u16>,
    prev_video_epoch_ms: Option<i64>,
}

impl DhavPacketScanner {
    pub fn new(fallback_epoch_ms: i64) -> Self {
        Self {
            carry: Vec::with_capacity(65_536),
            stream_offset: 0,
            fallback_epoch_ms,
            prev_u16_ts: None,
            prev_video_epoch_ms: None,
        }
    }

    pub fn unpack_rtc_datetime(date_word: u32) -> Option<NaiveDateTime> {
        let sec = date_word & 0x3F;
        let min = (date_word >> 6) & 0x3F;
        let hour = (date_word >> 12) & 0x1F;
        let day = (date_word >> 17) & 0x1F;
        let month = (date_word >> 22) & 0x0F;
        let year = ((date_word >> 26) & 0x3F) as i32 + 2000;

        let d = NaiveDate::from_ymd_opt(year, month, day)?;
        let t = NaiveTime::from_hms_opt(hour, min, sec)?;
        Some(NaiveDateTime::new(d, t))
    }

    pub fn pack_rtc_datetime(dt: NaiveDateTime) -> u32 {
        let year_off = (dt.year() - 2000).clamp(0, 63) as u32;
        let month = (dt.month() & 0x0F) << 22;
        let day = (dt.day() & 0x1F) << 17;
        let hour = (dt.hour() & 0x1F) << 12;
        let min = (dt.minute() & 0x3F) << 6;
        let sec = dt.second() & 0x3F;
        (year_off << 26) | month | day | hour | min | sec
    }

    pub fn rewrite_synthetic_monotonic_header(pkt: &mut [u8], synth_seq: u32) {
        if pkt.len() < DHAV_MIN_PACKET_LEN || &pkt[0..4] != b"DHAV" {
            return;
        }
        let old_sum: u8 = pkt[8..22].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));

        // Base synthetic clock: 2020-01-01 00:00:00 + synth_seq seconds (strictly +1s per packet)
        let total_secs = synth_seq as i64;
        let base_date = NaiveDate::from_ymd_opt(2020, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let synth_dt = base_date + chrono::Duration::seconds(total_secs);
        let packed_date = Self::pack_rtc_datetime(synth_dt);
        let synth_ms = ((synth_seq.wrapping_mul(1000)) & 0xFFFF) as u16;

        pkt[8..12].copy_from_slice(&synth_seq.to_le_bytes());
        pkt[16..20].copy_from_slice(&packed_date.to_le_bytes());
        pkt[20..22].copy_from_slice(&synth_ms.to_le_bytes());

        let new_sum: u8 = pkt[8..22].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        pkt[23] = pkt[23].wrapping_sub(old_sum).wrapping_add(new_sum);
    }

    pub fn push_chunk(&mut self, chunk: &[u8]) -> Vec<DhavPacketEntry> {
        let mut out = Vec::new();
        self.push_chunk_with_bytes(chunk, |entry, _| {
            out.push(entry);
            true
        });
        out
    }

    pub fn push_chunk_with_bytes<F>(&mut self, chunk: &[u8], mut on_packet: F) -> bool
    where
        F: FnMut(DhavPacketEntry, &[u8]) -> bool,
    {
        if chunk.is_empty() {
            return true;
        }
        self.carry.extend_from_slice(chunk);

        let mut cursor = 0usize;
        let mut keep_going = true;
        while cursor + DHAV_MIN_PACKET_LEN <= self.carry.len() {
            if &self.carry[cursor..cursor + 4] != b"DHAV" {
                if let Some(rel) = find_dhav_magic(&self.carry[cursor + 1..]) {
                    let skip = rel + 1;
                    cursor += skip;
                    self.stream_offset += skip as u64;
                    continue;
                } else {
                    // Keep up to 3 trailing bytes in case "DHAV" straddles a chunk boundary
                    let keep = self.carry.len().saturating_sub(cursor).min(3);
                    let drop_end = self.carry.len() - keep;
                    if drop_end > cursor {
                        self.stream_offset += (drop_end - cursor) as u64;
                        cursor = drop_end;
                    }
                    break;
                }
            }

            let hdr = &self.carry[cursor..cursor + DHAV_MIN_PACKET_LEN];
            let pkt_type = hdr[4];

            // [MED-08] Validate 24-byte header checksum for all non-0xF1 packets before accepting
            // a "DHAV" sync candidate so random payload bytes never stall `carry`.
            if pkt_type != PKT_METADATA_F1 {
                let csum = hdr[0..23].iter().fold(0u8, |a, &b| a.wrapping_add(b));
                if csum != hdr[23] {
                    cursor += 4;
                    self.stream_offset += 4;
                    continue;
                }
            }

            let frame_len = u32::from_le_bytes([hdr[12], hdr[13], hdr[14], hdr[15]]) as usize;
            if !(DHAV_MIN_PACKET_LEN..=DHAV_MAX_PACKET_LEN).contains(&frame_len) {
                // Corrupted length field: advance 4 bytes past "DHAV" and resync to next magic
                cursor += 4;
                self.stream_offset += 4;
                continue;
            }

            if cursor + frame_len > self.carry.len() {
                // Wait for the rest of this packet in subsequent chunks
                break;
            }

            // Optional trailer sanity check: valid DHAV packets end with b"dhav" + len,
            // though some firmware metadata packets omit lowercase trailer. If another "DHAV"
            // appears inside frame_len AND trailer does not match b"dhav", resync cleanly.
            if frame_len >= 32 && &self.carry[cursor + frame_len - 8..cursor + frame_len - 4] != b"dhav"
            {
                if let Some(inner_rel) = find_dhav_magic(&self.carry[cursor + 4..cursor + frame_len])
                {
                    let step = inner_rel + 4;
                    cursor += step;
                    self.stream_offset += step as u64;
                    continue;
                }
            }

            let date_word = u32::from_le_bytes([hdr[16], hdr[17], hdr[18], hdr[19]]);
            let u16_ts = u16::from_le_bytes([hdr[20], hdr[21]]);
            let is_video = pkt_type == PKT_VIDEO_IDR || pkt_type == PKT_VIDEO_P;

            let epoch_ms = if let Some(dt) = Self::unpack_rtc_datetime(date_word) {
                let base_sec_ms = local_naive_to_epoch_ms(dt);
                if is_video {
                    let computed = match (self.prev_u16_ts, self.prev_video_epoch_ms) {
                        (Some(prev_ts), Some(prev_ep)) => {
                            let delta = u16_ts.wrapping_sub(prev_ts) as i64;
                            if (1..=2_000).contains(&delta)
                                && prev_ep.saturating_add(delta).abs_diff(base_sec_ms) <= 1_500
                            {
                                prev_ep.saturating_add(delta)
                            } else if delta == 0
                                && prev_ep.saturating_add(1).abs_diff(base_sec_ms) <= 1_500
                            {
                                prev_ep.saturating_add(1)
                            } else if base_sec_ms >= prev_ep.saturating_sub(1_000) {
                                base_sec_ms.max(prev_ep.saturating_add(1))
                            } else {
                                base_sec_ms
                            }
                        }
                        _ => base_sec_ms.saturating_add((u16_ts % 1000) as i64),
                    };
                    self.prev_u16_ts = Some(u16_ts);
                    self.prev_video_epoch_ms = Some(computed);
                    computed
                } else if let Some(prev_ep) = self.prev_video_epoch_ms {
                    if prev_ep.abs_diff(base_sec_ms) <= 1_500 {
                        prev_ep
                    } else {
                        base_sec_ms.saturating_add((u16_ts % 1000) as i64)
                    }
                } else {
                    base_sec_ms.saturating_add((u16_ts % 1000) as i64)
                }
            } else if is_video {
                let next_ep = match (self.prev_u16_ts, self.prev_video_epoch_ms) {
                    (Some(prev_ts), Some(prev_ep)) => {
                        let delta = u16_ts.wrapping_sub(prev_ts) as i64;
                        if (1..=2_000).contains(&delta) {
                            prev_ep.saturating_add(delta)
                        } else {
                            prev_ep.saturating_add(40)
                        }
                    }
                    (_, Some(prev_ep)) => prev_ep.saturating_add(40),
                    _ => self.fallback_epoch_ms,
                };
                self.prev_u16_ts = Some(u16_ts);
                self.prev_video_epoch_ms = Some(next_ep);
                next_ep
            } else {
                self.prev_video_epoch_ms.unwrap_or(self.fallback_epoch_ms)
            };

            let entry = DhavPacketEntry {
                pkt_type,
                is_keyframe: pkt_type == PKT_VIDEO_IDR,
                epoch_ms,
                byte_offset: self.stream_offset,
                frame_len: frame_len as u32,
            };
            let pkt_bytes = &self.carry[cursor..cursor + frame_len];
            let cont = on_packet(entry, pkt_bytes);

            cursor += frame_len;
            self.stream_offset += frame_len as u64;

            if !cont {
                keep_going = false;
                break;
            }
        }

        if cursor > 0 {
            self.carry.drain(0..cursor);
        }

        keep_going
    }
}

fn find_dhav_magic(haystack: &[u8]) -> Option<usize> {
    haystack.windows(4).position(|w| w == b"DHAV")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SegmentKey {
    pub channel: usize,
    pub start_ms: i64,
}

#[inline]
pub fn align_segment_start_ms(epoch_ms: i64) -> i64 {
    epoch_ms.div_euclid(SEGMENT_DURATION_MS) * SEGMENT_DURATION_MS
}

fn create_unlinked_anonymous_tempfile_in(base_dir: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    let pid = std::process::id();
    let mut last_err = None;
    for _ in 0..8 {
        let nonce: u64 = rand::random();
        let tmp_path = base_dir.join(format!(".nvr-seg-{}-{:016x}.tmp", pid, nonce));
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .mode(0o600)
            .open(&tmp_path)
        {
            Ok(file) => {
                if let Err(rm_err) = std::fs::remove_file(&tmp_path) {
                    drop(file);
                    return Err(rm_err);
                }
                return Ok(file);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_err = Some(e);
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "Failed to allocate exclusive anonymous tempfile",
        )
    }))
}

fn create_unlinked_anonymous_tempfile() -> std::io::Result<File> {
    let primary_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    match create_unlinked_anonymous_tempfile_in(&primary_dir) {
        Ok(f) => Ok(f),
        Err(first_err) => {
            let fallback_dir = std::env::temp_dir();
            if fallback_dir != primary_dir {
                create_unlinked_anonymous_tempfile_in(&fallback_dir)
            } else {
                Err(first_err)
            }
        }
    }
}

fn create_anonymous_memfd() -> std::io::Result<File> {
    #[cfg(target_os = "linux")]
    unsafe {
        let fd = libc::memfd_create(c"nvr-seg".as_ptr(), libc::MFD_CLOEXEC);
        if fd >= 0 {
            return Ok(File::from_raw_fd(fd));
        }
    }
    create_unlinked_anonymous_tempfile()
}

pub struct MemfdSegment {
    file: File,
    written_bytes: AtomicUsize,
    complete: AtomicBool,
    aborted: AtomicBool,
    error: Mutex<Option<String>>,
    packets: Mutex<Vec<DhavPacketEntry>>,
    notify_lock: Mutex<u64>,
    notify_cvar: Condvar,
}

impl MemfdSegment {
    pub fn new(_key: SegmentKey) -> std::io::Result<Arc<Self>> {
        let file = create_anonymous_memfd()?;
        Ok(Arc::new(Self {
            file,
            written_bytes: AtomicUsize::new(0),
            complete: AtomicBool::new(false),
            aborted: AtomicBool::new(false),
            error: Mutex::new(None),
            packets: Mutex::new(Vec::with_capacity(1600)),
            notify_lock: Mutex::new(0),
            notify_cvar: Condvar::new(),
        }))
    }

    pub fn byte_len(&self) -> usize {
        self.written_bytes.load(Ordering::Acquire)
    }

    pub fn is_complete(&self) -> bool {
        self.complete.load(Ordering::Acquire)
    }

    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::Acquire)
    }

    pub fn error_text(&self) -> Option<String> {
        self.error.lock().ok().and_then(|g| g.clone())
    }

    pub fn append_chunk_and_index(
        &self,
        chunk: &[u8],
        new_packets: &[DhavPacketEntry],
    ) -> std::io::Result<usize> {
        if chunk.is_empty() {
            return Ok(self.byte_len());
        }
        let start_off = self.written_bytes.load(Ordering::Acquire) as u64;
        self.file.write_all_at(chunk, start_off)?;
        let new_total = start_off as usize + chunk.len();
        self.written_bytes.store(new_total, Ordering::Release);

        if !new_packets.is_empty() {
            if let Ok(mut guard) = self.packets.lock() {
                guard.extend_from_slice(new_packets);
            }
            self.bump_notify();
        }
        Ok(new_total)
    }

    pub fn mark_finished(&self, err: Option<String>, aborted: bool) {
        if let Some(e) = err {
            if let Ok(mut g) = self.error.lock() {
                *g = Some(e);
            }
        }
        if aborted {
            self.aborted.store(true, Ordering::Release);
        } else {
            self.complete.store(true, Ordering::Release);
        }
        self.bump_notify();
    }

    fn bump_notify(&self) {
        if let Ok(mut seq) = self.notify_lock.lock() {
            *seq = seq.wrapping_add(1);
            self.notify_cvar.notify_all();
        }
    }

    pub fn read_packet(&self, entry: &DhavPacketEntry, dst: &mut Vec<u8>) -> std::io::Result<()> {
        let len = entry.frame_len as usize;
        let end = entry.byte_offset.saturating_add(len as u64);
        if end > self.byte_len() as u64 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "Packet exceeds memfd written_bytes",
            ));
        }
        dst.resize(len, 0);
        self.file.read_exact_at(dst, entry.byte_offset)
    }

    #[cfg(test)]
    pub fn read_raw_at(&self, offset: u64, dst: &mut [u8]) -> std::io::Result<usize> {
        let avail = (self.byte_len() as u64).saturating_sub(offset) as usize;
        if avail == 0 {
            return Ok(0);
        }
        let to_read = dst.len().min(avail);
        self.file.read_exact_at(&mut dst[..to_read], offset)?;
        Ok(to_read)
    }

    pub fn packets_snapshot(&self) -> Vec<DhavPacketEntry> {
        self.packets
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    pub fn packet_at(&self, idx: usize) -> Option<DhavPacketEntry> {
        self.packets
            .lock()
            .ok()
            .and_then(|g| g.get(idx).copied())
    }

    pub fn packet_len(&self) -> usize {
        self.packets.lock().map(|g| g.len()).unwrap_or(0)
    }

    pub fn wait_for_change(&self, seen_seq: u64, timeout: Duration) -> u64 {
        let Ok(guard) = self.notify_lock.lock() else {
            return seen_seq;
        };
        if *guard != seen_seq || self.is_complete() || self.is_aborted() {
            return *guard;
        }
        match self.notify_cvar.wait_timeout(guard, timeout) {
            Ok((g, _)) => *g,
            Err(poisoned) => *poisoned.into_inner().0,
        }
    }

    pub fn notify_seq(&self) -> u64 {
        self.notify_lock.lock().map(|g| *g).unwrap_or(0)
    }
}

struct CachedSegmentMeta {
    segment: Arc<MemfdSegment>,
    accounted_bytes: usize,
    last_access_tick: u64,
    in_flight: bool,
}

pub struct MemfdSegmentCache {
    max_bytes: usize,
    total_bytes: usize,
    access_clock: u64,
    entries: HashMap<SegmentKey, CachedSegmentMeta>,
}

impl MemfdSegmentCache {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            total_bytes: 0,
            access_clock: 0,
            entries: HashMap::new(),
        }
    }

    #[cfg(test)]
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get_ready_or_inflight(&mut self, key: SegmentKey) -> Option<Arc<MemfdSegment>> {
        self.access_clock = self.access_clock.wrapping_add(1);
        let tick = self.access_clock;
        if let Some(meta) = self.entries.get_mut(&key) {
            if !meta.segment.is_aborted() && meta.segment.error_text().is_none() {
                meta.last_access_tick = tick;
                return Some(Arc::clone(&meta.segment));
            }
        }
        // Clean up aborted/failed entry if present
        if let Some(removed) = self.entries.remove(&key) {
            self.total_bytes = self.total_bytes.saturating_sub(removed.accounted_bytes);
        }
        None
    }

    /// Returns `(segment, should_download)`. If `should_download` is `true`, caller owns downloading it.
    pub fn acquire_or_create(
        &mut self,
        key: SegmentKey,
    ) -> std::io::Result<(Arc<MemfdSegment>, bool)> {
        if let Some(existing) = self.get_ready_or_inflight(key) {
            return Ok((existing, false));
        }
        let seg = MemfdSegment::new(key)?;
        self.access_clock = self.access_clock.wrapping_add(1);
        self.entries.insert(
            key,
            CachedSegmentMeta {
                segment: Arc::clone(&seg),
                accounted_bytes: 0,
                last_access_tick: self.access_clock,
                in_flight: true,
            },
        );
        self.evict_if_needed(Some(key));
        Ok((seg, true))
    }

    pub fn record_segment_growth(&mut self, key: SegmentKey, new_segment_bytes: usize) {
        self.access_clock = self.access_clock.wrapping_add(1);
        let tick = self.access_clock;
        if let Some(meta) = self.entries.get_mut(&key) {
            if new_segment_bytes >= meta.accounted_bytes {
                self.total_bytes += new_segment_bytes - meta.accounted_bytes;
            } else {
                self.total_bytes = self
                    .total_bytes
                    .saturating_sub(meta.accounted_bytes - new_segment_bytes);
            }
            meta.accounted_bytes = new_segment_bytes;
            meta.last_access_tick = tick;
        }
        self.evict_if_needed(Some(key));
    }

    pub fn finish_inflight(&mut self, key: SegmentKey, keep: bool) {
        if !keep {
            if let Some(removed) = self.entries.remove(&key) {
                self.total_bytes = self.total_bytes.saturating_sub(removed.accounted_bytes);
            }
            return;
        }
        if let Some(meta) = self.entries.get_mut(&key) {
            let final_len = meta.segment.byte_len();
            if final_len >= meta.accounted_bytes {
                self.total_bytes += final_len - meta.accounted_bytes;
            } else {
                self.total_bytes = self
                    .total_bytes
                    .saturating_sub(meta.accounted_bytes - final_len);
            }
            meta.accounted_bytes = final_len;
            meta.in_flight = false;
        }
        self.evict_if_needed(None);
    }

    fn evict_if_needed(&mut self, protected_key: Option<SegmentKey>) {
        while self.total_bytes > self.max_bytes && self.entries.len() > 1 {
            // [MED-10] Never evict in-flight segments while active downloads are writing to them.
            let candidate = self
                .entries
                .iter()
                .filter(|(&k, m)| Some(k) != protected_key && !m.in_flight)
                .min_by_key(|(_, m)| m.last_access_tick)
                .map(|(&k, _)| k);

            let Some(victim_key) = candidate else {
                break;
            };
            if let Some(removed) = self.entries.remove(&victim_key) {
                self.total_bytes = self.total_bytes.saturating_sub(removed.accounted_bytes);
            }
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.total_bytes = 0;
    }

    pub fn cached_ranges_for_channel(
        &self,
        channel: usize,
        view_start_ms: i64,
        view_end_ms: i64,
    ) -> Vec<(i64, i64)> {
        let (lo_ms, hi_ms) = if view_end_ms >= view_start_ms {
            (view_start_ms, view_end_ms)
        } else {
            (view_end_ms, view_start_ms)
        };
        let mut spans: Vec<(i64, i64)> = self
            .entries
            .iter()
            .filter_map(|(k, meta)| {
                if k.channel != channel {
                    return None;
                }
                if meta.segment.is_aborted()
                    || meta.segment.error_text().is_some()
                    || meta.segment.byte_len() == 0
                {
                    return None;
                }
                let seg_start = k.start_ms;
                let seg_end = seg_start.saturating_add(SEGMENT_DURATION_MS);
                if seg_end > lo_ms && seg_start < hi_ms {
                    Some((seg_start, seg_end))
                } else {
                    None
                }
            })
            .collect();
        if spans.is_empty() {
            return spans;
        }
        spans.sort_unstable_by_key(|&(s, _)| s);
        let mut merged: Vec<(i64, i64)> = Vec::with_capacity(spans.len());
        for (s, e) in spans {
            if let Some(last) = merged.last_mut() {
                if s <= last.1 {
                    last.1 = last.1.max(e);
                    continue;
                }
            }
            merged.push((s, e));
        }
        merged
    }
}

struct DownloadSemaphore {
    max_permits: AtomicUsize,
    in_use: Mutex<usize>,
    cvar: Condvar,
}

struct DownloadPermitGuard<'a> {
    sem: &'a DownloadSemaphore,
}

impl Drop for DownloadPermitGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut used) = self.sem.in_use.lock() {
            *used = used.saturating_sub(1);
            self.sem.cvar.notify_all();
        }
    }
}

impl DownloadSemaphore {
    fn new(max_permits: usize) -> Arc<Self> {
        Arc::new(Self {
            max_permits: AtomicUsize::new(max_permits.max(1)),
            in_use: Mutex::new(0),
            cvar: Condvar::new(),
        })
    }

    fn set_max_permits(&self, max_permits: usize) {
        self.max_permits
            .store(max_permits.max(1), Ordering::Release);
        self.cvar.notify_all();
    }

    fn notify_all(&self) {
        self.cvar.notify_all();
    }

    fn acquire_interruptible<'a>(
        &'a self,
        stop_flag: &AtomicBool,
    ) -> Option<DownloadPermitGuard<'a>> {
        let mut guard = self.in_use.lock().ok()?;
        while *guard >= self.max_permits.load(Ordering::Acquire).max(1) {
            if stop_flag.load(Ordering::Acquire) {
                return None;
            }
            let (g, _) = self.cvar.wait_timeout(guard, Duration::from_millis(50)).ok()?;
            guard = g;
        }
        if stop_flag.load(Ordering::Acquire) {
            return None;
        }
        *guard += 1;
        Some(DownloadPermitGuard { sem: self })
    }
}

struct DecodedRingFrame {
    epoch_ms: i64,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

struct DecodedFrameRing {
    capacity: usize,
    frames: VecDeque<DecodedRingFrame>,
    cursor: usize,
    spare_buffers: Vec<Vec<u8>>,
}

impl DecodedFrameRing {
    fn new(capacity: usize) -> Self {
        let cap = capacity.max(2);
        Self {
            capacity: cap,
            frames: VecDeque::with_capacity(cap),
            cursor: 0,
            spare_buffers: Vec::with_capacity(4),
        }
    }

    fn clear_frames(&mut self) {
        while let Some(f) = self.frames.pop_front() {
            if self.spare_buffers.len() < 4 {
                self.spare_buffers.push(f.rgba);
            }
        }
        self.cursor = 0;
    }

    fn take_spare_or_alloc(&mut self, byte_len: usize) -> Vec<u8> {
        if let Some(mut buf) = self.spare_buffers.pop() {
            buf.resize(byte_len, 0);
            buf
        } else {
            vec![0u8; byte_len]
        }
    }

    fn push_live_frame(&mut self, epoch_ms: i64, width: u32, height: u32, src_rgba: &[u8]) {
        if self.frames.len() >= self.capacity {
            if let Some(evicted) = self.frames.pop_front() {
                if self.spare_buffers.len() < 4 {
                    self.spare_buffers.push(evicted.rgba);
                }
            }
        }
        let mut rgba = self.take_spare_or_alloc(src_rgba.len());
        rgba.copy_from_slice(src_rgba);
        self.frames.push_back(DecodedRingFrame {
            epoch_ms,
            width,
            height,
            rgba,
        });
        self.cursor = self.frames.len().saturating_sub(1);
    }
}

pub struct PlaybackSlot {
    pub channel: usize,
    pub buffer: Mutex<FrameBuffer>,
    pub frame_seq: AtomicU64,
    pub current_epoch_ms: AtomicI64,
    pub status: AtomicU8,
    pub fps_x10: AtomicU32,
    pub last_error: Mutex<Option<String>>,

    ring: Mutex<DecodedFrameRing>,
    step_forward_budget: AtomicU32,
    rewind_target_ms: AtomicI64,
    active_child: Mutex<Option<Child>>,
}

impl PlaybackSlot {
    fn new(channel: usize, width: u32, height: u32, ring_capacity: usize, start_epoch_ms: i64) -> Arc<Self> {
        Arc::new(Self {
            channel,
            buffer: Mutex::new(FrameBuffer {
                rgba: Vec::new(),
                width,
                height,
                subtype: 0,
            }),
            frame_seq: AtomicU64::new(0),
            current_epoch_ms: AtomicI64::new(start_epoch_ms),
            status: AtomicU8::new(PlaybackStatus::Buffering as u8),
            fps_x10: AtomicU32::new(0),
            last_error: Mutex::new(None),
            ring: Mutex::new(DecodedFrameRing::new(ring_capacity)),
            step_forward_budget: AtomicU32::new(0),
            rewind_target_ms: AtomicI64::new(i64::MIN),
            active_child: Mutex::new(None),
        })
    }

    pub fn status(&self) -> PlaybackStatus {
        PlaybackStatus::from_u8(self.status.load(Ordering::Acquire))
    }

    pub fn set_status(&self, st: PlaybackStatus) {
        self.status.store(st as u8, Ordering::Release);
    }

    pub fn set_error(&self, st: PlaybackStatus, msg: impl Into<String>) {
        if let Ok(mut guard) = self.last_error.lock() {
            *guard = Some(msg.into());
        }
        self.set_status(st);
    }

    pub fn clear_error(&self) {
        if let Ok(mut guard) = self.last_error.lock() {
            *guard = None;
        }
    }

    fn kill_active_child(&self) {
        if let Ok(mut guard) = self.active_child.lock() {
            if let Some(ref mut child) = *guard {
                let _ = child.kill();
            }
        }
    }

    fn publish_frame(&self, width: u32, height: u32, epoch_ms: i64, rgba_slice: &[u8], record_in_ring: bool) {
        // [MED-09] Hold `self.ring` lock across `self.buffer` update + `frame_seq` bump
        // so `publish_frame` and `try_step_in_ring` serialize cleanly without tearing.
        let mut ring_guard = self.ring.lock().ok();
        if record_in_ring {
            if let Some(ref mut ring) = ring_guard {
                ring.push_live_frame(epoch_ms, width, height, rgba_slice);
            }
        }
        if let Ok(mut fb) = self.buffer.lock() {
            fb.width = width;
            fb.height = height;
            fb.subtype = if width >= 1280 { 0 } else { 1 };
            if fb.rgba.len() != rgba_slice.len() {
                fb.rgba.resize(rgba_slice.len(), 0);
            }
            fb.rgba.copy_from_slice(rgba_slice);
        }
        self.current_epoch_ms.store(epoch_ms, Ordering::Release);
        let next_seq = NEXT_FRAME_SEQ.fetch_add(1, Ordering::Relaxed);
        self.frame_seq.store(next_seq, Ordering::Release);
        drop(ring_guard);
    }

    /// Attempts to step `-1` (backward in time) or `+1` (forward in time) inside the
    /// already-decoded RGBA ring buffer in `0ms`.
    /// Returns `Some(new_epoch_ms)` if satisfied purely from RAM, or `None` if an extra frame
    /// needs to be pulled from the decoder / previous keyframe.
    fn try_step_in_ring(&self, delta_frames: i32) -> Option<i64> {
        let mut ring = self.ring.lock().ok()?;
        if ring.frames.is_empty() {
            return None;
        }
        // [HIGH-16] Reverse ring frames are stored newest -> oldest; invert ring index step
        // so `,` (-1) always steps backward in time and `.` (+1) always steps forward in time.
        let is_reverse_ordered = ring.frames.len() >= 2
            && ring.frames.back().map(|f| f.epoch_ms) < ring.frames.front().map(|f| f.epoch_ms);
        let ring_step = if is_reverse_ordered {
            -(delta_frames as isize)
        } else {
            delta_frames as isize
        };
        let cur = ring.cursor as isize;
        let target = cur + ring_step;
        if target < 0 || target >= ring.frames.len() as isize {
            return None;
        }
        let idx = target as usize;
        ring.cursor = idx;
        let frame = &ring.frames[idx];
        let epoch_ms = frame.epoch_ms;
        let width = frame.width;
        let height = frame.height;

        if let Ok(mut fb) = self.buffer.lock() {
            fb.width = width;
            fb.height = height;
            fb.subtype = if width >= 1280 { 0 } else { 1 };
            if fb.rgba.len() != frame.rgba.len() {
                fb.rgba.resize(frame.rgba.len(), 0);
            }
            fb.rgba.copy_from_slice(&frame.rgba);
        }
        self.current_epoch_ms.store(epoch_ms, Ordering::Release);
        let next_seq = NEXT_FRAME_SEQ.fetch_add(1, Ordering::Relaxed);
        self.frame_seq.store(next_seq, Ordering::Release);
        Some(epoch_ms)
    }

    /// When unpausing after stepping backward via `,`, advances `ring.cursor` by 1 and copies
    /// the buffered RGBA frame into `dst_rgba` so remaining ring frames are presented smoothly
    /// before reading new frames from FFmpeg `stdout`.
    fn step_unpaused_ring_into(&self, dst_rgba: &mut Vec<u8>) -> Option<(i64, i64, u32, u32)> {
        let mut ring = self.ring.lock().ok()?;
        if ring.cursor + 1 >= ring.frames.len() {
            return None;
        }
        let prev_ep = ring.frames[ring.cursor].epoch_ms;
        ring.cursor += 1;
        let frame = &ring.frames[ring.cursor];
        let next_ep = frame.epoch_ms;
        let width = frame.width;
        let height = frame.height;
        if dst_rgba.len() != frame.rgba.len() {
            dst_rgba.resize(frame.rgba.len(), 0);
        }
        dst_rgba.copy_from_slice(&frame.rgba);
        Some((prev_ep, next_ep, width, height))
    }

    /// Accumulates decoded frames `< ceiling_ms` into `ring` during a `-600ms` backward `,` refill,
    /// and once `epoch_ms >= ceiling_ms` arrives, keeps `epoch_ms` at `cursor + 1` while publishing
    /// and pausing on the exact last frame `< ceiling_ms`.
    fn handle_rewind_frame(
        &self,
        width: u32,
        height: u32,
        epoch_ms: i64,
        rgba_slice: &[u8],
        ceiling_ms: i64,
    ) -> Option<i64> {
        let mut ring = self.ring.lock().ok()?;
        if epoch_ms < ceiling_ms {
            ring.push_live_frame(epoch_ms, width, height, rgba_slice);
            return None;
        }

        self.rewind_target_ms.store(i64::MIN, Ordering::Release);
        let had_preceding = !ring.frames.is_empty();
        ring.push_live_frame(epoch_ms, width, height, rgba_slice);
        let target_idx = if had_preceding && ring.frames.len() >= 2 {
            ring.frames.len() - 2
        } else {
            ring.frames.len().saturating_sub(1)
        };
        ring.cursor = target_idx;
        let chosen = &ring.frames[target_idx];
        let pub_ep = chosen.epoch_ms;
        let pub_w = chosen.width;
        let pub_h = chosen.height;

        if let Ok(mut fb) = self.buffer.lock() {
            fb.width = pub_w;
            fb.height = pub_h;
            fb.subtype = if pub_w >= 1280 { 0 } else { 1 };
            if fb.rgba.len() != chosen.rgba.len() {
                fb.rgba.resize(chosen.rgba.len(), 0);
            }
            fb.rgba.copy_from_slice(&chosen.rgba);
        }
        self.current_epoch_ms.store(pub_ep, Ordering::Release);
        let next_seq = NEXT_FRAME_SEQ.fetch_add(1, Ordering::Relaxed);
        self.frame_seq.store(next_seq, Ordering::Release);
        Some(pub_ep)
    }

    fn flush_pending_rewind_on_eof(&self) -> Option<i64> {
        if self.rewind_target_ms.swap(i64::MIN, Ordering::AcqRel) == i64::MIN {
            return None;
        }
        let mut ring = self.ring.lock().ok()?;
        if ring.frames.is_empty() {
            return None;
        }
        let target_idx = ring.frames.len() - 1;
        ring.cursor = target_idx;
        let chosen = &ring.frames[target_idx];
        let pub_ep = chosen.epoch_ms;
        let pub_w = chosen.width;
        let pub_h = chosen.height;

        if let Ok(mut fb) = self.buffer.lock() {
            fb.width = pub_w;
            fb.height = pub_h;
            fb.subtype = if pub_w >= 1280 { 0 } else { 1 };
            if fb.rgba.len() != chosen.rgba.len() {
                fb.rgba.resize(chosen.rgba.len(), 0);
            }
            fb.rgba.copy_from_slice(&chosen.rgba);
        }
        self.current_epoch_ms.store(pub_ep, Ordering::Release);
        let next_seq = NEXT_FRAME_SEQ.fetch_add(1, Ordering::Relaxed);
        self.frame_seq.store(next_seq, Ordering::Release);
        Some(pub_ep)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ClipExportState {
    pub in_progress: bool,
    pub label: String,
    pub last_result: Option<Result<PathBuf, String>>,
    pub channel: usize,
    pub progress_frac: f32,
    pub bytes_written: u64,
    pub stage: String,
    pub capped_to_max: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SmartSkipDecision {
    None,
    SetEffectiveSpeed(f32),
    JumpTo(i64),
}

pub fn matches_event_filter(kind: NvrEventKind, filter: Option<NvrEventKind>) -> bool {
    if kind == NvrEventKind::Continuous {
        return false;
    }
    match filter {
        None => true,
        Some(NvrEventKind::Intrusion) | Some(NvrEventKind::Alarm) => {
            matches!(kind, NvrEventKind::Intrusion | NvrEventKind::Alarm)
        }
        Some(target) => kind == target,
    }
}

#[cfg(test)]
pub fn evaluate_smart_skip(
    mode: SmartSkipMode,
    paused: bool,
    base_speed: f32,
    playhead_ms: i64,
    events: &[RecordedSpan],
    filter: Option<NvrEventKind>,
    cooldown_active: bool,
) -> SmartSkipDecision {
    evaluate_smart_skip_with_last_jump(
        mode,
        paused,
        base_speed,
        playhead_ms,
        events,
        filter,
        cooldown_active,
        None,
    )
    .0
}

#[allow(clippy::too_many_arguments)]
pub fn evaluate_smart_skip_with_last_jump(
    mode: SmartSkipMode,
    paused: bool,
    base_speed: f32,
    playhead_ms: i64,
    events: &[RecordedSpan],
    filter: Option<NvrEventKind>,
    cooldown_active: bool,
    last_jump_event_start_ms: Option<i64>,
) -> (SmartSkipDecision, Option<i64>) {
    if paused || mode == SmartSkipMode::Off || base_speed < 0.0 {
        return (SmartSkipDecision::None, None);
    }

    match mode {
        SmartSkipMode::Off => (SmartSkipDecision::None, None),
        SmartSkipMode::FastGlide8x => {
            let near_event = events.iter().any(|ev| {
                matches_event_filter(ev.kind, filter)
                    && playhead_ms >= ev.start_ms.saturating_sub(3_000)
                    && playhead_ms <= ev.end_ms.saturating_add(3_000)
            });
            let eff = if near_event {
                1.0
            } else {
                base_speed.max(8.0)
            };
            (SmartSkipDecision::SetEffectiveSpeed(eff), None)
        }
        SmartSkipMode::InstantJump => {
            if cooldown_active {
                return (SmartSkipDecision::None, None);
            }
            let inside_active = events.iter().any(|ev| {
                matches_event_filter(ev.kind, filter)
                    && playhead_ms >= ev.start_ms.saturating_sub(3_500)
                    && playhead_ms <= ev.end_ms.saturating_add(3_000)
            });
            if inside_active {
                return (SmartSkipDecision::None, None);
            }
            let next_start = events
                .iter()
                .filter(|ev| {
                    matches_event_filter(ev.kind, filter)
                        && ev.start_ms > playhead_ms.saturating_add(2_000)
                        && Some(ev.start_ms) != last_jump_event_start_ms
                })
                .map(|ev| ev.start_ms)
                .min();
            match next_start {
                Some(start_ms) => (
                    SmartSkipDecision::JumpTo((start_ms - 3_000).max(0)),
                    Some(start_ms),
                ),
                None => (SmartSkipDecision::None, None),
            }
        }
    }
}

pub fn snap_playback_speed(raw: f32) -> f32 {
    if !raw.is_finite() {
        return 1.0;
    }
    let mut best = 1.0_f32;
    let mut best_diff = f32::MAX;
    for &cand in &ALLOWED_SPEEDS {
        let d = (cand - raw).abs();
        if d < best_diff {
            best_diff = d;
            best = cand;
        }
    }
    best
}

struct PlaybackSharedControl {
    stop_flag: AtomicBool,
    paused: AtomicBool,
    generation: AtomicU64,
    target_epoch_ms: AtomicI64,
    master_playhead_ms: AtomicI64,
    effective_speed_bits: AtomicU32,
    primary_channel: AtomicUsize,
    decode_width: AtomicU32,
    decode_height: AtomicU32,
    download_semaphore: Arc<DownloadSemaphore>,
    wake_lock: Mutex<u64>,
    wake_cvar: Condvar,
}

impl PlaybackSharedControl {
    fn new(
        start_ms: i64,
        speed: f32,
        primary_ch: usize,
        w: u32,
        h: u32,
        download_semaphore: Arc<DownloadSemaphore>,
    ) -> Arc<Self> {
        Arc::new(Self {
            stop_flag: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            generation: AtomicU64::new(1),
            target_epoch_ms: AtomicI64::new(start_ms),
            master_playhead_ms: AtomicI64::new(start_ms),
            effective_speed_bits: AtomicU32::new(speed.to_bits()),
            primary_channel: AtomicUsize::new(primary_ch),
            decode_width: AtomicU32::new(w),
            decode_height: AtomicU32::new(h),
            download_semaphore,
            wake_lock: Mutex::new(0),
            wake_cvar: Condvar::new(),
        })
    }

    fn effective_speed(&self) -> f32 {
        let v = f32::from_bits(self.effective_speed_bits.load(Ordering::Acquire));
        if v.is_finite() && v.abs() >= 0.1 {
            v
        } else {
            1.0
        }
    }

    fn set_effective_speed(&self, speed: f32) {
        self.effective_speed_bits
            .store(speed.to_bits(), Ordering::Release);
        self.notify_wake();
    }

    fn notify_wake(&self) {
        if let Ok(mut g) = self.wake_lock.lock() {
            *g = g.wrapping_add(1);
            self.wake_cvar.notify_all();
        }
    }

    fn sleep_interruptible(&self, dur: Duration, expected_gen: u64) {
        if dur.is_zero() || self.stop_flag.load(Ordering::Acquire) {
            return;
        }
        let Ok(guard) = self.wake_lock.lock() else {
            return;
        };
        let seen = *guard;
        let _ = self.wake_cvar.wait_timeout_while(guard, dur, |g| {
            *g == seen
                && !self.stop_flag.load(Ordering::Acquire)
                && self.generation.load(Ordering::Acquire) == expected_gen
        });
    }
}

struct TimelineCacheSlot {
    index: Option<DayTimelineIndex>,
    loading: bool,
    fetched_at: Option<Instant>,
}

struct MonthDaysCacheSlot {
    days: Option<Vec<u32>>,
    error: Option<String>,
    loading: bool,
    fetched_at: Option<Instant>,
}

type TimelineCacheMap = HashMap<(usize, NaiveDate), TimelineCacheSlot>;
type MonthDaysCacheMap = HashMap<(usize, i32, u32), MonthDaysCacheSlot>;

pub struct PlaybackManager {
    env_cfg: Arc<RwLock<NvrEnvConfig>>,
    p2p_ready: Arc<AtomicBool>,
    auth_gw: Arc<AuthGateway>,
    nvr_http: Arc<NvrHttpClient>,

    active: bool,
    sync_2x2: bool,
    active_channels: Vec<usize>,
    primary_ch: Option<usize>,
    slots: BTreeMap<usize, Arc<PlaybackSlot>>,
    workers: Vec<JoinHandle<()>>,
    shared_ctrl: Option<Arc<PlaybackSharedControl>>,
    segment_cache: Arc<Mutex<MemfdSegmentCache>>,
    download_semaphore: Arc<DownloadSemaphore>,

    speed: f32,
    effective_speed: f32,
    paused: bool,
    smart_skip_mode: SmartSkipMode,
    last_smart_jump_at: Option<Instant>,
    last_smart_jump_event_start_ms: Option<i64>,
    pending_seek: Option<(i64, Instant)>,
    last_drag_dispatch_at: Option<Instant>,
    ui_playhead_ms: i64,

    timeline_cache: Arc<Mutex<TimelineCacheMap>>,
    month_cache: Arc<Mutex<MonthDaysCacheMap>>,
    export_state: Arc<Mutex<ClipExportState>>,
    export_cancel_flag: Arc<AtomicBool>,
    export_active_child: Arc<Mutex<Option<Child>>>,
    export_active_job_id: Arc<AtomicU64>,
    export_worker: Option<JoinHandle<()>>,
}

impl PlaybackManager {
    pub fn new(
        env_cfg: NvrEnvConfig,
        p2p_ready: Arc<AtomicBool>,
        auth_gw: Arc<AuthGateway>,
        nvr_http: Arc<NvrHttpClient>,
    ) -> Self {
        let now_ms = current_wall_epoch_ms();
        let max_dl = if env_cfg.is_cloud_p2p_active() { 1 } else { 4 };
        Self {
            env_cfg: Arc::new(RwLock::new(env_cfg)),
            p2p_ready,
            auth_gw,
            nvr_http,
            active: false,
            sync_2x2: false,
            active_channels: Vec::new(),
            primary_ch: None,
            slots: BTreeMap::new(),
            workers: Vec::new(),
            shared_ctrl: None,
            segment_cache: Arc::new(Mutex::new(MemfdSegmentCache::new(MAX_CACHE_BYTES))),
            download_semaphore: DownloadSemaphore::new(max_dl),
            speed: 1.0,
            effective_speed: 1.0,
            paused: false,
            smart_skip_mode: SmartSkipMode::Off,
            last_smart_jump_at: None,
            last_smart_jump_event_start_ms: None,
            pending_seek: None,
            last_drag_dispatch_at: None,
            ui_playhead_ms: now_ms.saturating_sub(300_000),
            timeline_cache: Arc::new(Mutex::new(HashMap::new())),
            month_cache: Arc::new(Mutex::new(HashMap::new())),
            export_state: Arc::new(Mutex::new(ClipExportState::default())),
            export_cancel_flag: Arc::new(AtomicBool::new(false)),
            export_active_child: Arc::new(Mutex::new(None)),
            export_active_job_id: Arc::new(AtomicU64::new(0)),
            export_worker: None,
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn is_sync_2x2(&self) -> bool {
        self.active && self.sync_2x2
    }

    pub fn active_channels(&self) -> &[usize] {
        &self.active_channels
    }

    pub fn primary_channel(&self) -> Option<usize> {
        self.primary_ch
            .filter(|c| self.active_channels.contains(c))
            .or_else(|| self.active_channels.first().copied())
    }

    pub fn set_primary_channel(&mut self, channel: usize) {
        if !self.active || !self.active_channels.contains(&channel) {
            return;
        }
        self.primary_ch = Some(channel);
        self.last_smart_jump_event_start_ms = None;
        if let Some(ref ctrl) = self.shared_ctrl {
            ctrl.primary_channel.store(channel, Ordering::Release);
            if let Some(slot) = self.slots.get(&channel) {
                let ch_ms = slot.current_epoch_ms.load(Ordering::Acquire);
                ctrl.master_playhead_ms.store(ch_ms, Ordering::Release);
                self.ui_playhead_ms = ch_ms;
            }
            ctrl.notify_wake();
        }
        let cur_date = epoch_ms_to_local_naive(self.playhead_epoch_ms()).date();
        self.request_day_timeline(channel, cur_date, false);
        self.request_month_days(channel, cur_date.year(), cur_date.month());
    }

    #[allow(dead_code)]
    pub fn swap_sync_channels(&mut self, ch_a: usize, ch_b: usize) {
        if ch_a == ch_b {
            return;
        }
        if let (Some(idx_a), Some(idx_b)) = (
            self.active_channels.iter().position(|&c| c == ch_a),
            self.active_channels.iter().position(|&c| c == ch_b),
        ) {
            self.active_channels.swap(idx_a, idx_b);
        }
    }

    #[allow(dead_code)]
    pub fn replace_sync_channel(&mut self, old_ch: usize, new_ch: usize) {
        if !self.is_sync_2x2() || new_ch < 1 || self.active_channels.contains(&new_ch) {
            return;
        }
        let Some(idx) = self.active_channels.iter().position(|&c| c == old_ch) else {
            return;
        };
        let resume_ms = self.playhead_epoch_ms();
        let mut updated = self.active_channels.clone();
        updated[idx] = new_ch;
        let next_primary = match self.primary_channel() {
            Some(p) if p == old_ch => Some(new_ch),
            other => other,
        };
        self.start_session_with_options(updated, resume_ms, true, next_primary, true);
    }

    pub fn playhead_epoch_ms(&self) -> i64 {
        if self.pending_seek.is_some() {
            return self.ui_playhead_ms;
        }
        if let Some(ref ctrl) = self.shared_ctrl {
            ctrl.master_playhead_ms.load(Ordering::Acquire)
        } else {
            self.ui_playhead_ms
        }
    }

    pub fn speed(&self) -> f32 {
        self.speed
    }

    pub fn effective_speed(&self) -> f32 {
        self.effective_speed
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn smart_skip_mode(&self) -> SmartSkipMode {
        self.smart_skip_mode
    }

    pub fn slot(&self, channel: usize) -> Option<Arc<PlaybackSlot>> {
        self.slots.get(&channel).cloned()
    }

    pub fn day_timeline(&self, channel: usize, date: NaiveDate) -> Option<DayTimelineIndex> {
        self.timeline_cache
            .lock()
            .ok()
            .and_then(|map| map.get(&(channel, date)).and_then(|s| s.index.clone()))
    }

    #[allow(dead_code)]
    pub fn day_timelines_for_range(
        &self,
        channel: usize,
        start_ms: i64,
        end_ms: i64,
    ) -> Vec<DayTimelineIndex> {
        let (lo_ms, hi_ms) = if end_ms >= start_ms {
            (start_ms.max(0), end_ms.max(0))
        } else {
            (end_ms.max(0), start_ms.max(0))
        };
        let start_date = epoch_ms_to_local_naive(lo_ms).date();
        let end_date = epoch_ms_to_local_naive(hi_ms).date();

        let Ok(map) = self.timeline_cache.lock() else {
            return Vec::new();
        };
        let mut out: Vec<DayTimelineIndex> = map
            .iter()
            .filter_map(|(&(ch, d), slot)| {
                if ch == channel && d >= start_date && d <= end_date {
                    slot.index.clone()
                } else {
                    None
                }
            })
            .collect();
        out.sort_by_key(|idx| idx.date);
        out
    }

    #[allow(dead_code)]
    pub fn ensure_timelines_for_range(&mut self, channel: usize, start_ms: i64, end_ms: i64) {
        if channel == 0 {
            return;
        }
        let (lo_ms, hi_ms) = if end_ms >= start_ms {
            (start_ms.max(0), end_ms.max(0))
        } else {
            (end_ms.max(0), start_ms.max(0))
        };
        let start_date = epoch_ms_to_local_naive(lo_ms).date();
        let end_date = epoch_ms_to_local_naive(hi_ms).date();
        let max_future_date = chrono::Local::now()
            .date_naive()
            .succ_opt()
            .unwrap_or(end_date);
        let capped_end = end_date.min(max_future_date);

        let mut to_request = Vec::with_capacity(3);
        let mut cur = start_date;
        let mut steps = 0usize;
        while cur <= capped_end && to_request.len() < 3 && steps < 32 {
            let already_present = self
                .timeline_cache
                .lock()
                .ok()
                .and_then(|map| {
                    map.get(&(channel, cur))
                        .map(|s| s.loading || s.index.is_some())
                })
                .unwrap_or(false);
            if !already_present {
                to_request.push(cur);
            }
            let Some(next_d) = cur.succ_opt() else {
                break;
            };
            cur = next_d;
            steps += 1;
        }
        for d in to_request {
            self.request_day_timeline(channel, d, false);
        }
    }

    pub fn is_timeline_loading(&self, channel: usize, date: NaiveDate) -> bool {
        self.timeline_cache
            .lock()
            .ok()
            .and_then(|map| map.get(&(channel, date)).map(|s| s.loading))
            .unwrap_or(false)
    }

    pub fn request_day_timeline(&mut self, channel: usize, date: NaiveDate, force_refresh: bool) {
        if channel == 0 {
            return;
        }
        {
            let Ok(mut map) = self.timeline_cache.lock() else {
                return;
            };
            if let Some(existing) = map.get_mut(&(channel, date)) {
                if existing.loading {
                    return;
                }
                if !force_refresh && existing.index.is_some() {
                    let is_today = date == chrono::Local::now().date_naive();
                    let fresh_enough = existing
                        .fetched_at
                        .map(|t| t.elapsed() < Duration::from_secs(if is_today { 45 } else { 600 }))
                        .unwrap_or(false);
                    if fresh_enough {
                        return;
                    }
                }
                existing.loading = true;
            } else {
                map.insert(
                    (channel, date),
                    TimelineCacheSlot {
                        index: None,
                        loading: true,
                        fetched_at: None,
                    },
                );
            }
        }

        let env_snapshot = self.env_cfg.read().unwrap().clone();
        let http = Arc::clone(&self.nvr_http);
        let cache = Arc::clone(&self.timeline_cache);
        let _ = thread::Builder::new()
            .name(format!("nvr-tl-ch{}", channel))
            .spawn(move || {
                let result = block_on_nvr_io(http.query_day_timeline(&env_snapshot, channel, date));
                let resolved = match result {
                    Ok(idx) => idx,
                    Err(err) => DayTimelineIndex {
                        channel,
                        date,
                        coverage: Vec::new(),
                        events: Vec::new(),
                        api_supported: false,
                        note: Some(redact_secret_in_text(&err, &env_snapshot.password)),
                    },
                };
                if let Ok(mut map) = cache.lock() {
                    map.insert(
                        (channel, date),
                        TimelineCacheSlot {
                            index: Some(resolved),
                            loading: false,
                            fetched_at: Some(Instant::now()),
                        },
                    );
                }
            });
    }

    pub fn recorded_days_in_month(&self, channel: usize, year: i32, month: u32) -> Option<Vec<u32>> {
        self.month_cache
            .lock()
            .ok()
            .and_then(|map| map.get(&(channel, year, month)).and_then(|s| s.days.clone()))
    }

    #[allow(dead_code)]
    pub fn is_month_days_loading(&self, channel: usize, year: i32, month: u32) -> bool {
        self.month_cache
            .lock()
            .ok()
            .and_then(|map| map.get(&(channel, year, month)).map(|s| s.loading))
            .unwrap_or(false)
    }

    #[allow(dead_code)]
    pub fn month_days_error(&self, channel: usize, year: i32, month: u32) -> Option<String> {
        self.month_cache
            .lock()
            .ok()
            .and_then(|map| map.get(&(channel, year, month)).and_then(|s| s.error.clone()))
    }

    pub fn request_month_days(&mut self, channel: usize, year: i32, month: u32) {
        self.request_month_days_ex(channel, year, month, false);
    }

    pub fn request_month_days_ex(
        &mut self,
        channel: usize,
        year: i32,
        month: u32,
        force_refresh: bool,
    ) {
        if channel == 0 || !(1..=12).contains(&month) {
            return;
        }
        {
            let Ok(mut map) = self.month_cache.lock() else {
                return;
            };
            if let Some(existing) = map.get_mut(&(channel, year, month)) {
                if existing.loading {
                    return;
                }
                if !force_refresh {
                    if existing.days.is_some()
                        && existing
                            .fetched_at
                            .map(|t| t.elapsed() < Duration::from_secs(180))
                            .unwrap_or(false)
                    {
                        return;
                    }
                    if existing.error.is_some()
                        && existing
                            .fetched_at
                            .map(|t| t.elapsed() < Duration::from_secs(30))
                            .unwrap_or(false)
                    {
                        return;
                    }
                }
                existing.error = None;
                existing.loading = true;
            } else {
                map.insert(
                    (channel, year, month),
                    MonthDaysCacheSlot {
                        days: None,
                        error: None,
                        loading: true,
                        fetched_at: None,
                    },
                );
            }
        }

        let env_snapshot = self.env_cfg.read().unwrap().clone();
        let http = Arc::clone(&self.nvr_http);
        let cache = Arc::clone(&self.month_cache);
        let _ = thread::Builder::new()
            .name(format!("nvr-cal-ch{}", channel))
            .spawn(move || {
                let res = block_on_nvr_io(
                    http.query_month_recorded_days(&env_snapshot, channel, year, month),
                );
                let (days, error) = match res {
                    Ok(d) => (Some(d), None),
                    Err(e) => (None, Some(redact_secret_in_text(&e, &env_snapshot.password))),
                };
                if let Ok(mut map) = cache.lock() {
                    map.insert(
                        (channel, year, month),
                        MonthDaysCacheSlot {
                            days,
                            error,
                            loading: false,
                            fetched_at: Some(Instant::now()),
                        },
                    );
                }
            });
    }

    #[allow(dead_code)]
    pub fn cached_ranges_for_channel(
        &self,
        channel: usize,
        view_start_ms: i64,
        view_end_ms: i64,
    ) -> Vec<(i64, i64)> {
        self.segment_cache
            .lock()
            .map(|cache| cache.cached_ranges_for_channel(channel, view_start_ms, view_end_ms))
            .unwrap_or_default()
    }

    pub fn open_single(&mut self, channel: usize, start_epoch_ms: i64) {
        self.start_session(vec![channel.max(1)], start_epoch_ms, false);
    }

    pub fn open_sync_grid(&mut self, channels: Vec<usize>, start_epoch_ms: i64) {
        self.start_session(channels, start_epoch_ms, true);
    }

    fn start_session(
        &mut self,
        requested_channels: Vec<usize>,
        start_epoch_ms: i64,
        want_sync_grid: bool,
    ) {
        self.start_session_with_options(requested_channels, start_epoch_ms, want_sync_grid, None, false);
    }

    fn start_session_with_options(
        &mut self,
        requested_channels: Vec<usize>,
        start_epoch_ms: i64,
        want_sync_grid: bool,
        preferred_primary: Option<usize>,
        preserve_pause_and_speed: bool,
    ) {
        let saved_paused = self.paused;
        let saved_speed = self.speed;
        let saved_eff = self.effective_speed;

        self.stop_workers_keep_cache();

        let is_cloud = self.env_cfg.read().unwrap().is_cloud_p2p_active();
        self.download_semaphore
            .set_max_permits(if is_cloud { 1 } else { 4 });

        let mut clamped = NvrResourceGovernor::clamp_channels(is_cloud, &requested_channels);
        if clamped.is_empty() {
            clamped.push(1);
        }
        if !want_sync_grid {
            clamped.truncate(1);
        }
        let sync_2x2 = want_sync_grid && !is_cloud && clamped.len() > 1;
        let gov = NvrResourceGovernor::compute(is_cloud, if sync_2x2 { clamped.len() } else { 1 });

        let clamped_start = start_epoch_ms.max(0);
        let primary_ch = preferred_primary
            .filter(|c| clamped.contains(c))
            .unwrap_or(clamped[0]);

        self.active = true;
        self.sync_2x2 = sync_2x2;
        self.active_channels = clamped.clone();
        self.primary_ch = Some(primary_ch);
        if preserve_pause_and_speed {
            self.paused = saved_paused;
            self.speed = saved_speed;
            self.effective_speed = saved_eff;
        } else {
            self.paused = false;
            if self.speed < 0.0 {
                self.speed = 1.0;
            }
            self.effective_speed = self.speed;
        }
        self.last_smart_jump_event_start_ms = None;
        self.pending_seek = None;
        self.last_drag_dispatch_at = None;
        self.ui_playhead_ms = clamped_start;

        let ctrl = PlaybackSharedControl::new(
            clamped_start,
            self.effective_speed,
            primary_ch,
            gov.decode_width,
            gov.decode_height,
            Arc::clone(&self.download_semaphore),
        );
        if self.paused {
            ctrl.paused.store(true, Ordering::Release);
        }

        let start_date = epoch_ms_to_local_naive(clamped_start).date();
        for &ch in &clamped {
            self.request_day_timeline(ch, start_date, false);
            self.request_month_days(ch, start_date.year(), start_date.month());
        }

        for &ch in &clamped {
            let slot = PlaybackSlot::new(
                ch,
                gov.decode_width,
                gov.decode_height,
                gov.ring_capacity,
                clamped_start,
            );
            if self.paused {
                slot.step_forward_budget.store(1, Ordering::Release);
            }
            self.slots.insert(ch, Arc::clone(&slot));

            let worker_ctx = ChannelWorkerContext {
                slot,
                ctrl: Arc::clone(&ctrl),
                env_cfg: Arc::clone(&self.env_cfg),
                p2p_ready: Arc::clone(&self.p2p_ready),
                auth_gw: Arc::clone(&self.auth_gw),
                nvr_http: Arc::clone(&self.nvr_http),
                segment_cache: Arc::clone(&self.segment_cache),
                timeline_cache: Arc::clone(&self.timeline_cache),
                allow_prefetch: gov.allow_prefetch,
                is_sync_2x2: sync_2x2,
            };
            let handle = thread::Builder::new()
                .name(format!("nvr-pb-ch{}", ch))
                .spawn(move || playback_channel_worker_loop(worker_ctx))
                .expect("Failed to spawn playback channel worker");
            self.workers.push(handle);
        }

        self.shared_ctrl = Some(ctrl);
    }

    fn stop_workers_keep_cache(&mut self) {
        if let Some(ref ctrl) = self.shared_ctrl {
            ctrl.stop_flag.store(true, Ordering::Release);
            ctrl.generation.fetch_add(1, Ordering::AcqRel);
            ctrl.notify_wake();
        }
        self.download_semaphore.notify_all();
        for slot in self.slots.values() {
            slot.kill_active_child();
        }
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
        self.slots.clear();
        self.shared_ctrl = None;
        self.pending_seek = None;
        self.last_drag_dispatch_at = None;
    }

    pub fn close(&mut self) {
        self.stop_workers_keep_cache();
        if let Ok(mut cache) = self.segment_cache.lock() {
            cache.clear();
        }
        self.active = false;
        self.sync_2x2 = false;
        self.active_channels.clear();
        self.primary_ch = None;
        self.paused = false;
        self.speed = 1.0;
        self.effective_speed = 1.0;
        self.last_smart_jump_event_start_ms = None;
    }

    pub fn seek(&mut self, target_epoch_ms: i64, immediate: bool) {
        let clamped = target_epoch_ms.max(0);
        self.ui_playhead_ms = clamped;
        self.last_smart_jump_event_start_ms = None;

        if !self.active {
            return;
        }

        if immediate {
            self.pending_seek = None;
            self.last_drag_dispatch_at = Some(Instant::now());
            self.dispatch_seek(clamped);
        } else {
            if let Some((prev_ms, _)) = self.pending_seek {
                if (clamped - prev_ms).abs() <= 25 {
                    return;
                }
            }
            let throttle_ready = self
                .last_drag_dispatch_at
                .map(|t| t.elapsed() >= Duration::from_millis(SEEK_DEBOUNCE_MS))
                .unwrap_or(true);
            let moved_enough = self
                .shared_ctrl
                .as_ref()
                .map(|c| (clamped - c.target_epoch_ms.load(Ordering::Acquire)).abs() >= 250)
                .unwrap_or(true);
            if throttle_ready && moved_enough {
                self.pending_seek = None;
                self.last_drag_dispatch_at = Some(Instant::now());
                self.dispatch_seek(clamped);
            } else {
                self.pending_seek = Some((clamped, Instant::now()));
            }
        }
    }

    fn dispatch_seek(&mut self, target_epoch_ms: i64) {
        let clamped = target_epoch_ms.max(0);
        self.ui_playhead_ms = clamped;

        let target_date = epoch_ms_to_local_naive(clamped).date();
        let active_chs = self.active_channels.clone();
        for ch in active_chs {
            self.request_day_timeline(ch, target_date, false);
        }

        if let Some(ref ctrl) = self.shared_ctrl {
            let paused_preview_budget = if ctrl.paused.load(Ordering::Acquire) {
                1
            } else {
                0
            };
            ctrl.target_epoch_ms.store(clamped, Ordering::Release);
            ctrl.master_playhead_ms.store(clamped, Ordering::Release);
            ctrl.generation.fetch_add(1, Ordering::AcqRel);
            for slot in self.slots.values() {
                slot.step_forward_budget
                    .store(paused_preview_budget, Ordering::Release);
                slot.rewind_target_ms.store(i64::MIN, Ordering::Release);
                if let Ok(mut ring) = slot.ring.lock() {
                    ring.clear_frames();
                }
                slot.current_epoch_ms.store(clamped, Ordering::Release);
                slot.set_status(PlaybackStatus::Buffering);
                slot.kill_active_child();
            }
            ctrl.notify_wake();
        }
    }

    pub fn tick(&mut self, event_filter: Option<NvrEventKind>) {
        if !self.active {
            return;
        }

        // 1. Flush 200ms debounced drag seek if matured
        if let Some((target_ms, queued_at)) = self.pending_seek {
            if queued_at.elapsed() >= Duration::from_millis(SEEK_DEBOUNCE_MS) {
                self.pending_seek = None;
                self.last_drag_dispatch_at = Some(Instant::now());
                self.dispatch_seek(target_ms);
            }
        }

        let playhead_ms = self.playhead_epoch_ms();
        self.ui_playhead_ms = playhead_ms;

        // Ensure day timeline is loaded if playhead crossed midnight (and prefetch next day within 2m of midnight)
        let cur_dt = epoch_ms_to_local_naive(playhead_ms);
        let cur_date = cur_dt.date();
        if let Some(primary_ch) = self.primary_channel() {
            if self.day_timeline(primary_ch, cur_date).is_none()
                && !self.is_timeline_loading(primary_ch, cur_date)
            {
                self.request_day_timeline(primary_ch, cur_date, false);
            }
            if cur_dt.time().num_seconds_from_midnight() >= 86_400 - 120 {
                if let Some(succ_date) = cur_date.succ_opt() {
                    if self.day_timeline(primary_ch, succ_date).is_none()
                        && !self.is_timeline_loading(primary_ch, succ_date)
                    {
                        self.request_day_timeline(primary_ch, succ_date, false);
                    }
                }
            }
        }

        // 2. Run Smart Skip (FastGlide8x & InstantJump)
        if self.smart_skip_mode == SmartSkipMode::Off || self.paused || self.speed < 0.0 {
            if (self.effective_speed - self.speed).abs() > 0.01 {
                self.apply_effective_speed(self.speed);
            }
            return;
        }

        let Some(primary_ch) = self.primary_channel() else {
            return;
        };
        let Some(cur_tl) = self.day_timeline(primary_ch, cur_date) else {
            if (self.effective_speed - self.speed).abs() > 0.01 {
                self.apply_effective_speed(self.speed);
            }
            return;
        };
        if !cur_tl.api_supported {
            if (self.effective_speed - self.speed).abs() > 0.01 {
                self.apply_effective_speed(self.speed);
            }
            return;
        }

        // [HIGH-15] Gather events across [yesterday, today, tomorrow] so midnight-crossing spans are never missed.
        let mut combined_events: Vec<RecordedSpan> = Vec::new();
        for date_cand in [cur_date.pred_opt(), Some(cur_date), cur_date.succ_opt()]
            .into_iter()
            .flatten()
        {
            if let Some(tl) = self.day_timeline(primary_ch, date_cand) {
                if tl.api_supported {
                    combined_events.extend(tl.events);
                }
            }
        }

        let cooldown_active = self
            .last_smart_jump_at
            .map(|t| t.elapsed() < Duration::from_millis(1500))
            .unwrap_or(false);

        let (decision, jumped_ev_start) = evaluate_smart_skip_with_last_jump(
            self.smart_skip_mode,
            self.paused,
            self.speed,
            playhead_ms,
            &combined_events,
            event_filter,
            cooldown_active,
            self.last_smart_jump_event_start_ms,
        );

        match decision {
            SmartSkipDecision::None => {
                if self.smart_skip_mode != SmartSkipMode::FastGlide8x
                    && (self.effective_speed - self.speed).abs() > 0.01
                {
                    self.apply_effective_speed(self.speed);
                }
            }
            SmartSkipDecision::SetEffectiveSpeed(eff) => {
                if (self.effective_speed - eff).abs() > 0.01 {
                    self.apply_effective_speed(eff);
                }
            }
            SmartSkipDecision::JumpTo(target_ms) => {
                self.last_smart_jump_at = Some(Instant::now());
                if (self.effective_speed - self.speed).abs() > 0.01 {
                    self.apply_effective_speed(self.speed);
                }
                self.seek(target_ms, true);
                self.last_smart_jump_event_start_ms = jumped_ev_start;
            }
        }
    }

    fn apply_effective_speed(&mut self, new_eff: f32) {
        let old_eff = self.effective_speed;
        self.effective_speed = new_eff;
        let sign_flipped = (old_eff < 0.0) != (new_eff < 0.0);
        if let Some(ref ctrl) = self.shared_ctrl {
            ctrl.set_effective_speed(new_eff);
        }
        if sign_flipped && self.active {
            let cur = self.playhead_epoch_ms();
            self.dispatch_seek(cur);
        }
    }

    pub fn set_paused(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        if let Some(ref ctrl) = self.shared_ctrl {
            ctrl.paused.store(paused, Ordering::Release);
            for slot in self.slots.values() {
                if paused {
                    slot.fps_x10.store(0, Ordering::Release);
                    if slot.status() == PlaybackStatus::Playing {
                        slot.set_status(PlaybackStatus::Paused);
                    }
                } else if slot.status() == PlaybackStatus::Paused {
                    slot.set_status(PlaybackStatus::Playing);
                }
            }
            ctrl.notify_wake();
        }
    }

    pub fn toggle_pause(&mut self) {
        self.set_paused(!self.paused);
    }

    pub fn set_speed(&mut self, speed: f32) {
        let snapped = snap_playback_speed(speed);
        self.speed = snapped;
        if snapped < 0.0 && self.smart_skip_mode != SmartSkipMode::Off {
            self.smart_skip_mode = SmartSkipMode::Off;
        }
        if self.smart_skip_mode != SmartSkipMode::FastGlide8x || self.paused || snapped < 0.0 {
            self.apply_effective_speed(snapped);
        }
        if self.paused {
            self.set_paused(false);
        }
    }

    pub fn step_speed(&mut self, faster: bool) {
        let cur = snap_playback_speed(self.speed);
        let idx = ALLOWED_SPEEDS
            .iter()
            .position(|&s| (s - cur).abs() < 0.01)
            .unwrap_or(4);
        let new_idx = if faster {
            (idx + 1).min(ALLOWED_SPEEDS.len() - 1)
        } else {
            idx.saturating_sub(1)
        };
        self.set_speed(ALLOWED_SPEEDS[new_idx]);
    }

    pub fn step_frame(&mut self, delta_frames: i32) {
        if !self.active || delta_frames == 0 {
            return;
        }
        let time_dir = if delta_frames > 0 { 1 } else { -1 };
        if !self.paused {
            self.set_paused(true);
        }

        let reverse_mode = self.effective_speed < 0.0;
        let want_decoder_pull = (!reverse_mode && time_dir > 0) || (reverse_mode && time_dir < 0);

        let primary_ch = self.primary_channel();
        let cur_ms = primary_ch
            .and_then(|ch| self.slots.get(&ch))
            .map(|s| s.current_epoch_ms.load(Ordering::Acquire))
            .unwrap_or_else(|| self.playhead_epoch_ms());

        let mut primary_stepped_epoch = None;
        let mut need_decoder_pull_for: Vec<Arc<PlaybackSlot>> = Vec::new();
        let mut exhausted_backward = false;
        let mut exhausted_reverse_forward = false;

        for (&ch, slot) in &self.slots {
            if let Some(ep) = slot.try_step_in_ring(time_dir) {
                if Some(ch) == primary_ch {
                    primary_stepped_epoch = Some(ep);
                }
            } else if want_decoder_pull {
                need_decoder_pull_for.push(Arc::clone(slot));
            } else if Some(ch) == primary_ch {
                if !reverse_mode {
                    exhausted_backward = true;
                } else {
                    exhausted_reverse_forward = true;
                }
            }
        }

        if let Some(ep) = primary_stepped_epoch {
            self.ui_playhead_ms = ep;
            if let Some(ref ctrl) = self.shared_ctrl {
                ctrl.master_playhead_ms.store(ep, Ordering::Release);
            }
        }

        // [HIGH-16] Grant decoder pull budget to any secondary 2x2 slots that hit their ring end
        // even when the primary slot stepped in RAM.
        if want_decoder_pull && !need_decoder_pull_for.is_empty() {
            for slot in need_decoder_pull_for {
                slot.step_forward_budget.fetch_add(1, Ordering::AcqRel);
            }
            if let Some(ref ctrl) = self.shared_ctrl {
                ctrl.notify_wake();
            }
        } else if primary_stepped_epoch.is_none() && exhausted_backward && cur_ms > 0 {
            // [HIGH-16] Stepped past oldest frame in RAM ring: seek back 600ms and refill `[cur_ms - 600, cur_ms)`
            // into `ring`, pausing automatically on the last frame `< cur_ms`.
            let target = cur_ms.saturating_sub(600).max(0);
            self.dispatch_seek(target);
            self.ui_playhead_ms = cur_ms;
            if let Some(ref ctrl) = self.shared_ctrl {
                ctrl.master_playhead_ms.store(cur_ms, Ordering::Release);
            }
            for slot in self.slots.values() {
                slot.step_forward_budget.store(0, Ordering::Release);
                slot.rewind_target_ms.store(cur_ms, Ordering::Release);
            }
            if let Some(ref ctrl) = self.shared_ctrl {
                ctrl.notify_wake();
            }
        } else if primary_stepped_epoch.is_none() && exhausted_reverse_forward {
            let target = (cur_ms + 2_000).min(current_wall_epoch_ms());
            self.dispatch_seek(target);
            for slot in self.slots.values() {
                slot.step_forward_budget.store(1, Ordering::Release);
            }
            if let Some(ref ctrl) = self.shared_ctrl {
                ctrl.notify_wake();
            }
        }
    }

    pub fn set_smart_skip_mode(&mut self, mode: SmartSkipMode) {
        self.smart_skip_mode = mode;
        if mode != SmartSkipMode::FastGlide8x && (self.effective_speed - self.speed).abs() > 0.01 {
            self.apply_effective_speed(self.speed);
        }
    }

    pub fn cycle_smart_skip_mode(&mut self) -> SmartSkipMode {
        let next = self.smart_skip_mode.next();
        self.set_smart_skip_mode(next);
        next
    }

    pub fn jump_adjacent_event(&mut self, next: bool, filter: Option<NvrEventKind>) -> Option<i64> {
        let ch = self.primary_channel()?;
        let cur_ms = self.playhead_epoch_ms();
        let cur_date = epoch_ms_to_local_naive(cur_ms).date();

        let mut candidates: Vec<RecordedSpan> = Vec::new();
        for date_cand in [cur_date.pred_opt(), Some(cur_date), cur_date.succ_opt()]
            .into_iter()
            .flatten()
        {
            if let Some(tl) = self.day_timeline(ch, date_cand) {
                for ev in tl.events {
                    if matches_event_filter(ev.kind, filter) {
                        candidates.push(ev);
                    }
                }
            }
        }
        if candidates.is_empty() {
            return None;
        }
        candidates.sort_by_key(|ev| ev.start_ms);

        let target_ms = if next {
            candidates
                .iter()
                .find(|ev| ev.start_ms.saturating_sub(2_000) > cur_ms.saturating_add(1_200))
                .map(|ev| ev.start_ms.saturating_sub(2_000).max(0))
        } else {
            candidates
                .iter()
                .rev()
                .find(|ev| ev.start_ms.saturating_sub(2_000) < cur_ms.saturating_sub(2_500))
                .map(|ev| ev.start_ms.saturating_sub(2_000).max(0))
        }?;

        self.seek(target_ms, true);
        Some(target_ms)
    }

    pub fn update_env(&mut self, new_env: NvrEnvConfig) {
        let is_cloud = new_env.is_cloud_p2p_active();
        self.download_semaphore
            .set_max_permits(if is_cloud { 1 } else { 4 });
        {
            let mut guard = self.env_cfg.write().unwrap();
            *guard = new_env;
        }
        if let Ok(mut t_map) = self.timeline_cache.lock() {
            t_map.clear();
        }
        if let Ok(mut m_map) = self.month_cache.lock() {
            m_map.clear();
        }
        if self.active {
            let cur_ms = self.playhead_epoch_ms();
            let chs = self.active_channels.clone();
            let preferred_primary = self.primary_channel();
            let want_grid = self.sync_2x2 && !is_cloud;
            self.start_session_with_options(chs, cur_ms, want_grid, preferred_primary, true);
        }
    }

    pub fn start_clip_export(
        &mut self,
        channel: usize,
        camera_label: &str,
        recording_dir: &Path,
        in_epoch_ms: i64,
        out_epoch_ms: i64,
    ) {
        let (start_ms, end_raw) = if out_epoch_ms >= in_epoch_ms {
            (in_epoch_ms, out_epoch_ms)
        } else {
            (out_epoch_ms, in_epoch_ms)
        };
        if end_raw - start_ms < 500 {
            if let Ok(mut st) = self.export_state.lock() {
                st.in_progress = false;
                st.progress_frac = 0.0;
                st.stage.clear();
                st.last_result = Some(Err("Clip range must be at least 0.5 seconds".to_string()));
            }
            return;
        }
        // Cap single clip export to 2 hours max to guard disk & NVR bandwidth
        let capped_to_max = (end_raw - start_ms) > MAX_CLIP_EXPORT_MS;
        let end_ms = end_raw.min(start_ms + MAX_CLIP_EXPORT_MS);
        let ch_clamped = channel.max(1);

        let clean_label = sanitize_export_label(camera_label);
        {
            let Ok(mut st) = self.export_state.lock() else {
                return;
            };
            if st.in_progress {
                return;
            }
            st.in_progress = true;
            st.label = clean_label.clone();
            st.last_result = None;
            st.channel = ch_clamped;
            st.progress_frac = 0.02;
            st.bytes_written = 0;
            st.stage = "Preparing export...".to_string();
            st.capped_to_max = capped_to_max;
        }

        if let Some(prev_h) = self.export_worker.take() {
            if prev_h.is_finished() {
                let _ = prev_h.join();
            }
        }

        let job_cancel_flag = Arc::new(AtomicBool::new(false));
        let job_active_child = Arc::new(Mutex::new(None));
        let job_id = self
            .export_active_job_id
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);

        self.export_cancel_flag = Arc::clone(&job_cancel_flag);
        self.export_active_child = Arc::clone(&job_active_child);

        let env_snapshot = self.env_cfg.read().unwrap().clone();
        let auth_gw = Arc::clone(&self.auth_gw);
        let nvr_http = Arc::clone(&self.nvr_http);
        let seg_cache = Arc::clone(&self.segment_cache);
        let export_state = Arc::clone(&self.export_state);
        let active_job_id_ref = Arc::clone(&self.export_active_job_id);
        let target_dir = recording_dir.to_path_buf();

        self.export_worker = thread::Builder::new()
            .name(format!("nvr-export-ch{}", ch_clamped))
            .spawn(move || {
                let prog_state = Arc::clone(&export_state);
                let prog_job_ref = Arc::clone(&active_job_id_ref);
                let prog_cancel = Arc::clone(&job_cancel_flag);
                let mut report_progress = move |frac: f32, bytes: u64, stage: &str| {
                    if prog_job_ref.load(Ordering::Acquire) != job_id
                        || prog_cancel.load(Ordering::Acquire)
                    {
                        return;
                    }
                    if let Ok(mut st) = prog_state.lock() {
                        if st.in_progress {
                            st.progress_frac = frac.clamp(0.0, 1.0);
                            st.bytes_written = bytes;
                            if st.stage != stage {
                                st.stage = stage.to_string();
                            }
                        }
                    }
                };

                let res = run_clip_export_job(
                    &env_snapshot,
                    &auth_gw,
                    &nvr_http,
                    &seg_cache,
                    ch_clamped,
                    &clean_label,
                    &target_dir,
                    start_ms,
                    end_ms,
                    &job_cancel_flag,
                    &job_active_child,
                    &mut report_progress,
                );
                if active_job_id_ref.load(Ordering::Acquire) == job_id
                    && !job_cancel_flag.load(Ordering::Acquire)
                {
                    if let Ok(mut st) = export_state.lock() {
                        st.in_progress = false;
                        if let Ok(ref final_p) = res {
                            if let Ok(m) = std::fs::metadata(final_p) {
                                st.bytes_written = m.len();
                            }
                            st.progress_frac = 1.0;
                            st.stage = "Complete".to_string();
                        } else {
                            st.stage = "Failed".to_string();
                        }
                        st.last_result = Some(res);
                    }
                }
            })
            .ok();
    }

    pub fn cancel_clip_export(&mut self) {
        self.export_cancel_flag.store(true, Ordering::Release);
        if let Ok(mut child_guard) = self.export_active_child.lock() {
            if let Some(ref mut child) = *child_guard {
                let _ = child.kill();
            }
        }
        if let Ok(mut st) = self.export_state.lock() {
            if st.in_progress {
                st.in_progress = false;
                st.stage = "Cancelled".to_string();
                st.last_result = Some(Err("Clip export cancelled".to_string()));
            }
        }
    }

    pub fn export_state(&self) -> ClipExportState {
        self.export_state
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    pub fn take_finished_export(&mut self) -> Option<Result<PathBuf, String>> {
        let Ok(mut st) = self.export_state.lock() else {
            return None;
        };
        if st.in_progress {
            return None;
        }
        st.last_result.take()
    }
}

impl Drop for PlaybackManager {
    fn drop(&mut self) {
        self.cancel_clip_export();
        if let Some(h) = self.export_worker.take() {
            let _ = h.join();
        }
        self.close();
    }
}

struct ChannelWorkerContext {
    slot: Arc<PlaybackSlot>,
    ctrl: Arc<PlaybackSharedControl>,
    env_cfg: Arc<RwLock<NvrEnvConfig>>,
    p2p_ready: Arc<AtomicBool>,
    auth_gw: Arc<AuthGateway>,
    nvr_http: Arc<NvrHttpClient>,
    segment_cache: Arc<Mutex<MemfdSegmentCache>>,
    timeline_cache: Arc<Mutex<HashMap<(usize, NaiveDate), TimelineCacheSlot>>>,
    allow_prefetch: bool,
    is_sync_2x2: bool,
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

fn spawn_stderr_drain(
    stderr: Option<std::process::ChildStderr>,
) -> (Arc<Mutex<String>>, Option<JoinHandle<()>>) {
    let captured = Arc::new(Mutex::new(String::new()));
    let Some(mut pipe) = stderr else {
        return (captured, None);
    };
    let cap_clone = Arc::clone(&captured);
    let handle = thread::Builder::new()
        .name("nvr-pb-stderr".to_string())
        .spawn(move || {
            let mut scratch = [0u8; 2048];
            loop {
                match pipe.read(&mut scratch) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut text) = cap_clone.lock() {
                            if text.len() < 1536 {
                                let chunk_str = String::from_utf8_lossy(&scratch[..n]);
                                let remain = 1536usize.saturating_sub(text.len());
                                let take_s: String = chunk_str.chars().take(remain).collect();
                                text.push_str(&take_s);
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })
        .ok();
    (captured, handle)
}

fn join_stderr_and_read(
    stderr_log: &Arc<Mutex<String>>,
    stderr_handle: Option<JoinHandle<()>>,
) -> String {
    if let Some(h) = stderr_handle {
        let _ = h.join();
    }
    stderr_log.lock().map(|s| s.clone()).unwrap_or_default()
}

fn is_auth_rejection(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("circuit breaker")
        || lower.contains("auth locked")
}

fn is_cgi_unsupported_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("404")
        || lower.contains("400")
        || lower.contains("405")
        || lower.contains("501")
        || lower.contains("connection refused")
        || lower.contains("unsupported")
        || lower.contains("not implemented")
}

fn current_wall_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn park_until_state_change(ctrl: &PlaybackSharedControl, my_gen: u64) {
    while !ctrl.stop_flag.load(Ordering::Acquire)
        && ctrl.generation.load(Ordering::Acquire) == my_gen
    {
        ctrl.sleep_interruptible(Duration::from_millis(250), my_gen);
    }
}

fn playback_channel_worker_loop(ctx: ChannelWorkerContext) {
    let mut use_rtsp_fallback = false;

    while !ctx.ctrl.stop_flag.load(Ordering::Acquire) {
        let my_gen = ctx.ctrl.generation.load(Ordering::Acquire);
        let env_snapshot = ctx.env_cfg.read().unwrap().clone();

        if ctx.auth_gw.circuit_breaker().is_tripped() {
            ctx.slot
                .set_error(PlaybackStatus::AuthError, "Auth circuit breaker tripped");
            ctx.ctrl.sleep_interruptible(Duration::from_secs(2), my_gen);
            continue;
        }

        if env_snapshot.is_cloud_p2p_active() && !ctx.p2p_ready.load(Ordering::Acquire) {
            ctx.slot.set_status(PlaybackStatus::Buffering);
            ctx.ctrl
                .sleep_interruptible(Duration::from_millis(150), my_gen);
            continue;
        }

        let brand_lower = env_snapshot.brand.to_ascii_lowercase();
        if brand_lower == "hikvision" || brand_lower == "custom" {
            use_rtsp_fallback = true;
        }

        let start_ms = ctx.ctrl.target_epoch_ms.load(Ordering::Acquire);
        ctx.slot.clear_error();
        ctx.slot.set_status(PlaybackStatus::Buffering);

        if !use_rtsp_fallback {
            let outcome = run_dhav_tier1_pipeline(&ctx, &env_snapshot, start_ms, my_gen);
            if ctx.ctrl.stop_flag.load(Ordering::Acquire) {
                break;
            }
            if ctx.ctrl.generation.load(Ordering::Acquire) != my_gen {
                continue;
            }
            match outcome {
                DhavPipelineOutcome::SeekOrRestart => continue,
                DhavPipelineOutcome::SwitchToRtspFallback => {
                    use_rtsp_fallback = true;
                    continue;
                }
                DhavPipelineOutcome::AuthFailed(msg) => {
                    ctx.slot.set_error(PlaybackStatus::AuthError, msg);
                    ctx.ctrl.sleep_interruptible(Duration::from_secs(5), my_gen);
                }
                DhavPipelineOutcome::EndOfRecording => {
                    // [HIGH-14] Park cleanly at EndOfRecording without rewinding to initial target_epoch_ms
                    ctx.slot.fps_x10.store(0, Ordering::Release);
                    ctx.slot.set_status(PlaybackStatus::EndOfRecording);
                    park_until_state_change(&ctx.ctrl, my_gen);
                }
                DhavPipelineOutcome::FatalError(msg) => {
                    ctx.slot.fps_x10.store(0, Ordering::Release);
                    ctx.slot.set_error(PlaybackStatus::Error, msg);
                    ctx.ctrl.sleep_interruptible(Duration::from_secs(2), my_gen);
                }
            }
        } else {
            run_rtsp_tier3_fallback(&ctx, &env_snapshot, start_ms, my_gen);
        }
    }

    ctx.slot.kill_active_child();
}

enum DhavPipelineOutcome {
    SeekOrRestart,
    SwitchToRtspFallback,
    AuthFailed(String),
    EndOfRecording,
    FatalError(String),
}

fn ensure_segment_downloading(
    ctx: &ChannelWorkerContext,
    env_cfg: &NvrEnvConfig,
    seg_start_ms: i64,
) -> Result<Arc<MemfdSegment>, String> {
    let key = SegmentKey {
        channel: ctx.slot.channel,
        start_ms: seg_start_ms,
    };
    let (seg, should_download) = {
        let mut cache = ctx
            .segment_cache
            .lock()
            .map_err(|_| "Segment cache poisoned".to_string())?;
        cache
            .acquire_or_create(key)
            .map_err(|e| format!("memfd_create failed: {}", e))?
    };

    if !should_download {
        return Ok(seg);
    }

    let seg_bg = Arc::clone(&seg);
    let env_clone = env_cfg.clone();
    let http = Arc::clone(&ctx.nvr_http);
    let cache_ref = Arc::clone(&ctx.segment_cache);
    let ctrl_ref = Arc::clone(&ctx.ctrl);
    let channel = ctx.slot.channel;

    let _ = thread::Builder::new()
        .name(format!("nvr-dl-c{}", channel))
        .spawn(move || {
            // [HIGH-10] Bound concurrent loadfile.cgi downloads (max 4 on LAN, 1 on Cloud P2P)
            let Some(_permit) = ctrl_ref
                .download_semaphore
                .acquire_interruptible(&ctrl_ref.stop_flag)
            else {
                seg_bg.mark_finished(None, true);
                if let Ok(mut c) = cache_ref.lock() {
                    c.finish_inflight(key, false);
                }
                return;
            };

            let start_local = epoch_ms_to_local_naive(seg_start_ms);
            let end_local = epoch_ms_to_local_naive(seg_start_ms + SEGMENT_DURATION_MS);

            // [CRIT-09] Only abort shared segment download when session closes (`stop_flag`),
            // NOT on intra-session `generation` seeks so overlapping seeks reuse in-flight segments!
            let run_slice = |subtype: u8| -> (Result<u64, String>, Option<String>, bool) {
                let mut scanner = DhavPacketScanner::new(seg_start_ms);
                let mut slice_write_err: Option<String> = None;
                let mut slice_cancelled = false;
                let slice_res = block_on_nvr_io(http.stream_dhav_time_slice_cancellable(
                    &env_clone,
                    channel,
                    subtype,
                    start_local,
                    end_local,
                    160 * 1024 * 1024,
                    Some(&ctrl_ref.stop_flag),
                    |chunk| {
                        if ctrl_ref.stop_flag.load(Ordering::Acquire) {
                            slice_cancelled = true;
                            return false;
                        }
                        let pkts = scanner.push_chunk(chunk);
                        match seg_bg.append_chunk_and_index(chunk, &pkts) {
                            Ok(new_len) => {
                                if let Ok(mut c) = cache_ref.lock() {
                                    c.record_segment_growth(key, new_len);
                                }
                                true
                            }
                            Err(e) => {
                                slice_write_err = Some(format!("memfd write error: {}", e));
                                false
                            }
                        }
                    },
                ));
                if ctrl_ref.stop_flag.load(Ordering::Acquire) {
                    slice_cancelled = true;
                }
                (slice_res, slice_write_err, slice_cancelled)
            };

            let (mut res, mut write_err, mut cancelled) = run_slice(0);
            if !cancelled
                && write_err.is_none()
                && matches!(res, Ok(0))
                && seg_bg.byte_len() == 0
            {
                // Probe sub-stream recording if main-stream returned 0 bytes
                let (r1, w1, c1) = run_slice(1);
                res = r1;
                write_err = w1;
                cancelled = c1;
            }

            if let Some(w_err) = write_err {
                seg_bg.mark_finished(Some(w_err), true);
                if let Ok(mut c) = cache_ref.lock() {
                    c.finish_inflight(key, false);
                }
                return;
            }

            if cancelled {
                seg_bg.mark_finished(None, true);
                if let Ok(mut c) = cache_ref.lock() {
                    c.finish_inflight(key, false);
                }
                return;
            }

            match res {
                Ok(_) => {
                    seg_bg.mark_finished(None, false);
                    let keep = seg_bg.packet_len() > 0;
                    if let Ok(mut c) = cache_ref.lock() {
                        c.finish_inflight(key, keep);
                    }
                }
                Err(err) => {
                    seg_bg.mark_finished(Some(err), false);
                    if let Ok(mut c) = cache_ref.lock() {
                        c.finish_inflight(key, false);
                    }
                }
            }
        });

    Ok(seg)
}

fn find_next_recorded_span_start(
    timeline_cache: &Mutex<HashMap<(usize, NaiveDate), TimelineCacheSlot>>,
    channel: usize,
    from_ms: i64,
    forward: bool,
) -> Option<i64> {
    let date = epoch_ms_to_local_naive(from_ms).date();
    let adjacent_date = if forward {
        date.succ_opt()
    } else {
        date.pred_opt()
    };
    let map = timeline_cache.lock().ok()?;

    for cand_date in [Some(date), adjacent_date].into_iter().flatten() {
        let Some(idx) = map.get(&(channel, cand_date)).and_then(|s| s.index.as_ref()) else {
            continue;
        };
        if !idx.api_supported {
            continue;
        }
        let found = if forward {
            idx.coverage
                .iter()
                .chain(idx.events.iter())
                .filter(|sp| sp.end_ms > from_ms)
                .map(|sp| sp.start_ms.max(from_ms))
                .min()
        } else {
            idx.coverage
                .iter()
                .chain(idx.events.iter())
                .filter(|sp| sp.start_ms < from_ms)
                .map(|sp| (sp.end_ms - 1_000).min(from_ms))
                .max()
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

fn wait_feeder_backpressure(
    ctrl: &PlaybackSharedControl,
    pts_queue: &Mutex<VecDeque<(i64, bool)>>,
    fed_video_packets: u64,
    reverse_mode: bool,
    my_gen: u64,
) -> bool {
    if fed_video_packets < 8 {
        return true;
    }
    while !ctrl.stop_flag.load(Ordering::Acquire)
        && ctrl.generation.load(Ordering::Acquire) == my_gen
        && (ctrl.effective_speed() < 0.0) == reverse_mode
    {
        let q_len = pts_queue.lock().map(|q| q.len()).unwrap_or(0);
        if q_len < 64 {
            return true;
        }
        ctrl.sleep_interruptible(Duration::from_millis(10), my_gen);
    }
    false
}

fn run_dhav_tier1_pipeline(
    ctx: &ChannelWorkerContext,
    env_cfg: &NvrEnvConfig,
    initial_ms: i64,
    my_gen: u64,
) -> DhavPipelineOutcome {
    let width = ctx.ctrl.decode_width.load(Ordering::Acquire);
    let height = ctx.ctrl.decode_height.load(Ordering::Acquire);
    let initial_speed = ctx.ctrl.effective_speed();
    let reverse_mode = initial_speed < 0.0;

    // Verify first segment starts arriving or exists before spawning ffmpeg so CGI 404/401
    // or memfd unavailability triggers instant fallback or AuthError without wasted subprocess churn.
    let first_seg_start = align_segment_start_ms(initial_ms);
    let first_seg = match ensure_segment_downloading(ctx, env_cfg, first_seg_start) {
        Ok(s) => s,
        // [MED-11] Fall back to RTSP Tier-3 if MemfdSegment::new fails even after tempfile fallback
        Err(_) => return DhavPipelineOutcome::SwitchToRtspFallback,
    };

    let mut seen_seq = first_seg.notify_seq();
    while first_seg.packet_len() == 0 && !first_seg.is_complete() && !first_seg.is_aborted() {
        if ctx.ctrl.stop_flag.load(Ordering::Acquire)
            || ctx.ctrl.generation.load(Ordering::Acquire) != my_gen
        {
            return DhavPipelineOutcome::SeekOrRestart;
        }
        seen_seq = first_seg.wait_for_change(seen_seq, Duration::from_millis(100));
    }

    if let Some(err) = first_seg.error_text() {
        if is_auth_rejection(&err) || ctx.auth_gw.circuit_breaker().is_tripped() {
            return DhavPipelineOutcome::AuthFailed(err);
        }
        if is_cgi_unsupported_error(&err) {
            return DhavPipelineOutcome::SwitchToRtspFallback;
        }
        return DhavPipelineOutcome::FatalError(err);
    }

    let ffmpeg_bin = resolve_trusted_binary("ffmpeg");
    let mut cmd = Command::new(ffmpeg_bin);
    configure_child_lifecycle(&mut cmd);
    cmd.arg("-v")
        .arg("error")
        .arg("-protocol_whitelist")
        .arg("pipe")
        .arg("-max_alloc")
        .arg("134217728")
        .arg("-fflags")
        .arg("nobuffer+discardcorrupt")
        .arg("-flags")
        .arg("low_delay")
        .arg("-probesize")
        .arg("32768")
        .arg("-analyzeduration")
        .arg("0");
    if reverse_mode {
        cmd.arg("-threads").arg("1");
    } else if !ctx.is_sync_2x2 && env_cfg.hwaccel_mode.eq_ignore_ascii_case("auto") {
        cmd.arg("-hwaccel").arg("auto");
    }
    cmd.arg("-f")
        .arg("dhav")
        .arg("-i")
        .arg("pipe:0")
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
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return DhavPipelineOutcome::FatalError(format!("Failed to launch ffmpeg: {}", e)),
    };

    let Some(mut stdin_pipe) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return DhavPipelineOutcome::FatalError("Missing ffmpeg stdin pipe".to_string());
    };
    let Some(stdout_pipe) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return DhavPipelineOutcome::FatalError("Missing ffmpeg stdout pipe".to_string());
    };
    let (stderr_log, stderr_handle) = spawn_stderr_drain(child.stderr.take());

    // [MED-09] Re-check stop_flag / generation right after storing child so concurrent seek never misses killing it
    if let Ok(mut guard) = ctx.slot.active_child.lock() {
        *guard = Some(child);
        if ctx.ctrl.stop_flag.load(Ordering::Acquire)
            || ctx.ctrl.generation.load(Ordering::Acquire) != my_gen
        {
            if let Some(mut c) = guard.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            drop(guard);
            drop(stdin_pipe);
            let _ = join_stderr_and_read(&stderr_log, stderr_handle);
            return DhavPipelineOutcome::SeekOrRestart;
        }
    }

    let pts_queue: Arc<Mutex<VecDeque<(i64, bool)>>> =
        Arc::new(Mutex::new(VecDeque::with_capacity(64)));
    let feeder_done = Arc::new(AtomicBool::new(false));
    let pipeline_abort = Arc::new(AtomicBool::new(false));
    let hit_end_of_recording = Arc::new(AtomicBool::new(false));

    // Spawn Reader + Pacer thread for decoded RGBA frames
    let reader_slot = Arc::clone(&ctx.slot);
    let reader_ctrl = Arc::clone(&ctx.ctrl);
    let reader_pts = Arc::clone(&pts_queue);
    let reader_done = Arc::clone(&feeder_done);
    let reader_abort = Arc::clone(&pipeline_abort);
    let reader_handle = thread::Builder::new()
        .name(format!("nvr-pacer-c{}", ctx.slot.channel))
        .spawn(move || {
            run_rgba_reader_pacer(
                stdout_pipe,
                reader_slot,
                reader_ctrl,
                reader_pts,
                reader_done,
                reader_abort,
                width,
                height,
                initial_ms,
                my_gen,
            );
        })
        .expect("Failed to spawn playback RGBA pacer thread");

    // Feeder loop on current thread
    let mut pkt_buf: Vec<u8> = Vec::with_capacity(131_072);
    let mut seg_start = first_seg_start;
    let mut active_seg: Option<Arc<MemfdSegment>> = Some(first_seg);
    let mut empty_hops = 0u32;
    let max_empty_hops = 8u32;
    let mut fed_video_packets = 0u64;
    let mut sent_file_header = false;
    let mut segment_auth_err: Option<String> = None;
    let mut segment_fatal_err: Option<String> = None;
    let mut switch_to_rtsp = false;

    if !reverse_mode {
        let mut positioned_initial = false;
        // [HIGH-12] Enable keyframe-only trick-play at >= 4.0x
        let mut keyframe_only_mode = ctx.ctrl.effective_speed() >= 4.0;
        let mut max_fed_video_epoch_ms: Option<i64> = None;

        'forward_segments: loop {
            if ctx.ctrl.stop_flag.load(Ordering::Acquire)
                || ctx.ctrl.generation.load(Ordering::Acquire) != my_gen
                || ctx.ctrl.effective_speed() < 0.0
            {
                break 'forward_segments;
            }

            let seg = match active_seg.take() {
                Some(s) => s,
                None => match ensure_segment_downloading(ctx, env_cfg, seg_start) {
                    Ok(s) => s,
                    Err(_) => {
                        switch_to_rtsp = true;
                        break 'forward_segments;
                    }
                },
            };

            let mut pkt_idx = 0usize;
            let mut wrote_any_in_seg = false;
            let mut prefetched_next = false;
            // [HIGH-11] Require IDR sync at each segment entry so overlapped boundary GOPs decode cleanly
            let mut need_idr_sync = true;

            loop {
                if ctx.ctrl.stop_flag.load(Ordering::Acquire)
                    || ctx.ctrl.generation.load(Ordering::Acquire) != my_gen
                    || ctx.ctrl.effective_speed() < 0.0
                {
                    break 'forward_segments;
                }

                // [HIGH-10] Only prefetch next_seg_ms AFTER current segment download is complete
                if !prefetched_next && ctx.allow_prefetch && seg.is_complete() {
                    prefetched_next = true;
                    let next_seg_ms = seg_start + SEGMENT_DURATION_MS;
                    if next_seg_ms <= current_wall_epoch_ms() + SEGMENT_DURATION_MS {
                        let _ = ensure_segment_downloading(ctx, env_cfg, next_seg_ms);
                    }
                }

                // Wait until we have enough packets to pick the starting keyframe for initial seek
                if !positioned_initial {
                    let mut seq = seg.notify_seq();
                    while !seg.is_complete() && !seg.is_aborted() {
                        let snap = seg.packets_snapshot();
                        let reached_target = snap
                            .last()
                            .map(|p| p.epoch_ms >= initial_ms)
                            .unwrap_or(false);
                        if reached_target {
                            break;
                        }
                        if ctx.ctrl.stop_flag.load(Ordering::Acquire)
                            || ctx.ctrl.generation.load(Ordering::Acquire) != my_gen
                        {
                            break 'forward_segments;
                        }
                        seq = seg.wait_for_change(seq, Duration::from_millis(50));
                    }

                    let snap = seg.packets_snapshot();
                    if let Some(idr_pos) = snap
                        .iter()
                        .rposition(|p| p.is_keyframe && p.epoch_ms <= initial_ms)
                        .or_else(|| snap.iter().position(|p| p.is_keyframe))
                    {
                        if idr_pos > 0
                            && !sent_file_header
                            && snap.first().map(|p| p.pkt_type == PKT_FILE_HEADER).unwrap_or(false)
                            && seg.read_packet(&snap[0], &mut pkt_buf).is_ok()
                        {
                            if stdin_pipe.write_all(&pkt_buf).is_err() {
                                pipeline_abort.store(true, Ordering::Release);
                                break 'forward_segments;
                            }
                            sent_file_header = true;
                        }
                        pkt_idx = idr_pos;
                    }
                    positioned_initial = true;
                }

                if let Some(entry) = seg.packet_at(pkt_idx) {
                    pkt_idx += 1;

                    let eff_speed = ctx.ctrl.effective_speed();
                    if eff_speed < 0.0 {
                        break 'forward_segments;
                    }
                    let want_keyframe_only = eff_speed >= 4.0;
                    if want_keyframe_only != keyframe_only_mode {
                        keyframe_only_mode = want_keyframe_only;
                        need_idr_sync = true;
                    }

                    if entry.pkt_type == PKT_FILE_HEADER {
                        if !sent_file_header && seg.read_packet(&entry, &mut pkt_buf).is_ok() {
                            if stdin_pipe.write_all(&pkt_buf).is_err() {
                                pipeline_abort.store(true, Ordering::Release);
                                break 'forward_segments;
                            }
                            sent_file_header = true;
                        }
                        continue;
                    }

                    if entry.pkt_type != PKT_VIDEO_IDR && entry.pkt_type != PKT_VIDEO_P {
                        continue;
                    }

                    if keyframe_only_mode && !entry.is_keyframe {
                        continue;
                    }
                    if need_idr_sync {
                        if !entry.is_keyframe {
                            continue;
                        }
                        need_idr_sync = false;
                    }

                    if seg.read_packet(&entry, &mut pkt_buf).is_err() {
                        break;
                    }

                    // [CRIT-08] Feeder backpressure instead of popping unread pts_queue entries
                    if !wait_feeder_backpressure(
                        &ctx.ctrl,
                        &pts_queue,
                        fed_video_packets,
                        false,
                        my_gen,
                    ) {
                        break 'forward_segments;
                    }

                    // [HIGH-11] Drop overlapped 60s boundary GOP frames as 0ms reference pre-roll
                    let before_initial = entry.epoch_ms < initial_ms.saturating_sub(80);
                    let overlapped_boundary = max_fed_video_epoch_ms
                        .map(|max_fed| entry.epoch_ms <= max_fed)
                        .unwrap_or(false);
                    let is_preroll = before_initial || overlapped_boundary;

                    if let Ok(mut q) = pts_queue.lock() {
                        q.push_back((entry.epoch_ms, is_preroll));
                    }

                    if stdin_pipe.write_all(&pkt_buf).is_err() {
                        pipeline_abort.store(true, Ordering::Release);
                        break 'forward_segments;
                    }
                    max_fed_video_epoch_ms = Some(
                        max_fed_video_epoch_ms
                            .map(|m| m.max(entry.epoch_ms))
                            .unwrap_or(entry.epoch_ms),
                    );
                    fed_video_packets += 1;
                    wrote_any_in_seg = true;
                    empty_hops = 0;
                } else if seg.is_complete() || seg.is_aborted() {
                    break;
                } else {
                    let seq = seg.notify_seq();
                    let _ = seg.wait_for_change(seq, Duration::from_millis(40));
                }
            }

            if !prefetched_next && ctx.allow_prefetch && seg.is_complete() {
                let next_seg_ms = seg_start + SEGMENT_DURATION_MS;
                if next_seg_ms <= current_wall_epoch_ms() + SEGMENT_DURATION_MS {
                    let _ = ensure_segment_downloading(ctx, env_cfg, next_seg_ms);
                }
            }

            // [HIGH-13] Propagate segment errors (401 -> AuthFailed, CGI 404/501 -> RTSP fallback)
            if let Some(err) = seg.error_text() {
                if is_auth_rejection(&err) || ctx.auth_gw.circuit_breaker().is_tripped() {
                    segment_auth_err = Some(err);
                    pipeline_abort.store(true, Ordering::Release);
                    break 'forward_segments;
                }
                if !wrote_any_in_seg && is_cgi_unsupported_error(&err) && fed_video_packets == 0 {
                    switch_to_rtsp = true;
                    pipeline_abort.store(true, Ordering::Release);
                    break 'forward_segments;
                }
                if !wrote_any_in_seg {
                    segment_fatal_err = Some(err);
                    pipeline_abort.store(true, Ordering::Release);
                    break 'forward_segments;
                }
            }

            if !wrote_any_in_seg {
                if seg_start + SEGMENT_DURATION_MS >= current_wall_epoch_ms() {
                    hit_end_of_recording.store(true, Ordering::Release);
                    break 'forward_segments;
                }
                // [HIGH-13] Always increment empty_hops BEFORE jumping via find_next_recorded_span_start
                empty_hops += 1;
                if empty_hops > max_empty_hops {
                    hit_end_of_recording.store(true, Ordering::Release);
                    break 'forward_segments;
                }
                if let Some(jump_ms) = find_next_recorded_span_start(
                    &ctx.timeline_cache,
                    ctx.slot.channel,
                    seg_start + SEGMENT_DURATION_MS,
                    true,
                ) {
                    let aligned = align_segment_start_ms(jump_ms);
                    if aligned > seg_start {
                        seg_start = aligned;
                        continue 'forward_segments;
                    }
                }
            }

            seg_start += SEGMENT_DURATION_MS;
            if seg_start > current_wall_epoch_ms() + 2_000 {
                hit_end_of_recording.store(true, Ordering::Release);
                break 'forward_segments;
            }
        }
    } else {
        // Reverse keyframe trick-play (-1x, -2x, -4x)
        let mut synth_seq: u32 = 1;
        let mut ceiling_ms = initial_ms;

        'reverse_segments: loop {
            if ctx.ctrl.stop_flag.load(Ordering::Acquire)
                || ctx.ctrl.generation.load(Ordering::Acquire) != my_gen
                || ctx.ctrl.effective_speed() > 0.0
            {
                break 'reverse_segments;
            }

            let seg = match active_seg.take() {
                Some(s) => s,
                None => match ensure_segment_downloading(ctx, env_cfg, seg_start) {
                    Ok(s) => s,
                    Err(_) => {
                        switch_to_rtsp = true;
                        break 'reverse_segments;
                    }
                },
            };

            // Wait for segment download to finish so we can walk its keyframes newest -> oldest
            let mut seq = seg.notify_seq();
            while !seg.is_complete() && !seg.is_aborted() {
                if ctx.ctrl.stop_flag.load(Ordering::Acquire)
                    || ctx.ctrl.generation.load(Ordering::Acquire) != my_gen
                    || ctx.ctrl.effective_speed() > 0.0
                {
                    break 'reverse_segments;
                }
                seq = seg.wait_for_change(seq, Duration::from_millis(60));
            }

            // [HIGH-13] Check segment download errors on every reverse segment
            if let Some(err) = seg.error_text() {
                if is_auth_rejection(&err) || ctx.auth_gw.circuit_breaker().is_tripped() {
                    segment_auth_err = Some(err);
                    pipeline_abort.store(true, Ordering::Release);
                    break 'reverse_segments;
                }
                if is_cgi_unsupported_error(&err) && fed_video_packets == 0 {
                    switch_to_rtsp = true;
                    pipeline_abort.store(true, Ordering::Release);
                    break 'reverse_segments;
                }
                segment_fatal_err = Some(err);
                pipeline_abort.store(true, Ordering::Release);
                break 'reverse_segments;
            }

            // [HIGH-10] Only prefetch previous segment after current segment is complete
            if ctx.allow_prefetch && seg.is_complete() && seg_start >= SEGMENT_DURATION_MS {
                let _ = ensure_segment_downloading(ctx, env_cfg, seg_start - SEGMENT_DURATION_MS);
            }

            let snap = seg.packets_snapshot();
            if !sent_file_header
                && snap.first().map(|p| p.pkt_type == PKT_FILE_HEADER).unwrap_or(false)
                && seg.read_packet(&snap[0], &mut pkt_buf).is_ok()
                && stdin_pipe.write_all(&pkt_buf).is_ok()
            {
                sent_file_header = true;
            }

            let mut keyframes: Vec<DhavPacketEntry> = snap
                .into_iter()
                .filter(|p| p.is_keyframe && p.epoch_ms <= ceiling_ms)
                .collect();
            keyframes.reverse();

            if keyframes.is_empty() {
                empty_hops += 1;
                if empty_hops > max_empty_hops || seg_start < SEGMENT_DURATION_MS {
                    hit_end_of_recording.store(true, Ordering::Release);
                    break 'reverse_segments;
                }
                // [HIGH-12] Jump across >5m reverse gaps cleanly using timeline index
                if let Some(jump_ms) = find_next_recorded_span_start(
                    &ctx.timeline_cache,
                    ctx.slot.channel,
                    seg_start,
                    false,
                ) {
                    let aligned = align_segment_start_ms(jump_ms);
                    if aligned < seg_start {
                        seg_start = aligned;
                        ceiling_ms = seg_start + SEGMENT_DURATION_MS;
                        continue 'reverse_segments;
                    }
                }
            } else {
                empty_hops = 0;
                for kf in keyframes {
                    if ctx.ctrl.stop_flag.load(Ordering::Acquire)
                        || ctx.ctrl.generation.load(Ordering::Acquire) != my_gen
                        || ctx.ctrl.effective_speed() > 0.0
                    {
                        break 'reverse_segments;
                    }
                    if seg.read_packet(&kf, &mut pkt_buf).is_err() {
                        break;
                    }
                    if !wait_feeder_backpressure(
                        &ctx.ctrl,
                        &pts_queue,
                        fed_video_packets,
                        true,
                        my_gen,
                    ) {
                        break 'reverse_segments;
                    }

                    DhavPacketScanner::rewrite_synthetic_monotonic_header(&mut pkt_buf, synth_seq);
                    synth_seq = synth_seq.wrapping_add(1);

                    if let Ok(mut q) = pts_queue.lock() {
                        q.push_back((kf.epoch_ms, false));
                    }
                    if stdin_pipe.write_all(&pkt_buf).is_err() {
                        pipeline_abort.store(true, Ordering::Release);
                        break 'reverse_segments;
                    }
                    fed_video_packets += 1;
                }
            }

            if seg_start < SEGMENT_DURATION_MS {
                hit_end_of_recording.store(true, Ordering::Release);
                break 'reverse_segments;
            }
            seg_start -= SEGMENT_DURATION_MS;
            ceiling_ms = seg_start + SEGMENT_DURATION_MS;
        }
    }

    drop(stdin_pipe);
    feeder_done.store(true, Ordering::Release);

    let aborted = pipeline_abort.load(Ordering::Acquire)
        || ctx.ctrl.stop_flag.load(Ordering::Acquire)
        || ctx.ctrl.generation.load(Ordering::Acquire) != my_gen
        || (ctx.ctrl.effective_speed() < 0.0) != reverse_mode;
    if aborted {
        pipeline_abort.store(true, Ordering::Release);
        ctx.slot.kill_active_child();
    }
    ctx.ctrl.notify_wake();

    let _ = reader_handle.join();

    if let Ok(mut guard) = ctx.slot.active_child.lock() {
        if let Some(mut c) = guard.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
    let err_txt = join_stderr_and_read(&stderr_log, stderr_handle);

    if ctx.ctrl.stop_flag.load(Ordering::Acquire)
        || ctx.ctrl.generation.load(Ordering::Acquire) != my_gen
    {
        return DhavPipelineOutcome::SeekOrRestart;
    }

    if (ctx.ctrl.effective_speed() < 0.0) != reverse_mode {
        return DhavPipelineOutcome::SeekOrRestart;
    }

    if let Some(auth_err) = segment_auth_err {
        return DhavPipelineOutcome::AuthFailed(auth_err);
    }
    if switch_to_rtsp {
        return DhavPipelineOutcome::SwitchToRtspFallback;
    }
    if let Some(fatal_err) = segment_fatal_err {
        return DhavPipelineOutcome::FatalError(fatal_err);
    }

    if hit_end_of_recording.load(Ordering::Acquire) {
        return DhavPipelineOutcome::EndOfRecording;
    }

    if !err_txt.trim().is_empty() {
        DhavPipelineOutcome::FatalError(err_txt)
    } else {
        DhavPipelineOutcome::EndOfRecording
    }
}

fn pace_and_sync_frame(
    ctrl: &PlaybackSharedControl,
    channel: usize,
    frame_epoch_ms: i64,
    prev_pts_ms: &mut Option<i64>,
    prev_wall: &mut Instant,
    my_gen: u64,
) -> bool {
    let eff_speed = ctrl.effective_speed();
    let abs_speed = eff_speed.abs().max(0.25);
    let is_primary = ctrl.primary_channel.load(Ordering::Acquire) == channel;

    // [MED-08] 2x2 inter-channel sync: skip pacer sleep when secondary lags > 600ms behind primary
    let catch_up_fast = !is_primary
        && eff_speed > 0.0
        && frame_epoch_ms.saturating_add(600) < ctrl.master_playhead_ms.load(Ordering::Acquire);

    if !catch_up_fast {
        if let Some(last_pts) = *prev_pts_ms {
            // [HIGH-11] Non-increasing forward PTS gets 0ms; < 4.0x forward caps gap sleep to 250ms
            let media_delta_ms = if eff_speed >= 0.0 && frame_epoch_ms <= last_pts {
                0
            } else {
                let cap_ms = if eff_speed > 0.0 && eff_speed < 4.0 {
                    250
                } else {
                    2_500
                };
                (frame_epoch_ms - last_pts).unsigned_abs().min(cap_ms)
            };
            let target_wall_ms = (media_delta_ms as f32 / abs_speed) as u64;
            let elapsed_wall = prev_wall.elapsed();
            let target_dur = Duration::from_millis(target_wall_ms);
            if target_dur > elapsed_wall {
                ctrl.sleep_interruptible(target_dur - elapsed_wall, my_gen);
                if ctrl.stop_flag.load(Ordering::Acquire)
                    || ctrl.generation.load(Ordering::Acquire) != my_gen
                {
                    return false;
                }
            }
        }
    }

    // [MED-08] 2x2 inter-channel sync: hold secondary up to 2s if > 300ms ahead of primary
    if !is_primary && eff_speed > 0.0 {
        let wait_start = Instant::now();
        while wait_start.elapsed() < Duration::from_secs(2)
            && !ctrl.stop_flag.load(Ordering::Acquire)
            && ctrl.generation.load(Ordering::Acquire) == my_gen
            && !ctrl.paused.load(Ordering::Acquire)
            && ctrl.primary_channel.load(Ordering::Acquire) != channel
            && ctrl.effective_speed() > 0.0
            && frame_epoch_ms > ctrl.master_playhead_ms.load(Ordering::Acquire).saturating_add(300)
        {
            ctrl.sleep_interruptible(Duration::from_millis(25), my_gen);
        }
        if ctrl.stop_flag.load(Ordering::Acquire)
            || ctrl.generation.load(Ordering::Acquire) != my_gen
        {
            return false;
        }
    }

    *prev_pts_ms = Some(frame_epoch_ms);
    *prev_wall = Instant::now();
    true
}

#[allow(clippy::too_many_arguments)]
fn run_rgba_reader_pacer(
    mut stdout: std::process::ChildStdout,
    slot: Arc<PlaybackSlot>,
    ctrl: Arc<PlaybackSharedControl>,
    pts_queue: Arc<Mutex<VecDeque<(i64, bool)>>>,
    feeder_done: Arc<AtomicBool>,
    pipeline_abort: Arc<AtomicBool>,
    width: u32,
    height: u32,
    initial_ms: i64,
    my_gen: u64,
) {
    let frame_bytes = (width as usize) * (height as usize) * 4;
    let mut raw_buf = vec![0u8; frame_bytes];
    let mut prev_pts_ms: Option<i64> = None;
    let mut prev_wall = Instant::now();
    let mut fps_window_start = Instant::now();
    let mut fps_frames = 0u32;
    let mut fallback_pts = initial_ms;

    while !ctrl.stop_flag.load(Ordering::Acquire)
        && ctrl.generation.load(Ordering::Acquire) == my_gen
    {
        // Pause gate (wakes in 0ms on unpause, seek, stop, `.` step, or `,` rewind refill!)
        while ctrl.paused.load(Ordering::Acquire)
            && slot.step_forward_budget.load(Ordering::Acquire) == 0
            && slot.rewind_target_ms.load(Ordering::Acquire) == i64::MIN
        {
            if ctrl.stop_flag.load(Ordering::Acquire)
                || ctrl.generation.load(Ordering::Acquire) != my_gen
                || pipeline_abort.load(Ordering::Acquire)
            {
                return;
            }
            // [MED-09] Exit pause gate cleanly if feeder finished with zero unread frames remaining
            if feeder_done.load(Ordering::Acquire)
                && pts_queue.lock().map(|q| q.is_empty()).unwrap_or(true)
            {
                return;
            }
            if slot.status() != PlaybackStatus::Paused {
                slot.set_status(PlaybackStatus::Paused);
            }
            prev_pts_ms = None;
            ctrl.sleep_interruptible(Duration::from_millis(80), my_gen);
        }

        // [HIGH-16] When unpaused while ring.cursor + 1 < ring.frames.len(), present remaining
        // buffered ring frames before reading new frames from FFmpeg stdout so no frames are skipped!
        if !ctrl.paused.load(Ordering::Acquire)
            && slot.rewind_target_ms.load(Ordering::Acquire) == i64::MIN
        {
            if let Some((ring_prev_ep, ring_next_ep, ring_w, ring_h)) =
                slot.step_unpaused_ring_into(&mut raw_buf)
            {
                if prev_pts_ms.is_none() {
                    prev_pts_ms = Some(ring_prev_ep);
                    prev_wall = Instant::now();
                }
                if !pace_and_sync_frame(
                    &ctrl,
                    slot.channel,
                    ring_next_ep,
                    &mut prev_pts_ms,
                    &mut prev_wall,
                    my_gen,
                ) {
                    return;
                }
                slot.publish_frame(ring_w, ring_h, ring_next_ep, &raw_buf, false);
                if ctrl.primary_channel.load(Ordering::Acquire) == slot.channel {
                    ctrl.master_playhead_ms
                        .store(ring_next_ep, Ordering::Release);
                }
                slot.set_status(if ctrl.paused.load(Ordering::Acquire) {
                    PlaybackStatus::Paused
                } else {
                    PlaybackStatus::Playing
                });
                fps_frames += 1;
                let win_elapsed = fps_window_start.elapsed().as_secs_f32();
                if win_elapsed >= 1.0 {
                    let fps10 = ((fps_frames as f32 / win_elapsed) * 10.0).round() as u32;
                    slot.fps_x10.store(fps10, Ordering::Release);
                    fps_frames = 0;
                    fps_window_start = Instant::now();
                }
                continue;
            }
        }

        if stdout.read_exact(&mut raw_buf).is_err() {
            if let Some(landed_ep) = slot.flush_pending_rewind_on_eof() {
                if ctrl.primary_channel.load(Ordering::Acquire) == slot.channel {
                    ctrl.master_playhead_ms.store(landed_ep, Ordering::Release);
                }
                slot.set_status(PlaybackStatus::Paused);
            }
            break;
        }

        let mut wake_feeder = false;
        let (frame_epoch_ms, is_preroll) = pts_queue
            .lock()
            .ok()
            .and_then(|mut q| {
                let item = q.pop_front();
                if q.len() == 48 {
                    wake_feeder = true;
                }
                item
            })
            .unwrap_or_else(|| {
                let step = if ctrl.effective_speed() < 0.0 { -40 } else { 40 };
                fallback_pts = fallback_pts.saturating_add(step);
                (fallback_pts, false)
            });
        if wake_feeder {
            ctrl.notify_wake();
        }
        fallback_pts = frame_epoch_ms;

        if is_preroll {
            continue;
        }

        // [HIGH-16] Fast-fill ring up to rewind_target_ms and pause on the last frame < rewind_target_ms
        let rw_ceil = slot.rewind_target_ms.load(Ordering::Acquire);
        if rw_ceil != i64::MIN {
            if let Some(landed_ep) =
                slot.handle_rewind_frame(width, height, frame_epoch_ms, &raw_buf, rw_ceil)
            {
                if ctrl.primary_channel.load(Ordering::Acquire) == slot.channel {
                    ctrl.master_playhead_ms.store(landed_ep, Ordering::Release);
                }
                slot.set_status(PlaybackStatus::Paused);
                prev_pts_ms = None;
            }
            continue;
        }

        let stepped_single = if ctrl.paused.load(Ordering::Acquire) {
            let prev = slot.step_forward_budget.load(Ordering::Acquire);
            if prev > 0 {
                slot.step_forward_budget.fetch_sub(1, Ordering::AcqRel);
            }
            true
        } else {
            false
        };

        if !stepped_single
            && !pace_and_sync_frame(
                &ctrl,
                slot.channel,
                frame_epoch_ms,
                &mut prev_pts_ms,
                &mut prev_wall,
                my_gen,
            )
        {
            return;
        }

        slot.publish_frame(width, height, frame_epoch_ms, &raw_buf, true);

        let is_primary = ctrl.primary_channel.load(Ordering::Acquire) == slot.channel;
        if is_primary {
            ctrl.master_playhead_ms
                .store(frame_epoch_ms, Ordering::Release);
        }

        if ctrl.paused.load(Ordering::Acquire) {
            slot.set_status(PlaybackStatus::Paused);
        } else {
            slot.set_status(PlaybackStatus::Playing);
        }

        fps_frames += 1;
        let win_elapsed = fps_window_start.elapsed().as_secs_f32();
        if win_elapsed >= 1.0 {
            let fps10 = ((fps_frames as f32 / win_elapsed) * 10.0).round() as u32;
            slot.fps_x10.store(fps10, Ordering::Release);
            fps_frames = 0;
            fps_window_start = Instant::now();
        }
        // [CRIT-08] Drain until natural stdout EOF after drop(stdin_pipe) so tail frames are never truncated!
    }
}

fn run_rtsp_reverse_hop_loop(
    ctx: &ChannelWorkerContext,
    env_cfg: &NvrEnvConfig,
    resume_ms: &mut i64,
    my_gen: u64,
) -> bool {
    let width = ctx.ctrl.decode_width.load(Ordering::Acquire);
    let height = ctx.ctrl.decode_height.load(Ordering::Acquire);
    let frame_bytes = (width as usize) * (height as usize) * 4;
    let mut raw_buf = vec![0u8; frame_bytes];
    let mut empty_hops = 0u32;

    while !ctx.ctrl.stop_flag.load(Ordering::Acquire)
        && ctx.ctrl.generation.load(Ordering::Acquire) == my_gen
        && ctx.ctrl.effective_speed() < 0.0
    {
        while ctx.ctrl.paused.load(Ordering::Acquire)
            && ctx.slot.step_forward_budget.load(Ordering::Acquire) == 0
        {
            if ctx.ctrl.stop_flag.load(Ordering::Acquire)
                || ctx.ctrl.generation.load(Ordering::Acquire) != my_gen
                || ctx.ctrl.effective_speed() >= 0.0
            {
                return true;
            }
            ctx.slot.set_status(PlaybackStatus::Paused);
            ctx.ctrl
                .sleep_interruptible(Duration::from_millis(80), my_gen);
        }
        if ctx.ctrl.stop_flag.load(Ordering::Acquire)
            || ctx.ctrl.generation.load(Ordering::Acquire) != my_gen
            || ctx.ctrl.effective_speed() >= 0.0
        {
            return true;
        }

        if !ctx.ctrl.paused.load(Ordering::Acquire) {
            if let Some((_, ring_next_ep, ring_w, ring_h)) =
                ctx.slot.step_unpaused_ring_into(&mut raw_buf)
            {
                let abs_speed = ctx.ctrl.effective_speed().abs().max(1.0) as f64;
                ctx.ctrl.sleep_interruptible(
                    Duration::from_millis((2_000.0 / abs_speed) as u64),
                    my_gen,
                );
                ctx.slot
                    .publish_frame(ring_w, ring_h, ring_next_ep, &raw_buf, false);
                if ctx.ctrl.primary_channel.load(Ordering::Acquire) == ctx.slot.channel {
                    ctx.ctrl
                        .master_playhead_ms
                        .store(ring_next_ep, Ordering::Release);
                }
                *resume_ms = ring_next_ep;
                continue;
            }
        }

        let hop_start_wall = Instant::now();
        let cur_hop_ms = (*resume_ms).max(0);
        let req_start = epoch_ms_to_local_naive(cur_hop_ms.saturating_sub(1_000));
        let req_end = epoch_ms_to_local_naive(
            (cur_hop_ms + 3_000).min(current_wall_epoch_ms() + 60_000),
        );
        let rtsp_path = env_cfg.build_playback_rtsp_path(ctx.slot.channel, 0, req_start, req_end);
        let gw_url = match ctx.auth_gw.mint_rtsp_url(env_cfg, &rtsp_path, None) {
            Ok(u) => u,
            Err(e) => {
                let st = if is_auth_rejection(&e) {
                    PlaybackStatus::AuthError
                } else {
                    PlaybackStatus::Error
                };
                ctx.slot.set_error(st, e);
                ctx.ctrl.sleep_interruptible(Duration::from_secs(3), my_gen);
                return false;
            }
        };

        let ffmpeg_bin = resolve_trusted_binary("ffmpeg");
        let mut cmd = Command::new(ffmpeg_bin);
        configure_child_lifecycle(&mut cmd);
        cmd.arg("-nostdin")
            .arg("-v")
            .arg("error")
            .arg("-protocol_whitelist")
            .arg("rtsp,tcp,rtp")
            .arg("-rtsp_transport")
            .arg("tcp")
            .arg(crate::config::rtsp_timeout_ffmpeg_arg())
            .arg("5000000")
            .arg("-max_alloc")
            .arg("134217728")
            .arg("-probesize")
            .arg("65536")
            .arg("-analyzeduration")
            .arg("0")
            .arg("-threads")
            .arg("1")
            .arg("-i")
            .arg(&gw_url)
            .arg("-frames:v")
            .arg("1")
            .arg("-an")
            .arg("-sn")
            .arg("-dn")
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
            Err(e) => {
                ctx.slot
                    .set_error(PlaybackStatus::Error, format!("ffmpeg spawn failed: {}", e));
                ctx.ctrl.sleep_interruptible(Duration::from_secs(2), my_gen);
                return false;
            }
        };
        let Some(mut stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        };
        let (stderr_log, stderr_handle) = spawn_stderr_drain(child.stderr.take());

        if let Ok(mut guard) = ctx.slot.active_child.lock() {
            *guard = Some(child);
            if ctx.ctrl.stop_flag.load(Ordering::Acquire)
                || ctx.ctrl.generation.load(Ordering::Acquire) != my_gen
            {
                if let Some(mut c) = guard.take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                drop(guard);
                let _ = join_stderr_and_read(&stderr_log, stderr_handle);
                return false;
            }
        }

        let got_frame = stdout.read_exact(&mut raw_buf).is_ok();

        if let Ok(mut guard) = ctx.slot.active_child.lock() {
            if let Some(mut c) = guard.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
        let err_txt = join_stderr_and_read(&stderr_log, stderr_handle);

        if ctx.ctrl.stop_flag.load(Ordering::Acquire)
            || ctx.ctrl.generation.load(Ordering::Acquire) != my_gen
        {
            return false;
        }

        if got_frame {
            empty_hops = 0;
            let was_paused = ctx.ctrl.paused.load(Ordering::Acquire);
            if was_paused {
                let prev = ctx.slot.step_forward_budget.load(Ordering::Acquire);
                if prev > 0 {
                    ctx.slot.step_forward_budget.fetch_sub(1, Ordering::AcqRel);
                }
            }
            ctx.slot
                .publish_frame(width, height, cur_hop_ms, &raw_buf, true);
            if ctx.ctrl.primary_channel.load(Ordering::Acquire) == ctx.slot.channel {
                ctx.ctrl
                    .master_playhead_ms
                    .store(cur_hop_ms, Ordering::Release);
            }
            ctx.slot.set_status(if was_paused {
                PlaybackStatus::Paused
            } else {
                PlaybackStatus::Playing
            });

            if !was_paused {
                let abs_speed = ctx.ctrl.effective_speed().abs().max(1.0) as f64;
                let want_dur = Duration::from_millis((2_000.0 / abs_speed) as u64);
                let elapsed = hop_start_wall.elapsed();
                if want_dur > elapsed {
                    ctx.ctrl.sleep_interruptible(want_dur - elapsed, my_gen);
                }
            }

            if cur_hop_ms == 0 {
                ctx.slot.fps_x10.store(0, Ordering::Release);
                ctx.slot.set_status(PlaybackStatus::EndOfRecording);
                park_until_state_change(&ctx.ctrl, my_gen);
                return false;
            }
            *resume_ms = cur_hop_ms.saturating_sub(2_000);
        } else {
            if is_auth_rejection(&err_txt) || ctx.auth_gw.circuit_breaker().is_tripped() {
                ctx.slot.set_error(PlaybackStatus::AuthError, err_txt);
                ctx.ctrl.sleep_interruptible(Duration::from_secs(5), my_gen);
                return false;
            }
            empty_hops += 1;
            if empty_hops > 8 || cur_hop_ms < 2_000 {
                ctx.slot.fps_x10.store(0, Ordering::Release);
                ctx.slot.set_status(PlaybackStatus::EndOfRecording);
                park_until_state_change(&ctx.ctrl, my_gen);
                return false;
            }
            if let Some(jump_ms) = find_next_recorded_span_start(
                &ctx.timeline_cache,
                ctx.slot.channel,
                cur_hop_ms,
                false,
            ) {
                if jump_ms < cur_hop_ms {
                    *resume_ms = jump_ms;
                    continue;
                }
            }
            *resume_ms = cur_hop_ms.saturating_sub(10_000);
        }
    }
    true
}

fn run_rtsp_tier3_fallback(
    ctx: &ChannelWorkerContext,
    env_cfg: &NvrEnvConfig,
    start_ms: i64,
    my_gen: u64,
) {
    let width = ctx.ctrl.decode_width.load(Ordering::Acquire);
    let height = ctx.ctrl.decode_height.load(Ordering::Acquire);
    let frame_bytes = (width as usize) * (height as usize) * 4;
    let mut raw_buf = vec![0u8; frame_bytes];
    let mut resume_ms = start_ms;
    let mut active_subtype = 0u8;
    let mut ever_got_frame = false;

    while !ctx.ctrl.stop_flag.load(Ordering::Acquire)
        && ctx.ctrl.generation.load(Ordering::Acquire) == my_gen
    {
        let eff_speed = ctx.ctrl.effective_speed();
        if eff_speed < 0.0 {
            if !run_rtsp_reverse_hop_loop(ctx, env_cfg, &mut resume_ms, my_gen) {
                return;
            }
            continue;
        }

        let session_eff = eff_speed.max(0.5);
        let end_ms = (resume_ms + 3_600_000).min(current_wall_epoch_ms() + 60_000);
        let start_local = epoch_ms_to_local_naive(resume_ms);
        let end_local = epoch_ms_to_local_naive(end_ms.max(resume_ms + 2_000));
        let rtsp_path =
            env_cfg.build_playback_rtsp_path(ctx.slot.channel, active_subtype, start_local, end_local);

        // [CRIT-11] Only send Scale: when eff_speed > 1.05; handle 0.5x purely via client-side frame pacing
        let scale_opt = if session_eff > 1.05 {
            Some(session_eff)
        } else {
            None
        };

        let gw_url = match ctx.auth_gw.mint_rtsp_url(env_cfg, &rtsp_path, scale_opt) {
            Ok(u) => u,
            Err(e) => {
                let st = if is_auth_rejection(&e) {
                    PlaybackStatus::AuthError
                } else {
                    PlaybackStatus::Error
                };
                ctx.slot.set_error(st, e);
                ctx.ctrl.sleep_interruptible(Duration::from_secs(3), my_gen);
                return;
            }
        };

        let ffmpeg_bin = resolve_trusted_binary("ffmpeg");
        let mut cmd = Command::new(ffmpeg_bin);
        configure_child_lifecycle(&mut cmd);
        cmd.arg("-nostdin")
            .arg("-v")
            .arg("error")
            .arg("-protocol_whitelist")
            .arg("rtsp,tcp,rtp")
            .arg("-rtsp_transport")
            .arg("tcp")
            .arg(crate::config::rtsp_timeout_ffmpeg_arg())
            .arg("7000000")
            .arg("-max_alloc")
            .arg("134217728")
            .arg("-probesize")
            .arg("131072")
            .arg("-analyzeduration")
            .arg("500000");
        if !ctx.is_sync_2x2 && env_cfg.hwaccel_mode.eq_ignore_ascii_case("auto") {
            cmd.arg("-hwaccel").arg("auto");
        }
        cmd.arg("-i")
            .arg(&gw_url)
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
            Err(e) => {
                ctx.slot
                    .set_error(PlaybackStatus::Error, format!("ffmpeg spawn failed: {}", e));
                ctx.ctrl.sleep_interruptible(Duration::from_secs(2), my_gen);
                return;
            }
        };

        let Some(mut stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return;
        };
        let (stderr_log, stderr_handle) = spawn_stderr_drain(child.stderr.take());

        // [MED-09] Re-check stop_flag / generation right after storing child
        if let Ok(mut guard) = ctx.slot.active_child.lock() {
            *guard = Some(child);
            if ctx.ctrl.stop_flag.load(Ordering::Acquire)
                || ctx.ctrl.generation.load(Ordering::Acquire) != my_gen
            {
                if let Some(mut c) = guard.take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                drop(guard);
                let _ = join_stderr_and_read(&stderr_log, stderr_handle);
                return;
            }
        }

        let mut synthetic_ms = resume_ms as f64;
        let mut measured_fps = 25.0_f64;
        let mut fps_window_start = Instant::now();
        let mut fps_frames = 0u32;
        let mut got_frame_in_conn = false;
        let mut prev_delivery_wall = Instant::now();
        let mut reconnect_for_speed = false;

        while !ctx.ctrl.stop_flag.load(Ordering::Acquire)
            && ctx.ctrl.generation.load(Ordering::Acquire) == my_gen
        {
            while ctx.ctrl.paused.load(Ordering::Acquire)
                && ctx.slot.step_forward_budget.load(Ordering::Acquire) == 0
                && ctx.slot.rewind_target_ms.load(Ordering::Acquire) == i64::MIN
            {
                if ctx.ctrl.stop_flag.load(Ordering::Acquire)
                    || ctx.ctrl.generation.load(Ordering::Acquire) != my_gen
                {
                    break;
                }
                ctx.slot.set_status(PlaybackStatus::Paused);
                ctx.ctrl
                    .sleep_interruptible(Duration::from_millis(80), my_gen);
                prev_delivery_wall = Instant::now();
            }
            if ctx.ctrl.stop_flag.load(Ordering::Acquire)
                || ctx.ctrl.generation.load(Ordering::Acquire) != my_gen
            {
                break;
            }

            let cur_eff_check = ctx.ctrl.effective_speed();
            if cur_eff_check < 0.0
                || ((cur_eff_check - session_eff).abs() > 0.2
                    && (cur_eff_check > 1.05 || session_eff > 1.05))
            {
                reconnect_for_speed = true;
                break;
            }

            // [HIGH-16] Present remaining ring frames on unpause before reading new frames from stdout
            if !ctx.ctrl.paused.load(Ordering::Acquire)
                && ctx.slot.rewind_target_ms.load(Ordering::Acquire) == i64::MIN
            {
                if let Some((ring_prev_ep, ring_next_ep, ring_w, ring_h)) =
                    ctx.slot.step_unpaused_ring_into(&mut raw_buf)
                {
                    let mut p_pts = Some(ring_prev_ep);
                    if !pace_and_sync_frame(
                        &ctx.ctrl,
                        ctx.slot.channel,
                        ring_next_ep,
                        &mut p_pts,
                        &mut prev_delivery_wall,
                        my_gen,
                    ) {
                        break;
                    }
                    ctx.slot
                        .publish_frame(ring_w, ring_h, ring_next_ep, &raw_buf, false);
                    if ctx.ctrl.primary_channel.load(Ordering::Acquire) == ctx.slot.channel {
                        ctx.ctrl
                            .master_playhead_ms
                            .store(ring_next_ep, Ordering::Release);
                    }
                    synthetic_ms = ring_next_ep as f64;
                    continue;
                }
            }

            if stdout.read_exact(&mut raw_buf).is_err() {
                if let Some(landed_ep) = ctx.slot.flush_pending_rewind_on_eof() {
                    if ctx.ctrl.primary_channel.load(Ordering::Acquire) == ctx.slot.channel {
                        ctx.ctrl
                            .master_playhead_ms
                            .store(landed_ep, Ordering::Release);
                    }
                    ctx.slot.set_status(PlaybackStatus::Paused);
                }
                break;
            }

            let cur_eff = ctx.ctrl.effective_speed();
            if cur_eff < 0.0
                || ((cur_eff - session_eff).abs() > 0.2 && (cur_eff > 1.05 || session_eff > 1.05))
            {
                reconnect_for_speed = true;
                break;
            }

            let cur_abs = cur_eff.abs().max(0.25) as f64;
            let frame_step_ms = 1000.0 / measured_fps.clamp(5.0, 60.0);
            let rw_ceil = ctx.slot.rewind_target_ms.load(Ordering::Acquire);

            if !got_frame_in_conn {
                got_frame_in_conn = true;
                ever_got_frame = true;
                prev_delivery_wall = Instant::now();
            } else if rw_ceil != i64::MIN {
                synthetic_ms += frame_step_ms;
            } else if ctx.ctrl.paused.load(Ordering::Acquire) {
                let prev = ctx.slot.step_forward_budget.load(Ordering::Acquire);
                if prev > 0 {
                    ctx.slot.step_forward_budget.fetch_sub(1, Ordering::AcqRel);
                }
                synthetic_ms += frame_step_ms;
                prev_delivery_wall = Instant::now();
            } else {
                if scale_opt.is_none() && cur_abs < 0.95 {
                    // Client-side slow-motion 0.5x pacing without sending Scale: 0.50 to NVR
                    let want_dur = Duration::from_secs_f64((frame_step_ms / 1000.0) / cur_abs);
                    let elapsed = prev_delivery_wall.elapsed();
                    if want_dur > elapsed {
                        ctx.ctrl.sleep_interruptible(want_dur - elapsed, my_gen);
                        if ctx.ctrl.stop_flag.load(Ordering::Acquire)
                            || ctx.ctrl.generation.load(Ordering::Acquire) != my_gen
                        {
                            break;
                        }
                    }
                }
                // [CRIT-11] Advance synthetic_ms by wall-clock delta * cur_eff so 4x/8x/16x playhead moves at true speed!
                let now_wall = Instant::now();
                let wall_delta_ms =
                    now_wall.duration_since(prev_delivery_wall).as_secs_f64() * 1000.0;
                prev_delivery_wall = now_wall;
                let max_step = (frame_step_ms * cur_abs * 2.5).max(100.0);
                synthetic_ms += (wall_delta_ms * cur_abs).clamp(1.0, max_step);
            }

            let ep_ms = synthetic_ms.round() as i64;

            if rw_ceil != i64::MIN {
                if let Some(landed_ep) =
                    ctx.slot
                        .handle_rewind_frame(width, height, ep_ms, &raw_buf, rw_ceil)
                {
                    if ctx.ctrl.primary_channel.load(Ordering::Acquire) == ctx.slot.channel {
                        ctx.ctrl
                            .master_playhead_ms
                            .store(landed_ep, Ordering::Release);
                    }
                    ctx.slot.set_status(PlaybackStatus::Paused);
                    synthetic_ms = landed_ep as f64;
                }
                continue;
            }

            ctx.slot.publish_frame(width, height, ep_ms, &raw_buf, true);
            if ctx.ctrl.primary_channel.load(Ordering::Acquire) == ctx.slot.channel {
                ctx.ctrl.master_playhead_ms.store(ep_ms, Ordering::Release);
            }
            ctx.slot.set_status(if ctx.ctrl.paused.load(Ordering::Acquire) {
                PlaybackStatus::Paused
            } else {
                PlaybackStatus::Playing
            });

            fps_frames += 1;
            let win_elapsed = fps_window_start.elapsed().as_secs_f64();
            if win_elapsed >= 1.0 {
                let obs_fps = (fps_frames as f64 / win_elapsed).clamp(5.0, 60.0);
                measured_fps = obs_fps;
                ctx.slot
                    .fps_x10
                    .store((obs_fps * 10.0).round() as u32, Ordering::Release);
                fps_frames = 0;
                fps_window_start = Instant::now();
            }
        }

        if let Ok(mut guard) = ctx.slot.active_child.lock() {
            if let Some(mut c) = guard.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
        let err_txt = join_stderr_and_read(&stderr_log, stderr_handle);

        if ctx.ctrl.stop_flag.load(Ordering::Acquire)
            || ctx.ctrl.generation.load(Ordering::Acquire) != my_gen
        {
            return;
        }

        if reconnect_for_speed {
            resume_ms = ctx.slot.current_epoch_ms.load(Ordering::Acquire).max(0);
            continue;
        }

        if got_frame_in_conn {
            let cur_ms = ctx.slot.current_epoch_ms.load(Ordering::Acquire);
            if cur_ms + 2_000 >= current_wall_epoch_ms() {
                ctx.slot.fps_x10.store(0, Ordering::Release);
                ctx.slot.set_status(PlaybackStatus::EndOfRecording);
                park_until_state_change(&ctx.ctrl, my_gen);
                return;
            }
            // [CRIT-11] Transient RTSP EOF or pause timeout before end_ms: resume from current_epoch_ms!
            resume_ms = cur_ms.max(resume_ms + 40);
            ctx.ctrl
                .sleep_interruptible(Duration::from_millis(250), my_gen);
            continue;
        }

        if is_auth_rejection(&err_txt) || ctx.auth_gw.circuit_breaker().is_tripped() {
            ctx.slot.set_error(PlaybackStatus::AuthError, err_txt);
            ctx.ctrl.sleep_interruptible(Duration::from_secs(5), my_gen);
            return;
        }
        if !ever_got_frame && active_subtype == 0 {
            active_subtype = 1;
            continue;
        }
        if ever_got_frame {
            ctx.slot.fps_x10.store(0, Ordering::Release);
            ctx.slot.set_status(PlaybackStatus::EndOfRecording);
            park_until_state_change(&ctx.ctrl, my_gen);
            return;
        }

        let msg = if err_txt.trim().is_empty() {
            "RTSP playback stream unavailable".to_string()
        } else {
            err_txt
        };
        ctx.slot.set_error(PlaybackStatus::Error, msg);
        ctx.ctrl.sleep_interruptible(Duration::from_secs(2), my_gen);
        return;
    }
}

fn check_free_disk_bytes(dir: &Path) -> Result<u64, String> {
    let c_path = CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| "Invalid directory path".to_string())?;
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut stat) != 0 {
            return Err(format!(
                "statvfs failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let bsize = if stat.f_frsize > 0 {
            stat.f_frsize as u64
        } else {
            stat.f_bsize as u64
        };
        Ok((stat.f_bavail as u64).saturating_mul(bsize))
    }
}

fn truncate_part_file(part_path: &Path) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(part_path)
        .map(|_| ())
        .map_err(|e| format!("Failed to reset export temp file: {}", e))
}

fn build_file_out_arg(part_path: &Path) -> OsString {
    let mut out_arg = OsString::from("file:");
    out_arg.push(part_path.as_os_str());
    out_arg
}

fn register_export_child(
    active_child: &Mutex<Option<Child>>,
    cancel_flag: &AtomicBool,
    child: Child,
) -> Result<(), String> {
    let mut guard = active_child
        .lock()
        .map_err(|_| "Export child lock poisoned".to_string())?;
    *guard = Some(child);
    if cancel_flag.load(Ordering::Acquire) {
        if let Some(mut c) = guard.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        return Err("Clip export cancelled".to_string());
    }
    Ok(())
}

fn kill_and_reap_export_child(active_child: &Mutex<Option<Child>>) {
    if let Ok(mut guard) = active_child.lock() {
        if let Some(mut c) = guard.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn wait_export_child_cancellable(
    active_child: &Mutex<Option<Child>>,
    cancel_flag: &AtomicBool,
) -> Result<std::process::ExitStatus, String> {
    loop {
        if cancel_flag.load(Ordering::Acquire) {
            kill_and_reap_export_child(active_child);
            return Err("Clip export cancelled".to_string());
        }
        let step = {
            let mut guard = active_child
                .lock()
                .map_err(|_| "Export child lock poisoned".to_string())?;
            match guard.as_mut() {
                Some(c) => match c.try_wait() {
                    Ok(Some(status)) => {
                        guard.take();
                        Some(Ok(status))
                    }
                    Ok(None) => None,
                    Err(e) => {
                        guard.take();
                        Some(Err(format!("ffmpeg wait error: {}", e)))
                    }
                },
                None => Some(Err("Clip export cancelled".to_string())),
            }
        };
        if let Some(done) = step {
            return done;
        }
        thread::sleep(Duration::from_millis(15));
    }
}

#[allow(clippy::too_many_arguments)]
fn run_clip_export_job(
    env_cfg: &NvrEnvConfig,
    auth_gw: &AuthGateway,
    nvr_http: &NvrHttpClient,
    seg_cache: &Mutex<MemfdSegmentCache>,
    channel: usize,
    clean_label: &str,
    recording_dir: &Path,
    start_ms: i64,
    end_ms: i64,
    cancel_flag: &AtomicBool,
    active_child: &Mutex<Option<Child>>,
    on_progress: &mut dyn FnMut(f32, u64, &str),
) -> Result<PathBuf, String> {
    let _ = OsStrExt::as_bytes(recording_dir.as_os_str());

    if cancel_flag.load(Ordering::Acquire) {
        return Err("Clip export cancelled".to_string());
    }

    let canon_dir = ensure_private_dir(recording_dir)
        .map_err(|e| format!("Cannot prepare export directory: {}", e))?;

    let free_bytes = check_free_disk_bytes(&canon_dir)?;
    if free_bytes < MIN_EXPORT_FREE_BYTES {
        return Err(format!(
            "Insufficient disk space ({} MiB free, need >= 512 MiB)",
            free_bytes / (1024 * 1024)
        ));
    }

    let start_local = epoch_ms_to_local_naive(start_ms);
    let end_local = epoch_ms_to_local_naive(end_ms);
    let base_stem = format!(
        "CH{:02}_{}_{}_to_{}",
        channel,
        clean_label,
        start_local.format("%Y%m%d_%H%M%S"),
        end_local.format("%H%M%S")
    );

    let mut chosen_pair: Option<(PathBuf, PathBuf)> = None;
    for attempt in 0..32u32 {
        let stem = if attempt == 0 {
            base_stem.clone()
        } else {
            format!("{}_{}", base_stem, attempt)
        };
        let final_name = format!("{}.mp4", stem);
        let part_name = format!("{}.part.mp4", stem);
        let final_candidate = canon_dir.join(&final_name);
        if final_candidate.exists() {
            continue;
        }
        if let Ok((file_handle, part_path)) = create_private_file_exclusive(&canon_dir, &part_name) {
            drop(file_handle);
            chosen_pair = Some((part_path, final_candidate));
            break;
        }
    }

    let (part_path, final_path) =
        chosen_pair.ok_or_else(|| "Could not allocate exclusive .part.mp4 file".to_string())?;

    let brand_lower = env_cfg.brand.to_ascii_lowercase();
    let prefer_rtsp = brand_lower == "hikvision" || brand_lower == "custom";

    let export_res = if !prefer_rtsp {
        match remux_dhav_clip(
            env_cfg,
            nvr_http,
            seg_cache,
            channel,
            start_ms,
            end_ms,
            &part_path,
            cancel_flag,
            active_child,
            on_progress,
        ) {
            Ok(()) => Ok(()),
            Err(e) if !cancel_flag.load(Ordering::Acquire) && is_cgi_unsupported_error(&e) => {
                let _ = truncate_part_file(&part_path);
                remux_rtsp_clip(
                    env_cfg,
                    auth_gw,
                    channel,
                    start_ms,
                    end_ms,
                    &part_path,
                    cancel_flag,
                    active_child,
                    on_progress,
                )
            }
            Err(e) => Err(e),
        }
    } else {
        remux_rtsp_clip(
            env_cfg,
            auth_gw,
            channel,
            start_ms,
            end_ms,
            &part_path,
            cancel_flag,
            active_child,
            on_progress,
        )
    };

    if cancel_flag.load(Ordering::Acquire) {
        let _ = std::fs::remove_file(&part_path);
        return Err("Clip export cancelled".to_string());
    }

    if let Err(err) = export_res {
        let _ = std::fs::remove_file(&part_path);
        return Err(redact_secret_in_text(&err, &env_cfg.password));
    }

    let valid_size = std::fs::metadata(&part_path)
        .map(|m| m.len() > 256)
        .unwrap_or(false);
    if !valid_size {
        let _ = std::fs::remove_file(&part_path);
        return Err("Exported MP4 was empty (no recorded frames in selected range)".to_string());
    }

    if let Err(e) = std::fs::rename(&part_path, &final_path) {
        let _ = std::fs::remove_file(&part_path);
        return Err(format!("Atomic rename failed: {}", e));
    }

    Ok(final_path)
}

fn collect_cached_segments_for_range(
    seg_cache: &Mutex<MemfdSegmentCache>,
    channel: usize,
    start_ms: i64,
    end_ms: i64,
) -> Option<Vec<Arc<MemfdSegment>>> {
    let mut cache = seg_cache.lock().ok()?;
    let mut cur = align_segment_start_ms(start_ms);
    let mut out = Vec::new();
    while cur < end_ms {
        let seg = cache.get_ready_or_inflight(SegmentKey {
            channel,
            start_ms: cur,
        })?;
        if !seg.is_complete() || seg.packet_len() == 0 {
            return None;
        }
        out.push(seg);
        cur += SEGMENT_DURATION_MS;
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// [HIGH-17] Selects packets across cached 60s segments for zero-network MP4 export:
/// - Prepends `snap[0]` if `snap[0].pkt_type == PKT_FILE_HEADER` (`0xFE`) and `first_idx > 0`.
/// - Bounds backward pre-roll keyframe lookup to `[start_ms - 4_000, start_ms]`, else first keyframe `>= start_ms`.
/// - Tracks `last_written_epoch_ms` across segment transitions so overlapped 60s boundary packets are never written twice.
fn select_cached_export_packets(
    segment_snapshots: &[Vec<DhavPacketEntry>],
    start_ms: i64,
    end_ms: i64,
) -> Vec<(usize, DhavPacketEntry)> {
    let min_preroll_ms = start_ms.saturating_sub(4_000);
    let end_limit_ms = end_ms.saturating_add(500);
    let mut out: Vec<(usize, DhavPacketEntry)> = Vec::new();
    let mut started_from_idr = false;
    let mut wrote_file_header = false;
    let mut last_written_epoch_ms: Option<i64> = None;

    'seg_loop: for (seg_idx, snap) in segment_snapshots.iter().enumerate() {
        if snap.is_empty() {
            continue;
        }
        let prev_seg_max_epoch_ms = if seg_idx > 0 {
            last_written_epoch_ms
        } else {
            None
        };
        let mut passed_seg_overlap = prev_seg_max_epoch_ms.is_none();

        let first_idx = if !started_from_idr {
            snap.iter()
                .rposition(|p| {
                    p.is_keyframe && p.epoch_ms >= min_preroll_ms && p.epoch_ms <= start_ms
                })
                .or_else(|| snap.iter().position(|p| p.is_keyframe && p.epoch_ms >= start_ms))
                .unwrap_or(snap.len())
        } else {
            0
        };

        if !wrote_file_header && first_idx > 0 && snap[0].pkt_type == PKT_FILE_HEADER {
            out.push((seg_idx, snap[0]));
            wrote_file_header = true;
        }

        for entry in snap.iter().skip(first_idx) {
            if entry.pkt_type == PKT_FILE_HEADER {
                if !wrote_file_header {
                    out.push((seg_idx, *entry));
                    wrote_file_header = true;
                }
                continue;
            }
            if entry.epoch_ms > end_limit_ms {
                break 'seg_loop;
            }
            if !passed_seg_overlap {
                if let Some(floor_ms) = prev_seg_max_epoch_ms {
                    if entry.epoch_ms <= floor_ms {
                        continue;
                    }
                }
                passed_seg_overlap = true;
            }
            if !started_from_idr {
                if !entry.is_keyframe {
                    continue;
                }
                started_from_idr = true;
            }
            out.push((seg_idx, *entry));
            last_written_epoch_ms = Some(
                last_written_epoch_ms
                    .map(|m| m.max(entry.epoch_ms))
                    .unwrap_or(entry.epoch_ms),
            );
        }
    }

    if !started_from_idr {
        Vec::new()
    } else {
        out
    }
}

#[allow(clippy::too_many_arguments)]
fn remux_dhav_clip(
    env_cfg: &NvrEnvConfig,
    nvr_http: &NvrHttpClient,
    seg_cache: &Mutex<MemfdSegmentCache>,
    channel: usize,
    start_ms: i64,
    end_ms: i64,
    part_path: &Path,
    cancel_flag: &AtomicBool,
    active_child: &Mutex<Option<Child>>,
    on_progress: &mut dyn FnMut(f32, u64, &str),
) -> Result<(), String> {
    match remux_dhav_clip_once(
        env_cfg,
        nvr_http,
        seg_cache,
        channel,
        start_ms,
        end_ms,
        part_path,
        true,
        cancel_flag,
        active_child,
        on_progress,
    ) {
        Ok(()) => Ok(()),
        Err(e)
            if cancel_flag.load(Ordering::Acquire)
                || is_auth_rejection(&e)
                || is_cgi_unsupported_error(&e) =>
        {
            Err(e)
        }
        Err(_) => {
            // [CRIT-10] Retry once with video-only (`-an`) if camera has broken 0-channel 0x83 / G.726 / ADPCM audio
            truncate_part_file(part_path)?;
            on_progress(0.05, 0, "Retrying video-only remux...");
            remux_dhav_clip_once(
                env_cfg,
                nvr_http,
                seg_cache,
                channel,
                start_ms,
                end_ms,
                part_path,
                false,
                cancel_flag,
                active_child,
                on_progress,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn remux_dhav_clip_once(
    env_cfg: &NvrEnvConfig,
    nvr_http: &NvrHttpClient,
    seg_cache: &Mutex<MemfdSegmentCache>,
    channel: usize,
    start_ms: i64,
    end_ms: i64,
    part_path: &Path,
    with_audio: bool,
    cancel_flag: &AtomicBool,
    active_child: &Mutex<Option<Child>>,
    on_progress: &mut dyn FnMut(f32, u64, &str),
) -> Result<(), String> {
    if cancel_flag.load(Ordering::Acquire) {
        return Err("Clip export cancelled".to_string());
    }

    let ffmpeg_bin = resolve_trusted_binary("ffmpeg");
    let mut cmd = Command::new(ffmpeg_bin);
    configure_child_lifecycle(&mut cmd);
    let out_arg = build_file_out_arg(part_path);
    cmd.arg("-y")
        .arg("-v")
        .arg("error")
        .arg("-protocol_whitelist")
        .arg("pipe,file")
        .arg("-max_alloc")
        .arg("134217728")
        .arg("-fflags")
        .arg("+genpts")
        .arg("-f")
        .arg("dhav")
        .arg("-i")
        .arg("pipe:0");
    if with_audio {
        cmd.arg("-map")
            .arg("0:v:0")
            .arg("-map")
            .arg("0:a:0?")
            .arg("-sn")
            .arg("-dn")
            .arg("-c:v")
            .arg("copy")
            .arg("-c:a")
            .arg("aac")
            .arg("-ac")
            .arg("1")
            .arg("-ar")
            .arg("16000")
            .arg("-b:a")
            .arg("64k");
    } else {
        cmd.arg("-map")
            .arg("0:v:0")
            .arg("-c:v")
            .arg("copy")
            .arg("-an")
            .arg("-sn")
            .arg("-dn");
    }
    cmd.arg("-avoid_negative_ts")
        .arg("make_zero")
        .arg("-movflags")
        .arg("+faststart")
        .arg("-fs")
        .arg("4294967296")
        .arg("-f")
        .arg("mp4")
        .arg(&out_arg)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn ffmpeg exporter: {}", e))?;
    let mut stdin_pipe = match child.stdin.take() {
        Some(p) => p,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Missing ffmpeg stdin pipe".to_string());
        }
    };
    let (stderr_log, stderr_handle) = spawn_stderr_drain(child.stderr.take());
    if let Err(e) = register_export_child(active_child, cancel_flag, child) {
        drop(stdin_pipe);
        let _ = join_stderr_and_read(&stderr_log, stderr_handle);
        return Err(e);
    }

    let mut fed_bytes: u64 = 0;

    // Zero-network RAM cache fast path (< 0.2s remux when segments are already in MemfdSegmentCache!)
    if let Some(segments) = collect_cached_segments_for_range(seg_cache, channel, start_ms, end_ms) {
        let snapshots: Vec<Vec<DhavPacketEntry>> =
            segments.iter().map(|s| s.packets_snapshot()).collect();
        let selected = select_cached_export_packets(&snapshots, start_ms, end_ms);
        if !selected.is_empty() {
            let total_pkts = selected.len().max(1);
            on_progress(0.10, 0, "RAM Cache Remux");
            let mut last_reported_bytes: u64 = 0;
            let mut pkt_buf = Vec::with_capacity(131_072);
            for (idx, (seg_idx, entry)) in selected.into_iter().enumerate() {
                if cancel_flag.load(Ordering::Acquire) || fed_bytes >= MAX_EXPORT_FED_BYTES {
                    break;
                }
                if segments[seg_idx].read_packet(&entry, &mut pkt_buf).is_ok() {
                    if stdin_pipe.write_all(&pkt_buf).is_err() {
                        break;
                    }
                    fed_bytes = fed_bytes.saturating_add(pkt_buf.len() as u64);
                    if fed_bytes.saturating_sub(last_reported_bytes) >= 262_144
                        || idx + 1 == total_pkts
                    {
                        last_reported_bytes = fed_bytes;
                        let frac =
                            (0.10 + 0.80 * ((idx + 1) as f32 / total_pkts as f32)).clamp(0.10, 0.90);
                        on_progress(frac, fed_bytes, "RAM Cache Remux");
                    }
                }
            }
        }
    }

    // [CRIT-10] 60-second chunked network DHAV export (> 96 MiB safe, up to 4 GiB)
    if fed_bytes == 0 && !cancel_flag.load(Ordering::Acquire) {
        let floor_start_ms = ((start_ms.saturating_sub(1_500).max(0)) / 1_000) * 1_000;
        let ceil_end_ms = (((end_ms + 500) + 999) / 1_000) * 1_000;
        let safe_end_ms = ceil_end_ms.max(floor_start_ms + 1_000);
        let total_slices = ((safe_end_ms - floor_start_ms + 59_999) / 60_000).max(1) as usize;
        let total_span_f = (safe_end_ms - floor_start_ms).max(1) as f32;

        for subtype in [0u8, 1u8] {
            if subtype == 1 && (fed_bytes > 0 || cancel_flag.load(Ordering::Acquire)) {
                break;
            }

            let mut cur_ms = floor_start_ms;
            let mut slice_idx = 0usize;
            let mut wrote_header = false;
            let mut started_idr = false;
            let mut last_epoch_ms: Option<i64> = None;
            let mut last_reported_bytes: u64 = fed_bytes;
            let min_preroll_ms = start_ms.saturating_sub(4_000);
            let end_limit_ms = end_ms.saturating_add(500);

            let slice_loop_res: Result<(), String> = (|| {
                while cur_ms < safe_end_ms
                    && !cancel_flag.load(Ordering::Acquire)
                    && fed_bytes < MAX_EXPORT_FED_BYTES
                {
                    let next_ms = (cur_ms + 60_000).min(safe_end_ms);
                    if next_ms <= cur_ms {
                        break;
                    }
                    let stage_label =
                        format!("Downloading slice {}/{}", slice_idx + 1, total_slices);
                    let base_fraction = slice_idx as f32 / total_slices as f32;
                    let base_prog = (0.05 + 0.85 * base_fraction).clamp(0.05, 0.90);
                    on_progress(base_prog, fed_bytes, &stage_label);

                    let slice_start_local = epoch_ms_to_local_naive(cur_ms);
                    let slice_end_local = epoch_ms_to_local_naive(next_ms);
                    let prev_slice_max_ep = if slice_idx > 0 { last_epoch_ms } else { None };
                    let mut passed_overlap = prev_slice_max_ep.is_none();
                    let mut scanner = DhavPacketScanner::new(cur_ms);
                    let mut reached_clip_end = false;
                    let mut pipe_broken = false;

                    let res = block_on_nvr_io(nvr_http.stream_dhav_time_slice_cancellable(
                        env_cfg,
                        channel,
                        subtype,
                        slice_start_local,
                        slice_end_local,
                        256 * 1024 * 1024,
                        Some(cancel_flag),
                        |chunk| {
                            if cancel_flag.load(Ordering::Acquire)
                                || fed_bytes >= MAX_EXPORT_FED_BYTES
                            {
                                return false;
                            }
                            let cont = scanner.push_chunk_with_bytes(chunk, |entry, pkt_bytes| {
                                if fed_bytes >= MAX_EXPORT_FED_BYTES {
                                    return false;
                                }
                                if entry.pkt_type == PKT_FILE_HEADER {
                                    if !wrote_header {
                                        if stdin_pipe.write_all(pkt_bytes).is_err() {
                                            pipe_broken = true;
                                            return false;
                                        }
                                        fed_bytes = fed_bytes.saturating_add(pkt_bytes.len() as u64);
                                        wrote_header = true;
                                    }
                                    return true;
                                }
                                if entry.epoch_ms > end_limit_ms {
                                    reached_clip_end = true;
                                    return false;
                                }
                                if !passed_overlap {
                                    if let Some(floor_ep) = prev_slice_max_ep {
                                        if entry.epoch_ms <= floor_ep {
                                            return true;
                                        }
                                    }
                                    passed_overlap = true;
                                }
                                if !started_idr {
                                    if !entry.is_keyframe || entry.epoch_ms < min_preroll_ms {
                                        return true;
                                    }
                                    started_idr = true;
                                }
                                if stdin_pipe.write_all(pkt_bytes).is_err() {
                                    pipe_broken = true;
                                    return false;
                                }
                                fed_bytes = fed_bytes.saturating_add(pkt_bytes.len() as u64);
                                last_epoch_ms = Some(
                                    last_epoch_ms
                                        .map(|m| m.max(entry.epoch_ms))
                                        .unwrap_or(entry.epoch_ms),
                                );
                                true
                            });
                            if fed_bytes.saturating_sub(last_reported_bytes) >= 262_144 {
                                last_reported_bytes = fed_bytes;
                                let time_frac = last_epoch_ms
                                    .map(|ep| {
                                        ((ep - floor_start_ms).max(0) as f32 / total_span_f)
                                            .clamp(base_fraction, 1.0)
                                    })
                                    .unwrap_or(base_fraction);
                                let prog = (0.05 + 0.85 * time_frac).clamp(0.05, 0.90);
                                on_progress(prog, fed_bytes, &stage_label);
                            }
                            cont
                        },
                    ));

                    if cancel_flag.load(Ordering::Acquire) {
                        return Err("Clip export cancelled".to_string());
                    }
                    if let Err(err) = res {
                        if !pipe_broken && !reached_clip_end {
                            return Err(err);
                        }
                    }
                    if pipe_broken || reached_clip_end {
                        break;
                    }
                    cur_ms = next_ms;
                    slice_idx += 1;
                }
                Ok(())
            })();

            if let Err(err) = slice_loop_res {
                drop(stdin_pipe);
                kill_and_reap_export_child(active_child);
                let _ = join_stderr_and_read(&stderr_log, stderr_handle);
                return Err(err);
            }
        }
    }

    drop(stdin_pipe);
    on_progress(0.95, fed_bytes, "Finalizing MP4...");
    let status = match wait_export_child_cancellable(active_child, cancel_flag) {
        Ok(s) => s,
        Err(e) => {
            let _ = join_stderr_and_read(&stderr_log, stderr_handle);
            return Err(e);
        }
    };
    let err_txt = join_stderr_and_read(&stderr_log, stderr_handle);

    if !status.success() || fed_bytes == 0 {
        if !err_txt.trim().is_empty() {
            return Err(format!("MP4 remux failed: {}", err_txt.trim()));
        }
        return Err("No recorded DHAV video packets found in clip range".to_string());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn remux_rtsp_clip(
    env_cfg: &NvrEnvConfig,
    auth_gw: &AuthGateway,
    channel: usize,
    start_ms: i64,
    end_ms: i64,
    part_path: &Path,
    cancel_flag: &AtomicBool,
    active_child: &Mutex<Option<Child>>,
    on_progress: &mut dyn FnMut(f32, u64, &str),
) -> Result<(), String> {
    let mut last_err = String::new();
    for subtype in [0u8, 1u8] {
        if subtype == 1 {
            truncate_part_file(part_path)?;
        }
        match remux_rtsp_clip_once(
            env_cfg,
            auth_gw,
            channel,
            subtype,
            start_ms,
            end_ms,
            part_path,
            true,
            cancel_flag,
            active_child,
            on_progress,
        ) {
            Ok(()) => return Ok(()),
            Err(e) => {
                if cancel_flag.load(Ordering::Acquire)
                    || is_auth_rejection(&e)
                    || auth_gw.circuit_breaker().is_tripped()
                {
                    return Err(e);
                }
                last_err = e.clone();
                let lower = e.to_ascii_lowercase();
                let stream_missing = lower.contains("404") || lower.contains("454");
                if !stream_missing {
                    truncate_part_file(part_path)?;
                    match remux_rtsp_clip_once(
                        env_cfg,
                        auth_gw,
                        channel,
                        subtype,
                        start_ms,
                        end_ms,
                        part_path,
                        false,
                        cancel_flag,
                        active_child,
                        on_progress,
                    ) {
                        Ok(()) => return Ok(()),
                        Err(e2) => {
                            if cancel_flag.load(Ordering::Acquire)
                                || is_auth_rejection(&e2)
                                || auth_gw.circuit_breaker().is_tripped()
                            {
                                return Err(e2);
                            }
                            last_err = e2;
                        }
                    }
                }
            }
        }
    }
    Err(if last_err.is_empty() {
        "RTSP MP4 clip export failed".to_string()
    } else {
        last_err
    })
}

#[allow(clippy::too_many_arguments)]
fn remux_rtsp_clip_once(
    env_cfg: &NvrEnvConfig,
    auth_gw: &AuthGateway,
    channel: usize,
    subtype: u8,
    start_ms: i64,
    end_ms: i64,
    part_path: &Path,
    with_audio: bool,
    cancel_flag: &AtomicBool,
    active_child: &Mutex<Option<Child>>,
    on_progress: &mut dyn FnMut(f32, u64, &str),
) -> Result<(), String> {
    if cancel_flag.load(Ordering::Acquire) {
        return Err("Clip export cancelled".to_string());
    }

    let start_local = epoch_ms_to_local_naive(start_ms);
    let end_local = epoch_ms_to_local_naive(end_ms + 1_000);
    let rtsp_path = env_cfg.build_playback_rtsp_path(channel, subtype, start_local, end_local);
    let gw_url = auth_gw.mint_rtsp_url(env_cfg, &rtsp_path, None)?;
    let clip_dur_secs = ((end_ms - start_ms) as f64 / 1000.0).max(0.5);
    let dur_sec = format!("{:.2}", clip_dur_secs);
    let out_arg = build_file_out_arg(part_path);

    let ffmpeg_bin = resolve_trusted_binary("ffmpeg");
    let mut cmd = Command::new(ffmpeg_bin);
    configure_child_lifecycle(&mut cmd);
    cmd.arg("-nostdin")
        .arg("-y")
        .arg("-v")
        .arg("error")
        .arg("-protocol_whitelist")
        .arg("rtsp,tcp,rtp,file")
        .arg("-rtsp_transport")
        .arg("tcp")
        .arg(crate::config::rtsp_timeout_ffmpeg_arg())
        .arg("8000000")
        .arg("-max_alloc")
        .arg("134217728")
        .arg("-fflags")
        .arg("+genpts")
        .arg("-i")
        .arg(&gw_url)
        .arg("-t")
        .arg(&dur_sec);
    if with_audio {
        cmd.arg("-map")
            .arg("0:v:0")
            .arg("-map")
            .arg("0:a:0?")
            .arg("-sn")
            .arg("-dn")
            .arg("-c:v")
            .arg("copy")
            .arg("-c:a")
            .arg("aac")
            .arg("-ac")
            .arg("1")
            .arg("-ar")
            .arg("16000")
            .arg("-b:a")
            .arg("64k");
    } else {
        cmd.arg("-map")
            .arg("0:v:0")
            .arg("-c:v")
            .arg("copy")
            .arg("-an")
            .arg("-sn")
            .arg("-dn");
    }
    cmd.arg("-avoid_negative_ts")
        .arg("make_zero")
        .arg("-movflags")
        .arg("+faststart")
        .arg("-fs")
        .arg("4294967296")
        .arg("-f")
        .arg("mp4")
        .arg(&out_arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn RTSP exporter: {}", e))?;
    let (stderr_log, stderr_handle) = spawn_stderr_drain(child.stderr.take());
    if let Err(e) = register_export_child(active_child, cancel_flag, child) {
        let _ = join_stderr_and_read(&stderr_log, stderr_handle);
        return Err(e);
    }

    on_progress(0.08, 0, "Streaming RTSP clip...");
    let started_at = Instant::now();
    let mut last_poll = Instant::now();
    let status = loop {
        if cancel_flag.load(Ordering::Acquire) {
            kill_and_reap_export_child(active_child);
            let _ = join_stderr_and_read(&stderr_log, stderr_handle);
            return Err("Clip export cancelled".to_string());
        }
        let step = {
            let mut guard = active_child
                .lock()
                .map_err(|_| "Export child lock poisoned".to_string())?;
            match guard.as_mut() {
                Some(c) => match c.try_wait() {
                    Ok(Some(st)) => {
                        guard.take();
                        Some(Ok(st))
                    }
                    Ok(None) => None,
                    Err(e) => {
                        guard.take();
                        Some(Err(format!("ffmpeg wait error: {}", e)))
                    }
                },
                None => Some(Err("Clip export cancelled".to_string())),
            }
        };
        if let Some(done) = step {
            match done {
                Ok(st) => break st,
                Err(e) => {
                    let _ = join_stderr_and_read(&stderr_log, stderr_handle);
                    return Err(e);
                }
            }
        }
        if last_poll.elapsed() >= Duration::from_millis(250) {
            last_poll = Instant::now();
            let written = std::fs::metadata(part_path).map(|m| m.len()).unwrap_or(0);
            let time_frac = (started_at.elapsed().as_secs_f64() / clip_dur_secs).clamp(0.0, 1.0) as f32;
            let prog = (0.08 + 0.82 * time_frac).clamp(0.08, 0.90);
            on_progress(prog, written, "Streaming RTSP clip...");
        }
        thread::sleep(Duration::from_millis(20));
    };

    let written_final = std::fs::metadata(part_path).map(|m| m.len()).unwrap_or(0);
    on_progress(0.95, written_final, "Finalizing MP4...");
    let err_txt = join_stderr_and_read(&stderr_log, stderr_handle);

    if !status.success() {
        return Err(if err_txt.trim().is_empty() {
            "RTSP MP4 clip export failed".to_string()
        } else {
            err_txt.trim().to_string()
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_synthetic_dhav_packet(
        pkt_type: u8,
        seq: u32,
        dt: NaiveDateTime,
        ms: u16,
        payload_len: usize,
    ) -> Vec<u8> {
        let total_len = 24 + payload_len + 8;
        let mut buf = vec![0u8; total_len];
        buf[0..4].copy_from_slice(b"DHAV");
        buf[4] = pkt_type;
        buf[8..12].copy_from_slice(&seq.to_le_bytes());
        buf[12..16].copy_from_slice(&(total_len as u32).to_le_bytes());
        let packed_date = DhavPacketScanner::pack_rtc_datetime(dt);
        buf[16..20].copy_from_slice(&packed_date.to_le_bytes());
        buf[20..22].copy_from_slice(&ms.to_le_bytes());
        buf[22] = 0;
        buf[23] = buf[0..23].iter().fold(0u8, |a, &b| a.wrapping_add(b));
        for b in &mut buf[24..24 + payload_len] {
            *b = 0xAB;
        }
        buf[24 + payload_len..28 + payload_len].copy_from_slice(b"dhav");
        buf[28 + payload_len..32 + payload_len].copy_from_slice(&(total_len as u32).to_le_bytes());
        buf
    }

    #[test]
    fn test_dhav_rtc_pack_and_unpack_roundtrip() {
        let dt = NaiveDate::from_ymd_opt(2026, 11, 18)
            .unwrap()
            .and_hms_opt(14, 3, 22)
            .unwrap();
        let packed = DhavPacketScanner::pack_rtc_datetime(dt);
        let unpacked = DhavPacketScanner::unpack_rtc_datetime(packed).expect("valid RTC datetime");
        assert_eq!(unpacked, dt);

        // Invalid month 15 returns None safely without panic
        let bad = (26 << 26) | (15 << 22) | (18 << 17);
        assert!(DhavPacketScanner::unpack_rtc_datetime(bad).is_none());
    }

    #[test]
    fn test_dhav_scanner_24byte_headers_keyframe_filtering_and_corruption_resync() {
        let dt = NaiveDate::from_ymd_opt(2026, 11, 18)
            .unwrap()
            .and_hms_opt(14, 3, 22)
            .unwrap();
        let base_ms = local_naive_to_epoch_ms(dt);

        let p_idr = build_synthetic_dhav_packet(0xFD, 1, dt, 100, 120);
        let p_pframe = build_synthetic_dhav_packet(0xFC, 2, dt, 140, 60);
        let p_audio = build_synthetic_dhav_packet(0xF0, 3, dt, 160, 32);
        let p_same_ms = build_synthetic_dhav_packet(0xFD, 4, dt, 140, 100);

        let mut stream = Vec::new();
        stream.extend_from_slice(b"JUNK_PREFIX_BYTES");
        stream.extend_from_slice(&p_idr);
        // 1. Inject a corrupted DHAV header with bogus 99MB length to verify resync
        let mut corrupt_len = [0u8; 24];
        corrupt_len[0..4].copy_from_slice(b"DHAV");
        corrupt_len[4] = 0xFC;
        corrupt_len[12..16].copy_from_slice(&(99_000_000u32).to_le_bytes());
        corrupt_len[23] = corrupt_len[0..23].iter().fold(0u8, |a, &b| a.wrapping_add(b));
        stream.extend_from_slice(&corrupt_len);
        // 2. [MED-08] Inject false "DHAV" payload bytes with plausible 4MB length but invalid checksum:
        // must be rejected immediately in 24 bytes without stalling `carry` waiting for 4MB!
        let mut false_magic = [0u8; 24];
        false_magic[0..4].copy_from_slice(b"DHAV");
        false_magic[4] = 0xFC;
        false_magic[12..16].copy_from_slice(&(4_000_000u32).to_le_bytes());
        false_magic[23] = false_magic[0..23]
            .iter()
            .fold(0u8, |a, &b| a.wrapping_add(b))
            .wrapping_add(1);
        stream.extend_from_slice(&false_magic);
        stream.extend_from_slice(&p_pframe);
        stream.extend_from_slice(&p_audio);
        stream.extend_from_slice(&p_same_ms);

        // Feed in small irregular chunks (19 bytes at a time) to test boundary carry logic
        let mut scanner = DhavPacketScanner::new(base_ms);
        let mut parsed = Vec::new();
        for chunk in stream.chunks(19) {
            parsed.extend(scanner.push_chunk(chunk));
        }

        assert_eq!(parsed.len(), 4);
        assert!(parsed[0].is_keyframe);
        assert_eq!(parsed[0].pkt_type, 0xFD);
        assert_eq!(parsed[0].epoch_ms, base_ms + 100);

        assert!(!parsed[1].is_keyframe);
        assert_eq!(parsed[1].pkt_type, 0xFC);
        assert_eq!(parsed[1].epoch_ms, base_ms + 140);

        assert!(!parsed[2].is_keyframe);
        assert_eq!(parsed[2].pkt_type, 0xF0);

        // Monotonic resolution when duplicate u16_ts occurs in same RTC second
        assert!(parsed[3].is_keyframe);
        assert_eq!(parsed[3].epoch_ms, base_ms + 141);

        let keyframes: Vec<_> = parsed.iter().filter(|p| p.is_keyframe).collect();
        assert_eq!(keyframes.len(), 2);
    }

    #[test]
    fn test_dhav_scanner_25fps_free_running_u16_ts_wraps_999_to_1000_and_65535_to_0() {
        let start_dt = NaiveDate::from_ymd_opt(2026, 11, 18)
            .unwrap()
            .and_hms_opt(14, 0, 0)
            .unwrap();
        let base_ms = local_naive_to_epoch_ms(start_dt);

        // Simulate 1,700 frames at 25fps (68 seconds), starting at u16_ts = 920 so the 16-bit
        // millisecond counter wraps across BOTH 999 -> 1000 AND 65535 -> 0 (at ~64.6s) and
        // stays out-of-phase with 1000ms RTC seconds after wrapping 65536!
        let total_frames = 1_700usize;
        let mut scanner = DhavPacketScanner::new(base_ms);
        let mut parsed = Vec::with_capacity(total_frames);

        for i in 0..total_frames {
            let elapsed_ms = 920i64 + (i as i64) * 40;
            let sec_offset = elapsed_ms / 1_000;
            let dt = start_dt + chrono::Duration::seconds(sec_offset);
            let free_running_u16 = (elapsed_ms as u64 & 0xFFFF) as u16;
            let pkt_type = if i % 25 == 0 { PKT_VIDEO_IDR } else { PKT_VIDEO_P };
            let raw = build_synthetic_dhav_packet(pkt_type, (i + 1) as u32, dt, free_running_u16, 32);
            parsed.extend(scanner.push_chunk(&raw));
        }

        assert_eq!(parsed.len(), total_frames);
        assert_eq!(parsed[0].epoch_ms, base_ms + 920);
        for i in 1..parsed.len() {
            let delta = parsed[i].epoch_ms - parsed[i - 1].epoch_ms;
            assert_eq!(
                delta, 40,
                "Expected smooth 40ms frame delta at frame {}, got {}ms ({} -> {})",
                i, delta, parsed[i - 1].epoch_ms, parsed[i].epoch_ms
            );
        }

        // Also verify a > 1.5s motion clip gap re-anchors cleanly to the new clip's RTC timestamp
        let jumped_dt = start_dt + chrono::Duration::seconds(120);
        let jumped_pkt = build_synthetic_dhav_packet(PKT_VIDEO_IDR, 2_000, jumped_dt, 500, 32);
        let after_jump = scanner.push_chunk(&jumped_pkt);
        assert_eq!(after_jump.len(), 1);
        assert_eq!(after_jump[0].epoch_ms, local_naive_to_epoch_ms(jumped_dt));
    }

    #[test]
    fn test_synthetic_reverse_header_rewrite_preserves_checksum() {
        let dt = NaiveDate::from_ymd_opt(2026, 11, 18)
            .unwrap()
            .and_hms_opt(14, 3, 22)
            .unwrap();
        let mut pkt = build_synthetic_dhav_packet(0xFD, 77, dt, 420, 64);
        DhavPacketScanner::rewrite_synthetic_monotonic_header(&mut pkt, 5);
        let expected_csum = pkt[0..23].iter().fold(0u8, |a, &b| a.wrapping_add(b));
        assert_eq!(pkt[23], expected_csum);
        let seq = u32::from_le_bytes([pkt[8], pkt[9], pkt[10], pkt[11]]);
        assert_eq!(seq, 5);
    }

    #[test]
    fn test_memfd_segment_cache_lru_eviction_and_clear() {
        // Use a 4096-byte cap to exercise strict LRU eviction deterministically
        let mut cache = MemfdSegmentCache::new(4096);

        let k1 = SegmentKey {
            channel: 1,
            start_ms: 60_000,
        };
        let k2 = SegmentKey {
            channel: 1,
            start_ms: 120_000,
        };
        let k3 = SegmentKey {
            channel: 1,
            start_ms: 180_000,
        };

        let (s1, d1) = cache.acquire_or_create(k1).expect("memfd 1");
        assert!(d1);
        let payload = vec![0x55u8; 1800];
        let len1 = s1.append_chunk_and_index(&payload, &[]).unwrap();
        cache.record_segment_growth(k1, len1);
        s1.mark_finished(None, false);
        cache.finish_inflight(k1, true);

        let (s2, d2) = cache.acquire_or_create(k2).expect("memfd 2");
        assert!(d2);
        let len2 = s2.append_chunk_and_index(&payload, &[]).unwrap();
        cache.record_segment_growth(k2, len2);
        s2.mark_finished(None, false);
        cache.finish_inflight(k2, true);

        // Touch k1 so k2 becomes the oldest LRU victim
        assert!(cache.get_ready_or_inflight(k1).is_some());

        let (s3, d3) = cache.acquire_or_create(k3).expect("memfd 3");
        assert!(d3);
        let len3 = s3.append_chunk_and_index(&payload, &[]).unwrap();
        cache.record_segment_growth(k3, len3);
        s3.mark_finished(None, false);
        cache.finish_inflight(k3, true);

        assert!(cache.total_bytes() <= 4096);
        assert!(cache.get_ready_or_inflight(k1).is_some());
        assert!(cache.get_ready_or_inflight(k3).is_some());
        // k2 was evicted; verify concurrent read_raw_at on surviving k1 works
        let mut verify = [0u8; 16];
        assert_eq!(s1.read_raw_at(100, &mut verify).unwrap(), 16);
        assert_eq!(verify, [0x55u8; 16]);

        cache.clear();
        assert_eq!(cache.total_bytes(), 0);
        assert!(cache.is_empty());
    }

    #[test]
    fn test_memfd_cache_never_evicts_inflight_and_unlinked_tempfile_leaves_zero_dir_entry() {
        let mut cache = MemfdSegmentCache::new(2_048);
        let k1 = SegmentKey {
            channel: 1,
            start_ms: 60_000,
        };
        let k2 = SegmentKey {
            channel: 2,
            start_ms: 60_000,
        };

        // Both k1 and k2 remain in_flight and exceed 2048 bytes combined: neither must be evicted!
        let (s1, _) = cache.acquire_or_create(k1).unwrap();
        let (s2, _) = cache.acquire_or_create(k2).unwrap();
        let chunk = vec![0x77u8; 1_600];
        cache.record_segment_growth(k1, s1.append_chunk_and_index(&chunk, &[]).unwrap());
        cache.record_segment_growth(k2, s2.append_chunk_and_index(&chunk, &[]).unwrap());
        assert!(cache.get_ready_or_inflight(k1).is_some());
        assert!(cache.get_ready_or_inflight(k2).is_some());

        // Once both finish in-flight, LRU eviction enforces the 2048-byte cap cleanly
        s1.mark_finished(None, false);
        cache.finish_inflight(k1, true);
        s2.mark_finished(None, false);
        cache.finish_inflight(k2, true);
        assert!(cache.total_bytes() <= 2_048);

        // [MED-11] Verify unlinked anonymous tempfile fallback leaves zero directory entries
        let test_dir = std::env::temp_dir().join(format!(
            "nvr_pb_anon_test_{}_{:08x}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&test_dir).unwrap();
        let anon_file = create_unlinked_anonymous_tempfile_in(&test_dir).unwrap();
        assert_eq!(std::fs::read_dir(&test_dir).unwrap().count(), 0);
        anon_file.write_all_at(b"DHAV_ANON_OK", 0).unwrap();
        let mut read_back = [0u8; 12];
        anon_file.read_exact_at(&mut read_back, 0).unwrap();
        assert_eq!(&read_back, b"DHAV_ANON_OK");
        drop(anon_file);
        let _ = std::fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_download_semaphore_caps_concurrent_permits_and_aborts_on_stop() {
        let sem = DownloadSemaphore::new(2);
        let stop = AtomicBool::new(false);
        let p1 = sem.acquire_interruptible(&stop).expect("permit 1");
        let p2 = sem.acquire_interruptible(&stop).expect("permit 2");

        // Setting stop = true causes a blocked acquire to abort immediately with None
        stop.store(true, Ordering::Release);
        assert!(sem.acquire_interruptible(&stop).is_none());

        stop.store(false, Ordering::Release);
        drop(p1);
        let p3 = sem.acquire_interruptible(&stop).expect("permit 3 after drop");
        drop(p2);
        drop(p3);
    }

    #[test]
    fn test_smart_skip_fast_glide_and_instant_jump_logic() {
        let events = vec![
            RecordedSpan {
                channel: 1,
                start_ms: 20_000,
                end_ms: 35_000,
                kind: NvrEventKind::Motion,
                file_path: None,
                byte_len: 0,
                main_stream: true,
            },
            RecordedSpan {
                channel: 1,
                start_ms: 80_000,
                end_ms: 95_000,
                kind: NvrEventKind::Human,
                file_path: None,
                byte_len: 0,
                main_stream: true,
            },
        ];

        // 1. FastGlide8x in quiet zone -> 8.0x (and respects base_speed = 16.0x -> 16.0x, plus empty events -> 8.0x)
        assert_eq!(
            evaluate_smart_skip(
                SmartSkipMode::FastGlide8x,
                false,
                1.0,
                5_000,
                &events,
                None,
                false
            ),
            SmartSkipDecision::SetEffectiveSpeed(8.0)
        );
        assert_eq!(
            evaluate_smart_skip(
                SmartSkipMode::FastGlide8x,
                false,
                16.0,
                5_000,
                &events,
                None,
                false
            ),
            SmartSkipDecision::SetEffectiveSpeed(16.0)
        );
        assert_eq!(
            evaluate_smart_skip(
                SmartSkipMode::FastGlide8x,
                false,
                2.0,
                5_000,
                &[],
                None,
                false
            ),
            SmartSkipDecision::SetEffectiveSpeed(8.0)
        );

        // 2. FastGlide8x within [start_ms - 3000, end_ms + 3000] -> drops to 1.0x!
        assert_eq!(
            evaluate_smart_skip(
                SmartSkipMode::FastGlide8x,
                false,
                1.0,
                17_500,
                &events,
                None,
                false
            ),
            SmartSkipDecision::SetEffectiveSpeed(1.0)
        );

        // 3. FastGlide8x with Human filter ignores Motion span at 20_000 -> stays 8.0x!
        assert_eq!(
            evaluate_smart_skip(
                SmartSkipMode::FastGlide8x,
                false,
                1.0,
                25_000,
                &events,
                Some(NvrEventKind::Human),
                false
            ),
            SmartSkipDecision::SetEffectiveSpeed(8.0)
        );

        // 4. InstantJump with Human filter jumps directly to 80_000 - 3_000 = 77_000!
        let (dec, jumped_ev) = evaluate_smart_skip_with_last_jump(
            SmartSkipMode::InstantJump,
            false,
            1.0,
            10_000,
            &events,
            Some(NvrEventKind::Human),
            false,
            None,
        );
        assert_eq!(dec, SmartSkipDecision::JumpTo(77_000));
        assert_eq!(jumped_ev, Some(80_000));

        // [HIGH-15] Never jump to the exact same ev.start_ms (80_000) twice in a row
        let (dec_repeat, _) = evaluate_smart_skip_with_last_jump(
            SmartSkipMode::InstantJump,
            false,
            1.0,
            75_000,
            &events,
            Some(NvrEventKind::Human),
            false,
            Some(80_000),
        );
        assert_eq!(dec_repeat, SmartSkipDecision::None);

        // 5. InstantJump respects 1.5s cooldown so it never seek-loops
        assert_eq!(
            evaluate_smart_skip(
                SmartSkipMode::InstantJump,
                false,
                1.0,
                10_000,
                &events,
                Some(NvrEventKind::Human),
                true
            ),
            SmartSkipDecision::None
        );
    }

    #[test]
    fn test_resource_governor_clamps_cloud_p2p_vs_lan() {
        assert_eq!(
            NvrResourceGovernor::clamp_channels(true, &[1, 2, 3, 4]),
            vec![1]
        );
        assert_eq!(
            NvrResourceGovernor::clamp_channels(false, &[3, 3, 5, 7, 9, 11]),
            vec![3, 5, 7, 9]
        );

        let single_gov = NvrResourceGovernor::compute(false, 1);
        assert_eq!(
            (single_gov.decode_width, single_gov.decode_height),
            (1920, 1080)
        );
        let grid_gov = NvrResourceGovernor::compute(false, 4);
        assert_eq!(
            (grid_gov.decode_width, grid_gov.decode_height),
            (960, 540)
        );
    }

    #[test]
    fn test_decoded_rgba_ring_buffer_zero_ms_frame_stepping() {
        let slot = PlaybackSlot::new(1, 4, 4, 4, 1_000);
        for (idx, ep) in [1_000_i64, 1_040, 1_080, 1_120, 1_160].into_iter().enumerate() {
            let rgba = vec![idx as u8; 4 * 4 * 4];
            slot.publish_frame(4, 4, ep, &rgba, true);
        }

        // Capacity is 4, so frames in ring are [1040, 1080, 1120, 1160] with cursor at 1160
        let seq_slot1 = slot.frame_seq.load(Ordering::Acquire);
        assert_eq!(slot.current_epoch_ms.load(Ordering::Acquire), 1_160);
        assert_eq!(slot.try_step_in_ring(-1), Some(1_120));
        assert_eq!(slot.try_step_in_ring(-1), Some(1_080));
        assert_eq!(slot.try_step_in_ring(-1), Some(1_040));
        assert_eq!(slot.try_step_in_ring(-1), None); // Oldest frame boundary
        assert_eq!(slot.try_step_in_ring(1), Some(1_080));
        assert_eq!(slot.buffer.lock().unwrap().rgba[0], 2);

        // [HIGH-16] Unpause drain yields remaining frames [1120, 1160] without skipping
        let mut drain_buf = Vec::new();
        assert_eq!(
            slot.step_unpaused_ring_into(&mut drain_buf)
                .map(|(_, next_ep, _, _)| next_ep),
            Some(1_120)
        );
        assert_eq!(
            slot.step_unpaused_ring_into(&mut drain_buf)
                .map(|(_, next_ep, _, _)| next_ep),
            Some(1_160)
        );
        assert!(slot.step_unpaused_ring_into(&mut drain_buf).is_none());

        // [HIGH-16] Global NEXT_FRAME_SEQ never collides across newly recreated PlaybackSlots
        let slot2 = PlaybackSlot::new(1, 4, 4, 4, 2_000);
        slot2.publish_frame(4, 4, 2_000, &[9u8; 4 * 4 * 4], true);
        assert!(slot2.frame_seq.load(Ordering::Acquire) > seq_slot1);
    }

    #[test]
    fn test_reverse_ring_stepping_and_rewind_target_refill() {
        // 1. Reverse mode stores frames newest -> oldest: `,` (-1) must step backward in time!
        let rev_slot = PlaybackSlot::new(1, 2, 2, 4, 4_000);
        for ep in [4_000_i64, 3_000, 2_000, 1_000] {
            rev_slot.publish_frame(2, 2, ep, &[1u8; 16], true);
        }
        // Cursor is at index 3 (1_000ms): `.` (+1) steps forward in time -> 2_000 -> 3_000 -> 4_000
        assert_eq!(rev_slot.try_step_in_ring(1), Some(2_000));
        assert_eq!(rev_slot.try_step_in_ring(1), Some(3_000));
        // `,` (-1) steps backward in time -> 2_000 -> 1_000
        assert_eq!(rev_slot.try_step_in_ring(-1), Some(2_000));
        assert_eq!(rev_slot.try_step_in_ring(-1), Some(1_000));
        assert_eq!(rev_slot.try_step_in_ring(-1), None);

        // 2. Rewind refill `[9_400, 10_000)` pauses on exact `-1` frame `9_960` and keeps `10_000` at cursor + 1
        let rw_slot = PlaybackSlot::new(1, 2, 2, 16, 9_400);
        rw_slot.rewind_target_ms.store(10_000, Ordering::Release);
        for ep in (9_400..10_000).step_by(40) {
            assert!(rw_slot
                .handle_rewind_frame(2, 2, ep, &[(ep % 251) as u8; 16], 10_000)
                .is_none());
        }
        let landed = rw_slot.handle_rewind_frame(2, 2, 10_000, &[0xFF; 16], 10_000);
        assert_eq!(landed, Some(9_960));
        assert_eq!(rw_slot.rewind_target_ms.load(Ordering::Acquire), i64::MIN);
        assert_eq!(rw_slot.current_epoch_ms.load(Ordering::Acquire), 9_960);
        // Subsequent `,` steps immediately in 0ms from the refilled ring, and `.` hits 10_000!
        assert_eq!(rw_slot.try_step_in_ring(-1), Some(9_920));
        assert_eq!(rw_slot.try_step_in_ring(1), Some(9_960));
        assert_eq!(rw_slot.try_step_in_ring(1), Some(10_000));
    }

    #[test]
    fn test_cached_segment_export_header_bounded_preroll_and_boundary_dedup() {
        let mk = |pkt_type: u8, epoch_ms: i64, byte_offset: u64| DhavPacketEntry {
            pkt_type,
            is_keyframe: pkt_type == PKT_VIDEO_IDR,
            epoch_ms,
            byte_offset,
            frame_len: 64,
        };

        // Segment 0 (0..60_000): has 0xFE file header at idx 0, an old stale IDR at 5_000ms
        // (> 4s before start_ms = 50_000, so must NOT be pulled!), and target clip starting at 50_200ms.
        let seg0 = vec![
            mk(PKT_FILE_HEADER, 0, 0),
            mk(PKT_VIDEO_IDR, 5_000, 64),
            mk(PKT_VIDEO_P, 5_040, 128),
            mk(PKT_VIDEO_IDR, 50_200, 192),
            mk(0xF0, 50_200, 256), // Audio sharing exact epoch_ms with video packet
            mk(PKT_VIDEO_P, 59_960, 320),
            mk(0xF0, 59_960, 384),
        ];

        // Segment 1 (60_000..120_000): overlaps 58_500..59_960 at 60s boundary, then continues 60_000..62_000
        let seg1 = vec![
            mk(PKT_FILE_HEADER, 60_000, 0),
            mk(PKT_VIDEO_IDR, 58_500, 64),
            mk(PKT_VIDEO_P, 59_960, 128),
            mk(0xF0, 59_960, 192),
            mk(PKT_VIDEO_P, 60_000, 256),
            mk(0xF0, 60_000, 320),
            mk(PKT_VIDEO_IDR, 61_000, 384),
            mk(PKT_VIDEO_P, 65_000, 448), // Beyond end_ms (61_200) + 500 -> excluded
        ];

        let selected = select_cached_export_packets(&[seg0, seg1], 50_000, 61_200);
        let summary: Vec<(usize, u8, i64)> = selected
            .into_iter()
            .map(|(s, e)| (s, e.pkt_type, e.epoch_ms))
            .collect();

        assert_eq!(
            summary,
            vec![
                (0, PKT_FILE_HEADER, 0),    // snap[0] 0xFE preserved even though first_idx == 3!
                (0, PKT_VIDEO_IDR, 50_200), // Stale 5_000ms IDR skipped; picked first IDR >= 50_000
                (0, 0xF0, 50_200),          // Audio at same epoch_ms preserved
                (0, PKT_VIDEO_P, 59_960),
                (0, 0xF0, 59_960),
                (1, PKT_VIDEO_P, 60_000),   // 58_500..59_960 boundary overlap & duplicate 0xFE skipped!
                (1, 0xF0, 60_000),          // Audio at 60_000ms preserved!
                (1, PKT_VIDEO_IDR, 61_000),
            ]
        );
    }

    #[test]
    fn test_set_primary_channel_without_restarting_workers_and_cancel_clip_export() {
        let cb = crate::auth_gateway::AuthCircuitBreaker::new();
        cb.trip(); // Keep background playback workers parked without touching network
        let gw = AuthGateway::start(Arc::clone(&cb)).expect("start local auth gateway");
        let http = NvrHttpClient::new(cb);
        let mut cfg = NvrEnvConfig::default();
        cfg.use_cloud_p2p = false;

        let mut mgr = PlaybackManager::new(cfg, Arc::new(AtomicBool::new(false)), gw, http);
        mgr.open_sync_grid(vec![1, 2, 3, 4], 10_000);
        assert_eq!(mgr.primary_channel(), Some(1));
        assert_eq!(mgr.active_channels(), &[1, 2, 3, 4]);

        if let Some(s3) = mgr.slot(3) {
            s3.current_epoch_ms.store(12_480, Ordering::Release);
        }
        let gen_before = mgr
            .shared_ctrl
            .as_ref()
            .unwrap()
            .generation
            .load(Ordering::Acquire);

        // Bug 1.2 fix: set_primary_channel(3) MUST preserve 2x2 tile order [1, 2, 3, 4]!
        mgr.set_primary_channel(3);
        assert_eq!(mgr.primary_channel(), Some(3));
        assert_eq!(mgr.active_channels(), &[1, 2, 3, 4]);
        assert_eq!(
            mgr.shared_ctrl
                .as_ref()
                .unwrap()
                .primary_channel
                .load(Ordering::Acquire),
            3
        );
        assert_eq!(mgr.playhead_epoch_ms(), 12_480);
        assert_eq!(
            mgr.shared_ctrl
                .as_ref()
                .unwrap()
                .generation
                .load(Ordering::Acquire),
            gen_before
        );

        // swap_sync_channels swaps tile positions without restarting workers
        mgr.swap_sync_channels(2, 4);
        assert_eq!(mgr.active_channels(), &[1, 4, 3, 2]);
        assert_eq!(mgr.primary_channel(), Some(3));
        assert_eq!(
            mgr.shared_ctrl
                .as_ref()
                .unwrap()
                .generation
                .load(Ordering::Acquire),
            gen_before
        );

        // replace_sync_channel replaces in-place at exact index and preserves primary_ch/paused/speed
        mgr.set_speed(2.0);
        mgr.set_paused(true);
        mgr.replace_sync_channel(3, 7);
        assert_eq!(mgr.active_channels(), &[1, 4, 7, 2]);
        assert_eq!(mgr.primary_channel(), Some(7));
        assert!(mgr.is_paused());
        assert_eq!(mgr.speed(), 2.0);

        // Verify cancel_clip_export cleanly sets cancel_flag and transitions in-progress export state
        {
            let mut st = mgr.export_state.lock().unwrap();
            st.in_progress = true;
            st.label = "Front_Gate".to_string();
        }
        mgr.cancel_clip_export();
        assert!(mgr.export_cancel_flag.load(Ordering::Acquire));
        let finished = mgr.take_finished_export();
        assert!(matches!(finished, Some(Err(ref msg)) if msg.contains("cancelled")));

        mgr.close();
        assert_eq!(mgr.primary_channel(), None);
    }

    #[test]
    fn test_seek_drag_throttle_and_steady_hold_debounce() {
        let cb = crate::auth_gateway::AuthCircuitBreaker::new();
        cb.trip();
        let gw = AuthGateway::start(Arc::clone(&cb)).expect("start local auth gateway");
        let http = NvrHttpClient::new(cb);
        let mut mgr = PlaybackManager::new(
            NvrEnvConfig::default(),
            Arc::new(AtomicBool::new(false)),
            gw,
            http,
        );
        mgr.open_single(1, 10_000);

        // 1. First drag seek >= 250ms away dispatches immediately so scrubbing starts live preview
        mgr.seek(20_000, false);
        assert!(mgr.pending_seek.is_none());
        assert_eq!(
            mgr.shared_ctrl
                .as_ref()
                .unwrap()
                .target_epoch_ms
                .load(Ordering::Acquire),
            20_000
        );

        // 2. Subsequent drag within 200ms queues into pending_seek
        mgr.seek(25_000, false);
        let (queued_ms, first_queued_at) = mgr.pending_seek.expect("queued pending_seek");
        assert_eq!(queued_ms, 25_000);

        // 3. Holding cursor steady (repeated seek calls within <= 25ms jitter) MUST NOT reset queued_at!
        thread::sleep(Duration::from_millis(25));
        mgr.seek(25_000, false);
        mgr.seek(25_015, false);
        mgr.seek(24_985, false);
        let (still_ms, after_hold_at) = mgr.pending_seek.expect("still pending_seek");
        assert_eq!(still_ms, 25_000);
        assert_eq!(after_hold_at, first_queued_at);

        // 4. Slow continuous drag dispatches again once SEEK_DEBOUNCE_MS has elapsed
        mgr.last_drag_dispatch_at =
            Some(Instant::now() - Duration::from_millis(SEEK_DEBOUNCE_MS + 10));
        mgr.seek(30_000, false);
        assert!(mgr.pending_seek.is_none());
        assert_eq!(
            mgr.shared_ctrl
                .as_ref()
                .unwrap()
                .target_epoch_ms
                .load(Ordering::Acquire),
            30_000
        );

        mgr.close();
    }

    #[test]
    fn test_cached_ranges_for_channel_merges_adjacent_segments_and_multiday_timelines() {
        let cb = crate::auth_gateway::AuthCircuitBreaker::new();
        cb.trip();
        let gw = AuthGateway::start(Arc::clone(&cb)).expect("start local auth gateway");
        let http = NvrHttpClient::new(cb);
        let mgr = PlaybackManager::new(
            NvrEnvConfig::default(),
            Arc::new(AtomicBool::new(false)),
            gw,
            http,
        );

        {
            let mut cache = mgr.segment_cache.lock().unwrap();
            let payload = vec![0xAAu8; 512];

            // Contiguous 60s segments on channel 1: [60_000..120_000] + [120_000..180_000] -> [60_000, 180_000]
            for start_ms in [60_000_i64, 120_000, 240_000] {
                let key = SegmentKey {
                    channel: 1,
                    start_ms,
                };
                let (seg, _) = cache.acquire_or_create(key).unwrap();
                let n = seg.append_chunk_and_index(&payload, &[]).unwrap();
                cache.record_segment_growth(key, n);
                seg.mark_finished(None, false);
                cache.finish_inflight(key, true);
            }

            // Empty segment (0 bytes) at 180_000 and other channel (ch 2) at 60_000 must be ignored
            let empty_key = SegmentKey {
                channel: 1,
                start_ms: 180_000,
            };
            let _ = cache.acquire_or_create(empty_key).unwrap();

            let ch2_key = SegmentKey {
                channel: 2,
                start_ms: 180_000,
            };
            let (seg2, _) = cache.acquire_or_create(ch2_key).unwrap();
            let n2 = seg2.append_chunk_and_index(&payload, &[]).unwrap();
            cache.record_segment_growth(ch2_key, n2);
            seg2.mark_finished(None, false);
            cache.finish_inflight(ch2_key, true);
        }

        assert_eq!(
            mgr.cached_ranges_for_channel(1, 0, 350_000),
            vec![(60_000, 180_000), (240_000, 300_000)]
        );
        // Window clipping query [150_000, 250_000] overlaps [120_000..180_000] (merged with 60_000? only overlapping segments!)
        assert_eq!(
            mgr.cached_ranges_for_channel(1, 150_000, 250_000),
            vec![(120_000, 180_000), (240_000, 300_000)]
        );

        // Multi-day timelines sorted by date
        let d1 = NaiveDate::from_ymd_opt(2026, 11, 17).unwrap();
        let d2 = NaiveDate::from_ymd_opt(2026, 11, 18).unwrap();
        {
            let mut tl = mgr.timeline_cache.lock().unwrap();
            for d in [d2, d1] {
                tl.insert(
                    (1, d),
                    TimelineCacheSlot {
                        index: Some(DayTimelineIndex {
                            channel: 1,
                            date: d,
                            coverage: Vec::new(),
                            events: Vec::new(),
                            api_supported: true,
                            note: None,
                        }),
                        loading: false,
                        fetched_at: Some(Instant::now()),
                    },
                );
            }
        }
        let s_ms = local_naive_to_epoch_ms(d1.and_hms_opt(22, 0, 0).unwrap());
        let e_ms = local_naive_to_epoch_ms(d2.and_hms_opt(2, 0, 0).unwrap());
        let days = mgr.day_timelines_for_range(1, s_ms, e_ms);
        assert_eq!(days.len(), 2);
        assert_eq!(days[0].date, d1);
        assert_eq!(days[1].date, d2);
    }
}
