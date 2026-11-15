use crate::config::{expand_tilde, save_env_config, AppConfig, NvrEnvConfig};
use crate::p2p::{is_lan_reachable, P2PTunnelManager};
use crate::stream::{scan_active_nvr_channels, StreamManager, StreamStatus};
use crate::wayland_pinch::{PinchFrameSnapshot, WaylandPinchListener};
use chrono::Local;
use eframe::egui::{
    self, Align2, Color32, ColorImage, CursorIcon, FontFamily, FontId, Key, PointerButton, Pos2,
    Rect, RichText, Rounding, Sense, Stroke, TextureHandle, TextureOptions, Ui, Vec2,
};
use image::{ImageBuffer, Rgba, RgbaImage};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const BUY_ME_A_COFFEE_URL: &str = "https://buymeacoffee.com/pulkitmundra";
const GITHUB_SPONSORS_URL: &str = "https://github.com/sponsors/pulkitmundra";
const GITHUB_REPO_URL: &str = "https://github.com/pulkitmundra/nvr-command-center";
const PORTFOLIO_URL: &str = "https://pulkit-mundra-portfolio.vercel.app/projects/nvr-command-center";

const CHORD_WINDOW: Duration = Duration::from_millis(450);
const OS_FULLSCREEN_BAR_HIDE: Duration = Duration::from_millis(2800);

// Obsidian Studio Palette
const OBSIDIAN_BG: Color32 = Color32::from_rgb(9, 9, 11);
const GRAPHITE_SURFACE: Color32 = Color32::from_rgb(17, 17, 20);
const ZINC_ELEVATED: Color32 = Color32::from_rgb(24, 24, 27);
const ZINC_HOVER: Color32 = Color32::from_rgb(39, 39, 46);
const ZINC_DIM: Color32 = Color32::from_rgb(82, 82, 91);
const TEXT_PRIMARY: Color32 = Color32::from_rgb(244, 244, 245);
const TEXT_SECONDARY: Color32 = Color32::from_rgb(212, 212, 216);
const TEXT_MUTED: Color32 = Color32::from_rgb(161, 161, 170);

const STATUS_EMERALD: Color32 = Color32::from_rgb(16, 185, 129);
const STATUS_ICE: Color32 = Color32::from_rgb(56, 189, 248);
const STATUS_AMBER: Color32 = Color32::from_rgb(245, 158, 11);
const STATUS_CORAL: Color32 = Color32::from_rgb(239, 68, 68);

#[inline]
fn hairline_stroke() -> Color32 {
    Color32::from_rgba_unmultiplied(255, 255, 255, 20)
}

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

struct TileUiState {
    texture: Option<TextureHandle>,
    last_uploaded_seq: u64,
    tex_width: u32,
    tex_height: u32,
    zoom: f32,
    pan_uv: Vec2,
    anim_target: Option<(f32, Vec2)>,
    marquee_drag: Option<(Pos2, Pos2)>,
    box_zoom_armed: bool,
    zoom_history: Vec<(f32, Vec2)>,
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
            zoom: 1.0,
            pan_uv: Vec2::ZERO,
            anim_target: None,
            marquee_drag: None,
            box_zoom_armed: false,
            zoom_history: Vec::new(),
        }
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

pub struct CameraViewerApp {
    env_path: PathBuf,
    env_cfg: NvrEnvConfig,
    edit_env_cfg: NvrEnvConfig,
    app_cfg: AppConfig,
    p2p_mgr: P2PTunnelManager,
    stream_mgr: StreamManager,
    tiles: BTreeMap<usize, TileUiState>,
    upload_scratch: Vec<u8>,
    wayland_pinch: Option<WaylandPinchListener>,
    frame_pinch: PinchFrameSnapshot,

    fullscreen_channel: Option<usize>,
    selected_channel: Option<usize>,
    hovered_channel: Option<usize>,
    all_paused: bool,
    patrol_active: bool,
    last_patrol_switch: Instant,

    zen_mode: bool,
    os_fullscreen: bool,
    top_bar_peek: bool,
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

    toast_message: Option<(String, Instant)>,
    scan_in_progress: Arc<Mutex<Option<Option<Vec<usize>>>>>,

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

        let mut p2p_mgr = P2PTunnelManager::new();
        let mut initial_toast = None;
        let is_unconfigured = env_cfg.password.trim().is_empty();

        // Auto-detect whether local LAN NVR is reachable or away on another internet connection
        if !is_unconfigured && !env_cfg.p2p_serial.trim().is_empty() && !env_cfg.use_tailscale {
            if is_lan_reachable(&env_cfg.host, env_cfg.port, 500) {
                if env_cfg.use_cloud_p2p {
                    env_cfg.use_cloud_p2p = false;
                    initial_toast = Some((
                        "Local LAN NVR detected -> Switched to Direct LAN".to_string(),
                        Instant::now(),
                    ));
                }
            } else if !env_cfg.use_cloud_p2p {
                env_cfg.use_cloud_p2p = true;
                initial_toast = Some((
                    "Local LAN NVR unreachable -> Auto-switched to Cloud P2P Tunnel".to_string(),
                    Instant::now(),
                ));
            }
        }

        if !is_unconfigured && env_cfg.use_cloud_p2p && !env_cfg.p2p_serial.trim().is_empty() {
            let local_port = p2p_mgr.ensure_started(
                &env_cfg.p2p_serial,
                &env_cfg.username,
                &env_cfg.password,
                env_cfg.port,
            );
            env_cfg.p2p_local_port = local_port;
        }

        let app_cfg = AppConfig::load_and_sync(&env_cfg);
        let initial_selected = app_cfg.channel_order.first().copied();
        let mut stream_mgr = StreamManager::new(env_cfg.clone(), p2p_mgr.is_connected_handle());
        if !is_unconfigured {
            stream_mgr.start_streams(app_cfg.num_cameras);
        }

        let mut tiles = BTreeMap::new();
        for ch in 1..=app_cfg.num_cameras {
            tiles.insert(ch, TileUiState::new());
        }

