use crate::auth_gateway::{AuthCircuitBreaker, AuthGateway};
use crate::config::{
    create_private_file_exclusive, ensure_private_dir, expand_tilde,
    reject_any_symlink_component, resolve_trusted_binary, save_env_config, AppConfig,
    NvrEnvConfig,
};
use crate::nvr_api::{epoch_ms_to_local_naive, local_naive_to_epoch_ms, NvrHttpClient};
use crate::p2p::{is_lan_reachable, P2PTunnelManager};
#[allow(unused_imports)]
use crate::playback::{
    test_nvr_connection_sync, PlaybackManager, PlaybackStatus, SmartSkipMode, MAX_CLIP_EXPORT_MS,
};
use crate::stream::{
    now_epoch_ms, scan_active_nvr_channels_cancellable, ChannelScanReport, StreamManager,
    StreamStatus,
};
#[allow(unused_imports)]
use crate::timeline_ui::{
    compute_live_pip_rect, hairline_stroke, render_event_drawer, render_live_pip_overlay,
    render_timeline_dock, LivePipAction, LivePipCorner, TimelineAction, TimelineUiState,
    EVENT_DRAWER_WIDTH, GRAPHITE_SURFACE, LIVE_PIP_SIZE, OBSIDIAN_BG, STATUS_AMBER, STATUS_CORAL,
    STATUS_EMERALD, STATUS_ICE, STATUS_VIOLET, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY,
    TIMELINE_DOCK_HEIGHT, ZINC_DIM, ZINC_ELEVATED, ZINC_HOVER,
};
use crate::wayland_pinch::{PinchFrameSnapshot, WaylandPinchListener};
use chrono::{Datelike, Local, NaiveTime};
use eframe::egui::{
    self, Align2, Color32, ColorImage, CursorIcon, FontFamily, FontId, Key, PointerButton, Pos2,
    Rect, RichText, Rounding, Sense, Stroke, TextureHandle, TextureOptions, Ui, Vec2,
};
use image::{ImageBuffer, ImageFormat, Rgba, RgbaImage};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use zeroize::Zeroize;

const BUY_ME_A_COFFEE_URL: &str = "https://buymeacoffee.com/pulkitmundra";
const GITHUB_SPONSORS_URL: &str = "https://github.com/sponsors/pulkitmundra";
const GITHUB_REPO_URL: &str = "https://github.com/pulkitmundra/nvr-command-center";
const PORTFOLIO_URL: &str = "https://pulkit-mundra-portfolio.vercel.app/projects/nvr-command-center";

const CHORD_WINDOW: Duration = Duration::from_millis(450);
const OS_FULLSCREEN_BAR_HIDE: Duration = Duration::from_millis(2800);
const MAX_VISIBLE_TOASTS: usize = 3;
const MAX_CONCURRENT_SNAPSHOTS: usize = 3;
const SHUTTER_FLASH_DURATION: Duration = Duration::from_millis(140);

#[inline]
fn status_emerald_fill() -> Color32 {
    Color32::from_rgba_unmultiplied(16, 185, 129, 38)
}

#[inline]
fn status_ice_fill() -> Color32 {
    Color32::from_rgba_unmultiplied(56, 189, 248, 38)
}

#[inline]
fn status_amber_fill() -> Color32 {
    Color32::from_rgba_unmultiplied(245, 158, 11, 38)
}

#[inline]
fn status_coral_fill() -> Color32 {
    Color32::from_rgba_unmultiplied(239, 68, 68, 34)
}

#[inline]
fn safe_clamp_f32(val: f32, min: f32, max: f32) -> f32 {
    if !max.is_finite() {
        return min.max(0.0);
    }
    let hi = max.max(0.0);
    let lo = min.min(hi);
    val.clamp(lo, hi)
}

fn compute_video_rect(tile_rect: Rect, aspect_fit_16_9: bool, tex_w: u32, tex_h: u32) -> Rect {
    if !aspect_fit_16_9 {
        return tile_rect;
    }
    let avail_w = tile_rect.width().max(1.0);
    let avail_h = tile_rect.height().max(1.0);
    let aspect = if tex_w > 0 && tex_h > 0 {
        tex_w as f32 / tex_h as f32
    } else {
        16.0 / 9.0
    };
    let avail_aspect = avail_w / avail_h;
    let (fit_w, fit_h) = if avail_aspect > aspect {
        (avail_h * aspect, avail_h)
    } else {
        (avail_w, avail_w / aspect)
    };
    Rect::from_center_size(tile_rect.center(), Vec2::new(fit_w, fit_h))
}

/// Applies an aspect-preserving center-crop ("cover") UV adjustment when `Fill` mode is active,
/// ensuring objects are never stretched horizontally or squashed vertically across non-16:9 tiles.
fn apply_cover_crop_uv(uv_rect: Rect, target_aspect: f32, source_aspect: f32) -> Rect {
    let src_asp = if source_aspect.is_finite() && source_aspect > 0.05 {
        source_aspect
    } else {
        16.0 / 9.0
    };
    if !target_aspect.is_finite() || target_aspect <= 0.05 {
        return uv_rect;
    }
    let uv_w = uv_rect.width().max(1e-4);
    let uv_h = uv_rect.height().max(1e-4);
    let cur_phys_aspect = src_asp * (uv_w / uv_h);
    if (target_aspect - cur_phys_aspect).abs() <= 1e-4 {
        return uv_rect;
    }
    if target_aspect > cur_phys_aspect {
        // Tile is wider than video: preserve full UV width, center-crop UV height
        let keep_h = (uv_h * (cur_phys_aspect / target_aspect)).clamp(1e-4, uv_h);
        Rect::from_center_size(uv_rect.center(), Vec2::new(uv_w, keep_h))
    } else {
        // Tile is taller/narrower than video: preserve full UV height, center-crop UV width
        let keep_w = (uv_w * (target_aspect / cur_phys_aspect)).clamp(1e-4, uv_w);
        Rect::from_center_size(uv_rect.center(), Vec2::new(keep_w, uv_h))
    }
}

fn advance_chord(state: &mut Option<(Key, u8, Instant)>, key: Key, now: Instant) -> u8 {
    let count = match *state {
        Some((prev_key, prev_count, last_ts))
            if prev_key == key && now.saturating_duration_since(last_ts) <= CHORD_WINDOW =>
        {
            if prev_count >= 3 {
                1
            } else {
                prev_count + 1
            }
        }
        _ => 1,
    };
    *state = Some((key, count, now));
    count
}

fn move_grid_selection(
    order: &[usize],
    cols: usize,
    current: Option<usize>,
    dr: isize,
    dc: isize,
) -> Option<usize> {
    let len = order.len();
    if len == 0 {
        return None;
    }
    let cols = cols.max(1);
    let rows = len.div_ceil(cols).max(1);
    let idx = current
        .and_then(|ch| order.iter().position(|&c| c == ch))
        .unwrap_or(0);
    let r = idx / cols;
    let c = idx % cols;

    if dc != 0 {
        let row_start = r * cols;
        let row_len = (len - row_start).min(cols).max(1);
        let new_c = (c as isize + dc).rem_euclid(row_len as isize) as usize;
        return order.get(row_start + new_c).copied();
    }

    if dr != 0 {
        let new_r = (r as isize + dr).rem_euclid(rows as isize) as usize;
        let target_idx = (new_r * cols + c).min(len - 1);
        return order.get(target_idx).copied();
    }

    order.get(idx).copied()
}

fn resolve_live_target_channel(
    fullscreen_channel: Option<usize>,
    selected_channel: Option<usize>,
    hovered_channel: Option<usize>,
    use_hover_target: bool,
    channel_order: &[usize],
) -> Option<usize> {
    if let Some(fs) = fullscreen_channel {
        return Some(fs);
    }
    if use_hover_target {
        hovered_channel
            .or(selected_channel)
            .or_else(|| channel_order.first().copied())
    } else {
        selected_channel
            .or(hovered_channel)
            .or_else(|| channel_order.first().copied())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToastKind {
    Info,
    Success,
    Warning,
    Error,
}

impl ToastKind {
    fn accent_color(self) -> Color32 {
        match self {
            Self::Info => STATUS_ICE,
            Self::Success => STATUS_EMERALD,
            Self::Warning => STATUS_AMBER,
            Self::Error => STATUS_CORAL,
        }
    }

    fn default_duration(self, has_action: bool) -> Duration {
        if has_action || matches!(self, Self::Warning | Self::Error) {
            Duration::from_millis(5500)
        } else {
            Duration::from_millis(3200)
        }
    }
}

#[derive(Debug, Clone)]
struct ToastItem {
    id: u64,
    kind: ToastKind,
    message: String,
    remaining: Duration,
    last_tick: Instant,
    open_dir: Option<PathBuf>,
}

fn push_toast_to_queue(
    queue: &mut VecDeque<ToastItem>,
    next_id: &mut u64,
    kind: ToastKind,
    message: String,
    open_dir: Option<PathBuf>,
    now: Instant,
) {
    let dur = kind.default_duration(open_dir.is_some());
    if let Some(back) = queue.back_mut() {
        if back.message == message && back.open_dir == open_dir {
            back.kind = kind;
            back.remaining = dur;
            back.last_tick = now;
            return;
        }
    }
    *next_id = next_id.wrapping_add(1);
    queue.push_back(ToastItem {
        id: *next_id,
        kind,
        message,
        remaining: dur,
        last_tick: now,
        open_dir,
    });
    while queue.len() > MAX_VISIBLE_TOASTS {
        queue.pop_front();
    }
}

#[derive(Clone, Copy)]
enum PillAccent {
    Ice,
    Emerald,
    Amber,
}

impl PillAccent {
    fn colors(self) -> (Color32, Color32, Color32) {
        match self {
            Self::Ice => (status_ice_fill(), STATUS_ICE, TEXT_PRIMARY),
            Self::Emerald => (status_emerald_fill(), STATUS_EMERALD, TEXT_PRIMARY),
            Self::Amber => (
                status_amber_fill(),
                STATUS_AMBER,
                Color32::from_rgb(254, 243, 199),
            ),
        }
    }
}

const MAX_DIGITAL_ZOOM: f32 = 8.0;
const MIN_MARQUEE_PX: f32 = 18.0;

#[derive(Debug, Clone)]
struct ZoomViewportState {
    zoom: f32,
    pan_uv: Vec2,
    anim_target: Option<(f32, Vec2)>,
    marquee_drag: Option<(Pos2, Pos2)>,
    box_zoom_armed: bool,
    zoom_history: Vec<(f32, Vec2)>,
}

impl Default for ZoomViewportState {
    fn default() -> Self {
        Self::new()
    }
}

impl ZoomViewportState {
    fn new() -> Self {
        Self {
            zoom: 1.0,
            pan_uv: Vec2::ZERO,
            anim_target: None,
            marquee_drag: None,
            box_zoom_armed: false,
            zoom_history: Vec::new(),
        }
    }

    fn is_zoomed(&self) -> bool {
        self.zoom > 1.02 || self.anim_target.is_some_and(|(tz, _)| tz > 1.02)
    }

    fn clear_transient_drag(&mut self) {
        self.marquee_drag = None;
        self.box_zoom_armed = false;
    }

    fn reset_zoom(&mut self) {
        self.zoom = 1.0;
        self.pan_uv = Vec2::ZERO;
        self.anim_target = None;
        self.marquee_drag = None;
        self.box_zoom_armed = false;
        self.zoom_history.clear();
    }

    fn animate_reset_zoom(&mut self) {
        self.marquee_drag = None;
        self.box_zoom_armed = false;
        self.zoom_history.clear();
        if self.zoom <= 1.01 {
            self.reset_zoom();
        } else {
            self.anim_target = Some((1.0, Vec2::ZERO));
        }
    }

    fn push_zoom_history(&mut self) {
        if self.zoom_history.len() >= 6 {
            self.zoom_history.remove(0);
        }
        self.zoom_history.push((self.zoom, self.pan_uv));
    }

    fn step_back_zoom(&mut self) -> bool {
        self.marquee_drag = None;
        self.box_zoom_armed = false;
        if let Some((prev_z, prev_pan)) = self.zoom_history.pop() {
            if prev_z <= 1.01 {
                self.animate_reset_zoom();
            } else {
                self.anim_target = Some((prev_z, prev_pan));
            }
            true
        } else if self.zoom > 1.01 {
            self.animate_reset_zoom();
            true
        } else {
            false
        }
    }

    fn step_animation(&mut self, dt: f32) -> bool {
        let Some((target_z, target_pan)) = self.anim_target else {
            return false;
        };
        let clamped_dt = dt.clamp(0.001, 0.08);
        let alpha = (1.0 - (-22.0 * clamped_dt).exp()).clamp(0.12, 1.0);
        self.zoom += (target_z - self.zoom) * alpha;
        self.pan_uv += (target_pan - self.pan_uv) * alpha;
        self.clamp_pan();

        if (self.zoom - target_z).abs() < 0.008 && (self.pan_uv - target_pan).length() < 0.0015 {
            if target_z <= 1.005 {
                self.zoom = 1.0;
                self.pan_uv = Vec2::ZERO;
            } else {
                self.zoom = target_z;
                self.pan_uv = target_pan;
                self.clamp_pan();
            }
            self.anim_target = None;
            false
        } else {
            true
        }
    }

    #[inline]
    fn max_pan_for_zoom(z: f32) -> f32 {
        let clamped_z = z.clamp(1.0, MAX_DIGITAL_ZOOM);
        (0.5 - 0.5 / clamped_z).max(0.0)
    }

    fn clamp_pan(&mut self) {
        let max_pan = Self::max_pan_for_zoom(self.zoom);
        self.pan_uv.x = self.pan_uv.x.clamp(-max_pan, max_pan);
        self.pan_uv.y = self.pan_uv.y.clamp(-max_pan, max_pan);
    }

    fn pan_by_pixels(&mut self, delta_px: Vec2, video_size: Vec2) {
        self.anim_target = None;
        if self.zoom <= 1.01 {
            self.pan_uv = Vec2::ZERO;
            return;
        }
        let z = self.zoom.clamp(1.0, MAX_DIGITAL_ZOOM);
        self.pan_uv.x -= delta_px.x / (video_size.x.max(1.0) * z);
        self.pan_uv.y -= delta_px.y / (video_size.y.max(1.0) * z);
        self.clamp_pan();
    }

    fn jump_to_radar_uv(&mut self, norm_uv: Pos2) {
        if self.zoom <= 1.01 {
            return;
        }
        self.anim_target = None;
        self.pan_uv = Vec2::new(
            norm_uv.x.clamp(0.0, 1.0) - 0.5,
            norm_uv.y.clamp(0.0, 1.0) - 0.5,
        );
        self.clamp_pan();
    }

    fn preview_marquee_zoom(&self, sel_rect: Rect, video_rect: Rect) -> (f32, Vec2) {
        let vw = video_rect.width().max(1.0);
        let vh = video_rect.height().max(1.0);
        let nx0 = ((sel_rect.min.x - video_rect.min.x) / vw).clamp(0.0, 1.0);
        let ny0 = ((sel_rect.min.y - video_rect.min.y) / vh).clamp(0.0, 1.0);
        let nx1 = ((sel_rect.max.x - video_rect.min.x) / vw).clamp(0.0, 1.0);
        let ny1 = ((sel_rect.max.y - video_rect.min.y) / vh).clamp(0.0, 1.0);

        let cur_uv = self.uv_rect();
        let u0 = cur_uv.min.x + nx0 * cur_uv.width();
        let u1 = cur_uv.min.x + nx1 * cur_uv.width();
        let v0 = cur_uv.min.y + ny0 * cur_uv.height();
        let v1 = cur_uv.min.y + ny1 * cur_uv.height();

        let u_center = 0.5 * (u0 + u1);
        let v_center = 0.5 * (v0 + v1);
        let span = (u1 - u0).abs().max((v1 - v0).abs()).max(0.01);
        let target_z = (1.0 / span).clamp(1.0, MAX_DIGITAL_ZOOM);
        let max_pan = Self::max_pan_for_zoom(target_z);
        let target_pan = Vec2::new(
            (u_center - 0.5).clamp(-max_pan, max_pan),
            (v_center - 0.5).clamp(-max_pan, max_pan),
        );
        (target_z, target_pan)
    }

    fn zoom_to_marquee_rect(&mut self, sel_rect: Rect, video_rect: Rect) -> Option<f32> {
        let clipped = sel_rect.intersect(video_rect);
        if clipped.width() < MIN_MARQUEE_PX || clipped.height() < MIN_MARQUEE_PX {
            return None;
        }
        let (target_z, target_pan) = self.preview_marquee_zoom(clipped, video_rect);
        if target_z <= 1.02 {
            return None;
        }
        self.push_zoom_history();
        self.anim_target = Some((target_z, target_pan));
        Some(target_z)
    }

    fn zoom_by(&mut self, factor: f32, anchor_norm: Pos2) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        self.anim_target = None;
        let ax = anchor_norm.x.clamp(0.0, 1.0);
        let ay = anchor_norm.y.clamp(0.0, 1.0);

        let cur_uv = self.uv_rect();
        let u_anchor = cur_uv.min.x + ax * cur_uv.width();
        let v_anchor = cur_uv.min.y + ay * cur_uv.height();

        let new_z = (self.zoom * factor).clamp(1.0, MAX_DIGITAL_ZOOM);
        if new_z <= 1.0005 || (factor < 1.0 && new_z <= 1.008) {
            self.reset_zoom();
            return;
        }

        let new_span = 1.0 / new_z;
        let max_min = (1.0 - new_span).max(0.0);
        let new_min_x = (u_anchor - ax * new_span).clamp(0.0, max_min);
        let new_min_y = (v_anchor - ay * new_span).clamp(0.0, max_min);

        self.zoom = new_z;
        self.pan_uv = Vec2::new(
            new_min_x + 0.5 * new_span - 0.5,
            new_min_y + 0.5 * new_span - 0.5,
        );
        self.clamp_pan();
    }

    fn uv_rect(&self) -> Rect {
        let z = self.zoom.clamp(1.0, MAX_DIGITAL_ZOOM);
        if z <= 1.001 {
            return Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
        }
        let span = 1.0 / z;
        let max_offset = 1.0 - span;
        let cx = (0.5 + self.pan_uv.x).clamp(span * 0.5, 1.0 - span * 0.5);
        let cy = (0.5 + self.pan_uv.y).clamp(span * 0.5, 1.0 - span * 0.5);
        let min_x = (cx - span * 0.5).clamp(0.0, max_offset);
        let min_y = (cy - span * 0.5).clamp(0.0, max_offset);
        Rect::from_min_max(
            Pos2::new(min_x, min_y),
            Pos2::new(min_x + span, min_y + span),
        )
    }
}

struct TileUiState {
    texture: Option<TextureHandle>,
    last_uploaded_seq: u64,
    tex_width: u32,
    tex_height: u32,
    pb_texture: Option<TextureHandle>,
    pb_last_uploaded_seq: u64,
    pb_tex_width: u32,
    pb_tex_height: u32,
    live: ZoomViewportState,
    pb: ZoomViewportState,
}

impl Default for TileUiState {
    fn default() -> Self {
        Self::new()
    }
}

impl TileUiState {
    fn new() -> Self {
        Self {
            texture: None,
            last_uploaded_seq: 0,
            tex_width: 0,
            tex_height: 0,
            pb_texture: None,
            pb_last_uploaded_seq: 0,
            pb_tex_width: 0,
            pb_tex_height: 0,
            live: ZoomViewportState::new(),
            pb: ZoomViewportState::new(),
        }
    }

    #[inline]
    fn viewport(&self, is_playback: bool) -> &ZoomViewportState {
        if is_playback {
            &self.pb
        } else {
            &self.live
        }
    }

    #[inline]
    fn viewport_mut(&mut self, is_playback: bool) -> &mut ZoomViewportState {
        if is_playback {
            &mut self.pb
        } else {
            &mut self.live
        }
    }

    #[inline]
    fn has_texture(&self, is_playback: bool) -> bool {
        if is_playback {
            self.pb_texture.is_some()
        } else {
            self.texture.is_some()
        }
    }
}

/// Computes the exact multiplicative zoom factor for a hovered tile on this frame.
///
/// Combines:
/// 1. Native Wayland `zwp_pointer_gesture_pinch_v1` two-finger touchpad pinch (`pinch.scale_delta`).
/// 2. Touchscreen / macOS `Event::Zoom` pinch deltas.
/// 3. Direct, un-lagged `Event::MouseWheel` deltas (bypassing `egui`'s exponential
///    `unprocessed_scroll_delta` buffer so reversing two-finger scroll from zoom-in to
///    zoom-out responds instantaneously with 1:1 symmetry).
fn compute_tile_zoom_factor(events: &[egui::Event], touch_zoom: Option<f32>, pinch: PinchFrameSnapshot) -> f32 {
    let mut factor = if pinch.scale_delta.is_finite() && pinch.scale_delta > 0.01 {
        pinch.scale_delta
    } else {
        1.0
    };

    if let Some(tz) = touch_zoom {
        if tz.is_finite() && tz > 0.01 {
            factor *= tz;
        }
    }

    for ev in events {
        match ev {
            egui::Event::Zoom(z) if z.is_finite() && *z > 0.01 => {
                factor *= *z;
            }
            egui::Event::MouseWheel { unit, delta, .. } if !pinch.suppress_axis_scroll => {
                match unit {
                    egui::MouseWheelUnit::Point => {
                        if delta.y.abs() > 0.01 {
                            let exp = (delta.y * 0.0095).clamp(-1.8, 1.8);
                            factor *= exp.exp();
                        }
                    }
                    egui::MouseWheelUnit::Line => {
                        if delta.y.abs() > 0.01 {
                            factor *= 1.22_f32.powf(delta.y.clamp(-4.0, 4.0));
                        }
                    }
                    egui::MouseWheelUnit::Page => {
                        if delta.y.abs() > 0.01 {
                            factor *= 1.5_f32.powf(delta.y.clamp(-2.0, 2.0));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    factor.clamp(0.1, 10.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    NvrAndCameras,
    PerformanceAndNames,
    ShortcutsAndSupport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NetworkTargetMode {
    Lan,
    Tailscale,
    CloudP2P,
}

struct SnapshotPermit(Arc<AtomicUsize>);

impl Drop for SnapshotPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct TempP2PGuard(P2PTunnelManager);

impl Drop for TempP2PGuard {
    fn drop(&mut self) {
        self.0.stop();
    }
}

fn prepare_probe_env_with_optional_p2p(
    mut cfg: NvrEnvConfig,
    live_ports: Option<(u16, u16)>,
    cancel_flag: Option<&AtomicBool>,
) -> Result<(NvrEnvConfig, Option<TempP2PGuard>), String> {
    let is_cancelled = || cancel_flag.is_some_and(|f| f.load(Ordering::Acquire));
    if is_cancelled() {
        return Err("Cancelled".to_string());
    }
    cfg.validate_for_save()?;
    if !cfg.is_cloud_p2p_active() {
        return Ok((cfg, None));
    }
    if let Some((r_port, h_port)) = live_ports {
        cfg.p2p_local_port = r_port;
        cfg.p2p_local_http_port = h_port;
        return Ok((cfg, None));
    }

    let mut mgr = P2PTunnelManager::new();
    let (r_port, h_port) = mgr.ensure_started(
        &cfg.p2p_serial,
        &cfg.username,
        &cfg.password,
        cfg.port,
        cfg.http_port,
    );
    let guard = TempP2PGuard(mgr);
    if r_port == 0 || h_port == 0 {
        return Err(format!(
            "Cloud P2P tunnel failed to start: {}",
            guard.0.status_text()
        ));
    }
    let connected_ref = guard.0.is_connected_handle();
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        if is_cancelled() {
            return Err("Cancelled".to_string());
        }
        if connected_ref.load(Ordering::Acquire) {
            break;
        }
        if guard.0.is_halted() {
            return Err(format!("Cloud P2P rejected: {}", guard.0.status_text()));
        }
        thread::sleep(Duration::from_millis(50));
    }
    if is_cancelled() {
        return Err("Cancelled".to_string());
    }
    if !connected_ref.load(Ordering::Acquire) {
        return Err(format!(
            "Cloud P2P connection timed out ({})",
            guard.0.status_text()
        ));
    }
    cfg.p2p_local_port = r_port;
    cfg.p2p_local_http_port = h_port;
    Ok((cfg, Some(guard)))
}

fn normalized_camera_names(cfg: &AppConfig) -> BTreeMap<usize, String> {
    cfg.camera_names
        .iter()
        .filter_map(|(&ch, v)| {
            let trimmed = v.trim();
            if trimmed.is_empty() || trimmed == format!("Camera {}", ch) {
                None
            } else {
                Some((ch, trimmed.to_string()))
            }
        })
        .collect()
}

fn app_configs_equal_editable(a: &AppConfig, b: &AppConfig) -> bool {
    a.screenshot_dir == b.screenshot_dir
        && a.recording_dir == b.recording_dir
        && a.fullscreen_auto_hd == b.fullscreen_auto_hd
        && a.aspect_fit_16_9 == b.aspect_fit_16_9
        && a.patrol_interval_secs == b.patrol_interval_secs
        && normalized_camera_names(a) == normalized_camera_names(b)
}

type SnapshotResultQueue = Arc<Mutex<Vec<Result<(String, PathBuf), String>>>>;
type TestConnState = Arc<Mutex<Option<(u64, Option<Result<String, String>>)>>>;
type ScanInProgressState = Arc<Mutex<Option<(u64, Option<ChannelScanReport>)>>>;

pub struct CameraViewerApp {
    env_path: PathBuf,
    env_cfg: NvrEnvConfig,
    edit_env_cfg: NvrEnvConfig,
    app_cfg: AppConfig,
    edit_app_cfg: AppConfig,
    auth_gw: Arc<AuthGateway>,
    p2p_mgr: P2PTunnelManager,
    stream_mgr: StreamManager,
    playback_mgr: PlaybackManager,
    timeline_ui: TimelineUiState,
    p2p_playback_paused_live: bool,
    playback_entered_at: Option<Instant>,
    last_playback_exit_ms: Option<(i64, Instant)>,
    was_minimized: bool,
    minimized_paused_playback: bool,
    overlay_gesture_block: bool,
    last_tile_hud_click_at: Option<Instant>,
    floating_top_bar_visible: bool,
    floating_dock_visible: bool,
    tiles: BTreeMap<usize, TileUiState>,
    upload_scratch: Vec<u8>,
    wayland_pinch: Option<WaylandPinchListener>,
    frame_pinch: PinchFrameSnapshot,

    fullscreen_channel: Option<usize>,
    selected_channel: Option<usize>,
    hovered_channel: Option<usize>,
    use_hover_target: bool,
    all_paused: bool,
    patrol_active: bool,
    last_patrol_switch: Instant,

    zen_mode: bool,
    os_fullscreen: bool,
    top_bar_peek: bool,
    bottom_dock_peek: bool,
    last_pointer_activity: Instant,
    last_pointer_pos: Option<Pos2>,
    last_center_width: f32,

    show_settings: bool,
    settings_tab: SettingsTab,
    show_password: bool,
    show_shortcuts_modal: bool,
    chord_state: Option<(Key, u8, Instant)>,

    rename_modal_channel: Option<usize>,
    rename_input: String,
    rename_just_opened: bool,

    toasts: VecDeque<ToastItem>,
    next_toast_id: u64,
    shutter_flash_until: BTreeMap<usize, Instant>,
    in_flight_snapshots: Arc<AtomicUsize>,
    pending_snapshot_results: SnapshotResultQueue,
    probe_cancel: Arc<AtomicBool>,
    probe_gen: u64,
    scan_in_progress: ScanInProgressState,
    test_conn_state: TestConnState,
    last_test_conn_banner: Option<Result<String, String>>,

    last_window_title: String,
    rss_mb: f32,
    last_sys_poll: Instant,
}

impl CameraViewerApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        env_path: PathBuf,
        mut env_cfg: NvrEnvConfig,
    ) -> Self {
        configure_dark_theme(&cc.egui_ctx);
        let wayland_pinch = WaylandPinchListener::try_new(cc);

        let circuit_breaker = AuthCircuitBreaker::new();
        let auth_gw =
            AuthGateway::start(Arc::clone(&circuit_breaker)).expect("Failed to start AuthGateway");
        let nvr_http = NvrHttpClient::new(circuit_breaker);

        let mut p2p_mgr = P2PTunnelManager::new();
        let mut toasts = VecDeque::new();
        let mut next_toast_id = 0u64;
        let is_unconfigured = env_cfg.password.trim().is_empty();

        // Auto-detect whether local LAN NVR is reachable or away on another internet connection
        if !is_unconfigured && !env_cfg.p2p_serial.trim().is_empty() && !env_cfg.use_tailscale {
            if is_lan_reachable(&env_cfg.host, env_cfg.port, 500) {
                if env_cfg.use_cloud_p2p {
                    env_cfg.use_cloud_p2p = false;
                    push_toast_to_queue(
                        &mut toasts,
                        &mut next_toast_id,
                        ToastKind::Info,
                        "Local LAN NVR detected -> Switched to Direct LAN".to_string(),
                        None,
                        Instant::now(),
                    );
                }
            } else if !env_cfg.use_cloud_p2p {
                env_cfg.use_cloud_p2p = true;
                push_toast_to_queue(
                    &mut toasts,
                    &mut next_toast_id,
                    ToastKind::Warning,
                    "Local LAN NVR unreachable -> Auto-switched to Cloud P2P Tunnel".to_string(),
                    None,
                    Instant::now(),
                );
            }
        }

        if !is_unconfigured && env_cfg.is_cloud_p2p_active() {
            let (rtsp_port, http_port) = p2p_mgr.ensure_started(
                &env_cfg.p2p_serial,
                &env_cfg.username,
                &env_cfg.password,
                env_cfg.port,
                env_cfg.http_port,
            );
            env_cfg.p2p_local_port = rtsp_port;
            env_cfg.p2p_local_http_port = http_port;
        }

        let app_cfg = AppConfig::load_and_sync(&env_cfg);
        let edit_app_cfg = app_cfg.clone();
        let initial_selected = app_cfg.channel_order.first().copied();
        let p2p_connected = p2p_mgr.is_connected_handle();
        let mut stream_mgr = StreamManager::new(
            env_cfg.clone(),
            Arc::clone(&p2p_connected),
            Arc::clone(&auth_gw),
        );
        if !is_unconfigured {
            stream_mgr.start_streams(app_cfg.num_cameras);
        }

        let playback_mgr = PlaybackManager::new(
            env_cfg.clone(),
            p2p_connected,
            Arc::clone(&auth_gw),
            nvr_http,
        );
        let timeline_ui = TimelineUiState::default();

        let mut tiles = BTreeMap::new();
        for ch in 1..=app_cfg.num_cameras {
            tiles.insert(ch, TileUiState::new());
        }

        Self {
            env_path,
            edit_env_cfg: env_cfg.clone(),
            env_cfg,
            app_cfg,
            edit_app_cfg,
            auth_gw,
            p2p_mgr,
            stream_mgr,
            playback_mgr,
            timeline_ui,
            p2p_playback_paused_live: false,
            playback_entered_at: None,
            last_playback_exit_ms: None,
            was_minimized: false,
            minimized_paused_playback: false,
            overlay_gesture_block: false,
            last_tile_hud_click_at: None,
            floating_top_bar_visible: false,
            floating_dock_visible: false,
            tiles,
            upload_scratch: Vec::new(),
            wayland_pinch,
            frame_pinch: PinchFrameSnapshot::default(),
            fullscreen_channel: None,
            selected_channel: initial_selected,
            hovered_channel: None,
            use_hover_target: true,
            all_paused: false,
            patrol_active: false,
            last_patrol_switch: Instant::now(),
            zen_mode: false,
            os_fullscreen: false,
            top_bar_peek: false,
            bottom_dock_peek: false,
            last_pointer_activity: Instant::now(),
            last_pointer_pos: None,
            last_center_width: 380.0,
            show_settings: false,
            settings_tab: SettingsTab::NvrAndCameras,
            show_password: false,
            show_shortcuts_modal: false,
            chord_state: None,
            rename_modal_channel: None,
            rename_input: String::new(),
            rename_just_opened: false,
            toasts,
            next_toast_id,
            shutter_flash_until: BTreeMap::new(),
            in_flight_snapshots: Arc::new(AtomicUsize::new(0)),
            pending_snapshot_results: Arc::new(Mutex::new(Vec::new())),
            probe_cancel: Arc::new(AtomicBool::new(false)),
            probe_gen: 0,
            scan_in_progress: Arc::new(Mutex::new(None)),
            test_conn_state: Arc::new(Mutex::new(None)),
            last_test_conn_banner: None,
            last_window_title: String::new(),
            rss_mb: read_process_rss_mb(),
            last_sys_poll: Instant::now(),
        }
    }

    fn format_window_title(&self) -> String {
        let conn_desc = if self.env_cfg.is_cloud_p2p_active() {
            "Cloud P2P".to_string()
        } else if self.env_cfg.is_tailscale_active() {
            format!("Tailscale {}", self.env_cfg.active_host())
        } else {
            self.env_cfg.active_host().to_string()
        };
        format!(
            "NVR Command Center v{} - {} Cameras ({})",
            env!("CARGO_PKG_VERSION"),
            self.app_cfg.num_cameras,
            conn_desc
        )
    }

    fn sync_window_title(&mut self, ctx: &egui::Context) {
        let title = self.format_window_title();
        if self.last_window_title != title {
            self.last_window_title = title.clone();
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
        }
    }

    fn active_target_channel(&self) -> Option<usize> {
        if self.playback_mgr.is_active() {
            if self.playback_mgr.is_sync_2x2() {
                let active = self.playback_mgr.active_channels();
                let sel = self.selected_channel.filter(|c| active.contains(c));
                let hov = self.hovered_channel.filter(|c| active.contains(c));
                let preferred = if self.use_hover_target {
                    hov.or(sel)
                } else {
                    sel.or(hov)
                };
                return preferred
                    .or_else(|| self.playback_mgr.primary_channel())
                    .or_else(|| self.app_cfg.channel_order.first().copied());
            }
            return self
                .playback_mgr
                .primary_channel()
                .or(self.selected_channel)
                .or_else(|| self.app_cfg.channel_order.first().copied());
        }
        resolve_live_target_channel(
            self.fullscreen_channel,
            self.selected_channel,
            self.hovered_channel,
            self.use_hover_target,
            &self.app_cfg.channel_order,
        )
    }

    fn open_settings_modal(&mut self, tab: SettingsTab) {
        self.edit_env_cfg = self.env_cfg.clone();
        self.edit_app_cfg = self.app_cfg.clone();
        self.settings_tab = tab;
        self.show_password = false;
        self.last_test_conn_banner = None;
        self.show_settings = true;
    }

    fn cancel_active_probes(&mut self) {
        self.probe_cancel.store(true, Ordering::Release);
        self.probe_gen = self.probe_gen.wrapping_add(1);
        self.probe_cancel = Arc::new(AtomicBool::new(false));
        if let Ok(mut g) = self.test_conn_state.lock() {
            *g = None;
        }
        if let Ok(mut g) = self.scan_in_progress.lock() {
            *g = None;
        }
    }

    fn close_settings_modal(&mut self) {
        self.show_settings = false;
        self.show_password = false;
        self.edit_env_cfg.password.zeroize();
        self.cancel_active_probes();
    }

    fn sync_p2p_tunnel(&mut self) {
        if self.env_cfg.is_cloud_p2p_active() {
            let (rtsp_port, http_port) = self.p2p_mgr.ensure_started(
                &self.env_cfg.p2p_serial,
                &self.env_cfg.username,
                &self.env_cfg.password,
                self.env_cfg.port,
                self.env_cfg.http_port,
            );
            self.env_cfg.p2p_local_port = rtsp_port;
            self.env_cfg.p2p_local_http_port = http_port;
            self.edit_env_cfg.p2p_local_port = rtsp_port;
            self.edit_env_cfg.p2p_local_http_port = http_port;
        } else {
            self.p2p_mgr.stop();
            self.env_cfg.p2p_local_port = 0;
            self.env_cfg.p2p_local_http_port = 0;
            self.edit_env_cfg.p2p_local_port = 0;
            self.edit_env_cfg.p2p_local_http_port = 0;
        }
    }

    fn push_toast(
        &mut self,
        kind: ToastKind,
        msg: impl Into<String>,
        open_dir: Option<PathBuf>,
    ) {
        push_toast_to_queue(
            &mut self.toasts,
            &mut self.next_toast_id,
            kind,
            msg.into(),
            open_dir,
            Instant::now(),
        );
    }

    fn show_toast_kind(&mut self, kind: ToastKind, msg: impl Into<String>) {
        self.push_toast(kind, msg, None);
    }

    fn show_toast(&mut self, msg: impl Into<String>) {
        self.push_toast(ToastKind::Info, msg, None);
    }

    fn open_local_directory(&mut self, ctx: &egui::Context, raw_dir: &Path, allow_create: bool) {
        self.os_fullscreen = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));

        match validate_and_prepare_open_dir(raw_dir, allow_create) {
            Ok(prepared) => {
                let _ = thread::Builder::new()
                    .name("nvr-open-dir".to_string())
                    .spawn(move || {
                        let xdg_bin = resolve_trusted_binary("xdg-open");
                        let xdg_ok = Command::new(&xdg_bin)
                            .arg(&prepared)
                            .env_remove("LD_LIBRARY_PATH")
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status()
                            .is_ok_and(|s| s.success());
                        if !xdg_ok {
                            let gio_bin = resolve_trusted_binary("gio");
                            let _ = Command::new(&gio_bin)
                                .arg("open")
                                .arg("--")
                                .arg(&prepared)
                                .env_remove("LD_LIBRARY_PATH")
                                .stdin(Stdio::null())
                                .stdout(Stdio::null())
                                .stderr(Stdio::null())
                                .status();
                        }
                    });
            }
            Err(err) => {
                self.show_toast_kind(ToastKind::Error, err);
            }
        }
    }

    fn live_matching_p2p_ports(&self, candidate: &NvrEnvConfig) -> Option<(u16, u16)> {
        if candidate.is_cloud_p2p_active()
            && self.env_cfg.is_cloud_p2p_active()
            && !self.p2p_mgr.is_halted()
            && self.p2p_mgr.is_connected_handle().load(Ordering::Acquire)
            && self.env_cfg.p2p_local_port > 0
            && self.env_cfg.p2p_local_http_port > 0
            && candidate.p2p_serial.trim() == self.env_cfg.p2p_serial.trim()
            && candidate.username == self.env_cfg.username
            && candidate.password == self.env_cfg.password
            && candidate.port == self.env_cfg.port
            && candidate.http_port == self.env_cfg.http_port
        {
            Some((
                self.env_cfg.p2p_local_port,
                self.env_cfg.p2p_local_http_port,
            ))
        } else {
            None
        }
    }

    fn start_test_connection(&mut self) {
        if self.test_conn_state.lock().unwrap().is_some() {
            return;
        }
        let mut cfg_clone = self.edit_env_cfg.clone();
        cfg_clone.num_cameras = cfg_clone.num_cameras.clamp(1, 32);
        if let Err(err) = cfg_clone.validate_for_save() {
            self.last_test_conn_banner = Some(Err(err.clone()));
            self.show_toast_kind(ToastKind::Error, err);
            return;
        }
        self.cancel_active_probes();
        let gen = self.probe_gen;
        let cancel_flag = Arc::clone(&self.probe_cancel);
        let live_ports = self.live_matching_p2p_ports(&cfg_clone);
        let state_ref = Arc::clone(&self.test_conn_state);
        *state_ref.lock().unwrap() = Some((gen, None));
        self.last_test_conn_banner = None;

        let _ = thread::Builder::new()
            .name("nvr-test-conn".to_string())
            .spawn(move || {
                let outcome = match prepare_probe_env_with_optional_p2p(
                    cfg_clone,
                    live_ports,
                    Some(&cancel_flag),
                ) {
                    Ok((probe_cfg, _p2p_guard)) => {
                        if cancel_flag.load(Ordering::Acquire) {
                            Err("Cancelled".to_string())
                        } else {
                            test_nvr_connection_sync(&probe_cfg)
                        }
                    }
                    Err(e) => Err(e),
                };
                if !cancel_flag.load(Ordering::Acquire) {
                    if let Ok(mut guard) = state_ref.lock() {
                        *guard = Some((gen, Some(outcome)));
                    }
                }
            });
    }

    fn start_auto_scan(&mut self) {
        if self.scan_in_progress.lock().unwrap().is_some() {
            return;
        }
        let mut cfg_clone = self.edit_env_cfg.clone();
        let scan_max = cfg_clone.num_cameras.clamp(1, 32);
        cfg_clone.num_cameras = scan_max;
        if let Err(err) = cfg_clone.validate_for_save() {
            self.show_toast_kind(ToastKind::Error, err);
            return;
        }
        self.cancel_active_probes();
        let gen = self.probe_gen;
        let cancel_flag = Arc::clone(&self.probe_cancel);
        let live_ports = self.live_matching_p2p_ports(&cfg_clone);
        let state_ref = Arc::clone(&self.scan_in_progress);
        *state_ref.lock().unwrap() = Some((gen, None));

        let _ = thread::Builder::new()
            .name("nvr-auto-scan".to_string())
            .spawn(move || {
                let report = match prepare_probe_env_with_optional_p2p(
                    cfg_clone,
                    live_ports,
                    Some(&cancel_flag),
                ) {
                    Ok((probe_cfg, _p2p_guard)) => scan_active_nvr_channels_cancellable(
                        &probe_cfg,
                        scan_max,
                        Some(&cancel_flag),
                    ),
                    Err(e) => ChannelScanReport {
                        channels: Vec::new(),
                        auth_rejected: false,
                        error: Some(e),
                    },
                };
                if !cancel_flag.load(Ordering::Acquire) {
                    if let Ok(mut guard) = state_ref.lock() {
                        *guard = Some((gen, Some(report)));
                    }
                }
            });
    }

    fn poll_background_jobs(&mut self) {
        // 1. Poll background PNG snapshot encoding results
        let snap_results: Vec<Result<(String, PathBuf), String>> = self
            .pending_snapshot_results
            .lock()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default();
        for res in snap_results {
            match res {
                Ok((msg, dir)) => self.push_toast(ToastKind::Success, msg, Some(dir)),
                Err(err) => self.push_toast(ToastKind::Error, err, None),
            }
        }

        // 2. Poll NVR Test Connection probe completion (ignoring stale generations)
        let cur_gen = self.probe_gen;
        let test_done = self
            .test_conn_state
            .lock()
            .ok()
            .and_then(|mut guard| {
                if matches!(*guard, Some((_, Some(_)))) {
                    guard.take().and_then(|(g, res)| {
                        if g == cur_gen {
                            res
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
            });
        if let Some(outcome) = test_done {
            self.last_test_conn_banner = Some(outcome.clone());
            match outcome {
                Ok(msg) => self.show_toast_kind(ToastKind::Success, msg),
                Err(err) => self.show_toast_kind(ToastKind::Error, format!("Connection test failed: {}", err)),
            }
        }

        // 3. Poll NVR Auto-Scan completion (ignoring stale generations)
        let scan_done = self
            .scan_in_progress
            .lock()
            .ok()
            .and_then(|mut guard| {
                if matches!(*guard, Some((_, Some(_)))) {
                    guard.take().and_then(|(g, rep)| {
                        if g == cur_gen {
                            rep
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
            });
        if let Some(report) = scan_done {
            if !report.channels.is_empty() {
                let max_ch = *report.channels.iter().max().unwrap_or(&12);
                self.edit_env_cfg.num_cameras = max_ch.max(report.channels.len()).clamp(1, 32);
                self.show_toast_kind(
                    ToastKind::Success,
                    format!(
                        "Auto-Scan detected {} active channels (up to Channel {})",
                        report.channels.len(),
                        max_ch
                    ),
                );
            } else if report.auth_rejected {
                self.show_toast_kind(
                    ToastKind::Error,
                    report.error.unwrap_or_else(|| {
                        "Auto-Scan failed: Authentication rejected (401)".to_string()
                    }),
                );
            } else if let Some(err) = report.error {
                self.show_toast_kind(ToastKind::Error, format!("Auto-Scan failed: {}", err));
            } else {
                self.show_toast_kind(
                    ToastKind::Warning,
                    "Auto-Scan completed: NVR reachable, but 0 active video channels responded",
                );
            }
        }
    }

    fn default_playback_start_ms(&self, channel: usize) -> i64 {
        let now_local = Local::now().naive_local();
        let now_ms = local_naive_to_epoch_ms(now_local);
        let today = now_local.date();
        let day_start_ms =
            local_naive_to_epoch_ms(today.and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap()));

        if let Some(tl) = self.playback_mgr.day_timeline(channel, today) {
            if let Some(last_ev) = tl.events.last() {
                let hi = now_ms.saturating_sub(2_000).max(day_start_ms);
                return (last_ev.start_ms - 3_000).clamp(day_start_ms, hi);
            }
        }
        now_ms.saturating_sub(5 * 60_000).max(day_start_ms)
    }

    fn apply_playback_live_governor(&mut self, primary_channel: usize) {
        if self.env_cfg.is_cloud_p2p_active() {
            if !self.all_paused && !self.p2p_playback_paused_live {
                self.p2p_playback_paused_live = true;
                self.stream_mgr.set_all_paused(true);
            }
        } else {
            if self.p2p_playback_paused_live {
                self.p2p_playback_paused_live = false;
                if !self.all_paused {
                    self.stream_mgr.set_all_paused(false);
                }
            }
            // Keep primary live stream running smoothly for the 196x110 Live PiP corner window,
            // while placing background cameras in eco publish mode.
            self.stream_mgr
                .set_fullscreen_focus(Some(primary_channel), false);
        }
    }

    fn restore_live_stream_governor(&mut self) {
        if self.playback_mgr.is_active() {
            if self.playback_mgr.is_sync_2x2() {
                self.stream_mgr.set_fullscreen_focus(None, false);
                self.stream_mgr.set_window_minimized_eco(true);
            } else if let Some(ch) = self.playback_mgr.primary_channel() {
                self.apply_playback_live_governor(ch);
            }
        } else if let Some(fs_ch) = self.fullscreen_channel {
            self.stream_mgr
                .set_fullscreen_focus(Some(fs_ch), self.app_cfg.fullscreen_auto_hd);
        } else {
            self.stream_mgr
                .set_fullscreen_focus(None, self.app_cfg.fullscreen_auto_hd);
            self.stream_mgr.set_window_minimized_eco(false);
        }
    }

    fn enter_playback_single(&mut self, channel: usize, preserve_playhead: bool) {
        let ch = channel.clamp(1, self.app_cfg.num_cameras.max(1));
        self.patrol_active = false;
        self.minimized_paused_playback = false;
        self.selected_channel = Some(ch);
        if self.fullscreen_channel.is_some() {
            self.fullscreen_channel = Some(ch);
        }

        let start_ms = if preserve_playhead && self.playback_mgr.is_active() {
            self.playback_mgr.playhead_epoch_ms()
        } else {
            self.default_playback_start_ms(ch)
        };

        if let Some(tile) = self.tiles.get_mut(&ch) {
            tile.pb_last_uploaded_seq = 0;
            tile.pb.clear_transient_drag();
        }

        let was_active = self.playback_mgr.is_active();
        self.apply_playback_live_governor(ch);
        self.playback_mgr.open_single(ch, start_ms);
        if !was_active {
            self.timeline_ui.reset_for_open(start_ms);
            self.playback_entered_at = Some(Instant::now());
        } else {
            self.timeline_ui.recenter_on_playhead(start_ms);
        }

        let ts_label = epoch_ms_to_local_naive(start_ms).format("%H:%M:%S");
        self.show_toast_kind(
            ToastKind::Info,
            format!(
                "{} -> 1080p Playback ({})",
                self.app_cfg.camera_display_name(ch),
                ts_label
            ),
        );
    }

    fn enter_playback_sync_2x2(&mut self) {
        let recent_saved_ms = self.last_playback_exit_ms.take().and_then(|(ms, exited_at)| {
            if exited_at.elapsed() <= Duration::from_millis(600) {
                Some(ms)
            } else {
                None
            }
        });

        if self.env_cfg.is_cloud_p2p_active() {
            self.show_toast_kind(
                ToastKind::Warning,
                "2x2 Sync requires LAN/Tailscale (Cloud P2P uses 1-Cam 1080p)",
            );
            if !self.playback_mgr.is_active() {
                if let Some(ch) = self.active_target_channel() {
                    if let Some(saved_ms) = recent_saved_ms {
                        let ch_clamped = ch.clamp(1, self.app_cfg.num_cameras.max(1));
                        self.selected_channel = Some(ch_clamped);
                        self.apply_playback_live_governor(ch_clamped);
                        self.playback_mgr.open_single(ch_clamped, saved_ms);
                        self.timeline_ui.reset_for_open(saved_ms);
                        self.playback_entered_at = Some(Instant::now());
                    } else {
                        self.enter_playback_single(ch, false);
                    }
                }
            }
            return;
        }

        let primary = self.active_target_channel().unwrap_or(1);
        let mut chs = vec![primary];
        for &c in &self.app_cfg.channel_order {
            if !chs.contains(&c) && chs.len() < 4 {
                chs.push(c);
            }
        }
        if chs.len() < 2 {
            if let Some(saved_ms) = recent_saved_ms {
                let ch_clamped = primary.clamp(1, self.app_cfg.num_cameras.max(1));
                self.selected_channel = Some(ch_clamped);
                self.apply_playback_live_governor(ch_clamped);
                self.playback_mgr.open_single(ch_clamped, saved_ms);
                self.timeline_ui.reset_for_open(saved_ms);
                self.playback_entered_at = Some(Instant::now());
            } else {
                self.enter_playback_single(primary, self.playback_mgr.is_active());
            }
            return;
        }

        self.patrol_active = false;
        self.minimized_paused_playback = false;
        self.selected_channel = Some(primary);
        let start_ms = if self.playback_mgr.is_active() {
            self.playback_mgr.playhead_epoch_ms()
        } else if let Some(saved_ms) = recent_saved_ms {
            saved_ms
        } else {
            self.default_playback_start_ms(primary)
        };
        for &c in &chs {
            if let Some(tile) = self.tiles.get_mut(&c) {
                tile.pb_last_uploaded_seq = 0;
                tile.pb.clear_transient_drag();
            }
        }

        let was_active = self.playback_mgr.is_active();
        self.stream_mgr.set_fullscreen_focus(None, false);
        self.stream_mgr.set_window_minimized_eco(true);
        self.playback_mgr.open_sync_grid(chs.clone(), start_ms);
        if !was_active {
            self.timeline_ui.reset_for_open(start_ms);
            self.playback_entered_at = Some(Instant::now());
        }
        let ch_list = chs
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        self.show_toast_kind(
            ToastKind::Info,
            format!("2x2 Sync Playback (Ch {})", ch_list),
        );
    }

    fn exit_playback_to_live(&mut self) {
        if !self.playback_mgr.is_active() {
            return;
        }
        for tile in self.tiles.values_mut() {
            tile.pb_texture = None;
            tile.pb_last_uploaded_seq = 0;
            tile.pb.clear_transient_drag();
        }
        self.playback_mgr.close();
        self.playback_entered_at = None;
        self.minimized_paused_playback = false;
        self.timeline_ui.show_calendar_popup = false;
        self.timeline_ui.show_time_jump_popup = false;

        if self.p2p_playback_paused_live {
            self.p2p_playback_paused_live = false;
            if !self.all_paused {
                self.stream_mgr.set_all_paused(false);
            }
        }
        if let Some(fs_ch) = self.fullscreen_channel {
            self.stream_mgr
                .set_fullscreen_focus(Some(fs_ch), self.app_cfg.fullscreen_auto_hd);
        } else {
            self.stream_mgr
                .set_fullscreen_focus(None, self.app_cfg.fullscreen_auto_hd);
            self.stream_mgr.set_window_minimized_eco(false);
        }
        self.show_toast_kind(ToastKind::Info, "Returned to Live Edge");
    }

    fn start_current_clip_export(&mut self, in_ms: i64, out_ms: i64) {
        let Some(ch) = self.active_target_channel() else {
            return;
        };
        if self.playback_mgr.export_state().in_progress {
            self.show_toast_kind(ToastKind::Warning, "A clip export is already in progress...");
            return;
        }
        let cam_label = self.app_cfg.camera_display_name(ch);
        let out_dir = expand_tilde(&self.app_cfg.recording_dir);
        self.playback_mgr
            .start_clip_export(ch, &cam_label, &out_dir, in_ms, out_ms);
        let dur_s = ((out_ms.abs_diff(in_ms) as i64) / 1000).max(1);
        self.show_toast_kind(
            ToastKind::Info,
            format!("Exporting {}s MP4 clip for {}...", dur_s, cam_label),
        );
    }

    fn refresh_active_playback_timeline(&mut self) {
        if let Some(ch) = self.playback_mgr.primary_channel() {
            let date = self.timeline_ui.selected_date;
            self.playback_mgr.request_day_timeline(ch, date, true);
            let (y, m) = self.timeline_ui.calendar_ym();
            self.playback_mgr.request_month_days_ex(ch, y, m, true);
            self.show_toast_kind(ToastKind::Info, "Refreshing NVR timeline...");
        }
    }

    fn apply_playback_speed_with_toast(&mut self, set_fn: impl FnOnce(&mut PlaybackManager), announce: bool) {
        let had_smart_skip = self.playback_mgr.smart_skip_mode() != SmartSkipMode::Off;
        set_fn(&mut self.playback_mgr);
        let new_spd = self.playback_mgr.speed();
        let skip_paused = had_smart_skip
            && new_spd < 0.0
            && self.playback_mgr.smart_skip_mode() == SmartSkipMode::Off;
        if skip_paused {
            self.show_toast_kind(
                ToastKind::Warning,
                format!(
                    "Playback speed -> {:.1}x (Smart Skip paused in reverse)",
                    new_spd
                ),
            );
        } else if announce {
            self.show_toast_kind(
                ToastKind::Info,
                format!("Playback speed -> {:.1}x", new_spd),
            );
        }
    }

    fn handle_timeline_actions(&mut self, actions: Vec<TimelineAction>) {
        for act in actions {
            match act {
                TimelineAction::ExitToLive => self.exit_playback_to_live(),
                TimelineAction::ToggleSync2x2 => {
                    if self.playback_mgr.is_sync_2x2() {
                        let ch = self.active_target_channel().unwrap_or(1);
                        self.enter_playback_single(ch, true);
                    } else {
                        self.enter_playback_sync_2x2();
                    }
                }
                TimelineAction::SelectChannel(ch) => {
                    if self.playback_mgr.is_sync_2x2()
                        && self.playback_mgr.active_channels().contains(&ch)
                    {
                        self.selected_channel = Some(ch);
                        self.playback_mgr.set_primary_channel(ch);
                        let date = self.timeline_ui.selected_date;
                        self.playback_mgr.request_day_timeline(ch, date, false);
                        self.playback_mgr
                            .request_month_days(ch, date.year(), date.month());
                        self.timeline_ui
                            .recenter_on_playhead(self.playback_mgr.playhead_epoch_ms());
                        self.show_toast_kind(
                            ToastKind::Info,
                            format!(
                                "Timeline focused on {} (2x2 Sync)",
                                self.app_cfg.camera_display_name(ch)
                            ),
                        );
                    } else {
                        self.enter_playback_single(ch, true);
                    }
                }
                TimelineAction::ChangeDate(date) => {
                    for tile in self.tiles.values_mut() {
                        tile.pb_texture = None;
                        tile.pb_last_uploaded_seq = 0;
                    }
                    self.timeline_ui.clear_clip_range();
                    let cur_dt = epoch_ms_to_local_naive(self.playback_mgr.playhead_epoch_ms());
                    let now_local = Local::now().naive_local();
                    let day_floor = date.and_hms_opt(0, 0, 0).unwrap();
                    let target_dt = date
                        .and_time(cur_dt.time())
                        .min(if date == now_local.date() {
                            now_local - chrono::Duration::seconds(5)
                        } else {
                            date.and_time(NaiveTime::from_hms_opt(23, 59, 50).unwrap())
                        })
                        .max(day_floor);
                    let target_ms = local_naive_to_epoch_ms(target_dt);
                    if let Some(ch) = self.playback_mgr.primary_channel() {
                        self.playback_mgr.request_day_timeline(ch, date, false);
                        self.playback_mgr
                            .request_month_days(ch, date.year(), date.month());
                    }
                    self.timeline_ui.selected_date = date;
                    self.timeline_ui.recenter_on_playhead(target_ms);
                    self.playback_mgr.seek(target_ms, true);
                }
                TimelineAction::Seek {
                    epoch_ms,
                    immediate,
                } => {
                    self.playback_mgr.seek(epoch_ms, immediate);
                }
                TimelineAction::TogglePause => {
                    self.playback_mgr.toggle_pause();
                }
                TimelineAction::StepFrame(delta) => {
                    self.playback_mgr.step_frame(delta);
                }
                TimelineAction::SetSpeed(spd) => {
                    self.apply_playback_speed_with_toast(|mgr| mgr.set_speed(spd), false);
                }
                TimelineAction::CycleSmartSkip => {
                    let mode = self.playback_mgr.cycle_smart_skip_mode();
                    self.show_toast_kind(
                        ToastKind::Info,
                        format!("Smart Skip -> {}", mode.label()),
                    );
                }
                TimelineAction::JumpPrevEvent => {
                    let filt = self.timeline_ui.event_filter;
                    if let Some(ms) = self.playback_mgr.jump_adjacent_event(false, filt) {
                        self.timeline_ui.auto_scroll_to_active_event = true;
                        self.timeline_ui.recenter_on_playhead(ms);
                    } else {
                        self.show_toast_kind(ToastKind::Info, "No earlier matching event found");
                    }
                }
                TimelineAction::JumpNextEvent => {
                    let filt = self.timeline_ui.event_filter;
                    if let Some(ms) = self.playback_mgr.jump_adjacent_event(true, filt) {
                        self.timeline_ui.auto_scroll_to_active_event = true;
                        self.timeline_ui.recenter_on_playhead(ms);
                    } else {
                        self.show_toast_kind(ToastKind::Info, "No later matching event found");
                    }
                }
                TimelineAction::ExportClip { in_ms, out_ms } => {
                    self.start_current_clip_export(in_ms, out_ms);
                }
                TimelineAction::CancelClipExport => {
                    self.playback_mgr.cancel_clip_export();
                    let _ = self.playback_mgr.take_finished_export();
                    self.show_toast_kind(ToastKind::Warning, "Cancelled MP4 clip export");
                }
                TimelineAction::RefreshTimeline => {
                    self.refresh_active_playback_timeline();
                }
            }
        }
    }

    fn enter_fullscreen(&mut self, channel: usize) {
        if self.playback_mgr.is_active() {
            self.exit_playback_to_live();
        }
        self.fullscreen_channel = Some(channel);
        self.selected_channel = Some(channel);
        self.stream_mgr
            .set_fullscreen_focus(Some(channel), self.app_cfg.fullscreen_auto_hd);
    }

    fn exit_fullscreen(&mut self) {
        self.fullscreen_channel = None;
        self.patrol_active = false;
        self.stream_mgr
            .set_fullscreen_focus(None, self.app_cfg.fullscreen_auto_hd);
    }

    fn toggle_single_view_on(&mut self, channel: usize) {
        if channel < 1 || channel > self.app_cfg.num_cameras {
            return;
        }
        if self.playback_mgr.is_active() {
            if !self.playback_mgr.is_sync_2x2()
                && self.playback_mgr.primary_channel() == Some(channel)
            {
                return;
            }
            self.enter_playback_single(channel, true);
            return;
        }
        if self.fullscreen_channel == Some(channel) {
            self.exit_fullscreen();
        } else {
            self.enter_fullscreen(channel);
        }
    }

    fn set_camera_subtype(&mut self, channel: usize, subtype: u8) {
        if let Some(slot) = self.stream_mgr.slot(channel) {
            slot.set_manual_subtype(subtype);
            let label = if subtype == 0 { "HD" } else { "SD" };
            self.show_toast_kind(
                ToastKind::Info,
                format!(
                    "{} -> {}",
                    self.app_cfg.camera_display_name(channel),
                    label
                ),
            );
        }
    }

    fn set_all_cameras_subtype(&mut self, subtype: u8, toast: impl Into<String>) {
        self.env_cfg.default_subtype = subtype;
        self.edit_env_cfg.default_subtype = subtype;
        if !self.env_cfg.password.trim().is_empty() || self.env_path.exists() {
            let _ = save_env_config(&self.env_path, &self.env_cfg);
        }
        self.stream_mgr.set_all_subtype(subtype);
        self.show_toast_kind(ToastKind::Info, toast);
    }

    fn toggle_camera_hd_sd(&mut self, channel: usize) {
        if let Some(slot) = self.stream_mgr.slot(channel) {
            let current_target = slot.target_subtype.load(Ordering::Acquire);
            let next_sub = if current_target == 0 { 1 } else { 0 };
            slot.set_manual_subtype(next_sub);
            let label = if next_sub == 0 {
                "HD (Main-Stream)"
            } else {
                "SD (Sub-Stream)"
            };
            self.show_toast_kind(
                ToastKind::Info,
                format!(
                    "{} -> {}",
                    self.app_cfg.camera_display_name(channel),
                    label
                ),
            );
        }
    }

    fn reset_all_zoom(&mut self) {
        let is_pb = self.playback_mgr.is_active();
        for tile in self.tiles.values_mut() {
            tile.viewport_mut(is_pb).animate_reset_zoom();
        }
        self.show_toast_kind(ToastKind::Info, "Reset zoom on all cameras");
    }

    fn reset_tiles_for_restart(&mut self) {
        let max_ch = self.app_cfg.num_cameras;
        self.tiles.retain(|&ch, _| ch <= max_ch);
        for ch in 1..=max_ch {
            let tile = self.tiles.entry(ch).or_default();
            tile.texture = None;
            tile.pb_texture = None;
            tile.last_uploaded_seq = 0;
            tile.pb_last_uploaded_seq = 0;
        }
        if self.fullscreen_channel.is_some_and(|ch| ch > max_ch) {
            self.fullscreen_channel = None;
            self.patrol_active = false;
        }
        if self.selected_channel.is_none_or(|ch| ch > max_ch) {
            self.selected_channel = self.app_cfg.channel_order.first().copied();
        }
    }

    fn restart_streams_with_current_env(&mut self) {
        self.auth_gw.circuit_breaker().reset();
        self.reset_tiles_for_restart();
        self.stream_mgr.update_env_and_restart(self.env_cfg.clone());
        self.playback_mgr.update_env(self.env_cfg.clone());
        if self.all_paused {
            self.stream_mgr.set_all_paused(true);
        }
        if self.playback_mgr.is_active() {
            if self.env_cfg.is_cloud_p2p_active() && self.playback_mgr.is_sync_2x2() {
                let ch = self.playback_mgr.primary_channel().unwrap_or(1);
                self.enter_playback_single(ch, true);
            } else {
                self.restore_live_stream_governor();
                let cur_ms = self.playback_mgr.playhead_epoch_ms();
                self.playback_mgr.seek(cur_ms, true);
            }
        } else {
            self.restore_live_stream_governor();
        }
    }

    fn trigger_manual_resync(&mut self) {
        self.auth_gw.circuit_breaker().reset();
        if self.env_cfg.is_cloud_p2p_active() && self.p2p_mgr.is_halted() {
            self.p2p_mgr.clear_active_key();
            let prev_port = self.env_cfg.p2p_local_port;
            let prev_http = self.env_cfg.p2p_local_http_port;
            self.sync_p2p_tunnel();
            if self.env_cfg.p2p_local_port != prev_port
                || self.env_cfg.p2p_local_http_port != prev_http
            {
                self.restart_streams_with_current_env();
                return;
            }
        }
        self.stream_mgr.resync_all();
    }

    fn cycle_fullscreen(&mut self, delta: isize) {
        let order = &self.app_cfg.channel_order;
        if order.is_empty() {
            return;
        }
        let current = self.fullscreen_channel.unwrap_or(order[0]);
        let idx = order.iter().position(|&c| c == current).unwrap_or(0) as isize;
        let len = order.len() as isize;
        let next_idx = (idx + delta).rem_euclid(len) as usize;
        let next_ch = order[next_idx];
        self.enter_fullscreen(next_ch);
    }

    fn save_png_exclusive(img: &RgbaImage, out_dir: &Path, stem: &str) -> Result<PathBuf, String> {
        ensure_private_dir(out_dir).map_err(|e| format!("Directory error: {}", e))?;
        for attempt in 0..16u32 {
            let filename = if attempt == 0 {
                format!("{}.png", stem)
            } else {
                format!("{}_{}.png", stem, attempt)
            };
            match create_private_file_exclusive(out_dir, &filename) {
                Ok((file, created_path)) => {
                    let mut writer = BufWriter::new(file);
                    if let Err(e) = img.write_to(&mut writer, ImageFormat::Png) {
                        let _ = fs::remove_file(&created_path);
                        return Err(format!("PNG encode error: {}", e));
                    }
                    if let Err(e) = writer.flush() {
                        let _ = fs::remove_file(&created_path);
                        return Err(format!("PNG flush error: {}", e));
                    }
                    let inner_file = match writer.into_inner() {
                        Ok(f) => f,
                        Err(e) => {
                            let _ = fs::remove_file(&created_path);
                            return Err(format!("PNG buffer flush error: {}", e));
                        }
                    };
                    if let Err(e) = inner_file.sync_all() {
                        let _ = fs::remove_file(&created_path);
                        return Err(format!("PNG sync error: {}", e));
                    }
                    return Ok(created_path);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("File open error: {}", e)),
            }
        }
        Err("Too many snapshot filename collisions".to_string())
    }

    fn try_acquire_snapshot_permit(&mut self) -> Option<SnapshotPermit> {
        let mut cur = self.in_flight_snapshots.load(Ordering::Acquire);
        loop {
            if cur >= MAX_CONCURRENT_SNAPSHOTS {
                self.show_toast_kind(
                    ToastKind::Warning,
                    "Snapshot queue busy - wait for current save to finish",
                );
                return None;
            }
            match self.in_flight_snapshots.compare_exchange_weak(
                cur,
                cur + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(SnapshotPermit(Arc::clone(&self.in_flight_snapshots)));
                }
                Err(actual) => cur = actual,
            }
        }
    }

    fn spawn_png_save_job(
        &mut self,
        permit: SnapshotPermit,
        img: RgbaImage,
        out_dir: PathBuf,
        stem: String,
        success_prefix: String,
    ) {
        let results_ref = Arc::clone(&self.pending_snapshot_results);
        if thread::Builder::new()
            .name("nvr-png-save".to_string())
            .spawn(move || {
                let _permit = permit;
                let res = match Self::save_png_exclusive(&img, &out_dir, &stem) {
                    Ok(full_path) => {
                        let fname = full_path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| full_path.display().to_string());
                        let dir = full_path
                            .parent()
                            .map(|p| p.to_path_buf())
                            .unwrap_or(out_dir);
                        Ok((format!("{}: {}", success_prefix, fname), dir))
                    }
                    Err(e) => Err(format!("Snapshot failed: {}", e)),
                };
                if let Ok(mut queue) = results_ref.lock() {
                    queue.push(res);
                }
            })
            .is_err()
        {
            self.show_toast_kind(ToastKind::Error, "Snapshot failed: unable to spawn worker");
        }
    }

    fn save_single_snapshot(&mut self, channel: usize) {
        self.save_camera_snapshot(channel, true);
    }

    fn save_camera_snapshot(&mut self, channel: usize, crop_to_zoom: bool) {
        let Some(permit) = self.try_acquire_snapshot_permit() else {
            return;
        };
        let in_playback =
            self.playback_mgr.is_active() && self.playback_mgr.slot(channel).is_some();

        let (rgba, w, h, ts_tag) = if in_playback {
            let Some(pb_slot) = self.playback_mgr.slot(channel) else {
                return;
            };
            let ep_ms = pb_slot.current_epoch_ms.load(Ordering::Acquire);
            let hist_ts = epoch_ms_to_local_naive(ep_ms)
                .format("hist_%Y%m%d_%H%M%S_%3f")
                .to_string();
            let Ok(guard) = pb_slot.buffer.lock() else {
                return;
            };
            if guard.rgba.is_empty() || guard.width == 0 || guard.height == 0 {
                drop(guard);
                drop(permit);
                self.show_toast_kind(
                    ToastKind::Warning,
                    format!("Camera {} playback frame not ready yet", channel),
                );
                return;
            }
            (guard.rgba.clone(), guard.width, guard.height, hist_ts)
        } else {
            let Some(slot) = self.stream_mgr.slot(channel) else {
                return;
            };
            let live_ts = Local::now().format("%Y%m%d_%H%M%S_%3f").to_string();
            let Ok(guard) = slot.buffer.lock() else {
                return;
            };
            if guard.rgba.is_empty() || guard.width == 0 || guard.height == 0 {
                drop(guard);
                drop(permit);
                self.show_toast_kind(
                    ToastKind::Warning,
                    format!("Camera {} has no live frame ready yet", channel),
                );
                return;
            }
            (guard.rgba.clone(), guard.width, guard.height, live_ts)
        };

        self.shutter_flash_until
            .insert(channel, Instant::now() + SHUTTER_FLASH_DURATION);

        let zoom_crop = if crop_to_zoom {
            self.tiles.get(&channel).and_then(|t| {
                let vp = t.viewport(in_playback);
                if vp.zoom > 1.02 {
                    Some((vp.zoom, vp.uv_rect()))
                } else {
                    None
                }
            })
        } else {
            None
        };

        let out_dir = expand_tilde(&self.app_cfg.screenshot_dir);

        if let Some((z, uv)) = zoom_crop {
            let x0 = ((uv.min.x * w as f32).floor() as u32).min(w.saturating_sub(1));
            let y0 = ((uv.min.y * h as f32).floor() as u32).min(h.saturating_sub(1));
            let x1 = ((uv.max.x * w as f32).ceil() as u32).clamp(x0 + 1, w);
            let y1 = ((uv.max.y * h as f32).ceil() as u32).clamp(y0 + 1, h);
            let cw = x1 - x0;
            let ch_px = y1 - y0;
            let mut cropped = Vec::with_capacity((cw * ch_px * 4) as usize);
            for yy in y0..y1 {
                let row_start = ((yy * w + x0) * 4) as usize;
                let row_end = row_start + (cw * 4) as usize;
                if row_end <= rgba.len() {
                    cropped.extend_from_slice(&rgba[row_start..row_end]);
                }
            }
            let stem = format!("cam{:02}_zoom{:.1}x_{}", channel, z, ts_tag);
            if let Some(img) = ImageBuffer::<Rgba<u8>, _>::from_raw(cw, ch_px, cropped) {
                self.spawn_png_save_job(
                    permit,
                    img,
                    out_dir,
                    stem,
                    format!("Saved {:.1}x zoomed snapshot", z),
                );
            }
            return;
        }

        let stem = format!("cam{:02}_{}", channel, ts_tag);
        if let Some(img) = ImageBuffer::<Rgba<u8>, _>::from_raw(w, h, rgba) {
            self.spawn_png_save_job(permit, img, out_dir, stem, "Saved snapshot".to_string());
        }
    }

    fn save_composite_wall_snapshot(&mut self) {
        if self.playback_mgr.is_active() {
            if let Some(ch) = self.active_target_channel() {
                self.save_single_snapshot(ch);
            }
            return;
        }
        if self.fullscreen_channel.is_some() {
            if let Some(ch) = self.fullscreen_channel {
                self.save_single_snapshot(ch);
            }
            return;
        }

        let Some(permit) = self.try_acquire_snapshot_permit() else {
            return;
        };

        let (rows, cols) = self.app_cfg.grid_dimensions();
        let tile_w = 480u32;
        let tile_h = 270u32;
        let mut canvas = RgbaImage::from_pixel(
            cols as u32 * tile_w,
            rows as u32 * tile_h,
            Rgba([9, 9, 11, 255]),
        );
        let mut captured_channels = Vec::new();

        for (idx, &ch) in self.app_cfg.channel_order.iter().enumerate() {
            let r = (idx / cols) as u32;
            let c = (idx % cols) as u32;
            if r >= rows as u32 {
                break;
            }
            let Some(slot) = self.stream_mgr.slot(ch) else {
                continue;
            };
            let Ok(guard) = slot.buffer.lock() else {
                continue;
            };
            if guard.rgba.is_empty() || guard.width == 0 || guard.height == 0 {
                continue;
            }
            captured_channels.push(ch);
            let src_w = guard.width;
            let src_h = guard.height;
            let ox = c * tile_w;
            let oy = r * tile_h;

            for ty in 0..tile_h {
                let sy = (ty * src_h / tile_h).min(src_h - 1);
                for tx in 0..tile_w {
                    let sx = (tx * src_w / tile_w).min(src_w - 1);
                    let s_idx = ((sy * src_w + sx) * 4) as usize;
                    if s_idx + 3 < guard.rgba.len() {
                        canvas.put_pixel(
                            ox + tx,
                            oy + ty,
                            Rgba([
                                guard.rgba[s_idx],
                                guard.rgba[s_idx + 1],
                                guard.rgba[s_idx + 2],
                                255,
                            ]),
                        );
                    }
                }
            }
        }

        if captured_channels.is_empty() {
            drop(permit);
            self.show_toast_kind(
                ToastKind::Warning,
                "No live camera frames ready for wall snapshot yet",
            );
            return;
        }

        let flash_until = Instant::now() + SHUTTER_FLASH_DURATION;
        for ch in &captured_channels {
            self.shutter_flash_until.insert(*ch, flash_until);
        }

        let out_dir = expand_tilde(&self.app_cfg.screenshot_dir);
        let ts = Local::now().format("%Y%m%d_%H%M%S_%3f");
        let stem = format!("wall_{}cams_{}", self.app_cfg.num_cameras, ts);
        let prefix = format!("Saved {}-cam wall snapshot", captured_channels.len());
        self.spawn_png_save_job(permit, canvas, out_dir, stem, prefix);
    }

    fn apply_and_save_settings(&mut self, ctx: &egui::Context) -> bool {
        self.edit_env_cfg.num_cameras = self.edit_env_cfg.num_cameras.clamp(1, 32);
        if let Err(err_msg) = self.edit_env_cfg.validate_for_save() {
            self.show_toast_kind(ToastKind::Error, err_msg);
            return false;
        }

        let prev_env = self.env_cfg.clone();
        let prev_auto_hd = self.app_cfg.fullscreen_auto_hd;

        let mut next_app_cfg = self.edit_app_cfg.clone();
        next_app_cfg.num_cameras = self.edit_env_cfg.num_cameras;
        next_app_cfg.layout_mode = self.app_cfg.layout_mode.clone();
        next_app_cfg.channel_order = self.app_cfg.channel_order.clone();
        next_app_cfg.normalize_channels();
        // Trim and prune blank or unchanged default "Camera N" entries
        next_app_cfg.camera_names = normalized_camera_names(&next_app_cfg);

        if let Err(e) = save_env_config(&self.env_path, &self.edit_env_cfg) {
            self.show_toast_kind(
                ToastKind::Error,
                format!("Failed to save .env configuration: {}", e),
            );
            return false;
        }
        if let Err(e) = next_app_cfg.save() {
            self.show_toast_kind(
                ToastKind::Error,
                format!("Failed to save preferences (config.json): {}", e),
            );
            return false;
        }

        self.cancel_active_probes();
        self.env_cfg = self.edit_env_cfg.clone();
        let p2p_was_halted = self.p2p_mgr.is_halted();
        if p2p_was_halted {
            self.p2p_mgr.clear_active_key();
        }
        self.sync_p2p_tunnel();

        self.app_cfg = next_app_cfg;
        self.edit_app_cfg = self.app_cfg.clone();
        self.show_password = false;
        self.sync_window_title(ctx);

        let stream_env_changed =
            self.env_cfg != prev_env || (p2p_was_halted && self.env_cfg.is_cloud_p2p_active());
        if stream_env_changed {
            if self.playback_mgr.is_active() {
                self.exit_playback_to_live();
            }
            self.fullscreen_channel = None;
            self.patrol_active = false;
            self.restart_streams_with_current_env();
            self.show_toast_kind(
                ToastKind::Success,
                format!(
                    "Saved configuration ({} cameras on {}:{})",
                    self.env_cfg.num_cameras,
                    self.env_cfg.active_host(),
                    self.env_cfg.active_port()
                ),
            );
        } else {
            if !self.playback_mgr.is_active() && prev_auto_hd != self.app_cfg.fullscreen_auto_hd {
                if let Some(fs_ch) = self.fullscreen_channel {
                    self.stream_mgr
                        .set_fullscreen_focus(Some(fs_ch), self.app_cfg.fullscreen_auto_hd);
                }
            }
            self.show_toast_kind(ToastKind::Success, "Saved preferences & camera names");
        }
        true
    }

    fn set_network_mode(&mut self, ctx: &egui::Context, target: NetworkTargetMode) {
        match target {
            NetworkTargetMode::Lan => {
                if !self.env_cfg.use_cloud_p2p && !self.env_cfg.use_tailscale {
                    return;
                }
                self.env_cfg.use_cloud_p2p = false;
                self.env_cfg.use_tailscale = false;
                self.p2p_playback_paused_live = false;
            }
            NetworkTargetMode::Tailscale => {
                if self.env_cfg.tailscale_host.trim().is_empty() {
                    self.open_settings_modal(SettingsTab::NvrAndCameras);
                    self.show_toast_kind(
                        ToastKind::Info,
                        "Enter your Tailscale NVR IP in Settings",
                    );
                    return;
                }
                if self.env_cfg.is_tailscale_active() {
                    return;
                }
                self.env_cfg.use_tailscale = true;
                self.env_cfg.use_cloud_p2p = false;
                self.p2p_playback_paused_live = false;
            }
            NetworkTargetMode::CloudP2P => {
                if self.env_cfg.p2p_serial.trim().is_empty() {
                    self.open_settings_modal(SettingsTab::NvrAndCameras);
                    self.show_toast_kind(
                        ToastKind::Info,
                        "Enter your NVR Cloud P2P Serial in Settings",
                    );
                    return;
                }
                if self.env_cfg.is_cloud_p2p_active() && !self.p2p_mgr.is_halted() {
                    return;
                }
                self.env_cfg.use_cloud_p2p = true;
                self.env_cfg.use_tailscale = false;
                if self.p2p_mgr.is_halted() {
                    self.p2p_mgr.clear_active_key();
                }
            }
        }

        self.edit_env_cfg.use_cloud_p2p = self.env_cfg.use_cloud_p2p;
        self.edit_env_cfg.use_tailscale = self.env_cfg.use_tailscale;
        self.sync_p2p_tunnel();
        if !self.env_cfg.password.trim().is_empty() || self.env_path.exists() {
            let _ = save_env_config(&self.env_path, &self.env_cfg);
        }
        self.sync_window_title(ctx);
        self.restart_streams_with_current_env();

        let desc = match target {
            NetworkTargetMode::Lan => {
                format!(
                    "Switched to Local LAN Direct ({}:{})",
                    self.env_cfg.active_host(),
                    self.env_cfg.active_port()
                )
            }
            NetworkTargetMode::Tailscale => {
                format!(
                    "Switched to Tailscale Remote ({}:{})",
                    self.env_cfg.active_host(),
                    self.env_cfg.active_port()
                )
            }
            NetworkTargetMode::CloudP2P => "Switched to Cloud P2P Relay".to_string(),
        };
        self.show_toast_kind(ToastKind::Info, desc);
    }

    fn is_patrol_user_busy(&self, ctx: &egui::Context) -> bool {
        let zoomed_or_dragging = self
            .fullscreen_channel
            .and_then(|ch| self.tiles.get(&ch))
            .is_some_and(|t| {
                let vp = t.viewport(false);
                vp.zoom > 1.02 || vp.marquee_drag.is_some() || vp.box_zoom_armed
            });
        zoomed_or_dragging || ctx.input(|i| i.pointer.any_down())
    }

    fn toggle_patrol_mode(&mut self) {
        self.patrol_active = !self.patrol_active;
        self.last_patrol_switch = Instant::now();
        if self.patrol_active && self.fullscreen_channel.is_none() {
            if let Some(target_ch) = self.active_target_channel() {
                self.enter_fullscreen(target_ch);
            }
        }
        self.show_toast_kind(
            ToastKind::Info,
            if self.patrol_active {
                format!(
                    "Patrol Mode ON (cycling every {}s)",
                    self.app_cfg.patrol_interval_secs
                )
            } else {
                "Patrol Mode OFF".to_string()
            },
        );
    }

    fn toggle_aspect_mode(&mut self) {
        self.app_cfg.aspect_fit_16_9 = !self.app_cfg.aspect_fit_16_9;
        self.edit_app_cfg.aspect_fit_16_9 = self.app_cfg.aspect_fit_16_9;
        let _ = self.app_cfg.save();
        self.show_toast_kind(
            ToastKind::Info,
            if self.app_cfg.aspect_fit_16_9 {
                "Aspect mode: 16:9 Fit"
            } else {
                "Aspect mode: Fill Tile (Center-Crop)"
            },
        );
    }

    fn update_pointer_tracking(&mut self, ctx: &egui::Context) {
        let (latest_pos, any_down, raw_scroll, screen_rect) = ctx.input(|i| {
            (
                i.pointer.latest_pos(),
                i.pointer.any_down(),
                i.raw_scroll_delta,
                i.screen_rect(),
            )
        });
        let any_popup = ctx.memory(|m| m.any_popup_open())
            || ctx.is_context_menu_open()
            || self.timeline_ui.is_any_popup_open();
        let timeline_dragging = self.timeline_ui.is_actively_dragging();

        let moved = match (latest_pos, self.last_pointer_pos) {
            (Some(cur), Some(prev)) => cur.distance(prev) > 0.5,
            (Some(_), None) => true,
            _ => false,
        };
        if moved {
            self.use_hover_target = true;
        }
        if moved || any_down || raw_scroll != Vec2::ZERO {
            self.last_pointer_activity = Instant::now();
            self.last_pointer_pos = latest_pos;
        }

        if timeline_dragging {
            self.bottom_dock_peek = true;
        }

        if let Some(pos) = latest_pos {
            if pos.y <= 12.0 {
                self.top_bar_peek = true;
            } else if pos.y > 48.0 && !any_popup {
                self.top_bar_peek = false;
            }

            if pos.y >= screen_rect.max.y - 14.0 {
                self.bottom_dock_peek = true;
            } else if pos.y < screen_rect.max.y - TIMELINE_DOCK_HEIGHT - 16.0
                && !any_popup
                && !timeline_dragging
            {
                self.bottom_dock_peek = false;
            }
        } else if !any_popup && !timeline_dragging {
            self.top_bar_peek = false;
            self.bottom_dock_peek = false;
        }
    }

    fn should_show_top_bar(&self, ctx: &egui::Context) -> bool {
        let modal_blocking = self.show_settings
            || self.show_shortcuts_modal
            || self.rename_modal_channel.is_some();
        if (self.zen_mode || self.os_fullscreen) && modal_blocking {
            return false;
        }
        if self.top_bar_peek
            || ctx.memory(|m| m.any_popup_open())
            || ctx.is_context_menu_open()
        {
            return true;
        }
        if self.zen_mode {
            return false;
        }
        if self.os_fullscreen {
            return self.last_pointer_activity.elapsed() < OS_FULLSCREEN_BAR_HIDE;
        }
        true
    }

    fn should_show_playback_dock(&self, ctx: &egui::Context) -> bool {
        if !self.playback_mgr.is_active() {
            return false;
        }
        let modal_blocking = self.show_settings
            || self.show_shortcuts_modal
            || self.rename_modal_channel.is_some();
        if (self.zen_mode || self.os_fullscreen) && modal_blocking {
            return false;
        }
        let any_popup = ctx.memory(|m| m.any_popup_open())
            || ctx.is_context_menu_open()
            || self.timeline_ui.is_any_popup_open();
        let timeline_dragging = self.timeline_ui.is_actively_dragging();
        if self.zen_mode {
            return self.bottom_dock_peek || any_popup || timeline_dragging;
        }
        if self.os_fullscreen {
            return self.last_pointer_activity.elapsed() < OS_FULLSCREEN_BAR_HIDE
                || self.bottom_dock_peek
                || any_popup
                || timeline_dragging;
        }
        true
    }

    fn toggle_os_fullscreen(&mut self, ctx: &egui::Context) {
        self.os_fullscreen = !self.os_fullscreen;
        self.last_pointer_activity = Instant::now();
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.os_fullscreen));
        self.show_toast_kind(
            ToastKind::Info,
            if self.os_fullscreen {
                "OS Fullscreen ON (F11 or Esc to exit)"
            } else {
                "Exited OS Fullscreen"
            },
        );
    }

    fn handle_keyboard_shortcuts(&mut self, ctx: &egui::Context) {
        let is_pb = self.playback_mgr.is_active();

        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            let had_marquee = self.tiles.values().any(|t| {
                t.live.marquee_drag.is_some()
                    || t.live.box_zoom_armed
                    || t.pb.marquee_drag.is_some()
                    || t.pb.box_zoom_armed
            });
            if had_marquee {
                for tile in self.tiles.values_mut() {
                    tile.live.clear_transient_drag();
                    tile.pb.clear_transient_drag();
                }
                return;
            }
            if self.timeline_ui.is_actively_dragging() {
                self.timeline_ui.cancel_active_drags();
                return;
            }
            if ctx.is_context_menu_open() {
                return;
            }
            if ctx.memory(|m| m.any_popup_open()) {
                ctx.memory_mut(|m| m.close_popup());
                return;
            }
            if self.show_shortcuts_modal {
                self.show_shortcuts_modal = false;
            } else if self.rename_modal_channel.is_some() {
                self.rename_modal_channel = None;
                self.rename_just_opened = false;
            } else if self.show_settings {
                let had_dirty = self.edit_env_cfg != self.env_cfg
                    || !app_configs_equal_editable(&self.edit_app_cfg, &self.app_cfg);
                self.close_settings_modal();
                if had_dirty {
                    self.show_toast_kind(ToastKind::Info, "Closed Settings without saving");
                }
            } else if self.timeline_ui.show_time_jump_popup {
                self.timeline_ui.show_time_jump_popup = false;
            } else if self.timeline_ui.show_calendar_popup {
                self.timeline_ui.show_calendar_popup = false;
            } else if is_pb && self.timeline_ui.show_event_drawer {
                self.timeline_ui.show_event_drawer = false;
            } else if is_pb {
                let pb_ch = self.active_target_channel();
                let pb_zoomed = pb_ch
                    .and_then(|ch| self.tiles.get(&ch))
                    .is_some_and(|t| t.pb.is_zoomed());
                if let Some(ch) = pb_ch.filter(|_| pb_zoomed) {
                    if let Some(tile) = self.tiles.get_mut(&ch) {
                        tile.pb.animate_reset_zoom();
                    }
                    self.show_toast_kind(
                        ToastKind::Info,
                        format!(
                            "{} zoom -> 1.0x (Esc again for Live)",
                            self.app_cfg.camera_display_name(ch)
                        ),
                    );
                } else {
                    self.exit_playback_to_live();
                }
            } else if let Some(fs_ch) = self.fullscreen_channel {
                let fs_zoomed = self
                    .tiles
                    .get(&fs_ch)
                    .is_some_and(|t| t.live.is_zoomed());
                if fs_zoomed {
                    if let Some(tile) = self.tiles.get_mut(&fs_ch) {
                        tile.live.animate_reset_zoom();
                    }
                    self.show_toast_kind(
                        ToastKind::Info,
                        format!(
                            "{} zoom -> 1.0x (Esc again for Grid)",
                            self.app_cfg.camera_display_name(fs_ch)
                        ),
                    );
                } else {
                    self.exit_fullscreen();
                }
            } else if let Some(active_ch) = self
                .active_target_channel()
                .filter(|ch| self.tiles.get(ch).is_some_and(|t| t.live.is_zoomed()))
            {
                if let Some(tile) = self.tiles.get_mut(&active_ch) {
                    tile.live.animate_reset_zoom();
                }
                self.show_toast_kind(
                    ToastKind::Info,
                    format!(
                        "{} zoom -> 1.0x",
                        self.app_cfg.camera_display_name(active_ch)
                    ),
                );
            } else if self.zen_mode {
                self.zen_mode = false;
                self.show_toast_kind(ToastKind::Info, "Command bar visible");
            } else if self.os_fullscreen {
                self.os_fullscreen = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                self.show_toast_kind(ToastKind::Info, "Exited OS Fullscreen");
            }
            return;
        }

        // Ctrl+, toggles Settings modal
        if ctx.input(|i| (i.modifiers.ctrl || i.modifiers.command) && i.key_pressed(Key::Comma)) {
            if self.show_settings {
                self.close_settings_modal();
            } else {
                self.open_settings_modal(SettingsTab::NvrAndCameras);
            }
            return;
        }

        if self.show_settings
            || self.rename_modal_channel.is_some()
            || self.timeline_ui.is_any_popup_open()
            || ctx.memory(|m| m.any_popup_open())
            || ctx.is_context_menu_open()
            || ctx.wants_keyboard_input()
        {
            return;
        }

        let question_pressed = ctx.input(|i| {
            i.events
                .iter()
                .any(|ev| matches!(ev, egui::Event::Text(t) if t == "?"))
                || (i.modifiers.shift && i.key_pressed(Key::Slash))
        });
        if question_pressed {
            self.show_shortcuts_modal = !self.show_shortcuts_modal;
            return;
        }

        if self.show_shortcuts_modal {
            return;
        }

        let now = Instant::now();
        let dock_currently_visible = self.should_show_playback_dock(ctx);
        let mut toggle_os_fs = false;

        ctx.input(|i| {
            let mods = i.modifiers;
            let bare = !mods.ctrl && !mods.alt && !mods.command;

            // Ctrl+S or C -> Snapshot (crops zoomed region in Single View & Historical Playback)
            if ((mods.ctrl || mods.command) && i.key_pressed(Key::S))
                || (bare && !mods.shift && i.key_pressed(Key::C))
            {
                self.save_composite_wall_snapshot();
                return;
            }

            // Ctrl+E -> Export MP4 Clip when in Playback mode
            if (mods.ctrl || mods.command) && i.key_pressed(Key::E) && self.playback_mgr.is_active()
            {
                let (in_ms, out_ms) = self
                    .timeline_ui
                    .resolved_clip_range(self.playback_mgr.playhead_epoch_ms());
                self.start_current_clip_export(in_ms, out_ms);
                return;
            }

            // Ctrl+G -> Open Go-to-Timestamp Jump Popover in Playback mode
            if (mods.ctrl || mods.command) && i.key_pressed(Key::G) && self.playback_mgr.is_active()
            {
                self.timeline_ui
                    .open_time_jump_popover(self.playback_mgr.playhead_epoch_ms());
                return;
            }

            // T / TTT / Shift+T -> Interactive NVR Playback History & 2x2 Sync
            if bare && i.key_pressed(Key::T) {
                if mods.shift {
                    self.chord_state = None;
                    if self.playback_mgr.is_sync_2x2() {
                        if let Some(ch) = self.active_target_channel() {
                            self.enter_playback_single(ch, true);
                        }
                    } else {
                        self.enter_playback_sync_2x2();
                    }
                } else {
                    let taps = advance_chord(&mut self.chord_state, Key::T, now);
                    if taps >= 3 {
                        self.chord_state = None;
                        self.enter_playback_sync_2x2();
                    } else if taps == 1 {
                        if !self.playback_mgr.is_active() {
                            if let Some(ch) = self.active_target_channel() {
                                self.enter_playback_single(ch, false);
                            }
                        } else if self
                            .playback_entered_at
                            .map(|t| t.elapsed() > CHORD_WINDOW)
                            .unwrap_or(true)
                        {
                            // Keep chord_state intact so Tap 2 + Tap 3 within CHORD_WINDOW complete TTT
                            self.last_playback_exit_ms =
                                Some((self.playback_mgr.playhead_epoch_ms(), now));
                            self.exit_playback_to_live();
                        }
                    }
                }
                return;
            }

            // Backspace -> Step Back Previous Box Zoom
            if bare && !mods.shift && i.key_pressed(Key::Backspace) {
                let pb_mode = self.playback_mgr.is_active();
                if let Some(ch) = self.active_target_channel() {
                    let stepped_to = self.tiles.get_mut(&ch).and_then(|tile| {
                        let vp = tile.viewport_mut(pb_mode);
                        if vp.step_back_zoom() {
                            Some(vp.anim_target.map(|(tz, _)| tz).unwrap_or(1.0))
                        } else {
                            None
                        }
                    });
                    if let Some(tz) = stepped_to {
                        self.show_toast_kind(
                            ToastKind::Info,
                            format!(
                                "{} zoom -> {:.1}x",
                                self.app_cfg.camera_display_name(ch),
                                tz
                            ),
                        );
                    }
                }
            }

            // Z / ZZZ / Shift+Z -> Reset Zoom (independent per Live / Playback viewport)
            if bare && i.key_pressed(Key::Z) {
                if mods.shift {
                    self.chord_state = None;
                    self.reset_all_zoom();
                } else {
                    let taps = advance_chord(&mut self.chord_state, Key::Z, now);
                    if taps >= 3 {
                        self.chord_state = None;
                        self.reset_all_zoom();
                    } else if taps == 1 {
                        let pb_mode = self.playback_mgr.is_active();
                        if let Some(ch) = self.active_target_channel() {
                            if let Some(tile) = self.tiles.get_mut(&ch) {
                                tile.viewport_mut(pb_mode).animate_reset_zoom();
                            }
                            self.show_toast_kind(
                                ToastKind::Info,
                                format!(
                                    "{} zoom -> 1.0x",
                                    self.app_cfg.camera_display_name(ch)
                                ),
                            );
                        }
                    }
                }
            }

            // Keyboard Zoom In (+ / =) and Zoom Out (-):
            // Zooms timeline window if cursor is hovering the visible bottom timeline dock in Playback,
            // otherwise zooms the active camera viewport (1x - 8x) when a decoded texture is present.
            let over_timeline_dock = dock_currently_visible
                && i.pointer
                    .hover_pos()
                    .or_else(|| i.pointer.interact_pos())
                    .is_some_and(|p| p.y >= i.screen_rect().max.y - TIMELINE_DOCK_HEIGHT - 2.0);
            let plus_pressed = bare
                && (i.key_pressed(Key::Plus)
                    || i.key_pressed(Key::Equals)
                    || i.events
                        .iter()
                        .any(|ev| matches!(ev, egui::Event::Text(t) if t == "+" || t == "=")));
            if plus_pressed {
                if over_timeline_dock {
                    self.timeline_ui.zoom_around_playhead_preserving_follow(
                        1.0 / 1.5,
                        self.playback_mgr.playhead_epoch_ms(),
                    );
                } else if let Some(ch) = self.active_target_channel() {
                    let pb_mode = self.playback_mgr.is_active();
                    if let Some(tile) = self.tiles.get_mut(&ch) {
                        if tile.has_texture(pb_mode) {
                            tile.viewport_mut(pb_mode).zoom_by(1.25, Pos2::new(0.5, 0.5));
                        }
                    }
                }
            } else if bare && !mods.shift && i.key_pressed(Key::Minus) {
                if over_timeline_dock {
                    self.timeline_ui.zoom_around_playhead_preserving_follow(
                        1.5,
                        self.playback_mgr.playhead_epoch_ms(),
                    );
                } else if let Some(ch) = self.active_target_channel() {
                    let pb_mode = self.playback_mgr.is_active();
                    if let Some(tile) = self.tiles.get_mut(&ch) {
                        if tile.has_texture(pb_mode) {
                            tile.viewport_mut(pb_mode)
                                .zoom_by(1.0 / 1.25, Pos2::new(0.5, 0.5));
                        }
                    }
                }
            }

            // F11 -> True OS Monitor Fullscreen; Shift+F11 -> Camera 11
            if bare && !mods.shift && i.key_pressed(Key::F11) {
                toggle_os_fs = true;
            } else if bare && mods.shift && i.key_pressed(Key::F11) {
                self.toggle_single_view_on(11);
            }

            // Shared Aspect & Zen Shortcuts (A, B)
            if bare && !mods.shift {
                if i.key_pressed(Key::A) {
                    self.toggle_aspect_mode();
                } else if i.key_pressed(Key::B) {
                    self.zen_mode = !self.zen_mode;
                    if self.zen_mode {
                        self.timeline_ui.show_calendar_popup = false;
                        self.timeline_ui.show_time_jump_popup = false;
                    }
                    self.show_toast_kind(
                        ToastKind::Info,
                        if self.zen_mode {
                            "Zen Mode ON (Move cursor to top/bottom edge or press B)"
                        } else {
                            "Command bar visible"
                        },
                    );
                }
            }

            // Direct Camera Hotkeys: Num1..Num9, Num0, and F1..F10 / F12
            if bare && !mods.shift {
                let num_map = [
                    (Key::Num1, 1usize),
                    (Key::Num2, 2),
                    (Key::Num3, 3),
                    (Key::Num4, 4),
                    (Key::Num5, 5),
                    (Key::Num6, 6),
                    (Key::Num7, 7),
                    (Key::Num8, 8),
                    (Key::Num9, 9),
                ];
                for (k, ch) in num_map {
                    if i.key_pressed(k) {
                        self.toggle_single_view_on(ch);
                    }
                }

                if i.key_pressed(Key::Num0) {
                    let pb_mode = self.playback_mgr.is_active();
                    let target_zoomed = self
                        .active_target_channel()
                        .and_then(|ch| {
                            self.tiles
                                .get(&ch)
                                .map(|t| (ch, t.viewport(pb_mode).zoom > 1.01))
                        })
                        .unwrap_or((1, false));
                    if target_zoomed.1 {
                        if let Some(tile) = self.tiles.get_mut(&target_zoomed.0) {
                            tile.viewport_mut(pb_mode).animate_reset_zoom();
                        }
                        self.show_toast_kind(
                            ToastKind::Info,
                            format!(
                                "{} zoom -> 1.0x",
                                self.app_cfg.camera_display_name(target_zoomed.0)
                            ),
                        );
                    } else {
                        self.toggle_single_view_on(10);
                    }
                }

                let fn_map = [
                    (Key::F1, 1usize),
                    (Key::F2, 2),
                    (Key::F3, 3),
                    (Key::F4, 4),
                    (Key::F5, 5),
                    (Key::F6, 6),
                    (Key::F7, 7),
                    (Key::F8, 8),
                    (Key::F9, 9),
                    (Key::F10, 10),
                    (Key::F12, 12),
                ];
                for (k, ch) in fn_map {
                    if i.key_pressed(k) {
                        self.toggle_single_view_on(ch);
                    }
                }
            }

            // Mode-Specific Transport & Navigation Keys
            if self.playback_mgr.is_active() {
                let now_wall_ms = local_naive_to_epoch_ms(Local::now().naive_local()).max(0);
                if bare
                    && !mods.shift
                    && (i.key_pressed(Key::Space)
                        || i.key_pressed(Key::K)
                        || i.key_pressed(Key::P))
                {
                    self.playback_mgr.toggle_pause();
                } else if bare && !mods.shift && (i.key_pressed(Key::Enter) || i.key_pressed(Key::F))
                {
                    if self.playback_mgr.is_sync_2x2() {
                        if let Some(ch) = self.active_target_channel() {
                            self.enter_playback_single(ch, true);
                        }
                    } else if !self.env_cfg.is_cloud_p2p_active() && self.app_cfg.num_cameras >= 2 {
                        self.enter_playback_sync_2x2();
                    } else {
                        let ch = self.active_target_channel().unwrap_or(1);
                        self.exit_playback_to_live();
                        self.enter_fullscreen(ch);
                    }
                } else if bare && !mods.shift && i.key_pressed(Key::J) {
                    self.apply_playback_speed_with_toast(|mgr| mgr.step_speed(false), true);
                } else if bare && !mods.shift && i.key_pressed(Key::L) {
                    self.apply_playback_speed_with_toast(|mgr| mgr.step_speed(true), true);
                } else if bare && !mods.shift && i.key_pressed(Key::Comma) {
                    self.playback_mgr.step_frame(-1);
                } else if bare && !mods.shift && i.key_pressed(Key::Period) {
                    self.playback_mgr.step_frame(1);
                } else if bare && i.key_pressed(Key::ArrowLeft) {
                    let step_ms = if mods.shift { 60_000 } else { 10_000 };
                    let target = self
                        .playback_mgr
                        .playhead_epoch_ms()
                        .saturating_sub(step_ms)
                        .clamp(0, now_wall_ms);
                    self.timeline_ui.recenter_on_playhead(target);
                    self.playback_mgr.seek(target, true);
                } else if bare && i.key_pressed(Key::ArrowRight) {
                    let step_ms = if mods.shift { 60_000 } else { 10_000 };
                    let target = self
                        .playback_mgr
                        .playhead_epoch_ms()
                        .saturating_add(step_ms)
                        .clamp(0, now_wall_ms);
                    self.timeline_ui.recenter_on_playhead(target);
                    self.playback_mgr.seek(target, true);
                } else if bare && !mods.shift && i.key_pressed(Key::OpenBracket) {
                    let filt = self.timeline_ui.event_filter;
                    if let Some(ms) = self.playback_mgr.jump_adjacent_event(false, filt) {
                        self.timeline_ui.auto_scroll_to_active_event = true;
                        self.timeline_ui.recenter_on_playhead(ms);
                    } else {
                        self.show_toast_kind(ToastKind::Info, "No earlier matching event found");
                    }
                } else if bare && !mods.shift && i.key_pressed(Key::CloseBracket) {
                    let filt = self.timeline_ui.event_filter;
                    if let Some(ms) = self.playback_mgr.jump_adjacent_event(true, filt) {
                        self.timeline_ui.auto_scroll_to_active_event = true;
                        self.timeline_ui.recenter_on_playhead(ms);
                    } else {
                        self.show_toast_kind(ToastKind::Info, "No later matching event found");
                    }
                } else if bare && !mods.shift && i.key_pressed(Key::I) {
                    let ms = self.playback_mgr.playhead_epoch_ms();
                    self.timeline_ui.set_clip_in(ms);
                    let dt = epoch_ms_to_local_naive(ms);
                    self.show_toast_kind(
                        ToastKind::Info,
                        format!("Marked Clip IN at {}", dt.format("%H:%M:%S")),
                    );
                } else if bare && !mods.shift && i.key_pressed(Key::O) {
                    let ms = self.playback_mgr.playhead_epoch_ms();
                    self.timeline_ui.set_clip_out(ms);
                    let dt = epoch_ms_to_local_naive(ms);
                    self.show_toast_kind(
                        ToastKind::Info,
                        format!("Marked Clip OUT at {}", dt.format("%H:%M:%S")),
                    );
                } else if bare && !mods.shift && i.key_pressed(Key::E) {
                    self.timeline_ui.show_event_drawer = !self.timeline_ui.show_event_drawer;
                } else if bare && !mods.shift && i.key_pressed(Key::G) {
                    let mode = self.playback_mgr.cycle_smart_skip_mode();
                    self.show_toast_kind(
                        ToastKind::Info,
                        format!("Smart Skip -> {}", mode.label()),
                    );
                } else if bare && i.key_pressed(Key::R) {
                    if mods.shift {
                        self.exit_playback_to_live();
                        self.trigger_manual_resync();
                        self.show_toast_kind(
                            ToastKind::Info,
                            "Flushed all buffers -> Locked to live edge",
                        );
                    } else {
                        self.refresh_active_playback_timeline();
                    }
                }
                return;
            }

            // Live Mode Only: S / SSS / Shift+S -> Stream SD
            if bare && i.key_pressed(Key::S) {
                if mods.shift {
                    self.chord_state = None;
                    self.set_all_cameras_subtype(1, "All cameras -> SD (Sub-Stream)");
                } else {
                    let taps = advance_chord(&mut self.chord_state, Key::S, now);
                    if taps >= 3 {
                        self.chord_state = None;
                        self.set_all_cameras_subtype(1, "All cameras -> SD (sss)");
                    } else if taps == 1 {
                        if let Some(ch) = self.active_target_channel() {
                            self.set_camera_subtype(ch, 1);
                        }
                    }
                }
            }

            // Live Mode Only: H / HHH / Shift+H -> Stream HD
            if bare && i.key_pressed(Key::H) {
                if mods.shift {
                    self.chord_state = None;
                    self.set_all_cameras_subtype(0, "All cameras -> HD (Main-Stream)");
                } else {
                    let taps = advance_chord(&mut self.chord_state, Key::H, now);
                    if taps >= 3 {
                        self.chord_state = None;
                        self.set_all_cameras_subtype(0, "All cameras -> HD (hhh)");
                    } else if taps == 1 {
                        if let Some(ch) = self.active_target_channel() {
                            self.set_camera_subtype(ch, 0);
                        }
                    }
                }
            }

            // Live Navigation & Single View
            if self.fullscreen_channel.is_some() {
                if i.key_pressed(Key::ArrowRight)
                    || i.key_pressed(Key::ArrowDown)
                    || (bare && i.key_pressed(Key::CloseBracket))
                {
                    self.cycle_fullscreen(1);
                } else if i.key_pressed(Key::ArrowLeft)
                    || i.key_pressed(Key::ArrowUp)
                    || (bare && i.key_pressed(Key::OpenBracket))
                {
                    self.cycle_fullscreen(-1);
                }
            } else {
                let (_, cols) = self.app_cfg.grid_dimensions();
                if i.key_pressed(Key::ArrowLeft) {
                    self.use_hover_target = false;
                    self.selected_channel = move_grid_selection(
                        &self.app_cfg.channel_order,
                        cols,
                        self.selected_channel,
                        0,
                        -1,
                    );
                } else if i.key_pressed(Key::ArrowRight) {
                    self.use_hover_target = false;
                    self.selected_channel = move_grid_selection(
                        &self.app_cfg.channel_order,
                        cols,
                        self.selected_channel,
                        0,
                        1,
                    );
                } else if i.key_pressed(Key::ArrowUp) {
                    self.use_hover_target = false;
                    self.selected_channel = move_grid_selection(
                        &self.app_cfg.channel_order,
                        cols,
                        self.selected_channel,
                        -1,
                        0,
                    );
                } else if i.key_pressed(Key::ArrowDown) {
                    self.use_hover_target = false;
                    self.selected_channel = move_grid_selection(
                        &self.app_cfg.channel_order,
                        cols,
                        self.selected_channel,
                        1,
                        0,
                    );
                } else if bare && i.key_pressed(Key::OpenBracket) {
                    self.use_hover_target = false;
                    let len = self.app_cfg.channel_order.len();
                    if len > 0 {
                        let cur_idx = self
                            .selected_channel
                            .and_then(|ch| self.app_cfg.channel_order.iter().position(|&c| c == ch))
                            .unwrap_or(0);
                        let prev_idx = (cur_idx as isize - 1).rem_euclid(len as isize) as usize;
                        self.selected_channel = Some(self.app_cfg.channel_order[prev_idx]);
                    }
                } else if bare && i.key_pressed(Key::CloseBracket) {
                    self.use_hover_target = false;
                    let len = self.app_cfg.channel_order.len();
                    if len > 0 {
                        let cur_idx = self
                            .selected_channel
                            .and_then(|ch| self.app_cfg.channel_order.iter().position(|&c| c == ch))
                            .unwrap_or(0);
                        let next_idx = (cur_idx + 1) % len;
                        self.selected_channel = Some(self.app_cfg.channel_order[next_idx]);
                    }
                }
            }

            if bare && (i.key_pressed(Key::Enter) || i.key_pressed(Key::F)) {
                if self.fullscreen_channel.is_some() {
                    self.exit_fullscreen();
                } else if let Some(ch) = self.active_target_channel() {
                    self.enter_fullscreen(ch);
                }
            }

            // Live Wall Controls: M, P/K/Space, R/Shift+R
            if bare && !mods.shift {
                if i.key_pressed(Key::M) {
                    self.toggle_patrol_mode();
                } else if i.key_pressed(Key::P)
                    || i.key_pressed(Key::K)
                    || i.key_pressed(Key::Space)
                {
                    self.all_paused = !self.all_paused;
                    self.stream_mgr.set_all_paused(self.all_paused);
                    self.show_toast_kind(
                        if self.all_paused {
                            ToastKind::Warning
                        } else {
                            ToastKind::Info
                        },
                        if self.all_paused {
                            "Paused all live streams"
                        } else {
                            "Resumed all live streams"
                        },
                    );
                } else if i.key_pressed(Key::R) {
                    self.trigger_manual_resync();
                    self.show_toast_kind(
                        ToastKind::Info,
                        "Re-synced all camera streams to live edge",
                    );
                }
            } else if bare && mods.shift && i.key_pressed(Key::R) {
                self.trigger_manual_resync();
                self.show_toast_kind(
                    ToastKind::Info,
                    "Flushed all buffers -> Locked to live edge",
                );
            }
        });

        if toggle_os_fs {
            self.toggle_os_fullscreen(ctx);
        }
    }

    fn update_gpu_textures(&mut self, ctx: &egui::Context) {
        // 1. Upload Live Stream Textures (including the primary channel in 1-Cam Playback for Live PiP!)
        let visible_live_channels: Vec<usize> = if self.playback_mgr.is_active() {
            if !self.playback_mgr.is_sync_2x2() && !self.env_cfg.is_cloud_p2p_active() {
                self.playback_mgr.primary_channel().into_iter().collect()
            } else {
                Vec::new()
            }
        } else if let Some(fs_ch) = self.fullscreen_channel {
            vec![fs_ch]
        } else {
            self.app_cfg.channel_order.clone()
        };

        for ch in visible_live_channels {
            let Some(slot) = self.stream_mgr.slot(ch) else {
                continue;
            };
            let seq = slot.frame_seq.load(Ordering::Acquire);
            let tile = self.tiles.entry(ch).or_default();

            if seq > 0 && seq != tile.last_uploaded_seq {
                let dims = if let Ok(guard) = slot.buffer.lock() {
                    let w = guard.width as usize;
                    let h = guard.height as usize;
                    if w > 0 && h > 0 && guard.rgba.len() == w * h * 4 {
                        self.upload_scratch.clear();
                        self.upload_scratch.extend_from_slice(&guard.rgba);
                        Some((guard.width, guard.height))
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some((tex_w, tex_h)) = dims {
                    let color_img = ColorImage::from_rgba_unmultiplied(
                        [tex_w as usize, tex_h as usize],
                        &self.upload_scratch,
                    );
                    match &mut tile.texture {
                        Some(tex) if tile.tex_width == tex_w && tile.tex_height == tex_h => {
                            tex.set(color_img, TextureOptions::LINEAR);
                        }
                        _ => {
                            tile.texture = Some(ctx.load_texture(
                                format!("cam_tex_{}", ch),
                                color_img,
                                TextureOptions::LINEAR,
                            ));
                            tile.tex_width = tex_w;
                            tile.tex_height = tex_h;
                        }
                    }
                    tile.last_uploaded_seq = seq;
                }
            }
        }

        // 2. Upload Historical Playback Textures (`1920x1080` 1-Cam or `960x540` 2x2 Sync)
        if self.playback_mgr.is_active() {
            let pb_channels = self.playback_mgr.active_channels().to_vec();
            for ch in pb_channels {
                let Some(pb_slot) = self.playback_mgr.slot(ch) else {
                    continue;
                };
                let seq = pb_slot.frame_seq.load(Ordering::Acquire);
                let tile = self.tiles.entry(ch).or_default();

                if seq > 0 && seq != tile.pb_last_uploaded_seq {
                    let dims = if let Ok(guard) = pb_slot.buffer.lock() {
                        let w = guard.width as usize;
                        let h = guard.height as usize;
                        if w > 0 && h > 0 && guard.rgba.len() == w * h * 4 {
                            self.upload_scratch.clear();
                            self.upload_scratch.extend_from_slice(&guard.rgba);
                            Some((guard.width, guard.height))
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    if let Some((tex_w, tex_h)) = dims {
                        let color_img = ColorImage::from_rgba_unmultiplied(
                            [tex_w as usize, tex_h as usize],
                            &self.upload_scratch,
                        );
                        match &mut tile.pb_texture {
                            Some(tex)
                                if tile.pb_tex_width == tex_w && tile.pb_tex_height == tex_h =>
                            {
                                tex.set(color_img, TextureOptions::LINEAR);
                            }
                            _ => {
                                tile.pb_texture = Some(ctx.load_texture(
                                    format!("pb_tex_{}", ch),
                                    color_img,
                                    TextureOptions::LINEAR,
                                ));
                                tile.pb_tex_width = tex_w;
                                tile.pb_tex_height = tex_h;
                            }
                        }
                        tile.pb_last_uploaded_seq = seq;
                    }
                }
            }
        }
    }

    fn render_grid_layout_menu_items(&mut self, ui: &mut Ui, in_history: bool) {
        let n = self.app_cfg.num_cameras.max(1);
        let eff = self.app_cfg.effective_layout_mode();
        let presets: [(&str, &str, bool, &str); 6] = [
            ("auto", "Grid Auto (Smart)", true, ""),
            ("2x2", "Grid 2x2", n <= 4, "max 4 cams"),
            ("3x3", "Grid 3x3", n <= 9, "max 9 cams"),
            ("3x4", "Grid 3x4", n <= 12, "max 12 cams"),
            ("4x3", "Grid 4x3", n <= 12, "max 12 cams"),
            ("2x6", "Grid 2x6 (Wide)", n <= 12, "max 12 cams"),
        ];
        for (mode_val, label, enabled, note) in presets {
            let selected =
                !in_history && eff == mode_val && self.fullscreen_channel.is_none();
            if enabled {
                if ui.selectable_label(selected, label).clicked() {
                    if self.playback_mgr.is_active() {
                        self.exit_playback_to_live();
                    }
                    self.exit_fullscreen();
                    self.app_cfg.layout_mode = mode_val.to_string();
                    self.edit_app_cfg.layout_mode = mode_val.to_string();
                    let _ = self.app_cfg.save();
                    ui.close_menu();
                    ui.memory_mut(|mem| mem.close_popup());
                }
            } else {
                ui.add_enabled(
                    false,
                    egui::SelectableLabel::new(
                        false,
                        RichText::new(format!("{} ({})", label, note)).color(TEXT_MUTED),
                    ),
                );
            }
        }
    }

    fn render_top_command_bar(&mut self, ui: &mut Ui) {
        let total = self.app_cfg.num_cameras;
        let in_history = self.playback_mgr.is_active();
        let is_cloud_active = self.env_cfg.is_cloud_p2p_active();
        let is_tailscale_active = self.env_cfg.is_tailscale_active();

        let mut live_count = 0usize;
        let mut total_drained = 0u64;
        let mut sd_count = 0usize;
        let mut hd_count = 0usize;

        for ch in 1..=total {
            if let Some(slot) = self.stream_mgr.slot(ch) {
                if slot.status() == StreamStatus::Live {
                    live_count += 1;
                }
                total_drained += slot.drained_burst_frames.load(Ordering::Relaxed);
                if slot.target_subtype.load(Ordering::Relaxed) == 0 {
                    hd_count += 1;
                } else {
                    sd_count += 1;
                }
            }
        }

        let all_sd = total > 0 && sd_count == total;
        let all_hd = total > 0 && hd_count == total;
        let mixed_sd_hd = total > 0 && !all_sd && !all_hd;
        let bar_rect = ui.max_rect();
        let show_direct_window_pills = bar_rect.width() >= 1220.0 && !self.zen_mode;

        let patrol_label = if self.patrol_active && !in_history {
            if self.is_patrol_user_busy(ui.ctx()) {
                "Patrol (Hold)".to_string()
            } else {
                let interval = self.app_cfg.patrol_interval_secs.max(3);
                let elapsed = self.last_patrol_switch.elapsed().as_secs();
                let rem = interval.saturating_sub(elapsed).max(1);
                format!("Patrol ({}s)", rem)
            }
        } else {
            "Patrol".to_string()
        };

        // 1. Left Zone: Geometric NVR mark + Wordmark + Calm Status/Network Switcher Capsule
        let mut left_end_x = bar_rect.min.x;
        ui.allocate_new_ui(
            egui::UiBuilder::new()
                .max_rect(bar_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;

                // Minimal geometric 2x2 grid/camera emblem + NVR wordmark
                let (brand_rect, _) =
                    ui.allocate_exact_size(Vec2::new(52.0, 24.0), Sense::hover());
                {
                    let bp = ui.painter();
                    let icon_origin = Pos2::new(brand_rect.min.x, brand_rect.center().y - 7.0);
                    for r in 0..2 {
                        for c in 0..2 {
                            let cell = Rect::from_min_size(
                                Pos2::new(
                                    icon_origin.x + c as f32 * 7.5,
                                    icon_origin.y + r as f32 * 7.5,
                                ),
                                Vec2::new(5.8, 5.8),
                            );
                            let col = if r == 0 && c == 1 {
                                if in_history {
                                    STATUS_AMBER
                                } else {
                                    STATUS_ICE
                                }
                            } else {
                                Color32::from_rgb(228, 228, 231)
                            };
                            bp.rect_filled(cell, Rounding::same(1.5), col);
                        }
                    }
                    bp.text(
                        Pos2::new(brand_rect.min.x + 19.0, brand_rect.center().y),
                        Align2::LEFT_CENTER,
                        "NVR",
                        FontId::new(13.5, FontFamily::Proportional),
                        TEXT_PRIMARY,
                    );
                }

                // Status + Network Switcher Capsule
                let net_short = if is_cloud_active {
                    "Cloud P2P"
                } else if is_tailscale_active {
                    "Tailscale"
                } else {
                    "LAN"
                };

                let (status_dot, capsule_text) = if in_history {
                    (STATUS_AMBER, format!("HIST  {} v", net_short))
                } else if self.all_paused {
                    (STATUS_AMBER, format!("PAUSED  {} v", net_short))
                } else {
                    let dot = if live_count == 0 {
                        STATUS_CORAL
                    } else if is_cloud_active {
                        STATUS_AMBER
                    } else if live_count == total && total > 0 {
                        STATUS_EMERALD
                    } else {
                        STATUS_AMBER
                    };
                    (dot, format!("{}/{}  {} v", live_count, total, net_short))
                };

                let cap_font = FontId::new(12.0, FontFamily::Proportional);
                let cap_text_col = if is_cloud_active {
                    Color32::from_rgb(254, 243, 199)
                } else {
                    TEXT_PRIMARY
                };
                let cap_galley =
                    ui.painter()
                        .layout_no_wrap(capsule_text.clone(), cap_font.clone(), cap_text_col);
                let text_w = (cap_galley.size().x + 28.0).max(68.0);
                let (cap_rect, cap_resp) =
                    ui.allocate_exact_size(Vec2::new(text_w, 26.0), Sense::click());
                if cap_resp.hovered() {
                    ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                }
                let net_popup_id = ui.make_persistent_id("obsidian_net_mode_popup");
                let popup_is_open = ui.memory(|m| m.is_popup_open(net_popup_id));

                let cap_fill = if is_cloud_active {
                    Color32::from_rgba_unmultiplied(
                        245,
                        158,
                        11,
                        if cap_resp.hovered() || popup_is_open { 44 } else { 26 },
                    )
                } else if cap_resp.hovered() || popup_is_open {
                    ZINC_HOVER
                } else {
                    ZINC_ELEVATED
                };
                let cap_stroke = if is_cloud_active {
                    STATUS_AMBER
                } else if popup_is_open {
                    STATUS_ICE
                } else if cap_resp.hovered() {
                    Color32::from_rgba_unmultiplied(255, 255, 255, 45)
                } else {
                    hairline_stroke()
                };
                {
                    let cp = ui.painter();
                    cp.rect_filled(cap_rect, Rounding::same(7.0), cap_fill);
                    cp.rect_stroke(
                        cap_rect,
                        Rounding::same(7.0),
                        Stroke::new(1.0_f32, cap_stroke),
                    );
                    cp.circle_filled(
                        Pos2::new(cap_rect.min.x + 10.0, cap_rect.center().y),
                        3.5,
                        status_dot,
                    );
                    cp.galley(
                        Pos2::new(
                            cap_rect.min.x + 19.0,
                            cap_rect.center().y - cap_galley.size().y * 0.5,
                        ),
                        cap_galley,
                        cap_text_col,
                    );
                }

                if cap_resp.clicked() {
                    ui.memory_mut(|mem| mem.toggle_popup(net_popup_id));
                }

                egui::popup_below_widget(
                    ui,
                    net_popup_id,
                    &cap_resp,
                    egui::PopupCloseBehavior::CloseOnClickOutside,
                    |ui| {
                        ui.set_min_width(248.0);
                        let ctx_clone = ui.ctx().clone();
                        let lan_host_lbl = if self.env_cfg.host.trim().is_empty() {
                            "Configure...".to_string()
                        } else {
                            format!("{}:{}", self.env_cfg.host.trim(), self.env_cfg.port)
                        };
                        let ts_lbl = if self.env_cfg.tailscale_host.trim().is_empty() {
                            "Configure...".to_string()
                        } else {
                            format!(
                                "{}:{}",
                                self.env_cfg.tailscale_host.trim(),
                                self.env_cfg.port
                            )
                        };
                        let p2p_lbl = if self.env_cfg.p2p_serial.trim().is_empty() {
                            "Configure...".to_string()
                        } else {
                            self.env_cfg.p2p_serial.trim().to_string()
                        };

                        if ui
                            .selectable_label(
                                !is_cloud_active && !is_tailscale_active,
                                format!("Local LAN Direct ({})", lan_host_lbl),
                            )
                            .clicked()
                        {
                            ui.memory_mut(|mem| mem.close_popup());
                            self.set_network_mode(&ctx_clone, NetworkTargetMode::Lan);
                        }
                        if ui
                            .selectable_label(
                                is_tailscale_active,
                                format!("Tailscale Remote ({})", ts_lbl),
                            )
                            .clicked()
                        {
                            ui.memory_mut(|mem| mem.close_popup());
                            self.set_network_mode(&ctx_clone, NetworkTargetMode::Tailscale);
                        }
                        if ui
                            .selectable_label(
                                is_cloud_active,
                                format!("Cloud P2P Relay ({})", p2p_lbl),
                            )
                            .clicked()
                        {
                            ui.memory_mut(|mem| mem.close_popup());
                            self.set_network_mode(&ctx_clone, NetworkTargetMode::CloudP2P);
                        }
                        ui.separator();
                        if ui
                            .button("Test Connection / Network Settings...")
                            .clicked()
                        {
                            ui.memory_mut(|mem| mem.close_popup());
                            self.open_settings_modal(SettingsTab::NvrAndCameras);
                            if !self.env_cfg.password.trim().is_empty() {
                                self.start_test_connection();
                            }
                        }
                    },
                );

                let net_detail = if is_cloud_active {
                    format!(
                        "Cloud P2P External Relay ({}) - RTSP :{} / HTTP :{}",
                        self.p2p_mgr.status_text(),
                        self.env_cfg.active_port(),
                        self.env_cfg.active_http_port()
                    )
                } else if is_tailscale_active {
                    format!(
                        "Tailscale Remote - {}:{}",
                        self.env_cfg.active_host(),
                        self.env_cfg.active_port()
                    )
                } else {
                    format!(
                        "Local LAN Direct - {}:{}",
                        self.env_cfg.active_host(),
                        self.env_cfg.active_port()
                    )
                };
                let tooltip = format!(
                    "{}\nLive Cameras: {}/{}  |  AuthGateway: 127.0.0.1:{}\nBurst-Drained Frames: {}  |  RAM: {:.0} MB\n\nClick to choose LAN / Tailscale / Cloud P2P",
                    net_detail,
                    live_count,
                    total,
                    self.auth_gw.local_port(),
                    total_drained,
                    self.rss_mb
                );
                cap_resp.on_hover_text(tooltip);

                left_end_x = ui.min_rect().max.x;
            },
        );

        // 2. Right Zone: Quiet Ghost Pills (Settings, ?, Zen/Full or More v, Snap, Sync, Pause/Resume)
        let mut right_start_x = bar_rect.max.x;
        let mut overflow_anchor_resp: Option<egui::Response> = None;
        let overflow_popup_id = ui.make_persistent_id("obsidian_top_overflow_popup");

        ui.allocate_new_ui(
            egui::UiBuilder::new()
                .max_rect(bar_rect)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
            |ui| {
                ui.spacing_mut().item_spacing.x = 5.0;

                if self.zen_mode
                    && draw_pill_button(ui, "Exit Zen", true, PillAccent::Amber, false)
                        .on_hover_text("Restore persistent Top Bar & Dock (B / Esc)")
                        .clicked()
                {
                    self.zen_mode = false;
                    self.show_toast_kind(ToastKind::Info, "Command bar visible");
                }

                if draw_pill_button(
                    ui,
                    "Settings",
                    self.show_settings,
                    PillAccent::Ice,
                    true,
                )
                .on_hover_text("NVR, Camera & Performance Settings (Ctrl+,)")
                .clicked()
                {
                    if self.show_settings {
                        self.close_settings_modal();
                    } else {
                        self.open_settings_modal(SettingsTab::NvrAndCameras);
                    }
                }

                if draw_pill_button(
                    ui,
                    "?",
                    self.show_shortcuts_modal,
                    PillAccent::Ice,
                    true,
                )
                .on_hover_text("Keyboard Shortcuts Cheat-Sheet (?)")
                .clicked()
                {
                    self.show_shortcuts_modal = !self.show_shortcuts_modal;
                }

                if show_direct_window_pills {
                    let ctx_clone = ui.ctx().clone();
                    if draw_pill_button(
                        ui,
                        "Full",
                        self.os_fullscreen,
                        PillAccent::Ice,
                        true,
                    )
                    .on_hover_text("Toggle OS Monitor Fullscreen (F11)")
                    .clicked()
                    {
                        self.toggle_os_fullscreen(&ctx_clone);
                    }

                    if draw_pill_button(ui, "Zen", self.zen_mode, PillAccent::Ice, true)
                        .on_hover_text("Toggle Borderless Zen Wall with edge peek (B)")
                        .clicked()
                    {
                        self.zen_mode = !self.zen_mode;
                        if self.zen_mode {
                            self.timeline_ui.show_calendar_popup = false;
                            self.timeline_ui.show_time_jump_popup = false;
                        }
                        self.show_toast_kind(
                            ToastKind::Info,
                            if self.zen_mode {
                                "Zen Mode ON (Move cursor to top/bottom edge or press B)"
                            } else {
                                "Command bar visible"
                            },
                        );
                    }
                } else {
                    let ov_open = ui.memory(|m| m.is_popup_open(overflow_popup_id));
                    let resp = draw_pill_button(ui, "More v", ov_open, PillAccent::Ice, true)
                        .on_hover_text("Layout, Quality, Aspect, Patrol, Zen & Fullscreen controls");
                    if resp.clicked() {
                        ui.memory_mut(|mem| mem.toggle_popup(overflow_popup_id));
                    }
                    overflow_anchor_resp = Some(resp);
                }

                if draw_pill_button(ui, "Snap", false, PillAccent::Ice, true)
                    .on_hover_text(
                        "Save PNG Snapshot - composite wall in Grid, or full/zoomed frame in Single & History (C / Ctrl+S)",
                    )
                    .clicked()
                {
                    self.save_composite_wall_snapshot();
                }

                if draw_pill_button(ui, "Sync", false, PillAccent::Ice, true)
                    .on_hover_text(
                        "Flush buffers & re-sync all cameras to live edge (R in Live / Shift+R in History)",
                    )
                    .clicked()
                {
                    if self.playback_mgr.is_active() {
                        self.exit_playback_to_live();
                    }
                    self.trigger_manual_resync();
                    self.show_toast_kind(
                        ToastKind::Info,
                        "Flushed all buffers -> Locked to live edge",
                    );
                }

                let paused_state = if in_history {
                    self.playback_mgr.is_paused()
                } else {
                    self.all_paused
                };
                let pause_label = if paused_state { "Resume" } else { "Pause" };
                if draw_pill_button(
                    ui,
                    pause_label,
                    paused_state,
                    PillAccent::Amber,
                    !paused_state,
                )
                .on_hover_text("Pause / Resume playback or live wall (Space / K / P)")
                .clicked()
                {
                    if in_history {
                        self.playback_mgr.toggle_pause();
                    } else {
                        self.all_paused = !self.all_paused;
                        self.stream_mgr.set_all_paused(self.all_paused);
                        self.show_toast_kind(
                            if self.all_paused {
                                ToastKind::Warning
                            } else {
                                ToastKind::Info
                            },
                            if self.all_paused {
                                "Paused all live streams"
                            } else {
                                "Resumed all live streams"
                            },
                        );
                    }
                }

                right_start_x = ui.min_rect().min.x;
            },
        );

        // 3. Center Zone: Clean Segmented Capsules
        let avail_min_x = (left_end_x + 10.0).min(bar_rect.max.x);
        let avail_max_x = (right_start_x - 10.0).max(avail_min_x);
        let avail_center_w = (avail_max_x - avail_min_x).max(0.0);
        let is_fullscreen_live = !in_history && self.fullscreen_channel.is_some();
        let (show_grid_capsule, show_sd_hd_capsule, show_aspect_capsule, show_patrol_pill) =
            select_top_bar_center_capsules(avail_center_w, in_history, is_fullscreen_live);

        // Render responsive `More v` overflow popup when on narrower viewports
        if let Some(ref anchor_resp) = overflow_anchor_resp {
            egui::popup_below_widget(
                ui,
                overflow_popup_id,
                anchor_resp,
                egui::PopupCloseBehavior::CloseOnClickOutside,
                |ui| {
                    ui.set_min_width(210.0);
                    let ctx_clone = ui.ctx().clone();

                    if !show_grid_capsule {
                        if in_history {
                            if !is_cloud_active && total >= 2 {
                                if ui
                                    .selectable_label(
                                        !self.playback_mgr.is_sync_2x2(),
                                        "1-Cam 1080p Playback",
                                    )
                                    .clicked()
                                {
                                    ui.memory_mut(|mem| mem.close_popup());
                                    let ch = self.active_target_channel().unwrap_or(1);
                                    self.enter_playback_single(ch, true);
                                }
                                if ui
                                    .selectable_label(
                                        self.playback_mgr.is_sync_2x2(),
                                        "2x2 Sync Playback (TTT)",
                                    )
                                    .clicked()
                                {
                                    ui.memory_mut(|mem| mem.close_popup());
                                    self.enter_playback_sync_2x2();
                                }
                                ui.separator();
                            }
                        } else {
                            self.render_grid_layout_menu_items(ui, in_history);
                            if ui
                                .selectable_label(
                                    self.fullscreen_channel.is_some(),
                                    "Single Camera View (Enter)",
                                )
                                .clicked()
                            {
                                ui.memory_mut(|mem| mem.close_popup());
                                if self.fullscreen_channel.is_some() {
                                    self.exit_fullscreen();
                                } else if let Some(ch) = self.active_target_channel() {
                                    self.enter_fullscreen(ch);
                                }
                            }
                            if self.fullscreen_channel.is_some() {
                                ui.horizontal(|ui| {
                                    if ui.button("< Prev Cam").clicked() {
                                        self.cycle_fullscreen(-1);
                                    }
                                    if ui.button("Next Cam >").clicked() {
                                        self.cycle_fullscreen(1);
                                    }
                                });
                            }
                            ui.separator();
                        }
                    }

                    if !in_history && !show_sd_hd_capsule {
                        let sd_lbl = if mixed_sd_hd {
                            format!("All SD Sub-Stream ({} SD / {} HD)", sd_count, hd_count)
                        } else {
                            "All Cameras -> SD Sub-Stream".to_string()
                        };
                        if ui.selectable_label(all_sd, sd_lbl).clicked() {
                            ui.memory_mut(|mem| mem.close_popup());
                            self.set_all_cameras_subtype(1, "All cameras -> SD (Sub-Stream)");
                        }
                        if ui
                            .selectable_label(all_hd, "All Cameras -> HD Main-Stream")
                            .clicked()
                        {
                            ui.memory_mut(|mem| mem.close_popup());
                            self.set_all_cameras_subtype(0, "All cameras -> HD (Main-Stream)");
                        }
                        ui.separator();
                    }

                    if !show_aspect_capsule {
                        if ui
                            .selectable_label(self.app_cfg.aspect_fit_16_9, "Aspect: 16:9 Fit (A)")
                            .clicked()
                            && !self.app_cfg.aspect_fit_16_9
                        {
                            ui.memory_mut(|mem| mem.close_popup());
                            self.toggle_aspect_mode();
                        }
                        if ui
                            .selectable_label(
                                !self.app_cfg.aspect_fit_16_9,
                                "Aspect: Fill Tile (A)",
                            )
                            .clicked()
                            && self.app_cfg.aspect_fit_16_9
                        {
                            ui.memory_mut(|mem| mem.close_popup());
                            self.toggle_aspect_mode();
                        }
                        ui.separator();
                    }

                    if !in_history && !show_patrol_pill {
                        if ui
                            .selectable_label(
                                self.patrol_active,
                                format!("{} (M)", patrol_label),
                            )
                            .clicked()
                        {
                            ui.memory_mut(|mem| mem.close_popup());
                            self.toggle_patrol_mode();
                        }
                        ui.separator();
                    }

                    if ui
                        .selectable_label(self.zen_mode, "Zen Borderless Wall (B)")
                        .clicked()
                    {
                        ui.memory_mut(|mem| mem.close_popup());
                        self.zen_mode = !self.zen_mode;
                        if self.zen_mode {
                            self.timeline_ui.show_calendar_popup = false;
                            self.timeline_ui.show_time_jump_popup = false;
                        }
                        self.show_toast_kind(
                            ToastKind::Info,
                            if self.zen_mode {
                                "Zen Mode ON (Move cursor to top/bottom edge or press B)"
                            } else {
                                "Command bar visible"
                            },
                        );
                    }
                    if ui
                        .selectable_label(self.os_fullscreen, "OS Fullscreen (F11)")
                        .clicked()
                    {
                        ui.memory_mut(|mem| mem.close_popup());
                        self.toggle_os_fullscreen(&ctx_clone);
                    }
                },
            );
        }

        let est_center_w = self.last_center_width.min(avail_center_w);
        let ideal_min_x = bar_rect.center().x - est_center_w * 0.5;
        let center_start_x = ideal_min_x.clamp(
            avail_min_x,
            (avail_max_x - est_center_w).max(avail_min_x),
        );
        let center_rect = Rect::from_min_max(
            Pos2::new(center_start_x, bar_rect.min.y),
            Pos2::new(avail_max_x, bar_rect.max.y),
        );

        ui.allocate_new_ui(
            egui::UiBuilder::new()
                .max_rect(center_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;

                // Primary Mode Switcher: [ Live | History ] (always preserved first)
                draw_segmented_group(ui, false, |ui| {
                    if draw_segment_item(ui, "Live", !in_history, PillAccent::Emerald)
                        .on_hover_text("Real-time Live Camera Wall (T / Esc)")
                        .clicked()
                        && in_history
                    {
                        self.exit_playback_to_live();
                    }
                    if draw_segment_item(ui, "History", in_history, PillAccent::Amber)
                        .on_hover_text(
                            "Interactive 1080p NVR Playback + AI Event Timeline + Live PiP (T / TTT for 2x2 Sync)",
                        )
                        .clicked()
                        && !in_history
                    {
                        if let Some(ch) = self.active_target_channel() {
                            self.enter_playback_single(ch, false);
                        }
                    }
                });

                // View & Grid Layout Capsule (History-relevant [1-Cam 1080p | 2x2 Sync] vs Live [Grid | Single])
                if show_grid_capsule {
                    if in_history {
                        if !is_cloud_active && total >= 2 {
                            draw_segmented_group(ui, false, |ui| {
                                let is_sync = self.playback_mgr.is_sync_2x2();
                                if draw_segment_item(
                                    ui,
                                    "1-Cam 1080p",
                                    !is_sync,
                                    PillAccent::Amber,
                                )
                                .on_hover_text("Single-camera 1080p playback with Live PiP (Enter)")
                                .clicked()
                                    && is_sync
                                {
                                    let ch = self.active_target_channel().unwrap_or(1);
                                    self.enter_playback_single(ch, true);
                                }
                                if draw_segment_item(ui, "2x2 Sync", is_sync, PillAccent::Amber)
                                    .on_hover_text(
                                        "4-camera 540p synchronized playback wall (TTT / Shift+T)",
                                    )
                                    .clicked()
                                    && !is_sync
                                {
                                    self.enter_playback_sync_2x2();
                                }
                            });
                        }
                    } else {
                        draw_segmented_group(ui, false, |ui| {
                            let mode_display = match self.app_cfg.effective_layout_mode() {
                                "2x2" => "2x2",
                                "3x3" => "3x3",
                                "3x4" => "3x4",
                                "4x3" => "4x3",
                                "2x6" => "2x6",
                                _ => "Auto",
                            };
                            let grid_label = format!("Grid {} v", mode_display);
                            let popup_id = ui.make_persistent_id("obsidian_grid_layout_popup");
                            let grid_resp = draw_segment_item(
                                ui,
                                &grid_label,
                                self.fullscreen_channel.is_none(),
                                PillAccent::Ice,
                            );
                            if grid_resp.clicked() {
                                ui.memory_mut(|mem| mem.toggle_popup(popup_id));
                            }

                            egui::popup_below_widget(
                                ui,
                                popup_id,
                                &grid_resp,
                                egui::PopupCloseBehavior::CloseOnClickOutside,
                                |ui| {
                                    ui.set_min_width(148.0);
                                    self.render_grid_layout_menu_items(ui, false);
                                },
                            );
                            grid_resp.on_hover_text(
                                "Select live grid layout (Auto, 2x2, 3x3, 3x4, 4x3, 2x6)",
                            );

                            if draw_segment_item(
                                ui,
                                "Single",
                                self.fullscreen_channel.is_some(),
                                PillAccent::Ice,
                            )
                            .on_hover_text("Toggle Single Camera Live View (Enter / F)")
                            .clicked()
                            {
                                if self.fullscreen_channel.is_some() {
                                    self.exit_fullscreen();
                                } else if let Some(ch) = self.active_target_channel() {
                                    self.enter_fullscreen(ch);
                                }
                            }

                            if self.fullscreen_channel.is_some() {
                                if draw_segment_item(ui, "<", false, PillAccent::Ice)
                                    .on_hover_text("Previous Camera (Left / [)")
                                    .clicked()
                                {
                                    self.cycle_fullscreen(-1);
                                }
                                if draw_segment_item(ui, ">", false, PillAccent::Ice)
                                    .on_hover_text("Next Camera (Right / ])")
                                    .clicked()
                                {
                                    self.cycle_fullscreen(1);
                                }
                            }
                        });
                    }
                }

                if show_sd_hd_capsule {
                    let mixed_tip_prefix = if mixed_sd_hd {
                        format!("Mixed: {} HD / {} SD\n", hd_count, sd_count)
                    } else {
                        String::new()
                    };
                    draw_segmented_group(ui, mixed_sd_hd, |ui| {
                        if draw_segment_item(ui, "SD", all_sd, PillAccent::Ice)
                            .on_hover_text(format!(
                                "{}Switch all cameras to low-bandwidth SD Sub-Stream (sss / Shift+S)",
                                mixed_tip_prefix
                            ))
                            .clicked()
                        {
                            self.set_all_cameras_subtype(1, "All cameras -> SD (Sub-Stream)");
                        }
                        if draw_segment_item(ui, "HD", all_hd, PillAccent::Ice)
                            .on_hover_text(format!(
                                "{}Switch all cameras to 720p/1080p HD Main-Stream (hhh / Shift+H)",
                                mixed_tip_prefix
                            ))
                            .clicked()
                        {
                            self.set_all_cameras_subtype(0, "All cameras -> HD (Main-Stream)");
                        }
                    });
                }

                // Aspect Mode Segmented Capsule: [ 16:9 | Fill ]
                if show_aspect_capsule {
                    draw_segmented_group(ui, false, |ui| {
                        if draw_segment_item(
                            ui,
                            "16:9",
                            self.app_cfg.aspect_fit_16_9,
                            PillAccent::Ice,
                        )
                        .on_hover_text("Undistorted 16:9 letterbox/pillarbox framing (A)")
                        .clicked()
                            && !self.app_cfg.aspect_fit_16_9
                        {
                            self.toggle_aspect_mode();
                        }
                        if draw_segment_item(
                            ui,
                            "Fill",
                            !self.app_cfg.aspect_fit_16_9,
                            PillAccent::Ice,
                        )
                        .on_hover_text("Aspect-preserving edge-to-edge center crop (A)")
                        .clicked()
                            && self.app_cfg.aspect_fit_16_9
                        {
                            self.toggle_aspect_mode();
                        }
                    });
                }

                if show_patrol_pill
                    && draw_pill_button(
                        ui,
                        &patrol_label,
                        self.patrol_active,
                        PillAccent::Emerald,
                        false,
                    )
                    .on_hover_text(format!(
                        "Auto-cycle Single View every {}s - pauses automatically while zoomed in (M)",
                        self.app_cfg.patrol_interval_secs
                    ))
                    .clicked()
                {
                    self.toggle_patrol_mode();
                }

                self.last_center_width = ui.min_rect().width();
            },
        );
    }

    fn render_camera_tile(
        &mut self,
        ui: &mut Ui,
        channel: usize,
        rect: Rect,
        is_fullscreen: bool,
        is_playback: bool,
    ) {
        let dt = ui.input(|i| i.stable_dt);
        if self
            .tiles
            .entry(channel)
            .or_default()
            .viewport_mut(is_playback)
            .step_animation(dt)
        {
            ui.ctx().request_repaint();
        }

        let (has_texture, tex_w, tex_h, cur_zoom, cur_box_armed, has_marquee) = {
            let tile = self.tiles.entry(channel).or_default();
            let tex_present = tile.has_texture(is_playback);
            if !tex_present {
                tile.viewport_mut(is_playback).clear_transient_drag();
            }
            let (tw, th) = if is_playback {
                (tile.pb_tex_width, tile.pb_tex_height)
            } else {
                (tile.tex_width, tile.tex_height)
            };
            let vp = tile.viewport(is_playback);
            (
                tex_present,
                tw,
                th,
                vp.zoom,
                vp.box_zoom_armed,
                vp.marquee_drag.is_some(),
            )
        };
        let video_rect = compute_video_rect(rect, self.app_cfg.aspect_fit_16_9, tex_w, tex_h);

        let (live_status, live_status_label, acl_denied, last_frame_epoch_ms) = self
            .stream_mgr
            .slot(channel)
            .map(|s| {
                (
                    s.status(),
                    s.status_label(),
                    s.acl_denied.load(Ordering::Relaxed),
                    s.last_frame_epoch_ms.load(Ordering::Relaxed),
                )
            })
            .unwrap_or((StreamStatus::Error, "Offline", false, 0));
        let pip_live_status = self
            .stream_mgr
            .slot(channel)
            .map(|s| s.status())
            .unwrap_or(StreamStatus::Connecting);
        let pb_status = self
            .playback_mgr
            .slot(channel)
            .map(|s| s.status())
            .unwrap_or(PlaybackStatus::Idle);
        let pb_last_error = if is_playback && pb_status == PlaybackStatus::Error {
            self.playback_mgr
                .slot(channel)
                .and_then(|s| s.last_error.lock().ok().and_then(|g| g.clone()))
        } else {
            None
        };

        let screen_rect = ui.ctx().screen_rect();
        let top_inset = if self.floating_top_bar_visible
            && (rect.min.y - screen_rect.min.y).abs() < 6.0
        {
            38.0
        } else {
            0.0
        };
        let bot_inset = if self.floating_dock_visible
            && (screen_rect.max.y - rect.max.y).abs() < 6.0
        {
            TIMELINE_DOCK_HEIGHT
        } else {
            0.0
        };
        let hud_bounds_rect = Rect::from_min_max(
            Pos2::new(rect.min.x, (rect.min.y + top_inset).min(rect.max.y)),
            Pos2::new(
                rect.max.x,
                (rect.max.y - bot_inset).max((rect.min.y + top_inset).min(rect.max.y)),
            ),
        );

        // Compute movable/hideable Live PiP geometry up front in 1-Cam Playback
        let pip_eligible = is_playback
            && is_fullscreen
            && !has_marquee
            && !self.playback_mgr.is_sync_2x2()
            && !self.env_cfg.is_cloud_p2p_active();
        let pip_rect = if pip_eligible {
            compute_live_pip_rect(
                hud_bounds_rect,
                self.timeline_ui.pip_corner,
                cur_zoom > 1.05,
            )
        } else {
            None
        };

        // Compute optional 401 "Open Settings" CTA pill geometry up front for hit-testing
        let is_auth_401 = if is_playback {
            pb_status == PlaybackStatus::AuthError
        } else {
            live_status == StreamStatus::AuthError && !acl_denied
        };
        let auth_btn_y_off = if has_texture {
            if last_frame_epoch_ms > 0 && !is_playback {
                32.0
            } else {
                22.0
            }
        } else {
            58.0
        };
        let auth_settings_btn_rect = if is_auth_401 && rect.width() >= 132.0 {
            let candidate =
                Rect::from_center_size(rect.center() + Vec2::new(0.0, auth_btn_y_off), Vec2::new(106.0, 22.0));
            if candidate.max.y <= rect.max.y - 34.0 && candidate.min.y >= rect.min.y + 30.0 {
                Some(candidate)
            } else {
                None
            }
        } else {
            None
        };

        let response = ui.allocate_rect(rect, Sense::click_and_drag());
        let (pointer_pos, press_origin, primary_down, shift_held, secondary_clicked) =
            ui.input(|i| {
                (
                    i.pointer.interact_pos().or_else(|| i.pointer.hover_pos()),
                    i.pointer.press_origin(),
                    i.pointer.primary_down(),
                    i.modifiers.shift,
                    i.pointer.button_clicked(PointerButton::Secondary),
                )
            });

        let over_floating_bar = (self.floating_top_bar_visible
            && pointer_pos.is_some_and(|p| p.y <= 39.0))
            || (self.floating_dock_visible
                && pointer_pos
                    .is_some_and(|p| p.y >= screen_rect.max.y - TIMELINE_DOCK_HEIGHT - 1.0));

        let modal_open = self.show_settings
            || self.show_shortcuts_modal
            || self.rename_modal_channel.is_some()
            || self.timeline_ui.is_any_popup_open()
            || self.overlay_gesture_block
            || over_floating_bar;

        let is_hovered = !modal_open && response.hovered();
        if is_hovered {
            self.hovered_channel = Some(channel);
        }

        // Bottom-right floating glass control dock geometry (Bug 1.4 fixed: dock_fits computed up front!)
        let show_zoom_controls = has_texture;
        let show_box_btn =
            show_zoom_controls && (is_fullscreen || is_playback || cur_zoom > 1.01 || cur_box_armed);
        let dock_h = 24.0;
        let btn_h = 18.0;
        let box_w = if show_box_btn { 30.0 } else { 0.0 };
        let box_gap = if show_box_btn { 2.0 } else { 0.0 };
        let z_out_w = if show_zoom_controls { 19.0 } else { 0.0 };
        let z_rst_w = if show_zoom_controls { 34.0 } else { 0.0 };
        let z_in_w = if show_zoom_controls { 19.0 } else { 0.0 };
        let z_gaps = if show_zoom_controls { 4.0 } else { 0.0 };
        let sep_w = if show_zoom_controls { 7.0 } else { 0.0 };
        let hist_w = if is_playback { 0.0 } else { 32.0 };
        let hist_gap = if is_playback { 0.0 } else { 3.0 };
        let sd_hd_w = if is_playback { 36.0 } else { 34.0 };
        let expand_w = if is_playback && !is_fullscreen {
            44.0
        } else if is_playback || is_fullscreen {
            40.0
        } else {
            48.0
        };
        let dock_pad = 3.0;
        let total_dock_w = dock_pad * 2.0
            + box_w
            + box_gap
            + z_out_w
            + z_rst_w
            + z_in_w
            + z_gaps
            + sep_w
            + hist_w
            + hist_gap
            + sd_hd_w
            + 3.0
            + expand_w;

        let dock_fits = hud_bounds_rect.width() >= total_dock_w + 10.0
            && hud_bounds_rect.height() >= 54.0;
        let dock_visible = dock_fits && (is_hovered || cur_zoom > 1.01 || cur_box_armed);

        let dock_rect = Rect::from_min_size(
            Pos2::new(
                (hud_bounds_rect.max.x - total_dock_w - 6.0).max(hud_bounds_rect.min.x + 4.0),
                hud_bounds_rect.max.y - dock_h - 6.0,
            ),
            Vec2::new(total_dock_w, dock_h),
        );
        let btn_y = dock_rect.center().y - btn_h * 0.5;
        let box_rect = Rect::from_min_size(
            Pos2::new(dock_rect.min.x + dock_pad, btn_y),
            Vec2::new(box_w, btn_h),
        );
        let z_out_rect = Rect::from_min_size(
            Pos2::new(dock_rect.min.x + dock_pad + box_w + box_gap, btn_y),
            Vec2::new(z_out_w, btn_h),
        );
        let z_rst_rect = Rect::from_min_size(
            Pos2::new(
                z_out_rect.max.x + if show_zoom_controls { 2.0 } else { 0.0 },
                btn_y,
            ),
            Vec2::new(z_rst_w, btn_h),
        );
        let z_in_rect = Rect::from_min_size(
            Pos2::new(
                z_rst_rect.max.x + if show_zoom_controls { 2.0 } else { 0.0 },
                btn_y,
            ),
            Vec2::new(z_in_w, btn_h),
        );
        let right_grp_x = z_in_rect.max.x + sep_w;
        let hist_rect =
            Rect::from_min_size(Pos2::new(right_grp_x, btn_y), Vec2::new(hist_w, btn_h));
        let sd_hd_rect = Rect::from_min_size(
            Pos2::new(right_grp_x + hist_w + hist_gap, btn_y),
            Vec2::new(sd_hd_w, btn_h),
        );
        let expand_rect = Rect::from_min_size(
            Pos2::new(sd_hd_rect.max.x + 3.0, btn_y),
            Vec2::new(expand_w, btn_h),
        );

        // Bottom-left interactive zoom-radar minimap geometry
        let radar_size = if is_fullscreen {
            Vec2::new(108.0, 61.0)
        } else {
            Vec2::new(52.0, 30.0)
        };
        let radar_margin = if is_fullscreen { 10.0 } else { 7.0 };
        let radar_rect = Rect::from_min_size(
            Pos2::new(
                hud_bounds_rect.min.x + radar_margin,
                hud_bounds_rect.max.y - radar_size.y - radar_margin,
            ),
            radar_size,
        );
        let radar_visible = has_texture
            && (is_hovered || cur_zoom > 1.01)
            && cur_zoom > 1.05
            && (!dock_visible || radar_rect.max.x + 8.0 < dock_rect.min.x)
            && (radar_rect.min.y > hud_bounds_rect.min.y + 30.0);

        let over_pip = !modal_open
            && is_hovered
            && pip_rect
                .zip(pointer_pos)
                .is_some_and(|(pr, p)| pr.contains(p));
        let started_on_pip = !modal_open
            && pip_rect
                .zip(press_origin)
                .is_some_and(|(pr, o)| pr.contains(o));

        let over_auth_settings_btn = !modal_open
            && is_hovered
            && !has_marquee
            && !over_pip
            && auth_settings_btn_rect
                .zip(pointer_pos)
                .is_some_and(|(ar, p)| ar.contains(p));

        let over_box = show_box_btn
            && dock_visible
            && is_hovered
            && !has_marquee
            && !over_pip
            && pointer_pos.is_some_and(|p| box_rect.contains(p));
        let over_z_out = show_zoom_controls
            && dock_visible
            && is_hovered
            && !has_marquee
            && !over_pip
            && pointer_pos.is_some_and(|p| z_out_rect.contains(p));
        let over_z_rst = show_zoom_controls
            && dock_visible
            && is_hovered
            && !has_marquee
            && !over_pip
            && pointer_pos.is_some_and(|p| z_rst_rect.contains(p));
        let over_z_in = show_zoom_controls
            && dock_visible
            && is_hovered
            && !has_marquee
            && !over_pip
            && pointer_pos.is_some_and(|p| z_in_rect.contains(p));
        let over_hist = !is_playback
            && dock_visible
            && is_hovered
            && !has_marquee
            && !over_pip
            && pointer_pos.is_some_and(|p| hist_rect.contains(p));
        let over_sd_hd = dock_visible
            && is_hovered
            && !has_marquee
            && !over_pip
            && pointer_pos.is_some_and(|p| sd_hd_rect.contains(p));
        let over_expand = dock_visible
            && is_hovered
            && !has_marquee
            && !over_pip
            && pointer_pos.is_some_and(|p| expand_rect.contains(p));
        let over_any_micro_btn = over_box
            || over_z_out
            || over_z_rst
            || over_z_in
            || over_hist
            || over_sd_hd
            || over_expand;

        let started_on_hud = (dock_visible && press_origin.is_some_and(|o| dock_rect.contains(o)))
            || started_on_pip
            || auth_settings_btn_rect
                .zip(press_origin)
                .is_some_and(|(ar, o)| ar.contains(o));
        let started_on_radar = radar_visible
            && !started_on_hud
            && press_origin.is_some_and(|o| radar_rect.contains(o));
        let over_radar = radar_visible
            && is_hovered
            && !over_any_micro_btn
            && !over_pip
            && !over_auth_settings_btn
            && !has_marquee
            && pointer_pos.is_some_and(|p| radar_rect.contains(p));

        if !modal_open && (over_any_micro_btn || over_pip || over_auth_settings_btn) {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }

        // Crisp tooltips for micro-buttons, radar minimap, 401 CTA, and playback errors
        if !modal_open && is_hovered && !has_marquee {
            if over_auth_settings_btn {
                response
                    .clone()
                    .on_hover_text("Update NVR username & password in Settings (Ctrl+,)");
            } else if over_box {
                response.clone().on_hover_text(
                    "Area Box Zoom - click then drag a rectangle to zoom in (Shift+Drag)",
                );
            } else if over_z_out {
                response.clone().on_hover_text("Zoom Out (-)");
            } else if over_z_rst {
                response
                    .clone()
                    .on_hover_text("Reset Digital Zoom to 1.0x (Z / Middle-Click)");
            } else if over_z_in {
                response.clone().on_hover_text("Zoom In (+)");
            } else if over_hist {
                response
                    .clone()
                    .on_hover_text("Open 1080p Playback History & AI Timeline (T)");
            } else if over_sd_hd {
                response.clone().on_hover_text(if is_playback {
                    "Save PNG Snapshot of Playback Frame (C / Ctrl+S)"
                } else {
                    "Toggle Stream Quality: SD Sub-Stream vs HD Main-Stream (S / H)"
                });
            } else if over_expand {
                let tip = if is_playback && !is_fullscreen {
                    "Expand Camera to 1080p Single Playback (Enter / Double-Click)"
                } else if is_playback {
                    "Return to Live Camera Wall (T / Esc)"
                } else if is_fullscreen {
                    "Return to Multi-Camera Grid (Enter / Esc)"
                } else {
                    "Expand Camera to Single View (Enter / Double-Click)"
                };
                response.clone().on_hover_text(tip);
            } else if over_radar {
                response
                    .clone()
                    .on_hover_text("Zoom Radar Minimap - click or drag to pan zoomed viewport");
            } else if !over_pip {
                if let Some(ref full_err) = pb_last_error {
                    response.clone().on_hover_text(format!("Playback error: {}", full_err));
                }
            }
        }

        // Handle 1-click on 401 "Open Settings" CTA pill
        if !modal_open && response.clicked() && over_auth_settings_btn {
            self.last_tile_hud_click_at = Some(Instant::now());
            self.open_settings_modal(SettingsTab::NvrAndCameras);
            return;
        }

        // Handle 1-click on actual micro-buttons (never swallowing clicks on 2px empty dock gaps!)
        let mut hud_clicked = false;
        if !modal_open && response.clicked() && over_any_micro_btn {
            hud_clicked = true;
            self.last_tile_hud_click_at = Some(Instant::now());
            if over_box {
                let vp = self.tiles.entry(channel).or_default().viewport_mut(is_playback);
                vp.box_zoom_armed = !vp.box_zoom_armed;
            } else if over_z_out {
                self.tiles
                    .entry(channel)
                    .or_default()
                    .viewport_mut(is_playback)
                    .zoom_by(1.0 / 1.25, Pos2::new(0.5, 0.5));
            } else if over_z_rst {
                self.tiles
                    .entry(channel)
                    .or_default()
                    .viewport_mut(is_playback)
                    .animate_reset_zoom();
            } else if over_z_in {
                self.tiles
                    .entry(channel)
                    .or_default()
                    .viewport_mut(is_playback)
                    .zoom_by(1.25, Pos2::new(0.5, 0.5));
            } else if over_hist {
                self.enter_playback_single(channel, false);
                return;
            } else if over_sd_hd {
                if is_playback {
                    self.save_camera_snapshot(channel, true);
                } else {
                    self.toggle_camera_hd_sd(channel);
                }
            } else if over_expand {
                if is_playback {
                    if !is_fullscreen {
                        self.enter_playback_single(channel, true);
                    } else {
                        self.exit_playback_to_live();
                    }
                    return;
                } else if is_fullscreen {
                    self.exit_fullscreen();
                } else {
                    self.enter_fullscreen(channel);
                }
            }
        }

        // Interactive Radar Minimap Click & Drag Panning
        let mut radar_interacted = false;
        if !hud_clicked
            && !modal_open
            && ((over_radar && response.clicked())
                || (started_on_radar && response.dragged_by(PointerButton::Primary)))
        {
            if let Some(p) = pointer_pos {
                let rx = ((p.x - radar_rect.min.x) / radar_rect.width().max(1.0)).clamp(0.0, 1.0);
                let ry = ((p.y - radar_rect.min.y) / radar_rect.height().max(1.0)).clamp(0.0, 1.0);
                self.tiles
                    .entry(channel)
                    .or_default()
                    .viewport_mut(is_playback)
                    .jump_to_radar_uv(Pos2::new(rx, ry));
                self.last_tile_hud_click_at = Some(Instant::now());
                radar_interacted = true;
            }
        }

        // Middle-click smoothly resets digital zoom to 1.0x
        if !modal_open
            && has_texture
            && !over_any_micro_btn
            && !over_pip
            && !over_auth_settings_btn
            && response.clicked_by(PointerButton::Middle)
        {
            self.tiles
                .entry(channel)
                .or_default()
                .viewport_mut(is_playback)
                .animate_reset_zoom();
        }

        // Handle Wayland touchpad pinch, wheel/scroll zoom, Click-Drag Marquee Box Zoom, pan, and selection
        {
            let anchor_norm = pointer_pos
                .map(|p| {
                    Pos2::new(
                        ((p.x - video_rect.min.x) / video_rect.width().max(1.0)).clamp(0.0, 1.0),
                        ((p.y - video_rect.min.y) / video_rect.height().max(1.0)).clamp(0.0, 1.0),
                    )
                })
                .unwrap_or(Pos2::new(0.5, 0.5));

            let frame_pinch = self.frame_pinch;
            let zoom_factor = if has_texture && is_hovered && !modal_open && !over_pip {
                ui.input(|i| {
                    compute_tile_zoom_factor(
                        &i.events,
                        i.multi_touch().map(|t| t.zoom_delta),
                        frame_pinch,
                    )
                })
            } else {
                1.0
            };

            let mut toggle_fs = false;
            let mut select_tile = false;
            {
                let vp = self
                    .tiles
                    .entry(channel)
                    .or_default()
                    .viewport_mut(is_playback);
                if has_texture && (zoom_factor - 1.0).abs() > 0.0005 {
                    vp.zoom_by(zoom_factor, anchor_norm);
                }

                // Simultaneous two-finger pinch center panning when zoomed in
                if has_texture
                    && is_hovered
                    && !modal_open
                    && !over_pip
                    && frame_pinch.pan_delta != Vec2::ZERO
                    && vp.zoom > 1.01
                {
                    vp.pan_by_pixels(frame_pinch.pan_delta, video_rect.size());
                }

                let wants_marquee =
                    has_texture && (vp.zoom <= 1.01 || vp.box_zoom_armed || shift_held);
                if !modal_open
                    && !started_on_hud
                    && !started_on_radar
                    && !over_pip
                    && wants_marquee
                    && response.drag_started_by(PointerButton::Primary)
                {
                    let origin = press_origin.or(pointer_pos).unwrap_or(video_rect.center());
                    let p0 = clamp_pos_to_rect(origin, video_rect);
                    let p1 = clamp_pos_to_rect(pointer_pos.unwrap_or(p0), video_rect);
                    vp.marquee_drag = Some((p0, p1));
                }

                if let Some((p0, _)) = vp.marquee_drag {
                    if modal_open || secondary_clicked || !has_texture {
                        vp.marquee_drag = None;
                        vp.box_zoom_armed = false;
                    } else if primary_down {
                        if let Some(cur_p) = pointer_pos {
                            let p1 = clamp_pos_to_rect(cur_p, video_rect);
                            vp.marquee_drag = Some((p0, p1));
                        }
                        ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
                        ui.ctx().request_repaint();
                    } else {
                        let (_, p1) = vp.marquee_drag.take().unwrap();
                        vp.box_zoom_armed = false;
                        let sel_rect = Rect::from_two_pos(p0, p1);
                        if vp.zoom_to_marquee_rect(sel_rect, video_rect).is_some() {
                            select_tile = true;
                            ui.ctx().request_repaint();
                        } else if !is_fullscreen && (p1 - p0).length() < MIN_MARQUEE_PX {
                            select_tile = true;
                        }
                    }
                } else if has_texture
                    && !modal_open
                    && !started_on_hud
                    && !started_on_radar
                    && !over_pip
                    && !vp.box_zoom_armed
                    && !shift_held
                    && response.dragged_by(PointerButton::Primary)
                    && vp.zoom > 1.01
                {
                    vp.pan_by_pixels(response.drag_delta(), video_rect.size());
                    ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                } else if is_hovered
                    && !modal_open
                    && !over_any_micro_btn
                    && !over_pip
                    && !over_auth_settings_btn
                {
                    if over_radar
                        || (started_on_radar && primary_down)
                        || (has_texture && (vp.box_zoom_armed || shift_held))
                    {
                        ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
                    } else if has_texture && vp.zoom > 1.01 {
                        ui.ctx().set_cursor_icon(CursorIcon::Grab);
                    }
                }

                if !modal_open
                    && !hud_clicked
                    && !over_any_micro_btn
                    && !over_pip
                    && !over_auth_settings_btn
                    && !radar_interacted
                    && !started_on_radar
                {
                    if response.double_clicked()
                        && self
                            .last_tile_hud_click_at
                            .is_none_or(|t| t.elapsed() > Duration::from_millis(400))
                    {
                        toggle_fs = true;
                    } else if response.clicked() && !is_fullscreen {
                        select_tile = true;
                    }
                }
            }

            if toggle_fs {
                self.selected_channel = Some(channel);
                if is_playback {
                    if self.playback_mgr.is_sync_2x2() {
                        self.enter_playback_single(channel, true);
                    } else if !self.env_cfg.is_cloud_p2p_active() && self.app_cfg.num_cameras >= 2 {
                        self.enter_playback_sync_2x2();
                    } else {
                        self.exit_playback_to_live();
                    }
                    return;
                } else if is_fullscreen {
                    self.exit_fullscreen();
                } else {
                    self.enter_fullscreen(channel);
                }
            } else if select_tile {
                self.selected_channel = Some(channel);
                if is_playback
                    && self.playback_mgr.is_sync_2x2()
                    && self.playback_mgr.active_channels().contains(&channel)
                {
                    self.playback_mgr.set_primary_channel(channel);
                    let date = self.timeline_ui.selected_date;
                    self.playback_mgr.request_day_timeline(channel, date, false);
                    self.playback_mgr
                        .request_month_days(channel, date.year(), date.month());
                }
            }
        }

        let pb_epoch_ms = self
            .playback_mgr
            .slot(channel)
            .map(|s| s.current_epoch_ms.load(Ordering::Relaxed))
            .unwrap_or_else(|| self.playback_mgr.playhead_epoch_ms());
        let active_sub = self
            .stream_mgr
            .slot(channel)
            .map(|s| s.active_subtype.load(Ordering::Relaxed))
            .unwrap_or(1);
        let target_sub = self
            .stream_mgr
            .slot(channel)
            .map(|s| s.target_subtype.load(Ordering::Relaxed))
            .unwrap_or(1);
        let fps = if is_playback {
            self.playback_mgr
                .slot(channel)
                .map(|s| s.fps_x10.load(Ordering::Relaxed) as f32 / 10.0)
                .unwrap_or(0.0)
        } else {
            self.stream_mgr
                .slot(channel)
                .map(|s| s.fps_x10.load(Ordering::Relaxed) as f32 / 10.0)
                .unwrap_or(0.0)
        };

        let (zoom_val, uv_rect, box_armed_now, marquee_preview, has_zoom_history) = {
            let tile = self.tiles.get(&channel).unwrap();
            let vp = tile.viewport(is_playback);
            let preview = vp.marquee_drag.map(|(p0, p1)| {
                let sel = Rect::from_two_pos(p0, p1).intersect(video_rect);
                let (pz, _) = vp.preview_marquee_zoom(sel, video_rect);
                (p0, p1, pz)
            });
            (
                vp.zoom,
                vp.uv_rect(),
                vp.box_zoom_armed,
                preview,
                !vp.zoom_history.is_empty(),
            )
        };

        let src_aspect = if tex_w > 0 && tex_h > 0 {
            tex_w as f32 / tex_h as f32
        } else {
            16.0 / 9.0
        };
        let target_aspect = rect.width().max(1.0) / rect.height().max(1.0);
        let render_uv = if self.app_cfg.aspect_fit_16_9 {
            uv_rect
        } else {
            apply_cover_crop_uv(uv_rect, target_aspect, src_aspect)
        };

        let pulse = ((ui.input(|i| i.time) * 5.2).sin() as f32 * 0.5 + 0.5).clamp(0.0, 1.0);
        let raw_cam_name = self.app_cfg.camera_display_name(channel);

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, Rounding::same(6.0), OBSIDIAN_BG);

        if has_texture {
            let tex_opt = self.tiles.get(&channel).and_then(|t| {
                if is_playback {
                    t.pb_texture.as_ref()
                } else {
                    t.texture.as_ref()
                }
            });
            if let Some(tex) = tex_opt {
                painter.image(tex.id(), video_rect, render_uv, Color32::WHITE);
            }

            // Stale Frame Status Scrim: NEVER leave a frozen video frame unmarked when non-live / non-playing!
            if !is_playback && live_status != StreamStatus::Live {
                painter.rect_filled(
                    rect,
                    Rounding::same(6.0),
                    Color32::from_rgba_unmultiplied(9, 9, 11, 135),
                );
                let is_spinning = matches!(
                    live_status,
                    StreamStatus::Connecting | StreamStatus::Reconnecting
                );
                if is_spinning {
                    ui.ctx().request_repaint_after(Duration::from_millis(60));
                }
                let badge_txt = if is_spinning {
                    format!("{}...", live_status_label)
                } else {
                    live_status_label.to_string()
                };
                let badge_col = match live_status {
                    StreamStatus::Connecting | StreamStatus::Reconnecting | StreamStatus::Paused => {
                        STATUS_AMBER
                    }
                    StreamStatus::Error | StreamStatus::AuthError => STATUS_CORAL,
                    StreamStatus::Live => STATUS_EMERALD,
                };
                let y_shift = if auth_settings_btn_rect.is_some() {
                    -18.0
                } else if last_frame_epoch_ms > 0 {
                    -8.0
                } else {
                    0.0
                };
                let b_font = FontId::new(11.5, FontFamily::Proportional);
                let b_gal = painter.layout_no_wrap(badge_txt.clone(), b_font.clone(), badge_col);
                let dot_pad = if is_spinning { 14.0 } else { 0.0 };
                let b_w = safe_clamp_f32(
                    b_gal.size().x + 20.0 + dot_pad,
                    68.0,
                    rect.width() - 16.0,
                );
                let b_rect = Rect::from_center_size(
                    rect.center() + Vec2::new(0.0, y_shift),
                    Vec2::new(b_w, 22.0),
                );
                painter.rect_filled(
                    b_rect,
                    Rounding::same(11.0),
                    Color32::from_rgba_unmultiplied(17, 17, 20, 228),
                );
                painter.rect_stroke(
                    b_rect,
                    Rounding::same(11.0),
                    Stroke::new(1.0_f32, badge_col.gamma_multiply(0.75)),
                );
                if is_spinning {
                    let dot_alpha = (115.0 + pulse * 140.0) as u8;
                    painter.circle_filled(
                        Pos2::new(b_rect.min.x + 11.0, b_rect.center().y),
                        3.5,
                        Color32::from_rgba_unmultiplied(
                            badge_col.r(),
                            badge_col.g(),
                            badge_col.b(),
                            dot_alpha,
                        ),
                    );
                    let fitted = if b_gal.size().x <= b_w - 26.0 {
                        b_gal
                    } else {
                        truncate_galley_with_ellipsis(
                            &painter,
                            &badge_txt,
                            b_font,
                            badge_col,
                            (b_w - 26.0).max(16.0),
                        )
                    };
                    painter.galley(
                        Pos2::new(
                            b_rect.min.x + 19.0,
                            b_rect.center().y - fitted.size().y * 0.5,
                        ),
                        fitted,
                        badge_col,
                    );
                } else {
                    let fitted = if b_gal.size().x <= b_w - 14.0 {
                        b_gal
                    } else {
                        truncate_galley_with_ellipsis(
                            &painter,
                            &badge_txt,
                            b_font,
                            badge_col,
                            (b_w - 14.0).max(16.0),
                        )
                    };
                    painter.galley(
                        Pos2::new(
                            b_rect.center().x - fitted.size().x * 0.5,
                            b_rect.center().y - fitted.size().y * 0.5,
                        ),
                        fitted,
                        badge_col,
                    );
                }

                if last_frame_epoch_ms > 0 && b_rect.max.y + 18.0 < rect.max.y {
                    let age_s = now_epoch_ms().saturating_sub(last_frame_epoch_ms) / 1000;
                    let age_txt = if age_s < 120 {
                        format!("Last frame {}s ago", age_s)
                    } else {
                        format!("Last frame {}m {:02}s ago", age_s / 60, age_s % 60)
                    };
                    painter.text(
                        Pos2::new(rect.center().x, b_rect.max.y + 10.0),
                        Align2::CENTER_CENTER,
                        age_txt,
                        FontId::new(10.5, FontFamily::Proportional),
                        TEXT_SECONDARY,
                    );
                }
            } else if is_playback
                && !matches!(pb_status, PlaybackStatus::Playing | PlaybackStatus::Paused)
            {
                painter.rect_filled(
                    rect,
                    Rounding::same(6.0),
                    Color32::from_rgba_unmultiplied(9, 9, 11, 135),
                );
                let is_buf = matches!(pb_status, PlaybackStatus::Buffering | PlaybackStatus::Idle);
                if is_buf {
                    ui.ctx().request_repaint_after(Duration::from_millis(60));
                }
                let (badge_txt, badge_col) = match pb_status {
                    PlaybackStatus::Buffering | PlaybackStatus::Idle => {
                        ("Buffering...".to_string(), STATUS_ICE)
                    }
                    PlaybackStatus::EndOfRecording => {
                        ("No Recording at Timestamp".to_string(), STATUS_AMBER)
                    }
                    PlaybackStatus::AuthError => ("Auth Locked (401)".to_string(), STATUS_CORAL),
                    PlaybackStatus::Error => {
                        let detail = pb_last_error
                            .clone()
                            .unwrap_or_else(|| "Playback Unavailable".to_string());
                        (detail, STATUS_CORAL)
                    }
                    _ => (pb_status.label().to_string(), STATUS_AMBER),
                };
                let y_shift = if auth_settings_btn_rect.is_some() { -12.0 } else { 0.0 };
                let b_font = FontId::new(11.5, FontFamily::Proportional);
                let dot_pad = if is_buf { 14.0 } else { 0.0 };
                let max_txt_w = (rect.width() - 36.0 - dot_pad).max(32.0);
                let full_gal = painter.layout_no_wrap(badge_txt.clone(), b_font.clone(), badge_col);
                let fitted = if full_gal.size().x <= max_txt_w {
                    full_gal
                } else {
                    truncate_galley_with_ellipsis(&painter, &badge_txt, b_font, badge_col, max_txt_w)
                };
                let b_w = safe_clamp_f32(
                    fitted.size().x + 20.0 + dot_pad,
                    72.0,
                    rect.width() - 16.0,
                );
                let b_rect = Rect::from_center_size(
                    rect.center() + Vec2::new(0.0, y_shift),
                    Vec2::new(b_w, 22.0),
                );
                painter.rect_filled(
                    b_rect,
                    Rounding::same(11.0),
                    Color32::from_rgba_unmultiplied(17, 17, 20, 228),
                );
                painter.rect_stroke(
                    b_rect,
                    Rounding::same(11.0),
                    Stroke::new(1.0_f32, badge_col.gamma_multiply(0.75)),
                );
                if is_buf {
                    let dot_alpha = (115.0 + pulse * 140.0) as u8;
                    painter.circle_filled(
                        Pos2::new(b_rect.min.x + 11.0, b_rect.center().y),
                        3.5,
                        Color32::from_rgba_unmultiplied(
                            STATUS_ICE.r(),
                            STATUS_ICE.g(),
                            STATUS_ICE.b(),
                            dot_alpha,
                        ),
                    );
                    painter.galley(
                        Pos2::new(
                            b_rect.min.x + 19.0,
                            b_rect.center().y - fitted.size().y * 0.5,
                        ),
                        fitted,
                        badge_col,
                    );
                } else {
                    painter.galley(
                        Pos2::new(
                            b_rect.center().x - fitted.size().x * 0.5,
                            b_rect.center().y - fitted.size().y * 0.5,
                        ),
                        fitted,
                        badge_col,
                    );
                }
            }
        } else {
            // Minimalist Empty / Connecting / Buffering / Offline Tile State
            painter.rect_filled(rect, Rounding::same(6.0), Color32::from_rgb(13, 13, 16));
            let center = rect.center();
            let compact_tile = rect.height() < 104.0;

            if !compact_tile {
                let body =
                    Rect::from_center_size(center - Vec2::new(3.0, 22.0), Vec2::new(20.0, 14.0));
                painter.rect_stroke(body, Rounding::same(2.5), Stroke::new(1.4_f32, ZINC_DIM));
                painter.circle_stroke(body.center(), 3.0, Stroke::new(1.2_f32, ZINC_DIM));
                let lens_pts = vec![
                    Pos2::new(body.max.x + 1.0, body.center().y - 2.5),
                    Pos2::new(body.max.x + 6.5, body.center().y - 5.5),
                    Pos2::new(body.max.x + 6.5, body.center().y + 5.5),
                    Pos2::new(body.max.x + 1.0, body.center().y + 2.5),
                ];
                painter.add(egui::Shape::closed_line(
                    lens_pts,
                    Stroke::new(1.3_f32, ZINC_DIM),
                ));

                painter.text(
                    center + Vec2::new(0.0, -1.0),
                    Align2::CENTER_CENTER,
                    &raw_cam_name,
                    FontId::new(13.5, FontFamily::Proportional),
                    TEXT_SECONDARY,
                );
                painter.text(
                    center + Vec2::new(0.0, 15.0),
                    Align2::CENTER_CENTER,
                    format!("CH {:02}", channel),
                    FontId::new(10.5, FontFamily::Proportional),
                    Color32::from_rgb(113, 113, 122),
                );
            }

            let (st_text, st_col): (String, Color32) = if is_playback {
                match pb_status {
                    PlaybackStatus::Buffering | PlaybackStatus::Idle => {
                        ui.ctx().request_repaint_after(Duration::from_millis(60));
                        let load_lbl = if self.playback_mgr.is_sync_2x2() {
                            "Loading 540p Sync..."
                        } else {
                            "Loading 1080p History..."
                        };
                        (load_lbl.to_string(), STATUS_ICE)
                    }
                    PlaybackStatus::Playing => ("Playing...".to_string(), STATUS_EMERALD),
                    PlaybackStatus::Paused => ("Paused".to_string(), STATUS_AMBER),
                    PlaybackStatus::EndOfRecording => {
                        ("No Recording at Timestamp".to_string(), STATUS_AMBER)
                    }
                    PlaybackStatus::AuthError => ("Auth Locked (401)".to_string(), STATUS_CORAL),
                    PlaybackStatus::Error => {
                        let detail = pb_last_error
                            .clone()
                            .unwrap_or_else(|| "Playback Unavailable".to_string());
                        (detail, STATUS_CORAL)
                    }
                }
            } else {
                match live_status {
                    StreamStatus::Connecting => {
                        ui.ctx().request_repaint_after(Duration::from_millis(60));
                        ("Connecting...".to_string(), STATUS_AMBER)
                    }
                    StreamStatus::Reconnecting => {
                        ui.ctx().request_repaint_after(Duration::from_millis(60));
                        ("Reconnecting...".to_string(), STATUS_AMBER)
                    }
                    StreamStatus::Paused => ("Paused".to_string(), STATUS_AMBER),
                    StreamStatus::AuthError => (live_status_label.to_string(), STATUS_CORAL),
                    StreamStatus::Error => ("Offline".to_string(), STATUS_CORAL),
                    StreamStatus::Live => ("Ready".to_string(), STATUS_EMERALD),
                }
            };
            let st_font = FontId::new(10.5, FontFamily::Proportional);
            let max_st_txt_w = (rect.width() - 32.0).max(28.0);
            let st_full = painter.layout_no_wrap(st_text.clone(), st_font.clone(), st_col);
            let st_fitted = if st_full.size().x <= max_st_txt_w {
                st_full
            } else {
                truncate_galley_with_ellipsis(&painter, &st_text, st_font, st_col, max_st_txt_w)
            };
            let st_w = safe_clamp_f32(st_fitted.size().x + 18.0, 60.0, rect.width() - 16.0);
            let st_center = if compact_tile {
                Pos2::new(center.x, (rect.min.y + 28.0 + rect.max.y) * 0.5)
            } else {
                center + Vec2::new(0.0, 35.0)
            };
            let st_pill = Rect::from_center_size(st_center, Vec2::new(st_w, 19.0));
            if st_pill.max.y < rect.max.y - 4.0 {
                painter.rect_filled(st_pill, Rounding::same(9.0), ZINC_ELEVATED);
                painter.rect_stroke(
                    st_pill,
                    Rounding::same(9.0),
                    Stroke::new(1.0_f32, hairline_stroke()),
                );
                painter.galley(
                    Pos2::new(
                        st_pill.center().x - st_fitted.size().x * 0.5,
                        st_pill.center().y - st_fitted.size().y * 0.5,
                    ),
                    st_fitted,
                    st_col,
                );
            }
        }

        // Clickable "Open Settings" pill on 401 AuthError
        if let Some(ar) = auth_settings_btn_rect {
            let fill = if over_auth_settings_btn {
                Color32::from_rgba_unmultiplied(56, 189, 248, 64)
            } else {
                status_ice_fill()
            };
            painter.rect_filled(ar, Rounding::same(11.0), fill);
            painter.rect_stroke(ar, Rounding::same(11.0), Stroke::new(1.0_f32, STATUS_ICE));
            painter.text(
                ar.center(),
                Align2::CENTER_CENTER,
                "Open Settings",
                FontId::new(11.0, FontFamily::Proportional),
                TEXT_PRIMARY,
            );
        }

        // Subtle Shutter Flash Overlay when snapshot captured
        if let Some(&flash_until) = self.shutter_flash_until.get(&channel) {
            let now = Instant::now();
            if now < flash_until {
                let frac = (flash_until.duration_since(now).as_secs_f32()
                    / SHUTTER_FLASH_DURATION.as_secs_f32())
                .clamp(0.0, 1.0);
                let alpha = (frac * 92.0).round() as u8;
                painter.rect_filled(
                    rect,
                    Rounding::same(6.0),
                    Color32::from_rgba_unmultiplied(255, 255, 255, alpha),
                );
                ui.ctx().request_repaint();
            } else {
                self.shutter_flash_until.remove(&channel);
            }
        }

        // Optical Viewfinder Overlay during Click-Drag Area Box Zoom
        if let Some((p0, p1, preview_z)) = marquee_preview {
            paint_marquee_viewfinder(&painter, video_rect, p0, p1, preview_z);
        } else if box_armed_now && has_texture && rect.width() >= 210.0 && rect.height() >= 88.0 {
            let hint_txt = "Drag area to zoom (Esc to cancel)";
            let h_font = FontId::new(11.0, FontFamily::Proportional);
            let h_gal = painter.layout_no_wrap(hint_txt.to_string(), h_font, STATUS_ICE);
            let h_w = safe_clamp_f32(h_gal.size().x + 18.0, 80.0, rect.width() - 16.0);
            let h_rect =
                Rect::from_center_size(Pos2::new(rect.center().x, rect.min.y + 40.0), Vec2::new(h_w, 20.0));
            painter.rect_filled(
                h_rect,
                Rounding::same(10.0),
                Color32::from_rgba_unmultiplied(9, 9, 11, 212),
            );
            painter.rect_stroke(
                h_rect,
                Rounding::same(10.0),
                Stroke::new(1.0_f32, STATUS_ICE.gamma_multiply(0.75)),
            );
            painter.galley(
                Pos2::new(
                    h_rect.center().x - h_gal.size().x * 0.5,
                    h_rect.center().y - h_gal.size().y * 0.5,
                ),
                h_gal,
                STATUS_ICE,
            );
        }

        // Compute Top-Right Telemetry / Paused Pill UP FRONT so Top-Left camera name reserves space for it!
        let show_active_hud =
            (is_hovered || zoom_val > 1.01 || box_armed_now) && marquee_preview.is_none();
        let show_idle_paused_badge =
            is_playback && pb_status == PlaybackStatus::Paused && !show_active_hud && marquee_preview.is_none();

        let right_pill_spec: Option<(Rect, Arc<egui::Galley>, Color32, Color32)> =
            if show_active_hud || show_idle_paused_badge {
                let (right_text, r_fg, r_stroke) = if show_idle_paused_badge {
                    (
                        "PAUSED".to_string(),
                        STATUS_AMBER,
                        STATUS_AMBER.gamma_multiply(0.65),
                    )
                } else if is_playback {
                    let eff_spd = self.playback_mgr.effective_speed();
                    let master_ms = self.playback_mgr.playhead_epoch_ms();
                    let mut parts: Vec<String> = Vec::with_capacity(5);
                    if self.playback_mgr.is_sync_2x2()
                        && self.playback_mgr.primary_channel() == Some(channel)
                    {
                        parts.push("PRIMARY".to_string());
                    }
                    if pb_status == PlaybackStatus::Paused {
                        parts.push("PAUSED".to_string());
                    } else {
                        parts.push(pb_status.label().to_string());
                    }
                    parts.push(format!("{:.1}x", eff_spd));
                    if fps > 0.1 && pb_status == PlaybackStatus::Playing {
                        parts.push(format!("{:.0} fps", fps));
                    }
                    if self.playback_mgr.is_sync_2x2() {
                        let drift_ms = pb_epoch_ms - master_ms;
                        if drift_ms.abs() >= 350 {
                            parts.push(format!("{:+.1}s", drift_ms as f64 / 1000.0));
                        }
                    }
                    if zoom_val > 1.01 {
                        parts.push(format!("{:.1}x zoom", zoom_val));
                    }
                    let fg = if pb_status == PlaybackStatus::Paused {
                        STATUS_AMBER
                    } else {
                        TEXT_MUTED
                    };
                    (parts.join("  "), fg, hairline_stroke())
                } else {
                    let txt = if live_status == StreamStatus::Live && zoom_val > 1.01 {
                        format!("{:.0} fps  {:.1}x", fps, zoom_val)
                    } else if live_status == StreamStatus::Live {
                        format!("{:.0} fps", fps)
                    } else if zoom_val > 1.01 {
                        format!("{}  {:.1}x", live_status_label, zoom_val)
                    } else {
                        live_status_label.to_string()
                    };
                    (txt, TEXT_MUTED, hairline_stroke())
                };

                let r_font = FontId::new(11.0, FontFamily::Proportional);
                let max_r_w = (rect.width() * 0.56).clamp(42.0, (rect.width() - 16.0).max(42.0));
                let r_full = painter.layout_no_wrap(right_text.clone(), r_font.clone(), r_fg);
                let r_fitted = if r_full.size().x + 14.0 <= max_r_w {
                    r_full
                } else {
                    truncate_galley_with_ellipsis(
                        &painter,
                        &right_text,
                        r_font,
                        r_fg,
                        (max_r_w - 14.0).max(20.0),
                    )
                };
                let r_width = safe_clamp_f32(r_fitted.size().x + 14.0, 42.0, max_r_w);
                let r_pill = Rect::from_min_size(
                    Pos2::new(hud_bounds_rect.max.x - r_width - 6.0, hud_bounds_rect.min.y + 6.0),
                    Vec2::new(r_width, 21.0),
                );
                Some((r_pill, r_fitted, r_fg, r_stroke))
            } else {
                None
            };

        // Top-Left Translucent Pill (Status Dot + Channel-Prefixed Camera Name + HD or HIST tag)
        let default_cam_name = format!("Camera {}", channel);
        let tl_cam_label = if raw_cam_name != default_cam_name {
            format!("{:02} {}", channel, raw_cam_name)
        } else {
            raw_cam_name.clone()
        };
        let is_transitioning = !is_playback && target_sub != active_sub && !self.all_paused;
        let badge_info: Option<(String, Color32)> = if is_playback {
            let ts_str = epoch_ms_to_local_naive(pb_epoch_ms)
                .format("HIST %H:%M:%S")
                .to_string();
            Some((ts_str, STATUS_AMBER))
        } else if is_transitioning {
            let alpha = (145.0 + pulse * 110.0) as u8;
            let col = Color32::from_rgba_unmultiplied(245, 158, 11, alpha);
            Some((
                if target_sub == 0 {
                    "HD...".to_string()
                } else {
                    "SD...".to_string()
                },
                col,
            ))
        } else if active_sub == 0 {
            Some(("HD".to_string(), STATUS_ICE))
        } else {
            None
        };

        let name_font = FontId::new(11.5, FontFamily::Proportional);
        let name_col = Color32::from_rgb(228, 228, 231);
        let badge_font = FontId::new(10.5, FontFamily::Monospace);
        let full_name_galley =
            painter.layout_no_wrap(tl_cam_label.clone(), name_font.clone(), name_col);
        let badge_galley = badge_info
            .as_ref()
            .map(|(b_txt, b_col)| painter.layout_no_wrap(b_txt.clone(), badge_font.clone(), *b_col));
        let badge_reserved_w = badge_galley
            .as_ref()
            .map(|g| g.size().x + 14.0)
            .unwrap_or(6.0);
        let desired_tl_w = 17.5 + full_name_galley.size().x + badge_reserved_w;
        let max_tl_avail = if let Some((ref r_pill, ..)) = right_pill_spec {
            if hud_bounds_rect.width() >= 136.0 {
                (r_pill.min.x - 6.0 - (hud_bounds_rect.min.x + 6.0)).max(64.0)
            } else {
                hud_bounds_rect.width() - 12.0
            }
        } else {
            hud_bounds_rect.width() - 12.0
        };
        let tl_w = safe_clamp_f32(
            desired_tl_w,
            64.0,
            max_tl_avail.min(hud_bounds_rect.width() - 12.0),
        );
        let tl_pill = Rect::from_min_size(
            hud_bounds_rect.min + Vec2::new(6.0, 6.0),
            Vec2::new(tl_w, 21.0),
        );

        painter.rect_filled(
            tl_pill,
            Rounding::same(5.0),
            Color32::from_rgba_unmultiplied(9, 9, 11, 168),
        );

        let dot_color = if is_playback {
            match pb_status {
                PlaybackStatus::Playing => STATUS_EMERALD,
                PlaybackStatus::Buffering | PlaybackStatus::Idle => {
                    ui.ctx().request_repaint_after(Duration::from_millis(60));
                    let a = (130.0 + pulse * 125.0) as u8;
                    Color32::from_rgba_unmultiplied(STATUS_ICE.r(), STATUS_ICE.g(), STATUS_ICE.b(), a)
                }
                PlaybackStatus::Paused => STATUS_AMBER,
                PlaybackStatus::EndOfRecording => ZINC_DIM,
                PlaybackStatus::Error | PlaybackStatus::AuthError => STATUS_CORAL,
            }
        } else {
            match live_status {
                StreamStatus::Live => STATUS_EMERALD,
                StreamStatus::Connecting | StreamStatus::Reconnecting | StreamStatus::Paused => {
                    STATUS_AMBER
                }
                StreamStatus::Error | StreamStatus::AuthError => STATUS_CORAL,
            }
        };
        painter.circle_filled(
            Pos2::new(tl_pill.min.x + 9.5, tl_pill.center().y),
            3.5,
            dot_color,
        );

        let name_min_x = tl_pill.min.x + 17.5;
        let name_max_x = (tl_pill.max.x - badge_reserved_w).max(name_min_x);
        let max_name_w = (name_max_x - name_min_x).max(0.0);
        if max_name_w > 4.0 {
            let fitted_name = if full_name_galley.size().x <= max_name_w {
                full_name_galley
            } else {
                truncate_galley_with_ellipsis(
                    &painter,
                    &tl_cam_label,
                    name_font,
                    name_col,
                    max_name_w,
                )
            };
            let clip_r = Rect::from_min_max(
                Pos2::new(name_min_x, tl_pill.min.y),
                Pos2::new(name_max_x, tl_pill.max.y),
            )
            .intersect(rect);
            let name_pos = Pos2::new(
                name_min_x,
                tl_pill.center().y - fitted_name.size().y * 0.5,
            );
            painter
                .with_clip_rect(clip_r)
                .galley(name_pos, fitted_name, name_col);
        }
        if let Some((ref b_txt, b_col)) = badge_info {
            if tl_pill.width() >= badge_reserved_w + 18.0 {
                painter.text(
                    Pos2::new(tl_pill.max.x - 6.0, tl_pill.center().y),
                    Align2::RIGHT_CENTER,
                    b_txt,
                    badge_font,
                    b_col,
                );
            }
        }

        // Paint Top-Right Telemetry / Paused Pill
        if let Some((right_pill, r_galley, r_fg, r_stroke)) = right_pill_spec {
            if right_pill.min.x > tl_pill.max.x + 4.0 {
                painter.rect_filled(
                    right_pill,
                    Rounding::same(5.0),
                    Color32::from_rgba_unmultiplied(9, 9, 11, 175),
                );
                painter.rect_stroke(right_pill, Rounding::same(5.0), Stroke::new(1.0_f32, r_stroke));
                painter.galley(
                    Pos2::new(
                        right_pill.center().x - r_galley.size().x * 0.5,
                        right_pill.center().y - r_galley.size().y * 0.5,
                    ),
                    r_galley,
                    r_fg,
                );
            }
        }

        if show_active_hud {
            // Bottom-Left Interactive Wireframe Zoom-Radar Minimap (Click or Drag to jump pan)
            if radar_visible {
                let radar_rounding = if is_fullscreen { 4.5 } else { 3.0 };
                let radar_active = over_radar || (started_on_radar && primary_down);
                painter.rect_filled(
                    radar_rect,
                    Rounding::same(radar_rounding),
                    Color32::from_rgba_unmultiplied(9, 9, 11, 195),
                );
                if is_fullscreen {
                    let subtle_grid =
                        Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 13));
                    painter.vline(
                        radar_rect.center().x,
                        (radar_rect.min.y + 2.0)..=(radar_rect.max.y - 2.0),
                        subtle_grid,
                    );
                    painter.hline(
                        (radar_rect.min.x + 2.0)..=(radar_rect.max.x - 2.0),
                        radar_rect.center().y,
                        subtle_grid,
                    );
                }
                painter.rect_stroke(
                    radar_rect,
                    Rounding::same(radar_rounding),
                    Stroke::new(
                        1.0_f32,
                        if radar_active {
                            STATUS_ICE.gamma_multiply(0.8)
                        } else {
                            ZINC_DIM
                        },
                    ),
                );
                let inner_min = Pos2::new(
                    radar_rect.min.x + render_uv.min.x * radar_rect.width(),
                    radar_rect.min.y + render_uv.min.y * radar_rect.height(),
                );
                let inner_max = Pos2::new(
                    radar_rect.min.x + render_uv.max.x * radar_rect.width(),
                    radar_rect.min.y + render_uv.max.y * radar_rect.height(),
                );
                let inner_uv = Rect::from_min_max(inner_min, inner_max);
                painter.rect_filled(
                    inner_uv,
                    Rounding::same(1.5),
                    Color32::from_rgba_unmultiplied(
                        56,
                        189,
                        248,
                        if radar_active { 52 } else { 36 },
                    ),
                );
                painter.rect_stroke(
                    inner_uv,
                    Rounding::same(1.5),
                    Stroke::new(1.2_f32, STATUS_ICE),
                );
            }

            // Bottom-Right Unified Floating Glass Control Dock
            if dock_visible {
                painter.rect_filled(
                    dock_rect,
                    Rounding::same(7.0),
                    Color32::from_rgba_unmultiplied(17, 17, 20, 222),
                );
                painter.rect_stroke(
                    dock_rect,
                    Rounding::same(7.0),
                    Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 28)),
                );

                if show_zoom_controls {
                    if show_box_btn {
                        paint_dock_micro_btn(
                            &painter,
                            box_rect,
                            "Box",
                            over_box,
                            box_armed_now || shift_held,
                            STATUS_ICE,
                        );
                    }
                    paint_dock_micro_btn(&painter, z_out_rect, "-", over_z_out, false, STATUS_ICE);
                    let z_label = format!("{:.1}x", zoom_val);
                    paint_dock_micro_btn(
                        &painter,
                        z_rst_rect,
                        &z_label,
                        over_z_rst,
                        zoom_val > 1.01,
                        STATUS_ICE,
                    );
                    paint_dock_micro_btn(&painter, z_in_rect, "+", over_z_in, false, STATUS_ICE);

                    let sep_x = z_in_rect.max.x + sep_w * 0.5;
                    painter.vline(
                        sep_x,
                        (dock_rect.min.y + 5.0)..=(dock_rect.max.y - 5.0),
                        Stroke::new(1.0_f32, hairline_stroke()),
                    );
                }

                if is_playback {
                    paint_dock_micro_btn(&painter, sd_hd_rect, "Snap", over_sd_hd, false, STATUS_ICE);
                    if !is_fullscreen {
                        paint_dock_micro_btn(
                            &painter,
                            expand_rect,
                            "1-Cam",
                            over_expand,
                            true,
                            STATUS_ICE,
                        );
                    } else {
                        paint_dock_micro_btn(
                            &painter,
                            expand_rect,
                            "Live",
                            over_expand,
                            true,
                            STATUS_EMERALD,
                        );
                    }
                } else {
                    paint_dock_micro_btn(
                        &painter,
                        hist_rect,
                        "Hist",
                        over_hist,
                        false,
                        STATUS_AMBER,
                    );

                    let (qual_label, qual_active, qual_accent) = if is_transitioning {
                        (
                            if target_sub == 0 { "HD.." } else { "SD.." },
                            true,
                            STATUS_AMBER,
                        )
                    } else if target_sub == 0 {
                        ("HD", true, STATUS_ICE)
                    } else {
                        ("SD", false, STATUS_ICE)
                    };
                    paint_dock_micro_btn(
                        &painter,
                        sd_hd_rect,
                        qual_label,
                        over_sd_hd,
                        qual_active,
                        qual_accent,
                    );

                    let exp_label = if is_fullscreen { "Grid" } else { "Expand" };
                    paint_dock_micro_btn(
                        &painter,
                        expand_rect,
                        exp_label,
                        over_expand,
                        is_fullscreen,
                        STATUS_ICE,
                    );
                }
            }
        }

        // Movable / Hideable Live Picture-in-Picture (PiP) Corner Window during 1-Cam Playback
        if let Some(pr) = pip_rect {
            let (live_tex, live_tw, live_th) = self
                .tiles
                .get(&channel)
                .map(|t| (t.texture.clone(), t.tex_width, t.tex_height))
                .unwrap_or((None, 0, 0));
            let pip_hovered_pos = if !modal_open && over_pip {
                pointer_pos
            } else {
                None
            };
            let pip_clicked_pos = if !modal_open && over_pip && response.clicked() {
                pointer_pos
            } else {
                None
            };
            if let Some(pip_action) = render_live_pip_overlay(
                ui,
                pr,
                live_tex.as_ref(),
                live_tw,
                live_th,
                self.app_cfg.aspect_fit_16_9,
                pip_live_status,
                pip_hovered_pos,
                pip_clicked_pos,
            ) {
                self.last_tile_hud_click_at = Some(Instant::now());
                match pip_action {
                    LivePipAction::ReturnToLive => {
                        self.exit_playback_to_live();
                        return;
                    }
                    LivePipAction::CycleCorner => {
                        let next = self.timeline_ui.pip_corner.next_visible_corner();
                        self.timeline_ui.pip_corner = next;
                        self.show_toast_kind(
                            ToastKind::Info,
                            format!("Live PiP moved to {}", next.label()),
                        );
                    }
                    LivePipAction::Hide => {
                        self.timeline_ui.pip_corner = LivePipCorner::Hidden;
                        self.show_toast_kind(
                            ToastKind::Info,
                            "Live PiP hidden (restore from timeline bar or right-click menu)",
                        );
                    }
                }
            }
        }

        // Tile Border: Selected (1.5px Amber in Sync Playback / Soft Ice in Live) > Hovered > Idle
        let is_selected_in_grid = !is_fullscreen && self.selected_channel == Some(channel);
        let (border_w, border_col) = if is_playback && is_selected_in_grid {
            (1.5_f32, STATUS_AMBER)
        } else if is_selected_in_grid {
            (1.5_f32, STATUS_ICE)
        } else if is_hovered {
            (1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 65))
        } else {
            (1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 18))
        };
        painter.rect_stroke(rect, Rounding::same(6.0), Stroke::new(border_w, border_col));

        // Right-Click Context Menu (allow an already-open menu to finish rendering, block new ones when modal/overlay active)
        let block_new_ctx_menu = self.show_settings
            || self.show_shortcuts_modal
            || self.rename_modal_channel.is_some()
            || self.timeline_ui.is_any_popup_open()
            || ui.ctx().memory(|m| m.any_popup_open())
            || over_floating_bar;
        if !block_new_ctx_menu || response.context_menu_opened() {
            let all_channels = self.app_cfg.channel_order.clone();
            let num_cams = self.app_cfg.num_cameras;
            let any_zoomed = self
                .tiles
                .values()
                .any(|t| t.viewport(is_playback).zoom > 1.01);

            response.context_menu(|ui| {
                ui.label(
                    RichText::new(format!("{} (Channel {:02})", raw_cam_name, channel)).strong(),
                );
                ui.separator();

                if is_playback {
                    if ui.button("Return to Live Edge [T / Esc]").clicked() {
                        ui.close_menu();
                        self.exit_playback_to_live();
                    }
                    if !self.env_cfg.is_cloud_p2p_active()
                        && num_cams >= 2
                        && ui
                            .button(if self.playback_mgr.is_sync_2x2() {
                                "Focus 1-Cam 1080p Playback [Enter]"
                            } else {
                                "Switch to 2x2 Sync Playback [TTT]"
                            })
                            .clicked()
                    {
                        ui.close_menu();
                        if self.playback_mgr.is_sync_2x2() {
                            self.enter_playback_single(channel, true);
                        } else {
                            self.enter_playback_sync_2x2();
                        }
                    }

                    if !self.playback_mgr.is_sync_2x2() && !self.env_cfg.is_cloud_p2p_active() {
                        ui.menu_button("Show / Move Live PiP", |ui| {
                            for corner in [
                                LivePipCorner::TopRight,
                                LivePipCorner::BottomLeft,
                                LivePipCorner::TopLeft,
                                LivePipCorner::Hidden,
                            ] {
                                if ui
                                    .selectable_label(
                                        self.timeline_ui.pip_corner == corner,
                                        corner.label(),
                                    )
                                    .clicked()
                                {
                                    ui.close_menu();
                                    self.timeline_ui.pip_corner = corner;
                                }
                            }
                        });
                    }

                    if self.playback_mgr.is_sync_2x2() {
                        let active_sync = self.playback_mgr.active_channels().to_vec();
                        if num_cams > active_sync.len() {
                            ui.menu_button("Replace Slot Camera With...", |ui| {
                                egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                                    for cand_ch in 1..=num_cams {
                                        if !active_sync.contains(&cand_ch) {
                                            let lbl = format!(
                                                "Channel {:02} ({})",
                                                cand_ch,
                                                self.app_cfg.camera_display_name(cand_ch)
                                            );
                                            if ui.button(lbl).clicked() {
                                                ui.close_menu();
                                                for ch_reset in [channel, cand_ch] {
                                                    if let Some(t) = self.tiles.get_mut(&ch_reset) {
                                                        t.pb_texture = None;
                                                        t.pb_last_uploaded_seq = 0;
                                                        t.pb.clear_transient_drag();
                                                    }
                                                }
                                                self.playback_mgr
                                                    .replace_sync_channel(channel, cand_ch);
                                                if self.selected_channel == Some(channel) {
                                                    self.selected_channel = Some(cand_ch);
                                                }
                                                self.timeline_ui.auto_scroll_to_active_event = true;
                                                if self.playback_mgr.primary_channel()
                                                    == Some(cand_ch)
                                                {
                                                    let date = self.timeline_ui.selected_date;
                                                    self.playback_mgr
                                                        .request_day_timeline(cand_ch, date, false);
                                                    self.playback_mgr.request_month_days(
                                                        cand_ch,
                                                        date.year(),
                                                        date.month(),
                                                    );
                                                }
                                                self.show_toast_kind(
                                                    ToastKind::Info,
                                                    format!(
                                                        "Replaced Ch {} with Ch {} in 2x2 Sync",
                                                        channel, cand_ch
                                                    ),
                                                );
                                            }
                                        }
                                    }
                                });
                            });
                        }
                        if active_sync.len() >= 2 {
                            ui.menu_button("Swap 2x2 Tile With...", |ui| {
                                for &other_ch in &active_sync {
                                    if other_ch != channel {
                                        let lbl = format!(
                                            "Channel {:02} ({})",
                                            other_ch,
                                            self.app_cfg.camera_display_name(other_ch)
                                        );
                                        if ui.button(lbl).clicked() {
                                            ui.close_menu();
                                            self.playback_mgr.swap_sync_channels(channel, other_ch);
                                            self.show_toast_kind(
                                                ToastKind::Info,
                                                format!(
                                                    "Swapped Ch {} and Ch {} in 2x2 Sync",
                                                    channel, other_ch
                                                ),
                                            );
                                        }
                                    }
                                }
                            });
                        }
                    }
                    ui.separator();
                } else {
                    if ui.button("View Playback History (1080p + Timeline) [T]").clicked() {
                        ui.close_menu();
                        self.enter_playback_single(channel, false);
                    }
                    if !self.env_cfg.is_cloud_p2p_active()
                        && num_cams >= 2
                        && ui.button("Open 2x2 Synchronized Playback [TTT]").clicked()
                    {
                        ui.close_menu();
                        self.selected_channel = Some(channel);
                        self.enter_playback_sync_2x2();
                    }
                    ui.separator();
                }

                if zoom_val > 1.02 {
                    if ui.button("Reset Camera Zoom (1.0x) [Z / Esc]").clicked() {
                        ui.close_menu();
                        if let Some(t) = self.tiles.get_mut(&channel) {
                            t.viewport_mut(is_playback).animate_reset_zoom();
                        }
                    }
                    if has_zoom_history && ui.button("Step Back Previous Zoom [Backspace]").clicked()
                    {
                        ui.close_menu();
                        if let Some(t) = self.tiles.get_mut(&channel) {
                            t.viewport_mut(is_playback).step_back_zoom();
                        }
                    }
                    if has_texture && ui.button("Arm Nested Box Zoom [Shift+Drag]").clicked() {
                        ui.close_menu();
                        if let Some(t) = self.tiles.get_mut(&channel) {
                            t.viewport_mut(is_playback).box_zoom_armed = true;
                        }
                    }
                    ui.separator();
                }

                if !is_playback {
                    if ui
                        .button(if is_fullscreen {
                            "Return to Grid View [Enter / Esc]"
                        } else {
                            "Expand Single View [Enter / Double-Click]"
                        })
                        .clicked()
                    {
                        ui.close_menu();
                        if is_fullscreen {
                            self.exit_fullscreen();
                        } else {
                            self.enter_fullscreen(channel);
                        }
                    }

                    let cur_target = self
                        .stream_mgr
                        .slot(channel)
                        .map(|s| s.target_subtype.load(Ordering::Relaxed))
                        .unwrap_or(1);
                    if ui
                        .button(if cur_target == 0 {
                            "Switch to SD (Sub-Stream) [S]"
                        } else {
                            "Switch to HD (Main-Stream) [H]"
                        })
                        .clicked()
                    {
                        ui.close_menu();
                        self.set_camera_subtype(channel, if cur_target == 0 { 1 } else { 0 });
                    }
                }

                if zoom_val > 1.02 {
                    let zoom_snap_lbl = if is_playback || is_fullscreen {
                        "Save Zoomed Region Snapshot (PNG) [C]"
                    } else {
                        "Save Zoomed Region Snapshot (PNG)"
                    };
                    if ui.button(zoom_snap_lbl).clicked() {
                        ui.close_menu();
                        self.save_camera_snapshot(channel, true);
                    }
                    if ui.button("Save Full-Frame Snapshot (PNG)").clicked() {
                        ui.close_menu();
                        self.save_camera_snapshot(channel, false);
                    }
                } else {
                    let cam_snap_lbl = if is_playback || is_fullscreen {
                        "Save Camera Snapshot (PNG) [C]"
                    } else {
                        "Save Camera Snapshot (PNG)"
                    };
                    if ui.button(cam_snap_lbl).clicked() {
                        ui.close_menu();
                        self.save_camera_snapshot(channel, false);
                    }
                }

                if !is_playback
                    && !is_fullscreen
                    && num_cams >= 2
                    && ui.button("Save Wall Snapshot (PNG) [C]").clicked()
                {
                    ui.close_menu();
                    self.save_composite_wall_snapshot();
                }

                if !is_playback && ui.button("Re-Sync / Reconnect Camera").clicked() {
                    ui.close_menu();
                    self.auth_gw.circuit_breaker().reset();
                    if let Some(slot) = self.stream_mgr.slot(channel) {
                        slot.trigger_resync();
                    }
                    self.show_toast_kind(
                        ToastKind::Info,
                        format!("Reconnecting {}...", raw_cam_name),
                    );
                }

                if any_zoomed && ui.button("Reset Zoom on All Cameras [ZZZ]").clicked() {
                    ui.close_menu();
                    self.reset_all_zoom();
                }

                if ui.button("Rename Camera...").clicked() {
                    ui.close_menu();
                    self.rename_modal_channel = Some(channel);
                    self.rename_input = raw_cam_name.clone();
                    self.rename_just_opened = true;
                }

                if !is_playback && num_cams >= 2 {
                    ui.menu_button("Swap Tile Position With...", |ui| {
                        egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                            for other_ch in &all_channels {
                                if *other_ch != channel {
                                    let label = format!(
                                        "Channel {:02} ({})",
                                        other_ch,
                                        self.app_cfg.camera_display_name(*other_ch)
                                    );
                                    if ui.button(label).clicked() {
                                        ui.close_menu();
                                        self.app_cfg.swap_channels(channel, *other_ch);
                                        self.edit_app_cfg.channel_order =
                                            self.app_cfg.channel_order.clone();
                                        let _ = self.app_cfg.save();
                                        self.show_toast_kind(
                                            ToastKind::Info,
                                            format!(
                                                "Swapped Channel {} and Channel {}",
                                                channel, other_ch
                                            ),
                                        );
                                    }
                                }
                            }
                        });
                        if all_channels.iter().copied().ne(1..=num_cams) {
                            ui.separator();
                            if ui
                                .button(format!("Reset Tile Order (1..{})", num_cams))
                                .clicked()
                            {
                                ui.close_menu();
                                self.app_cfg.reset_channel_order();
                                self.edit_app_cfg.channel_order =
                                    self.app_cfg.channel_order.clone();
                                let _ = self.app_cfg.save();
                                self.show_toast_kind(
                                    ToastKind::Info,
                                    format!("Reset camera tile order (1..{})", num_cams),
                                );
                            }
                        }
                    });
                }
            });
        }
    }

    fn render_modal_backdrop_scrim(&self, ctx: &egui::Context) {
        if !(self.show_settings
            || self.show_shortcuts_modal
            || self.rename_modal_channel.is_some())
        {
            return;
        }
        egui::Area::new(egui::Id::new("obsidian_modal_backdrop_scrim"))
            .order(egui::Order::Middle)
            .fixed_pos(Pos2::ZERO)
            .show(ctx, |ui| {
                let screen = ctx.screen_rect();
                let (rect, _resp) =
                    ui.allocate_exact_size(screen.size(), Sense::click_and_drag());
                ui.painter().rect_filled(
                    rect,
                    Rounding::ZERO,
                    Color32::from_rgba_unmultiplied(0, 0, 0, 145),
                );
            });
    }

    fn render_onboarding_card(&mut self, ui: &mut Ui, avail: Rect) {
        let card_w = (avail.width() - 32.0).clamp(260.0, 560.0);
        let card_h = 276.0_f32.min((avail.height() - 24.0).max(180.0));
        let card_rect = Rect::from_center_size(avail.center(), Vec2::new(card_w, card_h));

        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(card_rect), |ui| {
            egui::Frame::none()
                .fill(GRAPHITE_SURFACE)
                .stroke(Stroke::new(
                    1.0_f32,
                    Color32::from_rgba_unmultiplied(56, 189, 248, 85),
                ))
                .rounding(Rounding::same(12.0))
                .inner_margin(egui::Margin::symmetric(24.0, 20.0))
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new("Welcome to NVR Command Center v3.0")
                                .size(19.0)
                                .strong()
                                .color(TEXT_PRIMARY),
                        );
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(
                                "Native Rust RTSP surveillance wall, GPU e-PTZ zoom, and 1080p AI timeline playback.",
                            )
                            .size(12.5)
                            .color(TEXT_SECONDARY),
                        );
                    });

                    ui.add_space(12.0);
                    egui::Frame::none()
                        .fill(ZINC_ELEVATED)
                        .stroke(Stroke::new(1.0_f32, hairline_stroke()))
                        .rounding(Rounding::same(8.0))
                        .inner_margin(egui::Margin::symmetric(14.0, 10.0))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(
                                    "- Local LAN Direct: Sub-second multi-camera live wall + 2x2 synchronized history\n\
                                     - Cloud P2P & Tailscale: Encrypted remote live streams & playback from any network\n\
                                     - Local-Only Credentials: Saved with 0600 permissions behind loopback AuthGateway",
                                )
                                .size(11.5)
                                .color(TEXT_MUTED),
                            );
                        });

                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        let cta = egui::Button::new(
                            RichText::new("Configure NVR Connection (Ctrl+,)")
                                .size(13.0)
                                .strong()
                                .color(TEXT_PRIMARY),
                        )
                        .fill(status_ice_fill())
                        .stroke(Stroke::new(1.2_f32, STATUS_ICE))
                        .rounding(Rounding::same(8.0))
                        .min_size(Vec2::new(240.0, 34.0));

                        if ui.add(cta).clicked() {
                            self.open_settings_modal(SettingsTab::NvrAndCameras);
                        }

                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("Keyboard Shortcuts (?)")
                                        .size(12.0)
                                        .color(TEXT_SECONDARY),
                                )
                                .rounding(Rounding::same(8.0))
                                .min_size(Vec2::new(150.0, 34.0)),
                            )
                            .clicked()
                        {
                            self.show_shortcuts_modal = true;
                        }
                    });
                });
        });
    }

    fn render_toast_stack(&mut self, ctx: &egui::Context, dock_shown: bool) {
        if self.toasts.is_empty() {
            return;
        }

        let now = Instant::now();
        let screen = ctx.screen_rect();
        let base_bottom_y = if dock_shown {
            screen.max.y - TIMELINE_DOCK_HEIGHT - 12.0
        } else {
            screen.max.y - 14.0
        };
        let max_toast_w = (screen.width() - 28.0).clamp(180.0, 680.0);
        let toast_h = 30.0_f32;
        let gap_y = 6.0_f32;

        let mut dismiss_ids: Vec<u64> = Vec::new();
        let mut dir_to_open: Option<PathBuf> = None;
        let mut any_hovered = false;

        let count = self.toasts.len().min(MAX_VISIBLE_TOASTS);
        for idx in 0..count {
            let stack_slot = count - 1 - idx; // newest at bottom, older stacked above
            let bottom_y = base_bottom_y - stack_slot as f32 * (toast_h + gap_y);
            let center_y = bottom_y - toast_h * 0.5;

            let (toast_id, kind, msg_str, open_dir_opt) = {
                let item = &self.toasts[idx];
                (
                    item.id,
                    item.kind,
                    item.message.clone(),
                    item.open_dir.clone(),
                )
            };

            let area_id = egui::Id::new(("obsidian_toast_pill", toast_id));
            egui::Area::new(area_id)
                .order(egui::Order::Tooltip)
                .pivot(Align2::CENTER_TOP)
                .fixed_pos(Pos2::new(screen.center().x, center_y - toast_h * 0.5))
                .show(ctx, |ui| {
                    let msg_font = FontId::new(12.0, FontFamily::Proportional);
                    let btn_font = FontId::new(11.0, FontFamily::Proportional);
                    let accent = kind.accent_color();

                    let open_btn_w = if open_dir_opt.is_some() {
                        let g = ui.painter().layout_no_wrap(
                            "Open Folder".to_string(),
                            btn_font.clone(),
                            STATUS_ICE,
                        );
                        g.size().x + 14.0
                    } else {
                        0.0
                    };
                    let close_btn_w = 20.0_f32;
                    let right_controls_w =
                        close_btn_w + 6.0 + if open_btn_w > 0.0 { open_btn_w + 6.0 } else { 0.0 };

                    let full_msg_gal = ui.painter().layout_no_wrap(
                        msg_str.clone(),
                        msg_font.clone(),
                        TEXT_PRIMARY,
                    );
                    let desired_w = 22.0 + full_msg_gal.size().x + 10.0 + right_controls_w;
                    let pill_w = safe_clamp_f32(desired_w, 168.0, max_toast_w);

                    let (pill_rect, resp) =
                        ui.allocate_exact_size(Vec2::new(pill_w, toast_h), Sense::click());
                    let hovered = resp.hovered();
                    if hovered {
                        any_hovered = true;
                    }

                    let ptr = ui.input(|i| i.pointer.interact_pos().or_else(|| i.pointer.hover_pos()));
                    let close_rect = Rect::from_center_size(
                        Pos2::new(pill_rect.max.x - 13.0, pill_rect.center().y),
                        Vec2::new(18.0, 18.0),
                    );
                    let open_rect_opt = if open_btn_w > 0.0 {
                        Some(Rect::from_min_size(
                            Pos2::new(
                                close_rect.min.x - 5.0 - open_btn_w,
                                pill_rect.center().y - 10.0,
                            ),
                            Vec2::new(open_btn_w, 20.0),
                        ))
                    } else {
                        None
                    };

                    let over_close = hovered && ptr.is_some_and(|p| close_rect.contains(p));
                    let over_open = hovered
                        && open_rect_opt
                            .zip(ptr)
                            .is_some_and(|(or, p)| or.contains(p));
                    if over_close || over_open {
                        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                    }

                    let painter = ui.painter();
                    painter.rect_filled(
                        pill_rect,
                        Rounding::same(8.0),
                        Color32::from_rgba_unmultiplied(17, 17, 20, 244),
                    );
                    painter.rect_stroke(
                        pill_rect,
                        Rounding::same(8.0),
                        Stroke::new(
                            1.0_f32,
                            accent.gamma_multiply(if hovered { 0.9 } else { 0.65 }),
                        ),
                    );
                    painter.circle_filled(
                        Pos2::new(pill_rect.min.x + 11.5, pill_rect.center().y),
                        3.8,
                        accent,
                    );

                    let text_min_x = pill_rect.min.x + 21.0;
                    let text_max_x = open_rect_opt
                        .map(|r| r.min.x - 7.0)
                        .unwrap_or(close_rect.min.x - 6.0)
                        .max(text_min_x);
                    let avail_txt_w = (text_max_x - text_min_x).max(12.0);
                    let fitted_msg = if full_msg_gal.size().x <= avail_txt_w {
                        full_msg_gal
                    } else {
                        truncate_galley_with_ellipsis(
                            painter,
                            &msg_str,
                            msg_font,
                            TEXT_PRIMARY,
                            avail_txt_w,
                        )
                    };
                    let clip_r = Rect::from_min_max(
                        Pos2::new(text_min_x, pill_rect.min.y),
                        Pos2::new(text_max_x, pill_rect.max.y),
                    );
                    painter.with_clip_rect(clip_r).galley(
                        Pos2::new(
                            text_min_x,
                            pill_rect.center().y - fitted_msg.size().y * 0.5,
                        ),
                        fitted_msg,
                        TEXT_PRIMARY,
                    );

                    if let Some(or) = open_rect_opt {
                        painter.rect_filled(
                            or,
                            Rounding::same(5.0),
                            if over_open {
                                Color32::from_rgba_unmultiplied(56, 189, 248, 58)
                            } else {
                                status_ice_fill()
                            },
                        );
                        painter.rect_stroke(
                            or,
                            Rounding::same(5.0),
                            Stroke::new(1.0_f32, STATUS_ICE.gamma_multiply(0.85)),
                        );
                        painter.text(
                            or.center(),
                            Align2::CENTER_CENTER,
                            "Open Folder",
                            btn_font,
                            TEXT_PRIMARY,
                        );
                    }

                    if over_close {
                        painter.rect_filled(close_rect, Rounding::same(4.0), ZINC_HOVER);
                    }
                    painter.text(
                        close_rect.center(),
                        Align2::CENTER_CENTER,
                        "x",
                        FontId::new(11.0, FontFamily::Monospace),
                        if over_close { TEXT_PRIMARY } else { TEXT_MUTED },
                    );

                    if resp.clicked() {
                        if over_close {
                            dismiss_ids.push(toast_id);
                        } else if over_open {
                            dir_to_open = open_dir_opt.clone();
                            dismiss_ids.push(toast_id);
                        }
                    }

                    if let Some(item_mut) = self.toasts.iter_mut().find(|t| t.id == toast_id) {
                        let dt = now.saturating_duration_since(item_mut.last_tick);
                        item_mut.last_tick = now;
                        if !hovered {
                            item_mut.remaining = item_mut.remaining.saturating_sub(dt);
                        }
                    }
                });
        }

        self.toasts
            .retain(|t| !t.remaining.is_zero() && !dismiss_ids.contains(&t.id));

        if let Some(dir) = dir_to_open {
            self.open_local_directory(ctx, &dir, false);
        }
        if !self.toasts.is_empty() || any_hovered {
            ctx.request_repaint_after(Duration::from_millis(80));
        }
    }

    fn render_shortcuts_modal(&mut self, ctx: &egui::Context) {
        if !self.show_shortcuts_modal {
            return;
        }

        let mut open = self.show_shortcuts_modal;
        let max_scroll_h = (ctx.screen_rect().height() - 150.0).max(200.0);

        egui::Window::new("Keyboard Shortcuts")
            .order(egui::Order::Foreground)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .default_width(690.0)
            .frame(
                egui::Frame::window(&ctx.style())
                    .fill(GRAPHITE_SURFACE)
                    .stroke(Stroke::new(
                        1.0_f32,
                        Color32::from_rgba_unmultiplied(255, 255, 255, 28),
                    ))
                    .rounding(Rounding::same(10.0))
                    .inner_margin(egui::Margin::same(16.0)),
            )
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "Single-tap targets the hovered camera (or keyboard-selected / fullscreen camera). Triple-tap within 450ms applies to the entire wall.",
                    )
                    .size(11.5)
                    .color(TEXT_MUTED),
                );
                ui.add_space(8.0);

                egui::ScrollArea::vertical()
                    .id_salt("shortcuts_modal_scroll")
                    .max_height(max_scroll_h)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.columns(2, |cols| {
                            // Left Column: Stream Quality + Zoom & Pan + Playback
                            render_shortcut_section(
                                &mut cols[0],
                                "Stream Quality",
                                &[
                                    (&["S"], "Switch active camera to SD"),
                                    (&["S S S", "Shift+S"], "Switch ALL cameras to SD"),
                                    (&["H"], "Switch active camera to HD"),
                                    (&["H H H", "Shift+H"], "Switch ALL cameras to HD"),
                                ],
                            );
                            cols[0].add_space(8.0);
                            render_shortcut_section(
                                &mut cols[0],
                                "Zoom & Pan (Independent Live / Playback e-PTZ)",
                                &[
                                    (&["Drag Box"], "Area Box Zoom at 1.0x (up to 8.0x)"),
                                    (&["Shift+Drag", "Box"], "Nested Area Box Zoom when zoomed"),
                                    (&["Scroll", "Pinch"], "Cursor-anchored zoom (1x - 8x)"),
                                    (&["+", "=", "-"], "Step zoom (or timeline zoom over dock)"),
                                    (&["Backspace"], "Step back to previous box zoom"),
                                    (&["Z", "Mid-Click"], "Smooth reset camera zoom (1.0x)"),
                                    (&["Z Z Z", "Shift+Z"], "Reset zoom on ALL cameras"),
                                    (&["Drag", "Radar"], "Pan viewport or click radar map"),
                                ],
                            );
                            cols[0].add_space(8.0);
                            render_shortcut_section(
                                &mut cols[0],
                                "Playback & AI Timeline",
                                &[
                                    (&["T"], "Toggle 1080p History Playback (+ Live PiP)"),
                                    (&["T T T", "Shift+T"], "Toggle 2x2 Synchronized Playback"),
                                    (&["J", "K", "L"], "Step slower/reverse, pause, faster (-4x..16x)"),
                                    (&[",", "."], "Instant 0ms single-frame step back / fwd"),
                                    (&["Left", "Right"], "Seek -10s/+10s (Shift: -60s/+60s)"),
                                    (&["[", "]", "Shift+Click"], "Prev / Next event / Snap to event"),
                                    (&["G", "E", "R"], "Smart Skip / Event Drawer / Refresh"),
                                    (&["Ctrl+G"], "Jump to exact HH:MM:SS timestamp"),
                                    (&["I", "O", "Ctrl+E"], "Mark Clip In / Out / Export MP4"),
                                ],
                            );

                            // Right Column: Grid & Camera Focus + Wall & App Controls
                            render_shortcut_section(
                                &mut cols[1],
                                "Grid & Camera Focus",
                                &[
                                    (&["Hover", "Arrows"], "Target hovered cam or 2D grid select"),
                                    (&["Enter", "F"], "Toggle Single View (or 2x2 in History)"),
                                    (&["[", "]"], "Prev / Next camera in Live Grid/Single"),
                                    (&["1-9", "F1-F10, F12"], "Direct camera focus (Live or History)"),
                                    (&["Shift+F11"], "Direct Camera 11 focus"),
                                    (&["0"], "Reset zoom if zoomed, else Camera 10"),
                                ],
                            );
                            cols[1].add_space(8.0);
                            render_shortcut_section(
                                &mut cols[1],
                                "Wall, Zen & App Controls",
                                &[
                                    (&["B"], "Borderless Zen Wall (top/bottom edge peek)"),
                                    (&["F11"], "OS Monitor Fullscreen (floating auto-hide)"),
                                    (&["A"], "Toggle 16:9 Fit vs Center-Crop Fill"),
                                    (&["C", "Ctrl+S"], "Save PNG Snapshot (+ Shutter Flash)"),
                                    (&["M"], "Toggle Patrol Mode (holds while zoomed)"),
                                    (&["Space", "K", "P"], "Pause / Resume Live or Playback"),
                                    (&["R", "Shift+R"], "Re-Sync Live Edge (Shift+R in History)"),
                                    (&["Ctrl+,", "?"], "Open Settings / Toggle Shortcuts"),
                                    (&["Esc"], "Box -> Popup -> Modal -> Zoom -> History -> Single -> Zen -> F11"),
                                ],
                            );
                        });
                    });

                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Tip: Hover any camera tile and press T, H, S, or Enter directly.")
                            .size(11.5)
                            .color(TEXT_MUTED),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Close (Esc)").clicked() {
                            self.show_shortcuts_modal = false;
                        }
                    });
                });
            });

        if !open {
            self.show_shortcuts_modal = false;
        }
    }

    fn render_settings_modal(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }

        let mut open = self.show_settings;
        let mut close_requested = false;
        let max_scroll_h = (ctx.screen_rect().height() - 165.0).max(200.0);
        let validation_err = self.edit_env_cfg.validate_for_save().err();
        let is_dirty = self.edit_env_cfg != self.env_cfg
            || !app_configs_equal_editable(&self.edit_app_cfg, &self.app_cfg);
        let is_scanning = self.scan_in_progress.lock().unwrap().is_some();
        let is_testing = self.test_conn_state.lock().unwrap().is_some();
        if is_scanning || is_testing {
            ctx.request_repaint_after(Duration::from_millis(80));
        }

        egui::Window::new("NVR Command Center - Settings")
            .order(egui::Order::Foreground)
            .open(&mut open)
            .resizable(true)
            .default_width(600.0)
            .frame(
                egui::Frame::window(&ctx.style())
                    .fill(GRAPHITE_SURFACE)
                    .stroke(Stroke::new(
                        1.0_f32,
                        Color32::from_rgba_unmultiplied(255, 255, 255, 28),
                    ))
                    .rounding(Rounding::same(10.0))
                    .inner_margin(egui::Margin::same(14.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(
                        &mut self.settings_tab,
                        SettingsTab::NvrAndCameras,
                        "1. NVR & Cameras",
                    );
                    ui.selectable_value(
                        &mut self.settings_tab,
                        SettingsTab::PerformanceAndNames,
                        "2. Performance & Names",
                    );
                    ui.selectable_value(
                        &mut self.settings_tab,
                        SettingsTab::ShortcutsAndSupport,
                        "3. About & Shortcuts",
                    );
                });
                if self.settings_tab != SettingsTab::NvrAndCameras {
                    self.show_password = false;
                }
                ui.separator();

                if let Some(ref err_msg) = validation_err {
                    egui::Frame::none()
                        .fill(status_coral_fill())
                        .stroke(Stroke::new(1.0_f32, STATUS_CORAL))
                        .rounding(Rounding::same(7.0))
                        .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(format!("Validation issue: {}", err_msg))
                                    .size(11.5)
                                    .color(Color32::from_rgb(254, 202, 202)),
                            );
                        });
                    ui.add_space(4.0);
                }

                egui::ScrollArea::vertical()
                    .id_salt("settings_modal_body_scroll")
                    .max_height(max_scroll_h)
                    .auto_shrink([false, true])
                    .show(ui, |ui| match self.settings_tab {
                        SettingsTab::NvrAndCameras => {
                            egui::Frame::none()
                                .fill(ZINC_ELEVATED)
                                .stroke(Stroke::new(
                                    1.0_f32,
                                    Color32::from_rgba_unmultiplied(56, 189, 248, 95),
                                ))
                                .rounding(Rounding::same(7.0))
                                .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new("Quick Setup Guide")
                                            .size(12.0)
                                            .strong()
                                            .color(STATUS_ICE),
                                    );
                                    ui.label(
                                        RichText::new(
                                            "- LAN Host / IP: Found on your NVR monitor under Menu -> Network -> TCP/IP (e.g. 192.168.1.108)\n\
                                             - Cloud P2P Serial (Optional): Found under Menu -> Network -> P2P (Status: Online) or NVR S/N sticker for Live + Playback on any internet connection.",
                                        )
                                        .size(11.5)
                                        .color(TEXT_SECONDARY),
                                    );
                                });
                            ui.add_space(6.0);

                            egui::Grid::new("nvr_settings_grid")
                                .num_columns(2)
                                .spacing([12.0, 8.0])
                                .show(ui, |ui| {
                                    ui.label("NVR / Camera Brand:");
                                    egui::ComboBox::from_id_salt("brand_combo")
                                        .selected_text(match self.edit_env_cfg.brand.as_str() {
                                            "hikvision" => "Hikvision (/Streaming/Channels/X01)",
                                            "custom" => "Custom RTSP URL Template",
                                            _ => "Dahua / CP Plus / Amcrest (/cam/realmonitor)",
                                        })
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(
                                                &mut self.edit_env_cfg.brand,
                                                "dahua".to_string(),
                                                "Dahua / CP Plus / Amcrest (/cam/realmonitor)",
                                            );
                                            ui.selectable_value(
                                                &mut self.edit_env_cfg.brand,
                                                "hikvision".to_string(),
                                                "Hikvision (/Streaming/Channels/X01)",
                                            );
                                            ui.selectable_value(
                                                &mut self.edit_env_cfg.brand,
                                                "custom".to_string(),
                                                "Custom RTSP URL Template",
                                            );
                                        });
                                    ui.end_row();

                                    ui.label("Primary LAN Host / IP:");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.edit_env_cfg.host)
                                            .hint_text("e.g. 192.168.1.108"),
                                    );
                                    ui.end_row();

                                    ui.label("Cloud P2P Serial (Any Internet):");
                                    ui.horizontal(|ui| {
                                        ui.add(
                                            egui::TextEdit::singleline(
                                                &mut self.edit_env_cfg.p2p_serial,
                                            )
                                            .hint_text("Optional NVR S/N (e.g. 9H0E...)"),
                                        );
                                        if ui
                                            .checkbox(
                                                &mut self.edit_env_cfg.use_cloud_p2p,
                                                "Use Cloud P2P",
                                            )
                                            .changed()
                                            && self.edit_env_cfg.use_cloud_p2p
                                        {
                                            self.edit_env_cfg.use_tailscale = false;
                                        }
                                    });
                                    ui.end_row();

                                    ui.label("Tailscale / Remote IP:");
                                    ui.horizontal(|ui| {
                                        ui.add(
                                            egui::TextEdit::singleline(
                                                &mut self.edit_env_cfg.tailscale_host,
                                            )
                                            .hint_text("Optional 100.x.y.z"),
                                        );
                                        if ui
                                            .checkbox(
                                                &mut self.edit_env_cfg.use_tailscale,
                                                "Use Tailscale",
                                            )
                                            .changed()
                                            && self.edit_env_cfg.use_tailscale
                                        {
                                            self.edit_env_cfg.use_cloud_p2p = false;
                                        }
                                    });
                                    ui.end_row();

                                    ui.label("RTSP Port:");
                                    let mut port_val = self.edit_env_cfg.port as u32;
                                    if ui
                                        .add(egui::DragValue::new(&mut port_val).range(1..=65535))
                                        .changed()
                                    {
                                        self.edit_env_cfg.port = port_val as u16;
                                    }
                                    ui.end_row();

                                    ui.label("HTTP / CGI Port (Playback):").on_hover_text(
                                        "NVR HTTP API port (default 80) used for AI event timelines, calendar recording dots, and fast segment fetches.",
                                    );
                                    let mut http_port_val = self.edit_env_cfg.http_port as u32;
                                    if ui
                                        .add(
                                            egui::DragValue::new(&mut http_port_val)
                                                .range(1..=65535),
                                        )
                                        .on_hover_text(
                                            "Default 80 on Dahua / CP Plus / Amcrest / Hikvision NVRs.",
                                        )
                                        .changed()
                                    {
                                        self.edit_env_cfg.http_port = http_port_val as u16;
                                    }
                                    ui.end_row();

                                    ui.label("RTSP Username:");
                                    ui.text_edit_singleline(&mut self.edit_env_cfg.username);
                                    ui.end_row();

                                    ui.label("RTSP Password:");
                                    ui.horizontal(|ui| {
                                        ui.add(
                                            egui::TextEdit::singleline(&mut self.edit_env_cfg.password)
                                                .password(!self.show_password),
                                        );
                                        ui.checkbox(&mut self.show_password, "Show");
                                    });
                                    ui.end_row();

                                    ui.label("Number of Cameras (1-32):");
                                    ui.horizontal(|ui| {
                                        ui.add(egui::Slider::new(
                                            &mut self.edit_env_cfg.num_cameras,
                                            1..=32,
                                        ));
                                        let scan_max = self.edit_env_cfg.num_cameras.clamp(1, 32);
                                        let scan_label = if is_scanning {
                                            "Scanning...".to_string()
                                        } else {
                                            format!("Auto-Scan (1-{})", scan_max)
                                        };
                                        if ui
                                            .add_enabled(
                                                !is_scanning && validation_err.is_none(),
                                                egui::Button::new(scan_label),
                                            )
                                            .on_hover_text(
                                                "Probes NVR channels 1..N with isolated credentials and updates camera count automatically.",
                                            )
                                            .clicked()
                                        {
                                            self.start_auto_scan();
                                        }
                                    });
                                    ui.end_row();

                                    ui.label("IP Camera Start Channel:").on_hover_text(
                                        "On hybrid XVR/DVR recorders, specifies the first IP channel index. Standard all-IP NVRs can leave this as default.",
                                    );
                                    ui.add(
                                        egui::DragValue::new(
                                            &mut self.edit_env_cfg.ip_camera_start_channel,
                                        )
                                        .range(1..=32),
                                    )
                                    .on_hover_text("Channel index (1-32) where IP cameras start.");
                                    ui.end_row();

                                    if self.edit_env_cfg.brand == "custom" {
                                        ui.label("Custom URL Template:");
                                        ui.text_edit_singleline(
                                            &mut self.edit_env_cfg.custom_url_template,
                                        );
                                        ui.end_row();
                                    }
                                });

                            ui.add_space(8.0);
                            let raw_preview = self
                                .edit_env_cfg
                                .build_redacted_url(1, self.edit_env_cfg.default_subtype);
                            let safe_preview = if self.edit_env_cfg.is_cloud_p2p_active()
                                && self.edit_env_cfg.p2p_local_port == 0
                            {
                                raw_preview.replacen("127.0.0.1:0", "127.0.0.1:<p2p-tunnel>", 1)
                            } else {
                                raw_preview
                            };
                            ui.label(
                                RichText::new(format!("Preview (Ch 1): {}", safe_preview))
                                    .size(11.5)
                                    .color(TEXT_MUTED),
                            );

                            ui.add_space(6.0);
                            ui.horizontal_wrapped(|ui| {
                                let test_lbl = if is_testing {
                                    "Testing Connection..."
                                } else {
                                    "Test Connection"
                                };
                                if ui
                                    .add_enabled(
                                        !is_testing && validation_err.is_none(),
                                        egui::Button::new(
                                            RichText::new(test_lbl).strong().color(TEXT_PRIMARY),
                                        )
                                        .fill(ZINC_ELEVATED)
                                        .stroke(Stroke::new(1.0_f32, STATUS_ICE.gamma_multiply(0.75))),
                                    )
                                    .on_hover_text(
                                        "Verify NVR reachability, credentials, RTSP port, and HTTP playback API before saving.",
                                    )
                                    .clicked()
                                {
                                    self.start_test_connection();
                                }

                                if is_testing {
                                    ui.label(
                                        RichText::new("Probing NVR RTSP & HTTP endpoints...")
                                            .size(11.5)
                                            .color(STATUS_ICE),
                                    );
                                }
                            });

                            if let Some(ref outcome) = self.last_test_conn_banner {
                                ui.add_space(4.0);
                                let (bg, border, fg, txt) = match outcome {
                                    Ok(msg) => (
                                        status_emerald_fill(),
                                        STATUS_EMERALD,
                                        Color32::from_rgb(209, 250, 229),
                                        msg.as_str(),
                                    ),
                                    Err(err) => (
                                        status_coral_fill(),
                                        STATUS_CORAL,
                                        Color32::from_rgb(254, 202, 202),
                                        err.as_str(),
                                    ),
                                };
                                egui::Frame::none()
                                    .fill(bg)
                                    .stroke(Stroke::new(1.0_f32, border))
                                    .rounding(Rounding::same(6.0))
                                    .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                                    .show(ui, |ui| {
                                        ui.label(RichText::new(txt).size(11.5).color(fg));
                                    });
                            }
                        }

                        SettingsTab::PerformanceAndNames => {
                            let mut open_snap_dir = false;
                            let mut open_clip_dir = false;

                            egui::Grid::new("perf_settings_grid")
                                .num_columns(2)
                                .spacing([12.0, 8.0])
                                .show(ui, |ui| {
                                    ui.label("Default Stream Quality:");
                                    egui::ComboBox::from_id_salt("default_subtype_combo")
                                        .selected_text(if self.edit_env_cfg.default_subtype == 0 {
                                            "HD Main-Stream (720p / 1080p)"
                                        } else {
                                            "SD Sub-Stream (Low CPU & Bandwidth)"
                                        })
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(
                                                &mut self.edit_env_cfg.default_subtype,
                                                1u8,
                                                "SD Sub-Stream (Low CPU & Bandwidth)",
                                            );
                                            ui.selectable_value(
                                                &mut self.edit_env_cfg.default_subtype,
                                                0u8,
                                                "HD Main-Stream (720p / 1080p)",
                                            );
                                        });
                                    ui.end_row();

                                    ui.label("Hardware Decode:");
                                    egui::ComboBox::from_id_salt("hwaccel_combo")
                                        .selected_text(&self.edit_env_cfg.hwaccel_mode)
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(
                                                &mut self.edit_env_cfg.hwaccel_mode,
                                                "auto".to_string(),
                                                "auto (VAAPI / NVDEC with CPU fallback)",
                                            );
                                            ui.selectable_value(
                                                &mut self.edit_env_cfg.hwaccel_mode,
                                                "software".to_string(),
                                                "software (Multi-threaded CPU)",
                                            );
                                        });
                                    ui.end_row();

                                    ui.label("Single View Auto-HD:");
                                    ui.checkbox(
                                        &mut self.edit_app_cfg.fullscreen_auto_hd,
                                        "Warm-swap focused Single View camera to HD (subtype=0)",
                                    );
                                    ui.end_row();

                                    ui.label("Aspect Framing:");
                                    ui.checkbox(
                                        &mut self.edit_app_cfg.aspect_fit_16_9,
                                        "Preserve full 16:9 frame inside tiles (uncheck for center-crop Fill) (A)",
                                    );
                                    ui.end_row();

                                    ui.label("Patrol Cycle Interval (sec):");
                                    ui.add(egui::Slider::new(
                                        &mut self.edit_app_cfg.patrol_interval_secs,
                                        3..=60,
                                    ));
                                    ui.end_row();

                                    ui.label("Snapshots Directory:");
                                    ui.horizontal(|ui| {
                                        ui.add(
                                            egui::TextEdit::singleline(
                                                &mut self.edit_app_cfg.screenshot_dir,
                                            )
                                            .desired_width(280.0),
                                        );
                                        if ui
                                            .button("Open")
                                            .on_hover_text("Open Snapshots folder in file manager")
                                            .clicked()
                                        {
                                            open_snap_dir = true;
                                        }
                                    });
                                    ui.end_row();

                                    ui.label("Clips Export Directory:");
                                    ui.horizontal(|ui| {
                                        ui.add(
                                            egui::TextEdit::singleline(
                                                &mut self.edit_app_cfg.recording_dir,
                                            )
                                            .desired_width(280.0),
                                        );
                                        if ui
                                            .button("Open")
                                            .on_hover_text("Open Clips Export folder in file manager")
                                            .clicked()
                                        {
                                            open_clip_dir = true;
                                        }
                                    });
                                    ui.end_row();
                                });

                            if open_snap_dir {
                                let dir = expand_tilde(&self.edit_app_cfg.screenshot_dir);
                                self.open_local_directory(ctx, &dir, false);
                            }
                            if open_clip_dir {
                                let dir = expand_tilde(&self.edit_app_cfg.recording_dir);
                                self.open_local_directory(ctx, &dir, false);
                            }

                            ui.separator();
                            ui.label(RichText::new("Custom Camera Names:").strong());
                            egui::ScrollArea::vertical()
                                .id_salt("cam_names_inner_scroll")
                                .max_height(180.0)
                                .show(ui, |ui| {
                                    egui::Grid::new("cam_names_grid")
                                        .num_columns(2)
                                        .spacing([10.0, 4.0])
                                        .show(ui, |ui| {
                                            for ch in 1..=self.edit_env_cfg.num_cameras.clamp(1, 32) {
                                                ui.label(format!("Channel {:02}:", ch));
                                                let default_lbl = format!("Camera {}", ch);
                                                let mut name_buf = self
                                                    .edit_app_cfg
                                                    .camera_names
                                                    .get(&ch)
                                                    .cloned()
                                                    .unwrap_or_default();
                                                let resp = ui.add(
                                                    egui::TextEdit::singleline(&mut name_buf)
                                                        .hint_text(&default_lbl)
                                                        .desired_width(240.0),
                                                );
                                                if resp.changed() {
                                                    if name_buf.is_empty() {
                                                        self.edit_app_cfg.camera_names.remove(&ch);
                                                    } else {
                                                        self.edit_app_cfg
                                                            .camera_names
                                                            .insert(ch, name_buf);
                                                    }
                                                }
                                                ui.end_row();
                                            }
                                        });
                                });
                        }

                        SettingsTab::ShortcutsAndSupport => {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "NVR Command Center v{}",
                                        env!("CARGO_PKG_VERSION")
                                    ))
                                    .strong()
                                    .color(TEXT_PRIMARY),
                                );
                                if ui.button("Open Full Cheat-Sheet (?)").clicked() {
                                    close_requested = true;
                                    self.show_shortcuts_modal = true;
                                }
                            });
                            ui.add_space(4.0);
                            ui.label("- T / TTT (or Shift+T) : 1080p History Playback (+ Live PiP) / 2x2 Sync Playback");
                            ui.label("- J K L / , . / G / E / Ctrl+G : Trick-Play, 0ms Frame-Step, Smart Skip, Events & Time Jump");
                            ui.label("- I / O / Ctrl+E : Mark Clip In / Out & Lossless MP4 Export");
                            ui.label("- S / SSS, H / HHH : Active camera or All cameras SD / HD");
                            ui.label("- Drag Box / Scroll / + / - / Z : Independent Live & Playback GPU e-PTZ Zoom (1x-8x)");
                            ui.label("- Hover / Arrows + Enter (or F) : Camera focus & Single View");
                            ui.label("- B / F11 / A / C / M / Space / R : Zen / Fullscreen / Aspect / Snapshot / Patrol / Pause / Re-Sync");

                            ui.add_space(8.0);
                            ui.separator();
                            ui.label(
                                RichText::new("Developer Portfolio & Open-Source Support")
                                    .strong()
                                    .color(TEXT_PRIMARY),
                            );
                            ui.label(
                                RichText::new(
                                    "Built in Native Rust by Pulkit Mundra. Explore the interactive architecture simulator, engineering deep-dives, and more Linux desktop apps:",
                                )
                                .size(11.5)
                                .color(TEXT_MUTED),
                            );
                            ui.add_space(4.0);
                            ui.horizontal_wrapped(|ui| {
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("Portfolio & More Linux Apps")
                                                .strong()
                                                .color(TEXT_PRIMARY),
                                        )
                                        .fill(status_ice_fill())
                                        .stroke(Stroke::new(1.0_f32, STATUS_ICE)),
                                    )
                                    .on_hover_text(PORTFOLIO_URL)
                                    .clicked()
                                {
                                    self.os_fullscreen = false;
                                    self.show_toast("Opening Portfolio & Architecture Lab...");
                                    open_url(ui.ctx(), PORTFOLIO_URL);
                                }
                                if ui
                                    .button("GitHub Repository")
                                    .on_hover_text(GITHUB_REPO_URL)
                                    .clicked()
                                {
                                    self.os_fullscreen = false;
                                    self.show_toast("Opening GitHub Repository...");
                                    open_url(ui.ctx(), GITHUB_REPO_URL);
                                }
                                if ui
                                    .button("Buy Me a Coffee")
                                    .on_hover_text(BUY_ME_A_COFFEE_URL)
                                    .clicked()
                                {
                                    self.os_fullscreen = false;
                                    self.show_toast("Opening Buy Me a Coffee...");
                                    open_url(ui.ctx(), BUY_ME_A_COFFEE_URL);
                                }
                                if ui
                                    .button("GitHub Sponsors")
                                    .on_hover_text(GITHUB_SPONSORS_URL)
                                    .clicked()
                                {
                                    self.os_fullscreen = false;
                                    self.show_toast("Opening GitHub Sponsors...");
                                    open_url(ui.ctx(), GITHUB_SPONSORS_URL);
                                }
                            });
                        }
                    });

                ui.add_space(6.0);
                ui.separator();
                ui.horizontal(|ui| {
                    let save_btn = egui::Button::new(
                        RichText::new("Save & Apply")
                            .strong()
                            .color(if validation_err.is_none() {
                                TEXT_PRIMARY
                            } else {
                                TEXT_MUTED
                            }),
                    )
                    .fill(if validation_err.is_none() {
                        status_ice_fill()
                    } else {
                        ZINC_ELEVATED
                    })
                    .stroke(Stroke::new(
                        1.0_f32,
                        if validation_err.is_none() {
                            STATUS_ICE
                        } else {
                            hairline_stroke()
                        },
                    ));

                    if ui
                        .add_enabled(validation_err.is_none(), save_btn)
                        .clicked()
                        && self.apply_and_save_settings(ctx)
                    {
                        close_requested = true;
                    }
                    if ui.button("Close").clicked() {
                        close_requested = true;
                    }

                    if is_dirty {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                RichText::new("Unsaved changes")
                                    .size(11.5)
                                    .color(STATUS_AMBER),
                            );
                            let (dot_r, _) =
                                ui.allocate_exact_size(Vec2::new(8.0, 8.0), Sense::hover());
                            ui.painter().circle_filled(dot_r.center(), 3.2, STATUS_AMBER);
                        });
                    }
                });
            });

        if !open || close_requested {
            self.close_settings_modal();
        }
    }

    fn commit_camera_rename(&mut self, ch: usize, custom_opt: Option<String>) {
        let default_lbl = format!("Camera {}", ch);
        match custom_opt {
            Some(raw) => {
                let clean = raw.trim().to_string();
                if clean.is_empty() || clean == default_lbl {
                    self.app_cfg.camera_names.remove(&ch);
                    self.edit_app_cfg.camera_names.remove(&ch);
                } else {
                    self.app_cfg.camera_names.insert(ch, clean.clone());
                    self.edit_app_cfg.camera_names.insert(ch, clean);
                }
            }
            None => {
                self.app_cfg.camera_names.remove(&ch);
                self.edit_app_cfg.camera_names.remove(&ch);
            }
        }

        match self.app_cfg.save() {
            Ok(()) => {
                self.show_toast_kind(
                    ToastKind::Success,
                    format!(
                        "Channel {:02} -> {}",
                        ch,
                        self.app_cfg.camera_display_name(ch)
                    ),
                );
            }
            Err(e) => {
                self.show_toast_kind(
                    ToastKind::Error,
                    format!("Failed to save camera name: {}", e),
                );
            }
        }
        self.rename_modal_channel = None;
        self.rename_just_opened = false;
    }

    fn render_rename_modal(&mut self, ctx: &egui::Context) {
        let Some(ch) = self.rename_modal_channel else {
            return;
        };
        let mut open = true;
        let mut submit_save = false;
        let mut submit_reset = false;
        let mut cancel_close = false;
        let default_lbl = format!("Camera {}", ch);
        let has_custom = self.app_cfg.camera_names.contains_key(&ch)
            || (!self.rename_input.trim().is_empty() && self.rename_input.trim() != default_lbl);

        egui::Window::new(format!("Rename Channel {:02}", ch))
            .order(egui::Order::Foreground)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .frame(
                egui::Frame::window(&ctx.style())
                    .fill(GRAPHITE_SURFACE)
                    .stroke(Stroke::new(
                        1.0_f32,
                        Color32::from_rgba_unmultiplied(255, 255, 255, 28),
                    ))
                    .rounding(Rounding::same(10.0))
                    .inner_margin(egui::Margin::same(14.0)),
            )
            .show(ctx, |ui| {
                ui.label(
                    RichText::new("Camera Display Name:")
                        .size(12.0)
                        .color(TEXT_SECONDARY),
                );
                ui.add_space(2.0);
                let mut te_out = egui::TextEdit::singleline(&mut self.rename_input)
                    .hint_text(&default_lbl)
                    .desired_width(280.0)
                    .show(ui);

                if self.rename_just_opened {
                    self.rename_just_opened = false;
                    te_out.response.request_focus();
                    let char_len = self.rename_input.chars().count();
                    te_out
                        .state
                        .cursor
                        .set_char_range(Some(egui::text::CCursorRange::two(
                            egui::text::CCursor::new(0),
                            egui::text::CCursor::new(char_len),
                        )));
                    te_out.state.store(ctx, te_out.response.id);
                }

                if ui.input(|i| i.key_pressed(Key::Enter)) {
                    submit_save = true;
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("Save").strong().color(TEXT_PRIMARY),
                            )
                            .fill(status_ice_fill())
                            .stroke(Stroke::new(1.0_f32, STATUS_ICE)),
                        )
                        .clicked()
                    {
                        submit_save = true;
                    }
                    if ui
                        .add_enabled(has_custom, egui::Button::new("Reset Default"))
                        .on_hover_text(format!("Restore default name '{}'", default_lbl))
                        .clicked()
                    {
                        submit_reset = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel_close = true;
                    }
                });
            });

        if submit_save {
            let input = self.rename_input.clone();
            self.commit_camera_rename(ch, Some(input));
        } else if submit_reset {
            self.commit_camera_rename(ch, None);
        } else if !open || cancel_close {
            self.rename_modal_channel = None;
            self.rename_just_opened = false;
        }
    }
}