        Self {
            env_path,
            edit_env_cfg: env_cfg.clone(),
            env_cfg,
            app_cfg,
            p2p_mgr,
            stream_mgr,
            tiles,
            upload_scratch: Vec::new(),
            wayland_pinch,
            frame_pinch: PinchFrameSnapshot::default(),
            fullscreen_channel: None,
            selected_channel: initial_selected,
            hovered_channel: None,
            all_paused: false,
            patrol_active: false,
            last_patrol_switch: Instant::now(),
            zen_mode: false,
            os_fullscreen: false,
            top_bar_peek: false,
            last_pointer_activity: Instant::now(),
            last_pointer_pos: None,
            last_center_width: 330.0,
            show_settings: is_unconfigured,
            settings_tab: SettingsTab::NvrAndCameras,
            show_password: false,
            show_shortcuts_modal: false,
            chord_state: None,
            rename_modal_channel: None,
            rename_input: String::new(),
            toast_message: initial_toast,
            scan_in_progress: Arc::new(Mutex::new(None)),
            rss_mb: read_process_rss_mb(),
            last_sys_poll: Instant::now(),
        }
    }

    fn active_target_channel(&self) -> Option<usize> {
        self.fullscreen_channel
            .or(self.selected_channel)
            .or(self.hovered_channel)
            .or_else(|| self.app_cfg.channel_order.first().copied())
    }

    fn sync_p2p_tunnel(&mut self) {
        if self.env_cfg.use_cloud_p2p && !self.env_cfg.p2p_serial.trim().is_empty() {
            let local_port = self.p2p_mgr.ensure_started(
                &self.env_cfg.p2p_serial,
                &self.env_cfg.username,
                &self.env_cfg.password,
                self.env_cfg.port,
            );
            self.env_cfg.p2p_local_port = local_port;
            self.edit_env_cfg.p2p_local_port = local_port;
        } else {
            self.p2p_mgr.stop();
        }
    }

    fn show_toast(&mut self, msg: impl Into<String>) {
        self.toast_message = Some((msg.into(), Instant::now()));
    }

    fn enter_fullscreen(&mut self, channel: usize) {
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
            self.show_toast(format!(
                "{} -> {}",
                self.app_cfg.camera_display_name(channel),
                label
            ));
        }
    }

    fn set_all_cameras_subtype(&mut self, subtype: u8, toast: impl Into<String>) {
        self.env_cfg.default_subtype = subtype;
        self.edit_env_cfg.default_subtype = subtype;
        self.stream_mgr.set_all_subtype(subtype);
        self.show_toast(toast);
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
            self.show_toast(format!(
                "{} -> {}",
                self.app_cfg.camera_display_name(channel),
                label
            ));
        }
    }

    fn reset_all_zoom(&mut self) {
        for tile in self.tiles.values_mut() {
            tile.animate_reset_zoom();
        }
        self.show_toast("Reset zoom on all cameras");
    }

    fn reset_tiles_for_restart(&mut self) {
        let max_ch = self.app_cfg.num_cameras;
        self.tiles.retain(|&ch, _| ch <= max_ch);
        for ch in 1..=max_ch {
            let tile = self.tiles.entry(ch).or_default();
            tile.last_uploaded_seq = 0;
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
        self.reset_tiles_for_restart();
        self.stream_mgr.update_env_and_restart(self.env_cfg.clone());
        if self.all_paused {
            self.stream_mgr.set_all_paused(true);
        }
        if let Some(fs_ch) = self.fullscreen_channel {
            self.stream_mgr
                .set_fullscreen_focus(Some(fs_ch), self.app_cfg.fullscreen_auto_hd);
        }
    }

    fn trigger_manual_resync(&mut self) {
        if self.env_cfg.use_cloud_p2p
            && !self.env_cfg.p2p_serial.trim().is_empty()
            && self.p2p_mgr.is_halted()
        {
            self.p2p_mgr.clear_active_key();
            let prev_port = self.env_cfg.p2p_local_port;
            self.sync_p2p_tunnel();
            if self.env_cfg.p2p_local_port != prev_port {
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

    fn save_single_snapshot(&mut self, channel: usize) {
        self.save_camera_snapshot(channel, true);
    }

    fn save_camera_snapshot(&mut self, channel: usize, crop_to_zoom: bool) {
        let Some(slot) = self.stream_mgr.slot(channel) else {
            return;
        };
        let (rgba, w, h) = {
            let Ok(guard) = slot.buffer.lock() else {
                return;
            };
            if guard.rgba.is_empty() || guard.width == 0 || guard.height == 0 {
                self.show_toast(format!("Camera {} has no frame yet", channel));
                return;
            }
            (guard.rgba.clone(), guard.width, guard.height)
        };

        let zoom_crop = if crop_to_zoom {
            self.tiles
                .get(&channel)
                .filter(|t| t.zoom > 1.02)
                .map(|t| (t.zoom, t.uv_rect()))
        } else {
            None
        };

        let out_dir = expand_tilde(&self.app_cfg.screenshot_dir);
        let _ = fs::create_dir_all(&out_dir);
        let ts = Local::now().format("%Y%m%d_%H%M%S");

        if let Some((z, uv)) = zoom_crop {
            let x0 = ((uv.min.x * w as f32).floor() as u32).min(w.saturating_sub(1));
            let y0 = ((uv.min.y * h as f32).floor() as u32).min(h.saturating_sub(1));
            let x1 = ((uv.max.x * w as f32).ceil() as u32).clamp(x0 + 1, w);
            let y1 = ((uv.max.y * h as f32).ceil() as u32).clamp(y0 + 1, h);
            let cw = x1 - x0;
            let ch = y1 - y0;
            let mut cropped = Vec::with_capacity((cw * ch * 4) as usize);
            for yy in y0..y1 {
                let row_start = ((yy * w + x0) * 4) as usize;
                let row_end = row_start + (cw * 4) as usize;
                if row_end <= rgba.len() {
                    cropped.extend_from_slice(&rgba[row_start..row_end]);
                }
            }
            let filename = format!("cam{:02}_zoom{:.1}x_{}.png", channel, z, ts);
            let full_path = out_dir.join(&filename);
            if let Some(img) = ImageBuffer::<Rgba<u8>, _>::from_raw(cw, ch, cropped) {
                match img.save(&full_path) {
                    Ok(()) => self.show_toast(format!(
                        "Saved {:.1}x zoomed snapshot: {}",
                        z,
                        full_path.display()
                    )),
                    Err(e) => self.show_toast(format!("Snapshot failed: {}", e)),
                }
            }
            return;
        }

        let filename = format!("cam{:02}_{}.png", channel, ts);
        let full_path = out_dir.join(&filename);

        if let Some(img) = ImageBuffer::<Rgba<u8>, _>::from_raw(w, h, rgba) {
            match img.save(&full_path) {
                Ok(()) => self.show_toast(format!("Saved snapshot: {}", full_path.display())),
                Err(e) => self.show_toast(format!("Snapshot failed: {}", e)),
            }
        }
    }

    fn save_composite_wall_snapshot(&mut self) {
        if let Some(ch) = self.fullscreen_channel {
            self.save_single_snapshot(ch);
            return;
        }

        let (rows, cols) = self.app_cfg.grid_dimensions();
        let tile_w = 480u32;
        let tile_h = 270u32;
        let mut canvas = RgbaImage::from_pixel(
            cols as u32 * tile_w,
            rows as u32 * tile_h,
            Rgba([9, 9, 11, 255]),
        );

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

        let out_dir = expand_tilde(&self.app_cfg.screenshot_dir);
        let _ = fs::create_dir_all(&out_dir);
        let ts = Local::now().format("%Y%m%d_%H%M%S");
        let full_path = out_dir.join(format!(
            "wall_{}cams_{}.png",
            self.app_cfg.num_cameras, ts
        ));

        match canvas.save(&full_path) {
            Ok(()) => self.show_toast(format!(
                "Saved {}-cam wall snapshot: {}",
                self.app_cfg.num_cameras,
                full_path.display()
            )),
            Err(e) => self.show_toast(format!("Snapshot error: {}", e)),
        }
    }

    fn apply_and_save_settings(&mut self) {
        self.edit_env_cfg.num_cameras = self.edit_env_cfg.num_cameras.clamp(1, 32);
        self.env_cfg = self.edit_env_cfg.clone();
        self.p2p_mgr.clear_active_key();
        self.sync_p2p_tunnel();
        self.app_cfg.num_cameras = self.env_cfg.num_cameras;
        self.app_cfg.normalize_channels();

        let _ = save_env_config(&self.env_path, &self.env_cfg);
        let _ = self.app_cfg.save();

        self.fullscreen_channel = None;
        self.patrol_active = false;
        self.restart_streams_with_current_env();
        self.show_toast(format!(
            "Saved configuration ({} cameras on {}:{})",
            self.env_cfg.num_cameras,
            self.env_cfg.active_host(),
            self.env_cfg.active_port()
        ));
    }

    fn switch_network_mode(&mut self) {
        if !self.env_cfg.p2p_serial.trim().is_empty() {
            self.env_cfg.use_cloud_p2p = !self.env_cfg.use_cloud_p2p;
            if self.env_cfg.use_cloud_p2p {
                self.env_cfg.use_tailscale = false;
                self.p2p_mgr.clear_active_key();
            }
            self.edit_env_cfg.use_cloud_p2p = self.env_cfg.use_cloud_p2p;
            self.edit_env_cfg.use_tailscale = self.env_cfg.use_tailscale;
            self.sync_p2p_tunnel();
            let _ = save_env_config(&self.env_path, &self.env_cfg);
            self.restart_streams_with_current_env();
            let mode_desc = if self.env_cfg.use_cloud_p2p {
                "Cloud P2P Tunnel"
            } else {
                "Local LAN Direct"
            };
            self.show_toast(format!("Switched to {}", mode_desc));
        } else if !self.env_cfg.tailscale_host.trim().is_empty() {
            self.env_cfg.use_tailscale = !self.env_cfg.use_tailscale;
            self.edit_env_cfg.use_tailscale = self.env_cfg.use_tailscale;
            let _ = save_env_config(&self.env_path, &self.env_cfg);
            self.restart_streams_with_current_env();
            self.show_toast(format!("Switched to {}", self.env_cfg.active_host()));
        } else {
            self.edit_env_cfg = self.env_cfg.clone();
            self.show_settings = true;
            self.settings_tab = SettingsTab::NvrAndCameras;
            self.show_toast("Configure Cloud P2P Serial or Tailscale IP in Settings");
        }
    }

    fn toggle_patrol_mode(&mut self) {
        self.patrol_active = !self.patrol_active;
        self.last_patrol_switch = Instant::now();
        if self.patrol_active && self.fullscreen_channel.is_none() {
            if let Some(target_ch) = self.active_target_channel() {
                self.enter_fullscreen(target_ch);
            }
        }
        self.show_toast(if self.patrol_active {
            format!(
                "Patrol Mode ON (cycling every {}s)",
                self.app_cfg.patrol_interval_secs
            )
        } else {
            "Patrol Mode OFF".to_string()
        });
    }

    fn toggle_aspect_mode(&mut self) {
        self.app_cfg.aspect_fit_16_9 = !self.app_cfg.aspect_fit_16_9;
        let _ = self.app_cfg.save();
        self.show_toast(if self.app_cfg.aspect_fit_16_9 {
            "Aspect mode: 16:9 Fit"
        } else {
            "Aspect mode: Fill Tile"
        });
    }

    fn update_pointer_tracking(&mut self, ctx: &egui::Context) {
        let (latest_pos, any_down, raw_scroll) = ctx.input(|i| {
            (
                i.pointer.latest_pos(),
                i.pointer.any_down(),
                i.raw_scroll_delta,
            )
        });
        let popup_open = ctx.memory(|m| m.any_popup_open());

        let moved = match (latest_pos, self.last_pointer_pos) {
            (Some(cur), Some(prev)) => cur.distance(prev) > 0.5,
            (Some(_), None) => true,
            _ => false,
        };
        if moved || any_down || raw_scroll != Vec2::ZERO {
            self.last_pointer_activity = Instant::now();
            self.last_pointer_pos = latest_pos;
        }

        if let Some(pos) = latest_pos {
            if pos.y <= 12.0 {
                self.top_bar_peek = true;
            } else if pos.y > 48.0 && !popup_open {
                self.top_bar_peek = false;
            }
        } else if !popup_open {
            self.top_bar_peek = false;
        }
    }

    fn should_show_top_bar(&self, ctx: &egui::Context) -> bool {
        if self.top_bar_peek || ctx.memory(|m| m.any_popup_open()) {
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

    fn handle_keyboard_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            let had_marquee = self
                .tiles
                .values()
                .any(|t| t.marquee_drag.is_some() || t.box_zoom_armed);
            if had_marquee {
                for tile in self.tiles.values_mut() {
                    tile.marquee_drag = None;
                    tile.box_zoom_armed = false;
                }
            } else if self.show_shortcuts_modal {
                self.show_shortcuts_modal = false;
            } else if self.rename_modal_channel.is_some() {
                self.rename_modal_channel = None;
            } else if self.show_settings {
                self.show_settings = false;
            } else if let Some(fs_ch) = self.fullscreen_channel {
                let fs_zoomed = self.tiles.get(&fs_ch).is_some_and(|t| {
                    t.zoom > 1.02 || t.anim_target.is_some_and(|(tz, _)| tz > 1.02)
                });
                if fs_zoomed {
                    if let Some(tile) = self.tiles.get_mut(&fs_ch) {
                        tile.animate_reset_zoom();
                    }
                    self.show_toast(format!(
                        "{} zoom -> 1.0x (Esc again for Grid)",
                        self.app_cfg.camera_display_name(fs_ch)
                    ));
                } else {
                    self.exit_fullscreen();
                }
            } else if let Some(active_ch) = self.active_target_channel().filter(|ch| {
                self.tiles.get(ch).is_some_and(|t| {
                    t.zoom > 1.02 || t.anim_target.is_some_and(|(tz, _)| tz > 1.02)
                })
            }) {
                if let Some(tile) = self.tiles.get_mut(&active_ch) {
                    tile.animate_reset_zoom();
                }
                self.show_toast(format!(
                    "{} zoom -> 1.0x",
                    self.app_cfg.camera_display_name(active_ch)
                ));
            } else if self.zen_mode {
                self.zen_mode = false;
                self.show_toast("Command bar visible");
            } else if self.os_fullscreen {
                self.os_fullscreen = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                self.show_toast("Exited OS Fullscreen");
            }
            return;
        }

        if self.show_settings || self.rename_modal_channel.is_some() || ctx.wants_keyboard_input() {
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
        let mut toggle_os_fs = false;

        ctx.input(|i| {
            let mods = i.modifiers;
            let bare = !mods.ctrl && !mods.alt && !mods.command;

            // Ctrl+S or C -> Snapshot (crops zoomed region when in Single View & zoomed)
            if ((mods.ctrl || mods.command) && i.key_pressed(Key::S))
                || (bare && !mods.shift && i.key_pressed(Key::C))
            {
                self.save_composite_wall_snapshot();
                return;
            }

            // Backspace -> Step Back Previous Box Zoom
            if bare && !mods.shift && i.key_pressed(Key::Backspace) {
                if let Some(ch) = self.active_target_channel() {
                    let stepped_to = self.tiles.get_mut(&ch).and_then(|tile| {
                        if tile.step_back_zoom() {
                            Some(tile.anim_target.map(|(tz, _)| tz).unwrap_or(1.0))
                        } else {
                            None
                        }
                    });
                    if let Some(tz) = stepped_to {
                        self.show_toast(format!(
                            "{} zoom -> {:.1}x",
                            self.app_cfg.camera_display_name(ch),
                            tz
                        ));
                    }
                }
            }

            // S / SSS / Shift+S -> Stream SD
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

            // H / HHH / Shift+H -> Stream HD
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

            // Z / ZZZ / Shift+Z -> Reset Zoom
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
                        if let Some(ch) = self.active_target_channel() {
                            if let Some(tile) = self.tiles.get_mut(&ch) {
                                tile.animate_reset_zoom();
                            }
                            self.show_toast(format!(
                                "{} zoom -> 1.0x",
                                self.app_cfg.camera_display_name(ch)
                            ));
                        }
                    }
                }
            }

            // Keyboard Zoom In (+ / =) and Zoom Out (-)
            let plus_pressed = bare
                && (i.key_pressed(Key::Plus)
                    || i.key_pressed(Key::Equals)
                    || i.events
                        .iter()
                        .any(|ev| matches!(ev, egui::Event::Text(t) if t == "+" || t == "=")));
            if plus_pressed {
                if let Some(ch) = self.active_target_channel() {
                    let tile = self.tiles.entry(ch).or_default();
                    tile.zoom_by(1.25, Pos2::new(0.5, 0.5));
                }
            } else if bare && !mods.shift && i.key_pressed(Key::Minus) {
                if let Some(ch) = self.active_target_channel() {
                    let tile = self.tiles.entry(ch).or_default();
                    tile.zoom_by(1.0 / 1.25, Pos2::new(0.5, 0.5));
                }
            }

            // Navigation & Single View
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
                    self.selected_channel = move_grid_selection(
                        &self.app_cfg.channel_order,
                        cols,
                        self.selected_channel,
                        0,
                        -1,
                    );
                } else if i.key_pressed(Key::ArrowRight) {
                    self.selected_channel = move_grid_selection(
                        &self.app_cfg.channel_order,
                        cols,
                        self.selected_channel,
                        0,
                        1,
                    );
                } else if i.key_pressed(Key::ArrowUp) {
                    self.selected_channel = move_grid_selection(
                        &self.app_cfg.channel_order,
                        cols,
                        self.selected_channel,
                        -1,
                        0,
                    );
                } else if i.key_pressed(Key::ArrowDown) {
                    self.selected_channel = move_grid_selection(
                        &self.app_cfg.channel_order,
                        cols,
                        self.selected_channel,
                        1,
                        0,
                    );
                } else if bare && i.key_pressed(Key::OpenBracket) {
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

            // F11 -> True OS Monitor Fullscreen
            if bare && !mods.shift && i.key_pressed(Key::F11) {
                toggle_os_fs = true;
            } else if bare && mods.shift && i.key_pressed(Key::F11) {
                self.toggle_single_view_on(11);
            }

            // Wall Controls: A, B, M, P/Space, R
            if bare && !mods.shift {
                if i.key_pressed(Key::A) {
                    self.toggle_aspect_mode();
                } else if i.key_pressed(Key::B) {
                    self.zen_mode = !self.zen_mode;
                    self.show_toast(if self.zen_mode {
                        "Zen Mode ON (Press B or Esc for bar)"
                    } else {
                        "Command bar visible"
                    });
                } else if i.key_pressed(Key::M) {
                    self.toggle_patrol_mode();
                } else if i.key_pressed(Key::P) || i.key_pressed(Key::Space) {
                    self.all_paused = !self.all_paused;
                    self.stream_mgr.set_all_paused(self.all_paused);
                    self.show_toast(if self.all_paused {
                        "Paused all streams"
                    } else {
                        "Resumed all streams"
                    });
                } else if i.key_pressed(Key::R) {
                    self.trigger_manual_resync();
                    self.show_toast("Re-synced all camera streams to live edge");
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
                    let target_zoomed = self
                        .active_target_channel()
                        .and_then(|ch| self.tiles.get(&ch).map(|t| (ch, t.zoom > 1.01)))
                        .unwrap_or((1, false));
                    if target_zoomed.1 {
                        if let Some(tile) = self.tiles.get_mut(&target_zoomed.0) {
                            tile.animate_reset_zoom();
                        }
                        self.show_toast(format!(
                            "{} zoom -> 1.0x",
                            self.app_cfg.camera_display_name(target_zoomed.0)
                        ));
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
        });

        if toggle_os_fs {
            self.os_fullscreen = !self.os_fullscreen;
            self.last_pointer_activity = Instant::now();
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.os_fullscreen));
            self.show_toast(if self.os_fullscreen {
                "OS Fullscreen ON (F11 or Esc to exit)"
            } else {
                "Exited OS Fullscreen"
            });
        }
    }

    fn update_gpu_textures(&mut self, ctx: &egui::Context) {
        let visible_channels: Vec<usize> = if let Some(fs_ch) = self.fullscreen_channel {
            vec![fs_ch]
        } else {
            self.app_cfg.channel_order.clone()
        };

        for ch in visible_channels {
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
    }

    fn render_top_command_bar(&mut self, ui: &mut Ui) {
        let total = self.app_cfg.num_cameras;
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
        let bar_rect = ui.max_rect();

        // 1. Left Zone: Geometric NVR mark + Wordmark + Calm Status/Network Capsule
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
                                STATUS_ICE
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

                // Calm Status + Network Switcher Capsule
                let status_dot = if self.all_paused {
                    STATUS_AMBER
                } else if live_count == total && total > 0 {
                    STATUS_EMERALD
                } else if live_count > 0 {
                    STATUS_AMBER
                } else {
                    STATUS_CORAL
                };

                let net_short = if self.env_cfg.use_cloud_p2p {
                    "Cloud P2P"
                } else if self.env_cfg.use_tailscale {
                    "Tailscale"
                } else {
                    "LAN"
                };
                let capsule_text = format!("{}/{}  {}", live_count, total, net_short);
                let text_w = capsule_text.len() as f32 * 6.8 + 26.0;
                let (cap_rect, cap_resp) =
                    ui.allocate_exact_size(Vec2::new(text_w, 26.0), Sense::click());
                if cap_resp.hovered() {
                    ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                }
                let cap_fill = if cap_resp.hovered() {
                    ZINC_HOVER
                } else {
                    ZINC_ELEVATED
                };
                let cap_stroke = if self.env_cfg.use_cloud_p2p {
                    Color32::from_rgba_unmultiplied(56, 189, 248, 95)
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
                    cp.text(
                        Pos2::new(cap_rect.min.x + 19.0, cap_rect.center().y),
                        Align2::LEFT_CENTER,
                        &capsule_text,
                        FontId::new(12.0, FontFamily::Proportional),
                        TEXT_PRIMARY,
                    );
                }

                let net_detail = if self.env_cfg.use_cloud_p2p {
                    format!(
                        "Cloud P2P Tunnel ({}) - {}:{}",
                        self.p2p_mgr.status_text(),
                        self.env_cfg.active_host(),
                        self.env_cfg.active_port()
                    )
                } else if self.env_cfg.use_tailscale {
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
                    "{}\nLive Cameras: {}/{}  |  Sync: <80ms live edge\nBurst-Drained Frames: {}  |  RAM: {:.0} MB\n\nClick to switch LAN <-> Cloud P2P / Tailscale",
                    net_detail, live_count, total, total_drained, self.rss_mb
                );
                if cap_resp.on_hover_text(tooltip).clicked() {
                    self.switch_network_mode();
                }

                left_end_x = ui.min_rect().max.x;
            },
        );

        // 2. Right Zone: Quiet Ghost Pills (Settings, ?, Snap, Sync, Pause/Resume)
        let mut right_start_x = bar_rect.max.x;
        ui.allocate_new_ui(
            egui::UiBuilder::new()
                .max_rect(bar_rect)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
            |ui| {
                ui.spacing_mut().item_spacing.x = 5.0;

                if draw_pill_button(
                    ui,
                    "Settings",
                    self.show_settings,
                    PillAccent::Ice,
                    true,
                )
                .on_hover_text("NVR, Camera & Performance Settings")
                .clicked()
                {
                    self.edit_env_cfg = self.env_cfg.clone();
                    self.show_settings = !self.show_settings;
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

                if draw_pill_button(ui, "Snap", false, PillAccent::Ice, true)
                    .on_hover_text(
                        "Save PNG Snapshot - composite wall in Grid, or full frame in Single View (C / Ctrl+S)",
                    )
                    .clicked()
                {
                    self.save_composite_wall_snapshot();
                }

                if draw_pill_button(ui, "Sync", false, PillAccent::Ice, true)
                    .on_hover_text("Flush buffers & re-sync all cameras to live edge (R)")
                    .clicked()
                {
                    self.trigger_manual_resync();
                    self.show_toast("Flushed all buffers -> Locked to live edge");
                }

                let pause_label = if self.all_paused { "Resume" } else { "Pause" };
                if draw_pill_button(
                    ui,
                    pause_label,
                    self.all_paused,
                    PillAccent::Amber,
                    !self.all_paused,
                )
                .on_hover_text("Pause / Resume all camera streams (P or Space)")
                .clicked()
                {
                    self.all_paused = !self.all_paused;
                    self.stream_mgr.set_all_paused(self.all_paused);
                }

                right_start_x = ui.min_rect().min.x;
            },
        );

        // 3. Center Zone: Clean Segmented Capsules (Grid/Single, SD|HD, 16:9|Fill, Patrol)
        let avail_min_x = (left_end_x + 10.0).min(bar_rect.max.x);
        let avail_max_x = (right_start_x - 10.0).max(avail_min_x);
        let ideal_min_x = bar_rect.center().x - self.last_center_width * 0.5;
        let center_start_x = ideal_min_x.clamp(
            avail_min_x,
            (avail_max_x - self.last_center_width).max(avail_min_x),
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

                // View & Grid Layout Capsule
                draw_segmented_group(ui, |ui| {
                    let mode_display = match self.app_cfg.layout_mode.to_lowercase().as_str() {
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
                            ui.set_min_width(124.0);
                            for (mode_val, label) in [
                                ("3x4", "Grid 3x4 (Default)"),
                                ("4x3", "Grid 4x3"),
                                ("2x6", "Grid 2x6 (Wide)"),
                                ("auto", "Grid Auto"),
                            ] {
                                let selected = self
                                    .app_cfg
                                    .layout_mode
                                    .eq_ignore_ascii_case(mode_val)
                                    && self.fullscreen_channel.is_none();
                                if ui.selectable_label(selected, label).clicked() {
                                    self.exit_fullscreen();
                                    self.app_cfg.layout_mode = mode_val.to_string();
                                    let _ = self.app_cfg.save();
                                    ui.memory_mut(|mem| mem.close_popup());
                                }
                            }
                        },
                    );
                    grid_resp.on_hover_text("Select grid layout (3x4, 4x3, 2x6, Auto)");

                    if draw_segment_item(
                        ui,
                        "Single",
                        self.fullscreen_channel.is_some(),
                        PillAccent::Ice,
                    )
                    .on_hover_text("Toggle Single Camera View (Enter / F)")
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

                // Global Quality Segmented Capsule: [ SD | HD ]
                draw_segmented_group(ui, |ui| {
                    if draw_segment_item(ui, "SD", all_sd, PillAccent::Ice)
                        .on_hover_text(
                            "Switch all cameras to low-bandwidth SD Sub-Stream (sss / Shift+S)",
                        )
                        .clicked()
                    {
                        self.set_all_cameras_subtype(1, "All cameras -> SD (Sub-Stream)");
                    }
                    if draw_segment_item(ui, "HD", all_hd, PillAccent::Ice)
                        .on_hover_text(
                            "Switch all cameras to 720p/1080p HD Main-Stream (hhh / Shift+H)",
                        )
                        .clicked()
                    {
                        self.set_all_cameras_subtype(0, "All cameras -> HD (Main-Stream)");
                    }
                });

                // Aspect Mode Segmented Capsule: [ 16:9 | Fill ]
                draw_segmented_group(ui, |ui| {
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
                    .on_hover_text("Edge-to-edge tile fill (A)")
                    .clicked()
                        && self.app_cfg.aspect_fit_16_9
                    {
                        self.toggle_aspect_mode();
                    }
                });

                // Patrol Pill
                if draw_pill_button(
                    ui,
                    "Patrol",
                    self.patrol_active,
                    PillAccent::Emerald,
                    false,
                )
                .on_hover_text(format!(
                    "Auto-cycle Single View every {}s (M)",
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
    ) {
        let dt = ui.input(|i| i.stable_dt);
        if self.tiles.entry(channel).or_default().step_animation(dt) {
            ui.ctx().request_repaint();
        }

        let (tex_w, tex_h, cur_zoom, cur_box_armed, has_marquee) = self
            .tiles
            .get(&channel)
            .map(|t| {
                (
                    t.tex_width,
                    t.tex_height,
                    t.zoom,
                    t.box_zoom_armed,
                    t.marquee_drag.is_some(),
                )
            })
            .unwrap_or((0, 0, 1.0, false, false));
        let video_rect = compute_video_rect(rect, self.app_cfg.aspect_fit_16_9, tex_w, tex_h);

        let response = ui.allocate_rect(rect, Sense::click_and_drag());
        let is_hovered = response.hovered();
        if is_hovered {
            self.hovered_channel = Some(channel);
        }

        let dock_visible = is_hovered || cur_zoom > 1.01 || cur_box_armed;
        let show_box_btn = is_fullscreen || cur_zoom > 1.01 || cur_box_armed;

        // Bottom-right floating glass control dock geometry:
        // [ Box ] [ - ] [ 1.0x ] [ + ] | [ SD/HD ] [ Expand/Grid ]
        let dock_h = 24.0;
        let btn_h = 18.0;
        let box_w = if show_box_btn { 30.0 } else { 0.0 };
        let box_gap = if show_box_btn { 2.0 } else { 0.0 };
        let z_out_w = 19.0;
        let z_rst_w = 34.0;
        let z_in_w = 19.0;
        let sep_w = 7.0;
        let sd_hd_w = 34.0;
        let expand_w = if is_fullscreen { 40.0 } else { 48.0 };
        let dock_pad = 3.0;
        let total_dock_w = dock_pad * 2.0
            + box_w
            + box_gap
            + z_out_w
            + 2.0
            + z_rst_w
            + 2.0
            + z_in_w
            + sep_w
            + sd_hd_w
            + 3.0
            + expand_w;

        let dock_rect = Rect::from_min_size(
            Pos2::new(
                (rect.max.x - total_dock_w - 6.0).max(rect.min.x + 4.0),
                rect.max.y - dock_h - 6.0,
            ),
            Vec2::new(total_dock_w.min((rect.width() - 8.0).max(40.0)), dock_h),
        );
        let btn_y = dock_rect.center().y - btn_h * 0.5;
        let box_rect =
            Rect::from_min_size(Pos2::new(dock_rect.min.x + dock_pad, btn_y), Vec2::new(box_w, btn_h));
        let z_out_x = dock_rect.min.x + dock_pad + box_w + box_gap;
        let z_out_rect = Rect::from_min_size(Pos2::new(z_out_x, btn_y), Vec2::new(z_out_w, btn_h));
        let z_rst_rect =
            Rect::from_min_size(Pos2::new(z_out_rect.max.x + 2.0, btn_y), Vec2::new(z_rst_w, btn_h));
        let z_in_rect =
            Rect::from_min_size(Pos2::new(z_rst_rect.max.x + 2.0, btn_y), Vec2::new(z_in_w, btn_h));
        let sd_hd_rect =
            Rect::from_min_size(Pos2::new(z_in_rect.max.x + sep_w, btn_y), Vec2::new(sd_hd_w, btn_h));
        let expand_rect =
            Rect::from_min_size(Pos2::new(sd_hd_rect.max.x + 3.0, btn_y), Vec2::new(expand_w, btn_h));

        // Bottom-left interactive zoom-radar minimap geometry
        let radar_size = if is_fullscreen {
            Vec2::new(108.0, 61.0)
        } else {
            Vec2::new(52.0, 30.0)
        };
        let radar_margin = if is_fullscreen { 10.0 } else { 7.0 };
        let radar_rect = Rect::from_min_size(
            Pos2::new(
                rect.min.x + radar_margin,
                rect.max.y - radar_size.y - radar_margin,
            ),
            radar_size,
        );
        let radar_visible = dock_visible
            && cur_zoom > 1.05
            && (radar_rect.max.x + 8.0 < dock_rect.min.x)
            && (radar_rect.min.y > rect.min.y + 30.0);

        let (pointer_pos, press_origin, primary_down, shift_held, secondary_clicked) = ui.input(|i| {
            (
                i.pointer.interact_pos().or_else(|| i.pointer.hover_pos()),
                i.pointer.press_origin(),
                i.pointer.primary_down(),
                i.modifiers.shift,
                i.pointer.button_clicked(PointerButton::Secondary),
            )
        });

        let over_box = show_box_btn
            && dock_visible
            && is_hovered
            && !has_marquee
            && pointer_pos.is_some_and(|p| box_rect.contains(p));
        let over_z_out = dock_visible
            && is_hovered
            && !has_marquee
            && pointer_pos.is_some_and(|p| z_out_rect.contains(p));
        let over_z_rst = dock_visible
            && is_hovered
            && !has_marquee
            && pointer_pos.is_some_and(|p| z_rst_rect.contains(p));
        let over_z_in = dock_visible
            && is_hovered
            && !has_marquee
            && pointer_pos.is_some_and(|p| z_in_rect.contains(p));
        let over_sd_hd = dock_visible
            && is_hovered
            && !has_marquee
            && pointer_pos.is_some_and(|p| sd_hd_rect.contains(p));
        let over_expand = dock_visible
            && is_hovered
            && !has_marquee
            && pointer_pos.is_some_and(|p| expand_rect.contains(p));
        let over_any_micro_btn =
            over_box || over_z_out || over_z_rst || over_z_in || over_sd_hd || over_expand;
        let over_hud_btn = dock_visible
            && is_hovered
            && !has_marquee
            && (over_any_micro_btn || pointer_pos.is_some_and(|p| dock_rect.contains(p)));

        let started_on_hud = dock_visible && press_origin.is_some_and(|o| dock_rect.contains(o));
        let started_on_radar =
            radar_visible && !started_on_hud && press_origin.is_some_and(|o| radar_rect.contains(o));
        let over_radar = radar_visible
            && is_hovered
            && !over_hud_btn
            && !has_marquee
            && pointer_pos.is_some_and(|p| radar_rect.contains(p));

        if over_any_micro_btn {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }

        let modal_open =
            self.show_settings || self.show_shortcuts_modal || self.rename_modal_channel.is_some();

        // Handle 1-click on floating dock micro-buttons BEFORE tile click/drag/double-click
        let mut hud_clicked = false;
        if response.clicked() && over_hud_btn {
            hud_clicked = true;
            if over_box {
                let tile = self.tiles.entry(channel).or_default();
                tile.box_zoom_armed = !tile.box_zoom_armed;
            } else if over_z_out {
                self.tiles
                    .entry(channel)
                    .or_default()
                    .zoom_by(1.0 / 1.25, Pos2::new(0.5, 0.5));
            } else if over_z_rst {
                self.tiles.entry(channel).or_default().animate_reset_zoom();
            } else if over_z_in {
                self.tiles
                    .entry(channel)
                    .or_default()
                    .zoom_by(1.25, Pos2::new(0.5, 0.5));
            } else if over_sd_hd {
                self.toggle_camera_hd_sd(channel);
            } else if over_expand {
                if is_fullscreen {
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
                    .jump_to_radar_uv(Pos2::new(rx, ry));
                radar_interacted = true;
            }
        }

        // Middle-click smoothly resets digital zoom to 1.0x
        if !over_hud_btn && response.clicked_by(PointerButton::Middle) {
            self.tiles.entry(channel).or_default().animate_reset_zoom();
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
            let zoom_factor = if is_hovered && !modal_open {
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
                let tile = self.tiles.entry(channel).or_default();
                if (zoom_factor - 1.0).abs() > 0.0005 {
                    tile.zoom_by(zoom_factor, anchor_norm);
                }

                // Simultaneous two-finger pinch center panning when zoomed in
                if is_hovered && !modal_open && frame_pinch.pan_delta != Vec2::ZERO && tile.zoom > 1.01
                {
                    tile.pan_by_pixels(frame_pinch.pan_delta, video_rect.size());
                }

                let wants_marquee = tile.zoom <= 1.01 || tile.box_zoom_armed || shift_held;
                if !modal_open
                    && !started_on_hud
                    && !started_on_radar
                    && wants_marquee
                    && response.drag_started_by(PointerButton::Primary)
                {
                    let origin = press_origin.or(pointer_pos).unwrap_or(video_rect.center());
                    let p0 = clamp_pos_to_rect(origin, video_rect);
                    let p1 = clamp_pos_to_rect(pointer_pos.unwrap_or(p0), video_rect);
                    tile.marquee_drag = Some((p0, p1));
                }

                if let Some((p0, _)) = tile.marquee_drag {
                    if modal_open || secondary_clicked {
                        tile.marquee_drag = None;
                        tile.box_zoom_armed = false;
                    } else if primary_down {
                        if let Some(cur_p) = pointer_pos {
                            let p1 = clamp_pos_to_rect(cur_p, video_rect);
                            tile.marquee_drag = Some((p0, p1));
                        }
                        ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
                        ui.ctx().request_repaint();
                    } else {
                        let (_, p1) = tile.marquee_drag.take().unwrap();
                        tile.box_zoom_armed = false;
                        let sel_rect = Rect::from_two_pos(p0, p1);
                        if tile.zoom_to_marquee_rect(sel_rect, video_rect).is_some() {
                            select_tile = true;
                            ui.ctx().request_repaint();
                        } else if !is_fullscreen && (p1 - p0).length() < MIN_MARQUEE_PX {
                            select_tile = true;
                        }
                    }
                } else if !started_on_hud
                    && !started_on_radar
                    && !tile.box_zoom_armed
                    && !shift_held
                    && response.dragged_by(PointerButton::Primary)
                    && tile.zoom > 1.01
                {
                    tile.pan_by_pixels(response.drag_delta(), video_rect.size());
                    ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                } else if is_hovered
                    && !over_any_micro_btn
                    && (over_radar
                        || (started_on_radar && primary_down)
                        || tile.box_zoom_armed
                        || shift_held)
                {
                    ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
                }

                if !hud_clicked && !over_hud_btn && !radar_interacted && !started_on_radar {
                    if response.double_clicked() {
                        toggle_fs = true;
                    } else if response.clicked() && !is_fullscreen {
                        select_tile = true;
                    }
                }
            }

            if toggle_fs {
                self.selected_channel = Some(channel);
                if is_fullscreen {
                    self.exit_fullscreen();
                } else {
                    self.enter_fullscreen(channel);
                }
            } else if select_tile {
                self.selected_channel = Some(channel);
            }
        }

        let status = self
            .stream_mgr
            .slot(channel)
            .map(|s| s.status())
            .unwrap_or(StreamStatus::Error);
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
        let fps = self
            .stream_mgr
            .slot(channel)
            .map(|s| s.fps_x10.load(Ordering::Relaxed) as f32 / 10.0)
            .unwrap_or(0.0);

        let (has_texture, zoom_val, uv_rect, box_armed_now, marquee_preview, has_zoom_history) = {
            let tile = self.tiles.get(&channel).unwrap();
            let preview = tile.marquee_drag.map(|(p0, p1)| {
                let sel = Rect::from_two_pos(p0, p1).intersect(video_rect);
                let (pz, _) = tile.preview_marquee_zoom(sel, video_rect);
                (p0, p1, pz)
            });
            (
                tile.texture.is_some(),
                tile.zoom,
                tile.uv_rect(),
                tile.box_zoom_armed,
                preview,
                !tile.zoom_history.is_empty(),
            )
        };

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, Rounding::same(6.0), OBSIDIAN_BG);

        if has_texture {
            if let Some(tex) = self.tiles.get(&channel).and_then(|t| t.texture.as_ref()) {
                painter.image(tex.id(), video_rect, uv_rect, Color32::WHITE);
            }
        } else {
            // Minimalist Empty / Connecting / Offline Tile State
            painter.rect_filled(rect, Rounding::same(6.0), Color32::from_rgb(13, 13, 16));
            let center = rect.center();

            // Clean vector camera icon
            let body = Rect::from_center_size(center - Vec2::new(3.0, 22.0), Vec2::new(20.0, 14.0));
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
                self.app_cfg.camera_display_name(channel),
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

            let (st_text, st_col) = match status {
                StreamStatus::Connecting => ("Connecting...", STATUS_AMBER),
                StreamStatus::Reconnecting => ("Reconnecting...", STATUS_AMBER),
                StreamStatus::Paused => ("Paused", STATUS_AMBER),
                StreamStatus::AuthError => ("Auth Failed", STATUS_CORAL),
                StreamStatus::Error => ("Offline", STATUS_CORAL),
                StreamStatus::Live => ("Ready", STATUS_EMERALD),
            };
            let st_w = (st_text.len() as f32 * 6.2 + 18.0).max(68.0);
            let st_pill = Rect::from_center_size(
                center + Vec2::new(0.0, 35.0),
                Vec2::new(safe_clamp_f32(st_w, 60.0, rect.width() - 16.0), 19.0),
            );
            if st_pill.max.y < rect.max.y - 4.0 {
                painter.rect_filled(st_pill, Rounding::same(9.0), ZINC_ELEVATED);
                painter.rect_stroke(
                    st_pill,
                    Rounding::same(9.0),
                    Stroke::new(1.0_f32, hairline_stroke()),
                );
                painter.text(
                    st_pill.center(),
                    Align2::CENTER_CENTER,
                    st_text,
                    FontId::new(10.5, FontFamily::Proportional),
                    st_col,
                );
            }
        }

        // Optical Viewfinder Overlay during Click-Drag Area Box Zoom
        if let Some((p0, p1, preview_z)) = marquee_preview {
            paint_marquee_viewfinder(&painter, video_rect, p0, p1, preview_z);
        }

        // Whisper-quiet Top-Left Translucent Pill (Status Dot + Camera Name + optional HD / transition tag)
        let cam_name = self.app_cfg.camera_display_name(channel);
        let is_transitioning = target_sub != active_sub && !self.all_paused;
        let badge_info: Option<(&str, Color32)> = if is_transitioning {
            let pulse = ((ui.input(|i| i.time) * 5.5).sin() as f32 * 0.5 + 0.5).clamp(0.0, 1.0);
            let alpha = (145.0 + pulse * 110.0) as u8;
            let col = Color32::from_rgba_unmultiplied(245, 158, 11, alpha);
            Some((if target_sub == 0 { "HD..." } else { "SD..." }, col))
        } else if active_sub == 0 {
            Some(("HD", STATUS_ICE))
        } else {
            None
        };

        let name_w = cam_name.len() as f32 * 6.6;
        let badge_w = badge_info.map(|(b, _)| b.len() as f32 * 6.5 + 10.0).unwrap_or(0.0);
        let tl_w = safe_clamp_f32(22.0 + name_w + badge_w, 72.0, rect.width() - 12.0);
        let tl_pill = Rect::from_min_size(rect.min + Vec2::new(6.0, 6.0), Vec2::new(tl_w, 21.0));

        painter.rect_filled(
            tl_pill,
            Rounding::same(5.0),
            Color32::from_rgba_unmultiplied(9, 9, 11, 155),
        );

        let dot_color = match status {
            StreamStatus::Live => STATUS_EMERALD,
            StreamStatus::Connecting | StreamStatus::Reconnecting | StreamStatus::Paused => {
                STATUS_AMBER
            }
            StreamStatus::Error | StreamStatus::AuthError => STATUS_CORAL,
        };
        painter.circle_filled(
            Pos2::new(tl_pill.min.x + 9.5, tl_pill.center().y),
            3.5,
            dot_color,
        );
        painter.text(
            Pos2::new(tl_pill.min.x + 17.5, tl_pill.center().y),
            Align2::LEFT_CENTER,
            &cam_name,
            FontId::new(11.5, FontFamily::Proportional),
            Color32::from_rgb(228, 228, 231),
        );
        if let Some((b_txt, b_col)) = badge_info {
            painter.text(
                Pos2::new(tl_pill.max.x - 6.0, tl_pill.center().y),
                Align2::RIGHT_CENTER,
                b_txt,
                FontId::new(10.5, FontFamily::Proportional),
                b_col,
            );
        }

        // Progressive Disclosure HUD: ONLY rendered when hovered, zoomed (> 1.01x), or Box Zoom armed
        let show_active_hud = (is_hovered || zoom_val > 1.01 || box_armed_now) && marquee_preview.is_none();
        if show_active_hud {
            // Top-Right Quiet Telemetry Pill
            let right_text = if status == StreamStatus::Live && zoom_val > 1.01 {
                format!("{:.0} fps  {:.1}x", fps, zoom_val)
            } else if status == StreamStatus::Live {
                format!("{:.0} fps", fps)
            } else if zoom_val > 1.01 {
                format!("{}  {:.1}x", status.label(), zoom_val)
            } else {
                status.label().to_string()
            };
            let r_width = safe_clamp_f32(
                right_text.len() as f32 * 6.5 + 14.0,
                48.0,
                rect.width() - 12.0,
            );
            let right_pill = Rect::from_min_size(
                Pos2::new(rect.max.x - r_width - 6.0, rect.min.y + 6.0),
                Vec2::new(r_width, 21.0),
            );
            if right_pill.min.x > tl_pill.max.x + 4.0 {
                painter.rect_filled(
                    right_pill,
                    Rounding::same(5.0),
                    Color32::from_rgba_unmultiplied(9, 9, 11, 170),
                );
                painter.rect_stroke(
                    right_pill,
                    Rounding::same(5.0),
                    Stroke::new(1.0_f32, hairline_stroke()),
                );
                painter.text(
                    right_pill.center(),
                    Align2::CENTER_CENTER,
                    right_text,
                    FontId::new(11.0, FontFamily::Proportional),
                    TEXT_MUTED,
                );
            }

            // Bottom-Left Interactive Wireframe Zoom-Radar Minimap (Click or Drag to jump pan)
            if zoom_val > 1.05
                && (radar_rect.max.x + 8.0 < dock_rect.min.x)
                && (radar_rect.min.y > rect.min.y + 30.0)
            {
                let radar_rounding = if is_fullscreen { 4.5 } else { 3.0 };
                let radar_active = over_radar || (started_on_radar && primary_down);
                painter.rect_filled(
                    radar_rect,
                    Rounding::same(radar_rounding),
                    Color32::from_rgba_unmultiplied(9, 9, 11, 195),
                );
                if is_fullscreen {
                    let subtle_grid = Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 13));
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
                    radar_rect.min.x + uv_rect.min.x * radar_rect.width(),
                    radar_rect.min.y + uv_rect.min.y * radar_rect.height(),
                );
                let inner_max = Pos2::new(
                    radar_rect.min.x + uv_rect.max.x * radar_rect.width(),
                    radar_rect.min.y + uv_rect.max.y * radar_rect.height(),
                );
                let inner_uv = Rect::from_min_max(inner_min, inner_max);
                painter.rect_filled(
                    inner_uv,
                    Rounding::same(1.5),
                    Color32::from_rgba_unmultiplied(56, 189, 248, if radar_active { 52 } else { 36 }),
                );
                painter.rect_stroke(
                    inner_uv,
                    Rounding::same(1.5),
                    Stroke::new(1.2_f32, STATUS_ICE),
                );
            }

            // Bottom-Right Unified Floating Glass Control Dock
            if rect.width() >= total_dock_w + 10.0 {
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

                // Subtle vertical hairline divider
                let sep_x = z_in_rect.max.x + sep_w * 0.5;
                painter.vline(
                    sep_x,
                    (dock_rect.min.y + 5.0)..=(dock_rect.max.y - 5.0),
                    Stroke::new(1.0_f32, hairline_stroke()),
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

        // Tile Border: Selected (1.5px Soft Ice) > Hovered (1.0px Bright Hairline) > Idle (1.0px Subtle Hairline)
        let is_selected_in_grid = !is_fullscreen && self.selected_channel == Some(channel);
        let (border_w, border_col) = if is_selected_in_grid {
            (1.5_f32, STATUS_ICE)
        } else if is_hovered {
            (1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 65))
        } else {
            (1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 18))
        };
        painter.rect_stroke(rect, Rounding::same(6.0), Stroke::new(border_w, border_col));

        // Right-Click Context Menu
        let all_channels = self.app_cfg.channel_order.clone();
        let any_zoomed = self.tiles.values().any(|t| t.zoom > 1.01);
        response.context_menu(|ui| {
            ui.label(RichText::new(format!("{} (Channel {})", cam_name, channel)).strong());
            ui.separator();

            if zoom_val > 1.02 {
                if ui.button("Reset Camera Zoom (1.0x) [Z / Esc]").clicked() {
                    ui.close_menu();
                    if let Some(t) = self.tiles.get_mut(&channel) {
                        t.animate_reset_zoom();
                    }
                }
                if has_zoom_history && ui.button("Step Back Previous Zoom [Backspace]").clicked() {
                    ui.close_menu();
                    if let Some(t) = self.tiles.get_mut(&channel) {
                        t.step_back_zoom();
                    }
                }
                if ui.button("Arm Nested Box Zoom [Shift+Drag]").clicked() {
                    ui.close_menu();
                    if let Some(t) = self.tiles.get_mut(&channel) {
                        t.box_zoom_armed = true;
                    }
                }
                ui.separator();
            }

            if ui
                .button(if is_fullscreen {
                    "Return to Grid View (Esc)"
                } else {
                    "Expand Single View (Enter / Double-Click)"
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

            if zoom_val > 1.02 {
                if ui.button("Save Zoomed Region Snapshot (PNG) [C]").clicked() {
                    ui.close_menu();
                    self.save_camera_snapshot(channel, true);
                }
                if ui.button("Save Full-Frame Snapshot (PNG)").clicked() {
                    ui.close_menu();
                    self.save_camera_snapshot(channel, false);
                }
            } else if ui.button("Save Camera Snapshot (PNG) [C]").clicked() {
                ui.close_menu();
                self.save_camera_snapshot(channel, false);
            }

            if ui.button("Re-Sync / Reconnect Camera").clicked() {
                ui.close_menu();
                if let Some(slot) = self.stream_mgr.slot(channel) {
                    slot.trigger_resync();
                }
            }

            if any_zoomed && ui.button("Reset Zoom on All Cameras [ZZZ]").clicked() {
                ui.close_menu();
                self.reset_all_zoom();
            }

            if ui.button("Rename Camera...").clicked() {
                ui.close_menu();
                self.rename_modal_channel = Some(channel);
                self.rename_input = self.app_cfg.camera_display_name(channel);
            }

            ui.menu_button("Swap Tile Position With...", |ui| {
                for other_ch in all_channels {
                    if other_ch != channel {
                        let label = format!(
                            "Channel {} ({})",
                            other_ch,
                            self.app_cfg.camera_display_name(other_ch)
                        );
                        if ui.button(label).clicked() {
                            ui.close_menu();
                            self.app_cfg.swap_channels(channel, other_ch);
                            let _ = self.app_cfg.save();
                        }
                    }
                }
            });
        });
    }

    fn render_shortcuts_modal(&mut self, ctx: &egui::Context) {
        if !self.show_shortcuts_modal {
            return;
        }

        let mut open = self.show_shortcuts_modal;
        egui::Window::new("Keyboard Shortcuts")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .default_width(640.0)
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
                        "Single-tap targets the active (selected, fullscreen, or hovered) camera. Triple-tap within 450ms applies to the entire wall.",
                    )
                    .size(11.5)
                    .color(TEXT_MUTED),
                );
                ui.add_space(10.0);

                ui.columns(2, |cols| {
                    // Left Column: Stream Quality + Zoom & Pan
                    render_shortcut_section(&mut cols[0], "Stream Quality", &[
                        (&["S"], "Switch active camera to SD"),
                        (&["S S S", "Shift+S"], "Switch ALL cameras to SD"),
                        (&["H"], "Switch active camera to HD"),
                        (&["H H H", "Shift+H"], "Switch ALL cameras to HD"),
                    ]);
                    cols[0].add_space(10.0);
                    render_shortcut_section(&mut cols[0], "Zoom & Pan (GPU e-PTZ 1x-8x)", &[
                        (&["Drag Box"], "Area Box Zoom at 1.0x (up to 8.0x)"),
                        (&["Shift+Drag", "Box"], "Nested Area Box Zoom when zoomed"),
                        (&["Scroll", "Pinch"], "Cursor-anchored zoom (1x - 8x)"),
                        (&["+", "-"], "Step zoom in / out on active cam"),
                        (&["Backspace"], "Step back to previous box zoom"),
                        (&["Z", "Mid-Click"], "Smooth reset camera zoom (1.0x)"),
                        (&["Z Z Z", "Shift+Z"], "Reset zoom on ALL cameras"),
                        (&["Drag", "Radar"], "Pan viewport or click radar map"),
                    ]);

                    // Right Column: Grid & Camera Focus + Wall & Zen Controls
                    render_shortcut_section(&mut cols[1], "Grid & Camera Focus", &[
                        (&["Click", "Arrows"], "Select camera across 2D grid"),
                        (&["Enter", "F"], "Toggle Single View on active cam"),
                        (&["[", "]"], "Cycle previous / next camera"),
                        (&["1-9", "F1-F10, F12"], "Direct Single View on channel"),
                        (&["0"], "Reset zoom if zoomed, else Cam 10"),
                    ]);
                    cols[1].add_space(10.0);
                    render_shortcut_section(&mut cols[1], "Wall & Zen Controls", &[
                        (&["B"], "Borderless Zen Wall (top-edge reveal)"),
                        (&["F11"], "OS Monitor Fullscreen (auto-hide bar)"),
                        (&["A"], "Toggle 16:9 Fit vs Fill Tile"),
                        (&["C", "Ctrl+S"], "Save PNG Snapshot (crops zoom)"),
                        (&["M"], "Toggle Patrol Mode auto-rotation"),
                        (&["P", "Space"], "Pause / Resume all streams"),
                        (&["R"], "Instant Re-Sync to live edge"),
                        (&["Esc"], "Reset zoom -> Single -> Zen -> F11"),
                    ]);
                });

                ui.add_space(10.0);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Tip: Drag a rectangle around any area at 1.0x (or Shift+Drag when zoomed) to inspect instantly.")
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

        // Check if background channel auto-scan completed
        let scanned_result = {
            let mut guard = self.scan_in_progress.lock().unwrap();
            if let Some(Some(done)) = guard.take() {
                Some(done)
            } else {
                None
            }
        };
        if let Some(found) = scanned_result {
            if !found.is_empty() {
                let max_ch = *found.iter().max().unwrap_or(&12);
                self.edit_env_cfg.num_cameras = max_ch.max(found.len());
                self.show_toast(format!(
                    "Auto-Scan detected {} active channels (up to Channel {})",
                    found.len(),
                    max_ch
                ));
            } else {
                self.show_toast(
                    "Auto-Scan completed: 0 channels responded (check NVR IP/credentials)",
                );
            }
        }

        let mut open = self.show_settings;
        egui::Window::new("NVR Command Center - Settings")
            .open(&mut open)
            .resizable(true)
            .default_width(580.0)
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
                ui.separator();

                match self.settings_tab {
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
                                         - Cloud P2P Serial (Optional): Found under Menu -> Network -> P2P (Status: Online) or NVR S/N sticker for viewing on any internet connection without port forwarding.",
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
                                    ui.text_edit_singleline(&mut self.edit_env_cfg.tailscale_host);
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
                                    let is_scanning =
                                        self.scan_in_progress.lock().unwrap().is_some();
                                    let scan_label = if is_scanning {
                                        "Scanning..."
                                    } else {
                                        "Auto-Scan NVR (1-16)"
                                    };
                                    if ui
                                        .add_enabled(!is_scanning, egui::Button::new(scan_label))
                                        .clicked()
                                    {
                                        let cfg_clone = self.edit_env_cfg.clone();
                                        let state_ref = Arc::clone(&self.scan_in_progress);
                                        *state_ref.lock().unwrap() = Some(None);
                                        thread::spawn(move || {
                                            let found = scan_active_nvr_channels(&cfg_clone, 16);
                                            *state_ref.lock().unwrap() = Some(Some(found));
                                        });
                                    }
                                });
                                ui.end_row();

                                ui.label("IP Camera Start Channel:");
                                ui.add(
                                    egui::DragValue::new(
                                        &mut self.edit_env_cfg.ip_camera_start_channel,
                                    )
                                    .range(1..=32),
                                );
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
                        ui.label(
                            RichText::new(format!(
                                "Preview (Ch 12): {}",
                                self.edit_env_cfg
                                    .build_redacted_url(12, self.edit_env_cfg.default_subtype)
                            ))
                            .size(11.5)
                            .color(TEXT_MUTED),
                        );
                    }

                    SettingsTab::PerformanceAndNames => {
                        egui::Grid::new("perf_settings_grid")
                            .num_columns(2)
                            .spacing([12.0, 8.0])
                            .show(ui, |ui| {
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
                                    &mut self.app_cfg.fullscreen_auto_hd,
                                    "Warm-swap focused Single View camera to HD (subtype=0)",
                                );
                                ui.end_row();

                                ui.label("Aspect Framing:");
                                ui.checkbox(
                                    &mut self.app_cfg.aspect_fit_16_9,
                                    "Preserve undistorted 16:9 aspect ratio inside camera tiles (A)",
                                );
                                ui.end_row();

                                ui.label("Patrol Cycle Interval (sec):");
                                ui.add(egui::Slider::new(
                                    &mut self.app_cfg.patrol_interval_secs,
                                    3..=60,
                                ));
                                ui.end_row();

                                ui.label("Snapshots Directory:");
                                ui.text_edit_singleline(&mut self.app_cfg.screenshot_dir);
                                ui.end_row();
                            });

                        ui.separator();
                        ui.label(RichText::new("Custom Camera Names:").strong());
                        egui::ScrollArea::vertical()
                            .max_height(180.0)
                            .show(ui, |ui| {
                                egui::Grid::new("cam_names_grid")
                                    .num_columns(2)
                                    .spacing([10.0, 4.0])
                                    .show(ui, |ui| {
                                        for ch in 1..=self.edit_env_cfg.num_cameras {
                                            ui.label(format!("Channel {}:", ch));
                                            let entry = self
                                                .app_cfg
                                                .camera_names
                                                .entry(ch)
                                                .or_insert_with(|| format!("Camera {}", ch));
                                            ui.text_edit_singleline(entry);
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
                                self.show_settings = false;
                                self.show_shortcuts_modal = true;
                            }
                        });
                        ui.add_space(4.0);
                        ui.label("- S / SSS (or Shift+S) : Active camera SD / All cameras SD");
                        ui.label("- H / HHH (or Shift+H) : Active camera HD / All cameras HD");
                        ui.label("- Drag Box / Shift+Drag / Box : Area Box Zoom (e-PTZ up to 8x)");
                        ui.label("- Scroll / Pinch / + / - : Cursor-anchored GPU digital zoom (1x-8x)");
                        ui.label("- Backspace / Z / ZZZ / Esc : Step back zoom / Reset camera or wall zoom");
                        ui.label("- Click / Arrows + Enter (or F) : 2D grid selection & Single View");
                        ui.label("- 1-9, 0, F1-F10, F12 : Direct camera Single View (0 resets zoom first)");
                        ui.label("- B / F11 / A : Borderless Zen Wall / OS Fullscreen / 16:9 Fit Toggle");
                        ui.label("- C (or Ctrl+S) / M / P / R : Snapshot (crops zoom) / Patrol / Pause / Re-Sync");

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
                }

                ui.add_space(6.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("Save & Apply (.env 0600)")
                                    .strong()
                                    .color(TEXT_PRIMARY),
                            )
                            .fill(status_ice_fill())
                            .stroke(Stroke::new(1.0_f32, STATUS_ICE)),
                        )
                        .clicked()
                    {
                        self.apply_and_save_settings();
                        self.show_settings = false;
                    }
                    if ui.button("Close").clicked() {
                        self.show_settings = false;
                    }
                });
            });

        if !open {
            self.show_settings = false;
        }
    }

    fn render_rename_modal(&mut self, ctx: &egui::Context) {
        let Some(ch) = self.rename_modal_channel else {
            return;
        };
        egui::Window::new(format!("Rename Channel {}", ch))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label("Camera Display Name:");
                let resp = ui.text_edit_singleline(&mut self.rename_input);
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    self.app_cfg
                        .camera_names
                        .insert(ch, self.rename_input.trim().to_string());
                    let _ = self.app_cfg.save();
                    self.rename_modal_channel = None;
                }
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        self.app_cfg
                            .camera_names
                            .insert(ch, self.rename_input.trim().to_string());
                        let _ = self.app_cfg.save();
                        self.rename_modal_channel = None;
                    }
                    if ui.button("Cancel").clicked() {
                        self.rename_modal_channel = None;
                    }
                });
            });
    }
}