impl eframe::App for CameraViewerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Keep OS fullscreen flag synchronized with window manager state every frame
        if let Some(fs) = ctx.input(|i| i.viewport().fullscreen) {
            self.os_fullscreen = fs;
        }

        // Keep OS window title synchronized on first frame & mode/count changes
        self.sync_window_title(ctx);

        // Poll background PNG snapshots, Test Connection, and Auto-Scan completions
        self.poll_background_jobs();

        // Poll process RSS memory every 2s
        if self.last_sys_poll.elapsed() >= Duration::from_secs(2) {
            self.rss_mb = read_process_rss_mb();
            self.last_sys_poll = Instant::now();
        }

        // Detect window minimized state for 24/7 eco mode & pause 1080p playback decode while minimized
        let minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        if minimized {
            if !self.was_minimized {
                self.was_minimized = true;
                self.show_password = false;
                self.stream_mgr.set_window_minimized_eco(true);
                if self.playback_mgr.is_active() && !self.playback_mgr.is_paused() {
                    self.playback_mgr.set_paused(true);
                    self.minimized_paused_playback = true;
                }
            }
            ctx.request_repaint_after(Duration::from_millis(250));
            return;
        } else if self.was_minimized {
            self.was_minimized = false;
            if self.minimized_paused_playback {
                self.minimized_paused_playback = false;
                if self.playback_mgr.is_active() {
                    self.playback_mgr.set_paused(false);
                }
            }
            self.restore_live_stream_governor();
        }

        // Full press-to-release overlay gesture block latch
        let any_popup_or_modal = self.show_settings
            || self.show_shortcuts_modal
            || self.rename_modal_channel.is_some()
            || self.timeline_ui.is_any_popup_open()
            || ctx.memory(|m| m.any_popup_open())
            || ctx.is_context_menu_open();
        if any_popup_or_modal {
            self.overlay_gesture_block = true;
        } else if !ctx.input(|i| {
            i.pointer.any_down() || i.pointer.any_released() || i.pointer.any_click()
        }) {
            self.overlay_gesture_block = false;
        }

        // Patrol auto-rotation timer (suspended in playback or while user is zoomed/dragging)
        if self.patrol_active && !self.playback_mgr.is_active() {
            if self.is_patrol_user_busy(ctx) {
                self.last_patrol_switch = Instant::now();
            } else if self.last_patrol_switch.elapsed()
                >= Duration::from_secs(self.app_cfg.patrol_interval_secs.max(3))
            {
                self.cycle_fullscreen(1);
                self.last_patrol_switch = Instant::now();
            }
        }

        self.frame_pinch = self
            .wayland_pinch
            .as_ref()
            .map(|p| p.take_frame_snapshot())
            .unwrap_or_default();
        if (self.frame_pinch.scale_delta - 1.0).abs() > 0.0005
            || self.frame_pinch.pan_delta != Vec2::ZERO
        {
            self.last_pointer_activity = Instant::now();
        }

        self.update_pointer_tracking(ctx);
        self.handle_keyboard_shortcuts(ctx);

        if self.playback_mgr.is_active() {
            self.playback_mgr.tick(self.timeline_ui.event_filter);
            self.timeline_ui.sync_playhead(
                self.playback_mgr.playhead_epoch_ms(),
                !self.playback_mgr.is_paused(),
            );
            if let Some(ch) = self.playback_mgr.primary_channel() {
                let (vs, ve) = self.timeline_ui.visible_range_ms();
                self.playback_mgr.ensure_timelines_for_range(ch, vs, ve);
                if self.timeline_ui.show_calendar_popup {
                    let (year, month) = self.timeline_ui.calendar_ym();
                    self.playback_mgr.request_month_days(ch, year, month);
                }
            }
        }
        if let Some(res) = self.playback_mgr.take_finished_export() {
            match res {
                Ok(path) => {
                    let parent_dir = path
                        .parent()
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|| expand_tilde(&self.app_cfg.recording_dir));
                    let fname = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| path.display().to_string());
                    self.push_toast(
                        ToastKind::Success,
                        format!("Saved MP4 clip: {}", fname),
                        Some(parent_dir),
                    );
                }
                Err(err) => {
                    self.push_toast(
                        ToastKind::Error,
                        format!("Clip export failed: {}", err),
                        None,
                    );
                }
            }
        }

        self.update_gpu_textures(ctx);

        // Render Event Drawer (SidePanel::right) BEFORE floating Top Bar & floating Timeline Dock
        // so both floating Areas constrain horizontally to ctx.available_rect().
        if self.playback_mgr.is_active() && self.timeline_ui.show_event_drawer {
            let drawer_actions = render_event_drawer(
                ctx,
                &mut self.timeline_ui,
                &self.playback_mgr,
                &self.app_cfg,
                self.playback_mgr.playhead_epoch_ms(),
            );
            if !drawer_actions.is_empty() {
                self.handle_timeline_actions(drawer_actions);
            }
        }

        let floating_chrome = self.zen_mode || self.os_fullscreen;
        let show_top_bar = self.should_show_top_bar(ctx);
        self.floating_top_bar_visible = floating_chrome && show_top_bar;

        if show_top_bar {
            if floating_chrome {
                let avail_x = ctx.available_rect();
                let bar_w = avail_x.width().max(120.0);
                let bar_pos = Pos2::new(avail_x.min.x, 0.0);
                egui::Area::new(egui::Id::new("command_center_top_bar_float"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(bar_pos)
                    .show(ctx, |ui| {
                        egui::Frame::none()
                            .fill(Color32::from_rgba_unmultiplied(17, 17, 20, 242))
                            .inner_margin(egui::Margin::symmetric(10.0, 5.0))
                            .show(ui, |ui| {
                                let inner_w = (bar_w - 20.0).max(100.0);
                                ui.set_min_size(Vec2::new(inner_w, 28.0));
                                ui.set_max_size(Vec2::new(inner_w, 28.0));
                                self.render_top_command_bar(ui);
                            });
                        let full_bar = Rect::from_min_size(bar_pos, Vec2::new(bar_w, 38.0));
                        ui.painter().hline(
                            full_bar.x_range(),
                            full_bar.max.y - 0.5,
                            Stroke::new(1.0_f32, hairline_stroke()),
                        );
                    });
            } else {
                egui::TopBottomPanel::top("command_center_top_bar")
                    .exact_height(38.0)
                    .frame(
                        egui::Frame::none()
                            .fill(GRAPHITE_SURFACE)
                            .inner_margin(egui::Margin::symmetric(10.0, 5.0)),
                    )
                    .show(ctx, |ui| {
                        self.render_top_command_bar(ui);
                        let panel_rect = ui.max_rect().expand2(Vec2::new(10.0, 5.0));
                        ui.painter().hline(
                            panel_rect.x_range(),
                            panel_rect.max.y - 0.5,
                            Stroke::new(1.0_f32, hairline_stroke()),
                        );
                    });
            }
        }

        let show_pb_dock = self.should_show_playback_dock(ctx);
        if !show_pb_dock && self.timeline_ui.is_actively_dragging() {
            self.timeline_ui.cancel_active_drags();
        }
        self.floating_dock_visible = floating_chrome && show_pb_dock;

        if show_pb_dock {
            let dock_actions = render_timeline_dock(
                ctx,
                &mut self.timeline_ui,
                &self.playback_mgr,
                &self.app_cfg,
                self.env_cfg.is_cloud_p2p_active(),
                floating_chrome,
                self.frame_pinch,
            );
            if !dock_actions.is_empty() {
                self.handle_timeline_actions(dock_actions);
            }
        }

        // Re-latch if a button clicked earlier in this frame opened a popup/modal
        if self.show_settings
            || self.show_shortcuts_modal
            || self.rename_modal_channel.is_some()
            || self.timeline_ui.is_any_popup_open()
            || ctx.memory(|m| m.any_popup_open())
            || ctx.is_context_menu_open()
        {
            self.overlay_gesture_block = true;
        }

        // Zero layout jump: wall_margin stays 0.0 permanently in Zen / OS Fullscreen!
        let wall_margin = if floating_chrome { 0.0 } else { 5.0 };
        self.hovered_channel = None;

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(OBSIDIAN_BG)
                    .inner_margin(egui::Margin::same(wall_margin)),
            )
            .show(ctx, |ui| {
                let avail = ui.available_rect_before_wrap();
                if self.env_cfg.password.trim().is_empty()
                    && !self.show_settings
                    && !self.playback_mgr.is_active()
                {
                    self.render_onboarding_card(ui, avail);
                } else if self.playback_mgr.is_active() {
                    if self.playback_mgr.is_sync_2x2() {
                        let pb_channels = self.playback_mgr.active_channels().to_vec();
                        let rows = 2usize;
                        let cols = 2usize;
                        let gap = 5.0f32;
                        let tile_w = ((avail.width() - gap) / cols as f32).max(40.0);
                        let tile_h = ((avail.height() - gap) / rows as f32).max(30.0);
                        for (idx, ch) in pb_channels.into_iter().take(4).enumerate() {
                            let r = idx / cols;
                            let c = idx % cols;
                            let x = avail.min.x + c as f32 * (tile_w + gap);
                            let y = avail.min.y + r as f32 * (tile_h + gap);
                            let rect =
                                Rect::from_min_size(Pos2::new(x, y), Vec2::new(tile_w, tile_h));
                            self.render_camera_tile(ui, ch, rect, false, true);
                        }
                    } else {
                        let primary_ch = self.playback_mgr.primary_channel().unwrap_or(1);
                        self.render_camera_tile(ui, primary_ch, avail, true, true);
                    }
                } else if let Some(fs_ch) = self.fullscreen_channel {
                    self.render_camera_tile(ui, fs_ch, avail, true, false);
                } else {
                    let (rows, cols) = self.app_cfg.grid_dimensions();
                    let gap = 5.0f32;
                    let tile_w =
                        ((avail.width() - gap * (cols.saturating_sub(1) as f32)) / cols as f32)
                            .max(40.0);
                    let tile_h =
                        ((avail.height() - gap * (rows.saturating_sub(1) as f32)) / rows as f32)
                            .max(30.0);

                    let channels = self.app_cfg.channel_order.clone();
                    for (idx, ch) in channels.into_iter().enumerate() {
                        let r = idx / cols;
                        let c = idx % cols;
                        if r >= rows {
                            break;
                        }
                        let x = avail.min.x + c as f32 * (tile_w + gap);
                        let y = avail.min.y + r as f32 * (tile_h + gap);
                        let rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(tile_w, tile_h));
                        self.render_camera_tile(ui, ch, rect, false, false);
                    }
                }
            });

        self.render_modal_backdrop_scrim(ctx);
        self.render_settings_modal(ctx);
        self.render_shortcuts_modal(ctx);
        self.render_rename_modal(ctx);
        self.render_toast_stack(ctx, show_pb_dock);

        let now = Instant::now();
        self.shutter_flash_until.retain(|_, until| *until > now);
        if !self.shutter_flash_until.is_empty() {
            ctx.request_repaint();
        }

        // Synchronized presentation tick (~30 Hz active UI check, 100ms paused playback, 250ms idle)
        let export_running = self.playback_mgr.export_state().in_progress;
        let repaint_ms = if self.playback_mgr.is_active() {
            if self.playback_mgr.is_paused() && !export_running {
                100
            } else {
                33
            }
        } else if self.all_paused && !export_running {
            250
        } else {
            33
        };
        ctx.request_repaint_after(Duration::from_millis(repaint_ms));
    }
}