impl eframe::App for CameraViewerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Poll process RSS memory every 2s
        if self.last_sys_poll.elapsed() >= Duration::from_secs(2) {
            self.rss_mb = read_process_rss_mb();
            self.last_sys_poll = Instant::now();
        }

        // Detect window minimized state for 24/7 eco mode
        let minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        if minimized {
            self.stream_mgr.set_window_minimized_eco(true);
            ctx.request_repaint_after(Duration::from_millis(250));
            return;
        } else if self.fullscreen_channel.is_none() {
            self.stream_mgr.set_window_minimized_eco(false);
        }

        // Patrol auto-rotation timer
        if self.patrol_active
            && self.last_patrol_switch.elapsed()
                >= Duration::from_secs(self.app_cfg.patrol_interval_secs.max(3))
        {
            self.cycle_fullscreen(1);
            self.last_patrol_switch = Instant::now();
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
        self.update_gpu_textures(ctx);

        let show_top_bar = self.should_show_top_bar(ctx);
        if show_top_bar {
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

        let wall_margin = if show_top_bar { 5.0 } else { 0.0 };
        self.hovered_channel = None;

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(OBSIDIAN_BG)
                    .inner_margin(egui::Margin::same(wall_margin)),
            )
            .show(ctx, |ui| {
                let avail = ui.available_rect_before_wrap();
                if let Some(fs_ch) = self.fullscreen_channel {
                    self.render_camera_tile(ui, fs_ch, avail, true);
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
                        self.render_camera_tile(ui, ch, rect, false);
                    }
                }

                // Render transient toast notification in bottom-center
                if let Some((ref msg, ts)) = self.toast_message {
                    if ts.elapsed() < Duration::from_millis(2800) {
                        let painter = ui.painter();
                        let width = safe_clamp_f32(
                            msg.len() as f32 * 7.2 + 28.0,
                            160.0,
                            avail.width() - 20.0,
                        );
                        let toast_rect = Rect::from_center_size(
                            Pos2::new(avail.center().x, avail.max.y - 26.0),
                            Vec2::new(width, 28.0),
                        );
                        painter.rect_filled(
                            toast_rect,
                            Rounding::same(8.0),
                            Color32::from_rgba_unmultiplied(17, 17, 20, 236),
                        );
                        painter.rect_stroke(
                            toast_rect,
                            Rounding::same(8.0),
                            Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(56, 189, 248, 130)),
                        );
                        painter.text(
                            toast_rect.center(),
                            Align2::CENTER_CENTER,
                            msg,
                            FontId::new(12.0, FontFamily::Proportional),
                            TEXT_PRIMARY,
                        );
                    } else {
                        self.toast_message = None;
                    }
                }
            });

        self.render_settings_modal(ctx);
        self.render_shortcuts_modal(ctx);
        self.render_rename_modal(ctx);

        // Synchronized presentation tick (~30 Hz active UI check, 250ms idle check when paused)
        let repaint_ms = if self.all_paused { 250 } else { 33 };
        ctx.request_repaint_after(Duration::from_millis(repaint_ms));
    }
}