fn draw_segmented_group(ui: &mut Ui, mixed_highlight: bool, add_contents: impl FnOnce(&mut Ui)) {
    let border_col = if mixed_highlight {
        STATUS_ICE.gamma_multiply(0.5)
    } else {
        hairline_stroke()
    };
    egui::Frame::none()
        .fill(ZINC_ELEVATED)
        .stroke(Stroke::new(1.0_f32, border_col))
        .rounding(Rounding::same(8.0))
        .inner_margin(egui::Margin::symmetric(2.0, 2.0))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            add_contents(ui);
        });
}

fn draw_segment_item(ui: &mut Ui, label: &str, active: bool, accent: PillAccent) -> egui::Response {
    let font = FontId::new(11.5, FontFamily::Proportional);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), font.clone(), TEXT_PRIMARY);
    let text_w = (galley.size().x + 15.0).max(24.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(text_w, 22.0), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let painter = ui.painter();
    let (acc_fill, acc_stroke, acc_fg) = accent.colors();
    if active {
        painter.rect_filled(rect, Rounding::same(6.0), acc_fill);
        painter.rect_stroke(
            rect,
            Rounding::same(6.0),
            Stroke::new(1.0_f32, acc_stroke.gamma_multiply(0.65)),
        );
        painter.text(rect.center(), Align2::CENTER_CENTER, label, font, acc_fg);
    } else {
        if resp.hovered() {
            painter.rect_filled(rect, Rounding::same(6.0), ZINC_HOVER);
        }
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            font,
            if resp.hovered() {
                TEXT_PRIMARY
            } else {
                TEXT_MUTED
            },
        );
    }
    resp
}

fn draw_pill_button(
    ui: &mut Ui,
    label: &str,
    active: bool,
    accent: PillAccent,
    ghost: bool,
) -> egui::Response {
    let font = FontId::new(11.5, FontFamily::Proportional);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), font.clone(), TEXT_PRIMARY);
    let text_w = (galley.size().x + 17.0).max(26.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(text_w, 26.0), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let painter = ui.painter();
    let (acc_fill, acc_stroke, acc_fg) = accent.colors();

    if active {
        painter.rect_filled(rect, Rounding::same(7.0), acc_fill);
        painter.rect_stroke(rect, Rounding::same(7.0), Stroke::new(1.0_f32, acc_stroke));
        painter.text(rect.center(), Align2::CENTER_CENTER, label, font, acc_fg);
    } else {
        let fill = if resp.hovered() {
            ZINC_HOVER
        } else if ghost {
            Color32::TRANSPARENT
        } else {
            ZINC_ELEVATED
        };
        let stroke_col = if resp.hovered() {
            Color32::from_rgba_unmultiplied(255, 255, 255, 42)
        } else {
            hairline_stroke()
        };
        if fill != Color32::TRANSPARENT {
            painter.rect_filled(rect, Rounding::same(7.0), fill);
        }
        painter.rect_stroke(rect, Rounding::same(7.0), Stroke::new(1.0_f32, stroke_col));
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            font,
            if resp.hovered() {
                TEXT_PRIMARY
            } else {
                TEXT_SECONDARY
            },
        );
    }
    resp
}