fn draw_segmented_group(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui)) {
    egui::Frame::none()
        .fill(ZINC_ELEVATED)
        .stroke(Stroke::new(1.0_f32, hairline_stroke()))
        .rounding(Rounding::same(8.0))
        .inner_margin(egui::Margin::symmetric(2.0, 2.0))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            add_contents(ui);
        });
}

fn draw_segment_item(ui: &mut Ui, label: &str, active: bool, accent: PillAccent) -> egui::Response {
    let text_w = (label.len() as f32 * 6.7 + 14.0).max(22.0);
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
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            FontId::new(11.5, FontFamily::Proportional),
            acc_fg,
        );
    } else {
        if resp.hovered() {
            painter.rect_filled(rect, Rounding::same(6.0), ZINC_HOVER);
        }
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            FontId::new(11.5, FontFamily::Proportional),
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
    let text_w = (label.len() as f32 * 6.8 + 16.0).max(26.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(text_w, 26.0), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let painter = ui.painter();
    let (acc_fill, acc_stroke, acc_fg) = accent.colors();

    if active {
        painter.rect_filled(rect, Rounding::same(7.0), acc_fill);
        painter.rect_stroke(rect, Rounding::same(7.0), Stroke::new(1.0_f32, acc_stroke));
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            FontId::new(11.5, FontFamily::Proportional),
            acc_fg,
        );
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
            FontId::new(11.5, FontFamily::Proportional),
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
        Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), if hovered { 64 } else { 42 })
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
        let reticle_stroke = Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(56, 189, 248, 130));
        painter.line_segment([c - Vec2::new(4.0, 0.0), c + Vec2::new(4.0, 0.0)], reticle_stroke);
        painter.line_segment([c - Vec2::new(0.0, 4.0), c + Vec2::new(0.0, 4.0)], reticle_stroke);
    }

    // 4. Live target zoom badge (e.g. "3.8x" or "8.0x MAX")
    if is_valid {
        let badge_txt = if preview_z >= MAX_DIGITAL_ZOOM - 0.05 {
            format!("{:.1}x MAX", preview_z)
        } else {
            format!("{:.1}x", preview_z)
        };
        let badge_w = (badge_txt.len() as f32 * 6.5 + 12.0).max(40.0);
        let badge_h = 18.0;
        let place_above = sel_rect.min.y - badge_h - 5.0 >= video_rect.min.y + 30.0;
        let bx = sel_rect
            .min
            .x
            .clamp(video_rect.min.x + 6.0, (video_rect.max.x - badge_w - 6.0).max(video_rect.min.x + 6.0));
        let by = if place_above {
            sel_rect.min.y - badge_h - 4.0
        } else {
            (sel_rect.min.y + 5.0).min((video_rect.max.y - badge_h - 6.0).max(video_rect.min.y + 6.0))
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
        painter.text(
            badge_rect.center(),
            Align2::CENTER_CENTER,
            badge_txt,
            FontId::new(10.5, FontFamily::Monospace),
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

fn open_url(ctx: &egui::Context, url: &str) {
    // Ensure OS fullscreen is cleared so the browser window can raise to the foreground
    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));

    let target = url.to_string();
    thread::spawn(move || {
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
            .arg("-c")
            .arg(py_launch)
            .arg(&target)
            .env("GDK_BACKEND", "x11")
            .env_remove("LD_LIBRARY_PATH")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        if !matches!(py_status, Ok(s) if s.success()) {
            let _ = Command::new("xdg-open")
                .arg(&target)
                .env_remove("LD_LIBRARY_PATH")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
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
        assert_eq!(tile.zoom, 1.0);
        assert_eq!(tile.pan_uv, Vec2::ZERO);
        assert_eq!(tile.last_uploaded_seq, 0);
    }

    #[test]
    fn test_cursor_anchored_zoom_preserves_uv_point_and_snaps_back() {
        let mut tile = TileUiState::default();
        let anchor = Pos2::new(0.25, 0.75);

        // Step 1: Zoom 2.0x anchored at (0.25, 0.75) -> UV point (0.25, 0.75) stays under cursor
        tile.zoom_by(2.0, anchor);
        assert!((tile.zoom - 2.0).abs() < 1e-5);
        let uv1 = tile.uv_rect();
        let u1 = uv1.min.x + anchor.x * uv1.width();
        let v1 = uv1.min.y + anchor.y * uv1.height();
        assert!((u1 - 0.25).abs() < 1e-5);
        assert!((v1 - 0.75).abs() < 1e-5);

        // Step 2: Further zoom 1.5x (to 3.0x) at a new cursor position (0.8, 0.3)
        let anchor2 = Pos2::new(0.8, 0.3);
        let expected_u = uv1.min.x + anchor2.x * uv1.width();
        let expected_v = uv1.min.y + anchor2.y * uv1.height();
        tile.zoom_by(1.5, anchor2);
        assert!((tile.zoom - 3.0).abs() < 1e-5);
        let uv2 = tile.uv_rect();
        let u2 = uv2.min.x + anchor2.x * uv2.width();
        let v2 = uv2.min.y + anchor2.y * uv2.height();
        assert!((u2 - expected_u).abs() < 1e-5);
        assert!((v2 - expected_v).abs() < 1e-5);

        // Gentle touchpad pinch-in from 1.0x must not be swallowed
        tile.reset_zoom();
        tile.zoom_by(1.005, Pos2::new(0.5, 0.5));
        assert!((tile.zoom - 1.005).abs() < 1e-5);

        // Zooming back out below 1.015x cleanly resets to 1.0x and Vec2::ZERO
        tile.zoom_by(0.99, Pos2::new(0.5, 0.5));
        assert_eq!(tile.zoom, 1.0);
        assert_eq!(tile.pan_uv, Vec2::ZERO);
    }

    #[test]
    fn test_tight_pan_clamping_prevents_dead_zone_overshoot() {
        let mut tile = TileUiState::default();
        tile.zoom_by(2.0, Pos2::new(0.5, 0.5));
        // At 2.0x zoom, max_pan = 0.5 - 0.25 = 0.25
        // Dragging massively to the left (pushing pan_uv toward +infinity) must clamp tightly at +0.25
        tile.pan_by_pixels(Vec2::new(-5000.0, -5000.0), Vec2::new(400.0, 225.0));
        assert!((tile.pan_uv.x - 0.25).abs() < 1e-5);
        assert!((tile.pan_uv.y - 0.25).abs() < 1e-5);

        // Reversing drag by 40px must immediately move pan_uv back without any dead-zone delay
        tile.pan_by_pixels(Vec2::new(40.0, 0.0), Vec2::new(400.0, 225.0));
        assert!(tile.pan_uv.x < 0.25);
        assert!((tile.pan_uv.x - 0.20).abs() < 1e-5);
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
        let mut tile = TileUiState::new();
        let anchor = Pos2::new(0.35, 0.65);

        // Zoom in via 2-finger trackpad scroll (+85 points)
        let ev_in = [egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: Vec2::new(0.0, 85.0),
            modifiers: egui::Modifiers::NONE,
        }];
        let f_in = compute_tile_zoom_factor(&ev_in, None, PinchFrameSnapshot::default());
        assert!(f_in > 2.0);
        tile.zoom_by(f_in, anchor);
        assert!((tile.zoom - f_in).abs() < 1e-4);

        // Immediate opposite 2-finger swipe (-85 points) returns cleanly to 1.0x with zero lag
        let ev_out = [egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: Vec2::new(0.0, -85.0),
            modifiers: egui::Modifiers::NONE,
        }];
        let f_out = compute_tile_zoom_factor(&ev_out, None, PinchFrameSnapshot::default());
        assert!((f_in * f_out - 1.0).abs() < 1e-5);
        tile.zoom_by(f_out, anchor);
        assert_eq!(tile.zoom, 1.0);
        assert_eq!(tile.pan_uv, Vec2::ZERO);

        // Fast single-flick zoom-out from 5.0x -> 1.0x
        tile.zoom_by(5.0, Pos2::new(0.5, 0.5));
        assert_eq!(tile.zoom, 5.0);
        let fast_out = [egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: Vec2::new(0.0, -180.0),
            modifiers: egui::Modifiers::NONE,
        }];
        let f_fast = compute_tile_zoom_factor(&fast_out, None, PinchFrameSnapshot::default());
        tile.zoom_by(f_fast, Pos2::new(0.5, 0.5));
        assert_eq!(tile.zoom, 1.0);
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
        let mut tile = TileUiState::new();
        let video = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(1920.0, 1080.0));

        // 1. Sloppy micro-drag (< MIN_MARQUEE_PX = 18px) is rejected so normal clicks never trigger box zoom
        let tiny_sel = Rect::from_min_size(Pos2::new(400.0, 300.0), Vec2::new(12.0, 10.0));
        assert!(tile.zoom_to_marquee_rect(tiny_sel, video).is_none());
        assert_eq!(tile.zoom, 1.0);
        assert!(tile.zoom_history.is_empty());

        // 2. Dragging a 480x270 box (25% width & height) centered at (0.75, 0.25) -> 4.0x zoom
        let box1 = Rect::from_center_size(Pos2::new(1440.0, 270.0), Vec2::new(480.0, 270.0));
        let z1 = tile.zoom_to_marquee_rect(box1, video).expect("valid marquee");
        assert!((z1 - 4.0).abs() < 1e-4);

        // Converge smooth glide animation
        for _ in 0..40 {
            if !tile.step_animation(0.016) {
                break;
            }
        }
        assert!((tile.zoom - 4.0).abs() < 1e-4);
        let uv1 = tile.uv_rect();
        assert!((uv1.center().x - 0.75).abs() < 1e-4);
        assert!((uv1.center().y - 0.25).abs() < 1e-4);

        // 3. Nested box zoom inside the 4.0x view (960x540 = 50% of viewport -> 8.0x MAX)
        let box2 = Rect::from_center_size(Pos2::new(960.0, 540.0), Vec2::new(960.0, 540.0));
        let z2 = tile.zoom_to_marquee_rect(box2, video).expect("nested marquee");
        assert!((z2 - 8.0).abs() < 1e-4);
        for _ in 0..40 {
            if !tile.step_animation(0.016) {
                break;
            }
        }
        assert!((tile.zoom - 8.0).abs() < 1e-4);
        assert_eq!(tile.zoom_history.len(), 2);

        // 4. Step back unwinds 8.0x -> 4.0x -> 1.0x
        assert!(tile.step_back_zoom());
        for _ in 0..40 {
            if !tile.step_animation(0.016) {
                break;
            }
        }
        assert!((tile.zoom - 4.0).abs() < 1e-4);

        assert!(tile.step_back_zoom());
        for _ in 0..40 {
            if !tile.step_animation(0.016) {
                break;
            }
        }
        assert_eq!(tile.zoom, 1.0);
        assert_eq!(tile.pan_uv, Vec2::ZERO);
    }

    #[test]
    fn test_interactive_radar_jump_to_uv_clamps_cleanly() {
        let mut tile = TileUiState::new();
        tile.zoom_by(4.0, Pos2::new(0.5, 0.5));

        // Jump to (0.7, 0.3) via radar click
        tile.jump_to_radar_uv(Pos2::new(0.7, 0.3));
        assert!((tile.uv_rect().center().x - 0.7).abs() < 1e-4);
        assert!((tile.uv_rect().center().y - 0.3).abs() < 1e-4);

        // Clicking extreme top-left corner (0.0, 0.0) clamps UV center to 0.5 / 4.0 = 0.125
        tile.jump_to_radar_uv(Pos2::new(0.0, 0.0));
        assert!((tile.uv_rect().min.x - 0.0).abs() < 1e-5);
        assert!((tile.uv_rect().min.y - 0.0).abs() < 1e-5);
        assert!((tile.uv_rect().center().x - 0.125).abs() < 1e-4);
    }
}