fn paint_dock_micro_btn(
    painter: &egui::Painter,
    rect: Rect,
    label: &str,
    hovered: bool,
    active: bool,
    accent: Color32,
) {
    let fill = if active {
        Color32::from_rgba_unmultiplied(
            accent.r(),
            accent.g(),
            accent.b(),
            if hovered { 64 } else { 42 },
        )
    } else if hovered {
        ZINC_HOVER
    } else {
        Color32::TRANSPARENT
    };
    if fill != Color32::TRANSPARENT {
        painter.rect_filled(rect, Rounding::same(5.0), fill);
    }
    if active || hovered {
        let stroke_col = if active {
            accent.gamma_multiply(0.85)
        } else {
            Color32::from_rgba_unmultiplied(255, 255, 255, 40)
        };
        painter.rect_stroke(rect, Rounding::same(5.0), Stroke::new(1.0_f32, stroke_col));
    }
    let fg = if active || hovered {
        TEXT_PRIMARY
    } else {
        TEXT_SECONDARY
    };
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::new(10.8, FontFamily::Proportional),
        fg,
    );
}

#[inline]
fn clamp_pos_to_rect(p: Pos2, r: Rect) -> Pos2 {
    Pos2::new(
        p.x.clamp(r.min.x, r.max.x.max(r.min.x)),
        p.y.clamp(r.min.y, r.max.y.max(r.min.y)),
    )
}

fn paint_marquee_viewfinder(
    painter: &egui::Painter,
    video_rect: Rect,
    p0: Pos2,
    p1: Pos2,
    preview_z: f32,
) {
    if (p1 - p0).length() < 8.0 {
        return;
    }
    let sel_rect = Rect::from_two_pos(p0, p1).intersect(video_rect);
    if sel_rect.width() < 4.0 || sel_rect.height() < 4.0 {
        return;
    }

    let is_valid = sel_rect.width() >= MIN_MARQUEE_PX && sel_rect.height() >= MIN_MARQUEE_PX;

    // 1. Optical rangefinder dimmed backdrop around the selected rectangle
    let dim_col = Color32::from_rgba_unmultiplied(9, 9, 11, 108);
    if sel_rect.min.y > video_rect.min.y {
        painter.rect_filled(
            Rect::from_min_max(video_rect.min, Pos2::new(video_rect.max.x, sel_rect.min.y)),
            Rounding::ZERO,
            dim_col,
        );
    }
    if sel_rect.max.y < video_rect.max.y {
        painter.rect_filled(
            Rect::from_min_max(Pos2::new(video_rect.min.x, sel_rect.max.y), video_rect.max),
            Rounding::ZERO,
            dim_col,
        );
    }
    if sel_rect.min.x > video_rect.min.x {
        painter.rect_filled(
            Rect::from_min_max(
                Pos2::new(video_rect.min.x, sel_rect.min.y),
                Pos2::new(sel_rect.min.x, sel_rect.max.y),
            ),
            Rounding::ZERO,
            dim_col,
        );
    }
    if sel_rect.max.x < video_rect.max.x {
        painter.rect_filled(
            Rect::from_min_max(
                Pos2::new(sel_rect.max.x, sel_rect.min.y),
                Pos2::new(video_rect.max.x, sel_rect.max.y),
            ),
            Rounding::ZERO,
            dim_col,
        );
    }

    // 2. Crisp hairline frame + subtle ice wash
    painter.rect_filled(
        sel_rect,
        Rounding::same(2.0),
        Color32::from_rgba_unmultiplied(56, 189, 248, if is_valid { 14 } else { 6 }),
    );
    let frame_col = if is_valid {
        Color32::from_rgba_unmultiplied(56, 189, 248, 135)
    } else {
        Color32::from_rgba_unmultiplied(161, 161, 170, 110)
    };
    painter.rect_stroke(sel_rect, Rounding::same(2.0), Stroke::new(1.0_f32, frame_col));

    // 3. Precision L-shaped corner brackets
    let arm = 10.0_f32
        .min(sel_rect.width() * 0.35)
        .min(sel_rect.height() * 0.35);
    let bracket_stroke = Stroke::new(1.8_f32, if is_valid { STATUS_ICE } else { TEXT_MUTED });
    let tl = sel_rect.left_top();
    let tr = sel_rect.right_top();
    let bl = sel_rect.left_bottom();
    let br = sel_rect.right_bottom();

    painter.line_segment([tl, tl + Vec2::new(arm, 0.0)], bracket_stroke);
    painter.line_segment([tl, tl + Vec2::new(0.0, arm)], bracket_stroke);
    painter.line_segment([tr, tr - Vec2::new(arm, 0.0)], bracket_stroke);
    painter.line_segment([tr, tr + Vec2::new(0.0, arm)], bracket_stroke);
    painter.line_segment([bl, bl + Vec2::new(arm, 0.0)], bracket_stroke);
    painter.line_segment([bl, bl - Vec2::new(0.0, arm)], bracket_stroke);
    painter.line_segment([br, br - Vec2::new(arm, 0.0)], bracket_stroke);
    painter.line_segment([br, br - Vec2::new(0.0, arm)], bracket_stroke);

    // Subtle center reticle tick when box is comfortably sized
    if is_valid && sel_rect.width() >= 48.0 && sel_rect.height() >= 36.0 {
        let c = sel_rect.center();
        let reticle_stroke =
            Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(56, 189, 248, 130));
        painter.line_segment(
            [c - Vec2::new(4.0, 0.0), c + Vec2::new(4.0, 0.0)],
            reticle_stroke,
        );
        painter.line_segment(
            [c - Vec2::new(0.0, 4.0), c + Vec2::new(0.0, 4.0)],
            reticle_stroke,
        );
    }

    // 4. Live target zoom badge (e.g. "3.8x" or "8.0x MAX") measured accurately via layout_no_wrap
    if is_valid {
        let badge_txt = if preview_z >= MAX_DIGITAL_ZOOM - 0.05 {
            format!("{:.1}x MAX", preview_z)
        } else {
            format!("{:.1}x", preview_z)
        };
        let b_font = FontId::new(10.5, FontFamily::Monospace);
        let b_galley = painter.layout_no_wrap(badge_txt, b_font, STATUS_ICE);
        let badge_w = (b_galley.size().x + 12.0).max(40.0);
        let badge_h = 18.0;
        let place_above = sel_rect.min.y - badge_h - 5.0 >= video_rect.min.y + 30.0;
        let bx = sel_rect.min.x.clamp(
            video_rect.min.x + 6.0,
            (video_rect.max.x - badge_w - 6.0).max(video_rect.min.x + 6.0),
        );
        let by = if place_above {
            sel_rect.min.y - badge_h - 4.0
        } else {
            (sel_rect.min.y + 5.0)
                .min((video_rect.max.y - badge_h - 6.0).max(video_rect.min.y + 6.0))
        };
        let badge_rect = Rect::from_min_size(Pos2::new(bx, by), Vec2::new(badge_w, badge_h));
        painter.rect_filled(
            badge_rect,
            Rounding::same(4.5),
            Color32::from_rgba_unmultiplied(9, 9, 11, 224),
        );
        painter.rect_stroke(
            badge_rect,
            Rounding::same(4.5),
            Stroke::new(1.0_f32, STATUS_ICE.gamma_multiply(0.8)),
        );
        painter.galley(
            Pos2::new(
                badge_rect.center().x - b_galley.size().x * 0.5,
                badge_rect.center().y - b_galley.size().y * 0.5,
            ),
            b_galley,
            STATUS_ICE,
        );
    }
}

fn render_shortcut_section(ui: &mut Ui, title: &str, rows: &[(&[&str], &str)]) {
    egui::Frame::none()
        .fill(ZINC_ELEVATED)
        .stroke(Stroke::new(1.0_f32, hairline_stroke()))
        .rounding(Rounding::same(8.0))
        .inner_margin(egui::Margin::symmetric(10.0, 8.0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new(title).size(12.0).strong().color(STATUS_ICE));
            ui.add_space(4.0);
            for (keys, desc) in rows {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for (idx, k) in keys.iter().enumerate() {
                        if idx > 0 {
                            ui.label(RichText::new("/").size(10.5).color(ZINC_DIM));
                        }
                        egui::Frame::none()
                            .fill(GRAPHITE_SURFACE)
                            .stroke(Stroke::new(
                                1.0_f32,
                                Color32::from_rgba_unmultiplied(255, 255, 255, 34),
                            ))
                            .rounding(Rounding::same(4.5))
                            .inner_margin(egui::Margin::symmetric(5.5, 1.5))
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(*k)
                                        .font(FontId::new(10.5, FontFamily::Monospace))
                                        .color(TEXT_PRIMARY),
                                );
                            });
                    }
                    ui.add_space(4.0);
                    ui.label(RichText::new(*desc).size(11.2).color(TEXT_SECONDARY));
                });
                ui.add_space(2.0);
            }
        });
}

fn configure_dark_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.spacing.button_padding = Vec2::new(10.0, 5.0);
    style.spacing.item_spacing = Vec2::new(6.0, 6.0);

    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = OBSIDIAN_BG;
    visuals.window_fill = GRAPHITE_SURFACE;
    visuals.window_rounding = Rounding::same(10.0);
    visuals.window_stroke = Stroke::new(1.0_f32, hairline_stroke());
    visuals.menu_rounding = Rounding::same(8.0);
    visuals.extreme_bg_color = OBSIDIAN_BG;
    visuals.faint_bg_color = ZINC_ELEVATED;

    visuals.widgets.noninteractive.bg_fill = ZINC_ELEVATED;
    visuals.widgets.noninteractive.weak_bg_fill = ZINC_ELEVATED;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, hairline_stroke());
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT_SECONDARY);
    visuals.widgets.noninteractive.rounding = Rounding::same(7.0);

    visuals.widgets.inactive.bg_fill = ZINC_ELEVATED;
    visuals.widgets.inactive.weak_bg_fill = ZINC_ELEVATED;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, hairline_stroke());
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT_SECONDARY);
    visuals.widgets.inactive.rounding = Rounding::same(7.0);

    visuals.widgets.hovered.bg_fill = ZINC_HOVER;
    visuals.widgets.hovered.weak_bg_fill = ZINC_HOVER;
    visuals.widgets.hovered.bg_stroke =
        Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 45));
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, TEXT_PRIMARY);
    visuals.widgets.hovered.rounding = Rounding::same(7.0);

    visuals.widgets.active.bg_fill = status_ice_fill();
    visuals.widgets.active.weak_bg_fill = status_ice_fill();
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, STATUS_ICE);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, TEXT_PRIMARY);
    visuals.widgets.active.rounding = Rounding::same(7.0);

    visuals.widgets.open.bg_fill = ZINC_HOVER;
    visuals.widgets.open.weak_bg_fill = ZINC_HOVER;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0_f32, STATUS_ICE);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0_f32, TEXT_PRIMARY);
    visuals.widgets.open.rounding = Rounding::same(7.0);

    visuals.selection.bg_fill = status_ice_fill();
    visuals.selection.stroke = Stroke::new(1.0_f32, STATUS_ICE);

    style.visuals = visuals;
    ctx.set_style(style);
}

fn read_process_rss_mb() -> f32 {
    if let Ok(statm) = fs::read_to_string("/proc/self/statm") {
        let mut parts = statm.split_whitespace();
        let _vmsize = parts.next();
        if let Some(rss_pages) = parts.next().and_then(|s| s.parse::<u64>().ok()) {
            return (rss_pages * 4096) as f32 / (1024.0 * 1024.0);
        }
    }
    0.0
}

fn select_top_bar_center_capsules(
    avail_center_w: f32,
    in_history: bool,
    is_fullscreen_live: bool,
) -> (bool, bool, bool, bool) {
    let grid_min_w = if is_fullscreen_live { 325.0 } else { 270.0 };
    let sd_hd_min_w = if is_fullscreen_live { 395.0 } else { 340.0 };
    let aspect_min_w = if in_history {
        365.0
    } else if is_fullscreen_live {
        490.0
    } else {
        435.0
    };
    let patrol_min_w = if is_fullscreen_live { 555.0 } else { 500.0 };

    let show_grid = avail_center_w >= grid_min_w;
    let show_sd_hd = !in_history && avail_center_w >= sd_hd_min_w;
    let show_aspect = avail_center_w >= aspect_min_w;
    let show_patrol = !in_history && avail_center_w >= patrol_min_w;
    (show_grid, show_sd_hd, show_aspect, show_patrol)
}

fn truncate_galley_with_ellipsis(
    painter: &egui::Painter,
    text: &str,
    font: FontId,
    color: Color32,
    max_w: f32,
) -> Arc<egui::Galley> {
    let chars: Vec<char> = text.chars().collect();
    let mut lo = 0usize;
    let mut hi = chars.len();
    let mut best: Option<Arc<egui::Galley>> = None;
    while lo <= hi {
        let mid = (lo + hi) / 2;
        let mut candidate: String = chars[..mid].iter().collect();
        candidate.push_str("...");
        let g = painter.layout_no_wrap(candidate, font.clone(), color);
        if g.size().x <= max_w {
            best = Some(g);
            lo = mid + 1;
        } else if mid == 0 {
            break;
        } else {
            hi = mid - 1;
        }
    }
    best.unwrap_or_else(|| painter.layout_no_wrap("...".to_string(), font, color))
}

fn validate_open_dir_input(raw_dir: &Path) -> Result<(), String> {
    let bytes = raw_dir.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.contains(&0) {
        return Err("Invalid directory path".to_string());
    }
    if !raw_dir.is_absolute() || !bytes.starts_with(b"/") {
        return Err(
            "Directory path must be absolute (e.g. /home/... or ~/...)".to_string(),
        );
    }
    Ok(())
}

fn validate_and_prepare_open_dir(raw_dir: &Path, allow_create: bool) -> Result<PathBuf, String> {
    validate_open_dir_input(raw_dir)?;
    reject_any_symlink_component(raw_dir)
        .map_err(|e| format!("Refusing directory with symlink component: {}", e))?;

    let prepared = if allow_create {
        ensure_private_dir(raw_dir).map_err(|e| format!("Cannot prepare directory: {}", e))?
    } else {
        let meta = fs::symlink_metadata(raw_dir)
            .map_err(|e| format!("Directory does not exist: {}", e))?;
        if !meta.is_dir() {
            return Err("Path is not a directory".to_string());
        }
        fs::canonicalize(raw_dir)
            .map_err(|e| format!("Failed to resolve directory: {}", e))?
    };

    let canon_bytes = prepared.as_os_str().as_bytes();
    if canon_bytes.is_empty()
        || canon_bytes.contains(&0)
        || !prepared.is_absolute()
        || !canon_bytes.starts_with(b"/")
    {
        return Err("Refusing non-absolute directory path".to_string());
    }
    Ok(prepared)
}

fn open_url(ctx: &egui::Context, url: &str) {
    if !url.starts_with("https://")
        || !url.is_ascii()
        || url.bytes().any(|b| b <= 0x20 || b == 0x7f)
    {
        return;
    }

    // Ensure OS fullscreen is cleared so the browser window can raise to the foreground
    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));

    let target = url.to_string();
    thread::spawn(move || {
        let in_snap = std::env::var_os("SNAP").is_some();
        let launched_via_gdk = if in_snap {
            false
        } else {
            let py_launch = concat!(
                "import os, sys, time, gi; ",
                "gi.require_version('Gdk', '3.0'); ",
                "from gi.repository import Gdk, Gio; ",
                "u = sys.argv[1]; ",
                "d = Gdk.Display.get_default(); ",
                "ctx = d.get_app_launch_context(); ",
                "ctx.set_timestamp(int(time.monotonic() * 1000) & 0xFFFFFFFF); ",
                "app = Gio.AppInfo.get_default_for_type('x-scheme-handler/https', False); ",
                "tok = ctx.get_startup_notify_id(app, []); ",
                "d.sync(); ",
                "os.environ['DESKTOP_STARTUP_ID'] = tok; ",
                "os.environ['XDG_ACTIVATION_TOKEN'] = tok; ",
                "app.launch_uris([u], ctx); ",
                "d.sync()"
            );

            let py_status = Command::new("/usr/bin/python3")
                .arg("-I")
                .arg("-c")
                .arg(py_launch)
                .arg(&target)
                .env("GDK_BACKEND", "x11")
                .env_remove("LD_LIBRARY_PATH")
                .env_remove("PYTHONPATH")
                .env_remove("PYTHONHOME")
                .env_remove("PYTHONSTARTUP")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();

            matches!(py_status, Ok(s) if s.success())
        };

        if !launched_via_gdk {
            let xdg_bin = resolve_trusted_binary("xdg-open");
            let _ = Command::new(&xdg_bin)
                .arg(&target)
                .env_remove("LD_LIBRARY_PATH")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_safe_clamp_never_panics_on_narrow_viewports() {
        assert_eq!(safe_clamp_f32(120.0, 95.0, 280.0), 120.0);
        assert_eq!(safe_clamp_f32(40.0, 95.0, 280.0), 95.0);
        assert_eq!(safe_clamp_f32(400.0, 95.0, 280.0), 280.0);

        // Narrow tiles / split-screen windows where max < min or max <= 0
        assert_eq!(safe_clamp_f32(120.0, 95.0, 28.0), 28.0);
        assert_eq!(safe_clamp_f32(10.0, 95.0, 28.0), 28.0);
        assert_eq!(safe_clamp_f32(120.0, 180.0, 0.0), 0.0);
        assert_eq!(safe_clamp_f32(120.0, 180.0, -15.0), 0.0);
        assert_eq!(safe_clamp_f32(120.0, 95.0, f32::NAN), 95.0);
    }

    #[test]
    fn test_tile_ui_state_default_zoom_is_one() {
        let tile = TileUiState::default();
        assert_eq!(tile.live.zoom, 1.0);
        assert_eq!(tile.live.pan_uv, Vec2::ZERO);
        assert_eq!(tile.pb.zoom, 1.0);
        assert_eq!(tile.pb.pan_uv, Vec2::ZERO);
        assert_eq!(tile.last_uploaded_seq, 0);
    }

    #[test]
    fn test_independent_live_vs_playback_zoom_isolation() {
        let mut tile = TileUiState::new();

        // Zoom Playback viewport to 4.0x and arm box zoom
        tile.viewport_mut(true).zoom_by(4.0, Pos2::new(0.25, 0.75));
        tile.viewport_mut(true).box_zoom_armed = true;
        assert!((tile.viewport(true).zoom - 4.0).abs() < 1e-5);
        assert!(tile.viewport(true).box_zoom_armed);

        // Live viewport remains completely untouched at 1.0x
        assert_eq!(tile.viewport(false).zoom, 1.0);
        assert_eq!(tile.viewport(false).pan_uv, Vec2::ZERO);
        assert!(!tile.viewport(false).box_zoom_armed);

        // Zoom Live viewport to 2.5x; Playback stays at 4.0x
        tile.viewport_mut(false).zoom_by(2.5, Pos2::new(0.8, 0.2));
        assert!((tile.viewport(false).zoom - 2.5).abs() < 1e-5);
        assert!((tile.viewport(true).zoom - 4.0).abs() < 1e-5);

        // Resetting Playback zoom leaves Live zoom intact
        tile.viewport_mut(true).reset_zoom();
        assert_eq!(tile.viewport(true).zoom, 1.0);
        assert!((tile.viewport(false).zoom - 2.5).abs() < 1e-5);
    }

    #[test]
    fn test_cursor_anchored_zoom_preserves_uv_point_and_snaps_back() {
        let mut vp = ZoomViewportState::default();
        let anchor = Pos2::new(0.25, 0.75);

        // Step 1: Zoom 2.0x anchored at (0.25, 0.75) -> UV point (0.25, 0.75) stays under cursor
        vp.zoom_by(2.0, anchor);
        assert!((vp.zoom - 2.0).abs() < 1e-5);
        let uv1 = vp.uv_rect();
        let u1 = uv1.min.x + anchor.x * uv1.width();
        let v1 = uv1.min.y + anchor.y * uv1.height();
        assert!((u1 - 0.25).abs() < 1e-5);
        assert!((v1 - 0.75).abs() < 1e-5);

        // Step 2: Further zoom 1.5x (to 3.0x) at a new cursor position (0.8, 0.3)
        let anchor2 = Pos2::new(0.8, 0.3);
        let expected_u = uv1.min.x + anchor2.x * uv1.width();
        let expected_v = uv1.min.y + anchor2.y * uv1.height();
        vp.zoom_by(1.5, anchor2);
        assert!((vp.zoom - 3.0).abs() < 1e-5);
        let uv2 = vp.uv_rect();
        let u2 = uv2.min.x + anchor2.x * uv2.width();
        let v2 = uv2.min.y + anchor2.y * uv2.height();
        assert!((u2 - expected_u).abs() < 1e-5);
        assert!((v2 - expected_v).abs() < 1e-5);

        // Gentle touchpad pinch-in from 1.0x must not be swallowed
        vp.reset_zoom();
        vp.zoom_by(1.005, Pos2::new(0.5, 0.5));
        assert!((vp.zoom - 1.005).abs() < 1e-5);

        // Zooming back out below 1.015x cleanly resets to 1.0x and Vec2::ZERO
        vp.zoom_by(0.99, Pos2::new(0.5, 0.5));
        assert_eq!(vp.zoom, 1.0);
        assert_eq!(vp.pan_uv, Vec2::ZERO);
    }

    #[test]
    fn test_tight_pan_clamping_prevents_dead_zone_overshoot() {
        let mut vp = ZoomViewportState::default();
        vp.zoom_by(2.0, Pos2::new(0.5, 0.5));
        // At 2.0x zoom, max_pan = 0.5 - 0.25 = 0.25
        // Dragging massively to the left (pushing pan_uv toward +infinity) must clamp tightly at +0.25
        vp.pan_by_pixels(Vec2::new(-5000.0, -5000.0), Vec2::new(400.0, 225.0));
        assert!((vp.pan_uv.x - 0.25).abs() < 1e-5);
        assert!((vp.pan_uv.y - 0.25).abs() < 1e-5);

        // Reversing drag by 40px must immediately move pan_uv back without any dead-zone delay
        vp.pan_by_pixels(Vec2::new(40.0, 0.0), Vec2::new(400.0, 225.0));
        assert!(vp.pan_uv.x < 0.25);
        assert!((vp.pan_uv.x - 0.20).abs() < 1e-5);
    }

    #[test]
    fn test_compute_video_rect_16_9_fit_vs_fill() {
        let wide_tile = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(400.0, 180.0));
        // Fill mode returns exact tile rect
        assert_eq!(compute_video_rect(wide_tile, false, 1280, 720), wide_tile);

        // 16:9 Fit inside wider tile -> pillarboxed (height = 180, width = 320, centered)
        let fit_wide = compute_video_rect(wide_tile, true, 1280, 720);
        assert!((fit_wide.width() - 320.0).abs() < 1e-4);
        assert!((fit_wide.height() - 180.0).abs() < 1e-4);
        assert!((fit_wide.center().x - wide_tile.center().x).abs() < 1e-4);
        assert!((fit_wide.center().y - wide_tile.center().y).abs() < 1e-4);

        // 16:9 Fit inside taller tile -> letterboxed (width = 320, height = 180, centered)
        let tall_tile = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(320.0, 260.0));
        let fit_tall = compute_video_rect(tall_tile, true, 0, 0);
        assert!((fit_tall.width() - 320.0).abs() < 1e-4);
        assert!((fit_tall.height() - 180.0).abs() < 1e-4);
        assert!((fit_tall.center().y - 130.0).abs() < 1e-4);
    }

    #[test]
    fn test_apply_cover_crop_uv_aspect_preserving_center_crop() {
        let full_uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
        let src_16_9 = 16.0 / 9.0;

        // Matching 16:9 tile -> identity UV
        let same = apply_cover_crop_uv(full_uv, src_16_9, src_16_9);
        assert!((same.width() - 1.0).abs() < 1e-5);
        assert!((same.height() - 1.0).abs() < 1e-5);

        // Taller 4:3 tile -> crops horizontally (width = 0.75, height = 1.0, centered at 0.5)
        let tall = apply_cover_crop_uv(full_uv, 4.0 / 3.0, src_16_9);
        assert!((tall.width() - 0.75).abs() < 1e-5);
        assert!((tall.height() - 1.0).abs() < 1e-5);
        assert!((tall.center().x - 0.5).abs() < 1e-5);
        assert!((tall.min.x - 0.125).abs() < 1e-5);

        // Ultra-wide 32:9 tile -> crops vertically (width = 1.0, height = 0.5, centered at 0.5)
        let wide = apply_cover_crop_uv(full_uv, 32.0 / 9.0, src_16_9);
        assert!((wide.width() - 1.0).abs() < 1e-5);
        assert!((wide.height() - 0.5).abs() < 1e-5);
        assert!((wide.center().y - 0.5).abs() < 1e-5);

        // Zoomed sub-UV preserves its zoomed center while applying cover crop
        let zoomed = Rect::from_min_max(Pos2::new(0.5, 0.0), Pos2::new(1.0, 0.5));
        let cropped_zoom = apply_cover_crop_uv(zoomed, 4.0 / 3.0, src_16_9);
        assert!((cropped_zoom.center().x - 0.75).abs() < 1e-5);
        assert!((cropped_zoom.center().y - 0.25).abs() < 1e-5);
        assert!((cropped_zoom.width() - 0.375).abs() < 1e-5);
        assert!((cropped_zoom.height() - 0.5).abs() < 1e-5);
    }

    #[test]
    fn test_hover_vs_keyboard_selection_targeting() {
        let order = vec![1, 2, 3, 4];

        // Fullscreen always wins
        assert_eq!(
            resolve_live_target_channel(Some(4), Some(1), Some(3), true, &order),
            Some(4)
        );
        // Pointer moved recently (use_hover_target = true) -> hovered camera wins over selected
        assert_eq!(
            resolve_live_target_channel(None, Some(1), Some(3), true, &order),
            Some(3)
        );
        // Arrow / [ / ] pressed (use_hover_target = false) -> keyboard selection wins over hovered
        assert_eq!(
            resolve_live_target_channel(None, Some(2), Some(3), false, &order),
            Some(2)
        );
        // Graceful fallback when hovered is None
        assert_eq!(
            resolve_live_target_channel(None, Some(2), None, true, &order),
            Some(2)
        );
        assert_eq!(
            resolve_live_target_channel(None, None, None, true, &order),
            Some(1)
        );
    }

    #[test]
    fn test_toast_queue_cap_dedup_and_open_dir_validation() {
        let mut q = VecDeque::new();
        let mut next_id = 0u64;
        let t0 = Instant::now();

        push_toast_to_queue(&mut q, &mut next_id, ToastKind::Info, "One".into(), None, t0);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].remaining, Duration::from_millis(3200));

        // Duplicate back-to-back message refreshes timer without adding a duplicate entry
        push_toast_to_queue(
            &mut q,
            &mut next_id,
            ToastKind::Warning,
            "One".into(),
            None,
            t0 + Duration::from_millis(500),
        );
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].remaining, Duration::from_millis(5500));

        // Pushing 4 more distinct items caps queue strictly at MAX_VISIBLE_TOASTS (3)
        for label in ["Two", "Three", "Four", "Five"] {
            push_toast_to_queue(
                &mut q,
                &mut next_id,
                ToastKind::Success,
                label.into(),
                Some(PathBuf::from("/tmp")),
                t0,
            );
        }
        assert_eq!(q.len(), MAX_VISIBLE_TOASTS);
        assert_eq!(q[0].message, "Three");
        assert_eq!(q[1].message, "Four");
        assert_eq!(q[2].message, "Five");
        // Actionable Success toast gets extended 5.5s duration
        assert_eq!(q[2].remaining, Duration::from_millis(5500));

        // Secure directory input validation blocks relative paths, "-flag" injection, and NUL bytes
        assert!(validate_open_dir_input(Path::new("/tmp")).is_ok());
        assert!(validate_open_dir_input(Path::new("-evil-flag")).is_err());
        assert!(validate_open_dir_input(Path::new("relative/dir")).is_err());
        assert!(validate_open_dir_input(Path::new("")).is_err());

        // Headless egui pass verifying toast ellipsis truncation never overflows max_w
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            let painter = ctx.layer_painter(egui::LayerId::background());
            let font = FontId::new(12.5, FontFamily::Proportional);
            let long_msg =
                "Saved snapshot: /home/operator/Pictures/NVR_Command_Center/ch01_front_gate_20261119_073500.png";
            let max_w = 180.0;
            let fitted = truncate_galley_with_ellipsis(&painter, long_msg, font.clone(), TEXT_PRIMARY, max_w);
            assert!(fitted.size().x <= max_w + 0.5);
            assert!(fitted.text().ends_with("..."));
            assert!(fitted.text().len() < long_msg.len());

            // Ultra-narrow fallback returns "..." without panicking
            let tiny = truncate_galley_with_ellipsis(&painter, long_msg, font, TEXT_PRIMARY, 4.0);
            assert_eq!(tiny.text(), "...");
        });
    }

    #[test]
    fn test_multi_tap_chord_state_machine_450ms_window() {
        let mut state = None;
        let t0 = Instant::now();

        // s -> ss -> sss within 450ms
        assert_eq!(advance_chord(&mut state, Key::S, t0), 1);
        assert_eq!(
            advance_chord(&mut state, Key::S, t0 + Duration::from_millis(180)),
            2
        );
        assert_eq!(
            advance_chord(&mut state, Key::S, t0 + Duration::from_millis(350)),
            3
        );

        // 4th tap wraps cleanly back to 1
        assert_eq!(
            advance_chord(&mut state, Key::S, t0 + Duration::from_millis(420)),
            1
        );

        // Tap after > 450ms starts fresh at 1
        assert_eq!(
            advance_chord(&mut state, Key::S, t0 + Duration::from_millis(950)),
            1
        );

        // Switching to another key immediately resets count to 1
        assert_eq!(
            advance_chord(&mut state, Key::H, t0 + Duration::from_millis(1020)),
            1
        );
    }

    #[test]
    fn test_2d_grid_selection_navigation() {
        let order: Vec<usize> = (1..=12).collect();
        // Move right & wrap within row (3x4 grid, cols=4)
        assert_eq!(move_grid_selection(&order, 4, Some(4), 0, 1), Some(1));
        assert_eq!(move_grid_selection(&order, 4, Some(1), 0, -1), Some(4));
        // Move down & wrap across rows
        assert_eq!(move_grid_selection(&order, 4, Some(2), 1, 0), Some(6));
        assert_eq!(move_grid_selection(&order, 4, Some(10), 1, 0), Some(2));
        assert_eq!(move_grid_selection(&order, 4, Some(3), -1, 0), Some(11));

        // Partial last row (10 cameras in 4 cols: row 2 has [9, 10])
        let ten: Vec<usize> = (1..=10).collect();
        assert_eq!(move_grid_selection(&ten, 4, Some(8), 1, 0), Some(10));
        assert_eq!(move_grid_selection(&ten, 4, Some(10), 0, 1), Some(9));
    }

    #[test]
    fn test_trackpad_scroll_zoom_symmetry_and_fast_zoom_out() {
        let mut vp = ZoomViewportState::new();
        let anchor = Pos2::new(0.35, 0.65);

        // Zoom in via 2-finger trackpad scroll (+85 points)
        let ev_in = [egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: Vec2::new(0.0, 85.0),
            modifiers: egui::Modifiers::NONE,
        }];
        let f_in = compute_tile_zoom_factor(&ev_in, None, PinchFrameSnapshot::default());
        assert!(f_in > 2.0);
        vp.zoom_by(f_in, anchor);
        assert!((vp.zoom - f_in).abs() < 1e-4);

        // Immediate opposite 2-finger swipe (-85 points) returns cleanly to 1.0x with zero lag
        let ev_out = [egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: Vec2::new(0.0, -85.0),
            modifiers: egui::Modifiers::NONE,
        }];
        let f_out = compute_tile_zoom_factor(&ev_out, None, PinchFrameSnapshot::default());
        assert!((f_in * f_out - 1.0).abs() < 1e-5);
        vp.zoom_by(f_out, anchor);
        assert_eq!(vp.zoom, 1.0);
        assert_eq!(vp.pan_uv, Vec2::ZERO);

        // Fast single-flick zoom-out from 5.0x -> 1.0x
        vp.zoom_by(5.0, Pos2::new(0.5, 0.5));
        assert_eq!(vp.zoom, 5.0);
        let fast_out = [egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: Vec2::new(0.0, -180.0),
            modifiers: egui::Modifiers::NONE,
        }];
        let f_fast = compute_tile_zoom_factor(&fast_out, None, PinchFrameSnapshot::default());
        vp.zoom_by(f_fast, Pos2::new(0.5, 0.5));
        assert_eq!(vp.zoom, 1.0);
    }

    #[test]
    fn test_wayland_pinch_suppresses_stray_axis_scroll() {
        // When pinching IN (zooming out: scale_delta = 0.72), a stray positive axis scroll
        // event in the same frame must NOT invert or cancel the zoom-out pinch.
        let events = [egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: Vec2::new(0.0, 40.0),
            modifiers: egui::Modifiers::NONE,
        }];
        let pinch = PinchFrameSnapshot {
            scale_delta: 0.72,
            pan_delta: Vec2::new(3.0, -2.0),
            suppress_axis_scroll: true,
        };
        let f = compute_tile_zoom_factor(&events, None, pinch);
        assert!((f - 0.72).abs() < 1e-6);
    }

    #[test]
    fn test_marquee_box_zoom_nested_zoom_and_step_back_history() {
        let mut vp = ZoomViewportState::new();
        let video = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(1920.0, 1080.0));

        // 1. Sloppy micro-drag (< MIN_MARQUEE_PX = 18px) is rejected so normal clicks never trigger box zoom
        let tiny_sel = Rect::from_min_size(Pos2::new(400.0, 300.0), Vec2::new(12.0, 10.0));
        assert!(vp.zoom_to_marquee_rect(tiny_sel, video).is_none());
        assert_eq!(vp.zoom, 1.0);
        assert!(vp.zoom_history.is_empty());

        // 2. Dragging a 480x270 box (25% width & height) centered at (0.75, 0.25) -> 4.0x zoom
        let box1 = Rect::from_center_size(Pos2::new(1440.0, 270.0), Vec2::new(480.0, 270.0));
        let z1 = vp.zoom_to_marquee_rect(box1, video).expect("valid marquee");
        assert!((z1 - 4.0).abs() < 1e-4);

        // Converge smooth glide animation
        for _ in 0..40 {
            if !vp.step_animation(0.016) {
                break;
            }
        }
        assert!((vp.zoom - 4.0).abs() < 1e-4);
        let uv1 = vp.uv_rect();
        assert!((uv1.center().x - 0.75).abs() < 1e-4);
        assert!((uv1.center().y - 0.25).abs() < 1e-4);

        // 3. Nested box zoom inside the 4.0x view (960x540 = 50% of viewport -> 8.0x MAX)
        let box2 = Rect::from_center_size(Pos2::new(960.0, 540.0), Vec2::new(960.0, 540.0));
        let z2 = vp.zoom_to_marquee_rect(box2, video).expect("nested marquee");
        assert!((z2 - 8.0).abs() < 1e-4);
        for _ in 0..40 {
            if !vp.step_animation(0.016) {
                break;
            }
        }
        assert!((vp.zoom - 8.0).abs() < 1e-4);
        assert_eq!(vp.zoom_history.len(), 2);

        // 4. Step back unwinds 8.0x -> 4.0x -> 1.0x
        assert!(vp.step_back_zoom());
        for _ in 0..40 {
            if !vp.step_animation(0.016) {
                break;
            }
        }
        assert!((vp.zoom - 4.0).abs() < 1e-4);

        assert!(vp.step_back_zoom());
        for _ in 0..40 {
            if !vp.step_animation(0.016) {
                break;
            }
        }
        assert_eq!(vp.zoom, 1.0);
        assert_eq!(vp.pan_uv, Vec2::ZERO);
    }

    #[test]
    fn test_interactive_radar_jump_to_uv_clamps_cleanly() {
        let mut vp = ZoomViewportState::new();
        vp.zoom_by(4.0, Pos2::new(0.5, 0.5));

        // Jump to (0.7, 0.3) via radar click
        vp.jump_to_radar_uv(Pos2::new(0.7, 0.3));
        assert!((vp.uv_rect().center().x - 0.7).abs() < 1e-4);
        assert!((vp.uv_rect().center().y - 0.3).abs() < 1e-4);

        // Clicking extreme top-left corner (0.0, 0.0) clamps UV center to 0.5 / 4.0 = 0.125
        vp.jump_to_radar_uv(Pos2::new(0.0, 0.0));
        assert!((vp.uv_rect().min.x - 0.0).abs() < 1e-5);
        assert!((vp.uv_rect().min.y - 0.0).abs() < 1e-5);
        assert!((vp.uv_rect().center().x - 0.125).abs() < 1e-4);
    }

    #[test]
    fn test_responsive_top_bar_center_capsules_progressive_disclosure() {
        // Wide desktop (1600px -> avail_center_w ~1130px): all 4 secondary capsules visible in Live Grid
        assert_eq!(
            select_top_bar_center_capsules(600.0, false, false),
            (true, true, true, true)
        );
        // Drop Patrol first, then [16:9 | Fill], then [SD | HD], then [Grid | Single], preserving [Live | History]
        assert_eq!(
            select_top_bar_center_capsules(460.0, false, false),
            (true, true, true, false)
        );
        assert_eq!(
            select_top_bar_center_capsules(380.0, false, false),
            (true, true, false, false)
        );
        assert_eq!(
            select_top_bar_center_capsules(300.0, false, false),
            (true, false, false, false)
        );
        assert_eq!(
            select_top_bar_center_capsules(220.0, false, false),
            (false, false, false, false)
        );

        // In History mode, SD/HD and Patrol are always hidden, Grid + Aspect progressively drop on narrow width
        assert_eq!(
            select_top_bar_center_capsules(400.0, true, false),
            (true, false, true, false)
        );
        assert_eq!(
            select_top_bar_center_capsules(300.0, true, false),
            (true, false, false, false)
        );
    }

    #[test]
    fn test_normalized_camera_names_trims_and_prunes_defaults() {
        let mut cfg = AppConfig::default();
        cfg.camera_names.insert(1, "  Front Gate  ".to_string());
        cfg.camera_names.insert(2, "Camera 2".to_string());
        cfg.camera_names.insert(3, "   ".to_string());
        cfg.camera_names.insert(4, "  Camera 4 ".to_string());
        cfg.camera_names.insert(5, "Driveway".to_string());

        let norm = normalized_camera_names(&cfg);
        assert_eq!(norm.len(), 2);
        assert_eq!(norm.get(&1).map(String::as_str), Some("Front Gate"));
        assert_eq!(norm.get(&5).map(String::as_str), Some("Driveway"));
        assert!(!norm.contains_key(&2));
        assert!(!norm.contains_key(&3));
        assert!(!norm.contains_key(&4));
    }

    #[test]
    fn test_validate_and_prepare_open_dir_rejects_symlinks() {
        let base = std::env::temp_dir().join(format!(
            "nvr_cam_test_open_dir_{}",
            std::process::id()
        ));
        let real_dir = base.join("real");
        let link_dir = base.join("link");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&real_dir).unwrap();
        std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();

        let err_false = validate_and_prepare_open_dir(&link_dir, false);
        assert!(err_false.is_err());

        let err_true = validate_and_prepare_open_dir(&link_dir, true);
        assert!(err_true.is_err());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_apply_cover_crop_uv_tiny_uv_spans_do_not_panic() {
        let tiny_w = Rect::from_min_size(Pos2::new(0.5, 0.2), Vec2::new(1e-6, 0.5));
        let res_w = apply_cover_crop_uv(tiny_w, 16.0 / 9.0, 16.0 / 9.0);
        assert!(res_w.width().is_finite() && res_w.height().is_finite());

        let tiny_h = Rect::from_min_size(Pos2::new(0.2, 0.5), Vec2::new(0.5, 1e-6));
        let res_h = apply_cover_crop_uv(tiny_h, 4.0 / 3.0, 16.0 / 9.0);
        assert!(res_h.width().is_finite() && res_h.height().is_finite());

        let zero_span = Rect::from_min_size(Pos2::new(0.5, 0.5), Vec2::new(0.0, 0.0));
        let res_z = apply_cover_crop_uv(zero_span, 32.0 / 9.0, 16.0 / 9.0);
        assert!(res_z.width().is_finite() && res_z.height().is_finite());
    }
}
