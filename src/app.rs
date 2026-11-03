use crate::config::{
    expand_tilde, save_env_config, AppConfig, NvrEnvConfig,
};
use crate::p2p::{is_lan_reachable, P2PTunnelManager};
use crate::stream::{
    scan_active_nvr_channels, StreamManager, StreamStatus,
};
use chrono::Local;
use eframe::egui::{
    self, Align2, Color32, ColorImage, CursorIcon, FontFamily, FontId, Key,
    PointerButton, Pos2, Rect, RichText, Rounding, Sense, Stroke, TextureHandle,
    TextureOptions, Ui, Vec2,
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

#[derive(Default)]
struct TileUiState {
    texture: Option<TextureHandle>,
    last_uploaded_seq: u64,
    tex_width: u32,
    tex_height: u32,
    zoom: f32,
    pan_uv: Vec2,
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
        }
    }

    fn reset_zoom(&mut self) {
        self.zoom = 1.0;
        self.pan_uv = Vec2::ZERO;
    }

    fn uv_rect(&self) -> Rect {
        let z = self.zoom.clamp(1.0, 5.0);
        if z <= 1.001 {
            return Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
        }
        let span = 1.0 / z;
        let max_offset = 1.0 - span;
        let cx = (0.5 + self.pan_uv.x).clamp(span * 0.5, 1.0 - span * 0.5);
        let cy = (0.5 + self.pan_uv.y).clamp(span * 0.5, 1.0 - span * 0.5);
        let min_x = (cx - span * 0.5).clamp(0.0, max_offset);
        let min_y = (cy - span * 0.5).clamp(0.0, max_offset);
        Rect::from_min_max(Pos2::new(min_x, min_y), Pos2::new(min_x + span, min_y + span))
    }
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

    fullscreen_channel: Option<usize>,
    all_paused: bool,
    patrol_active: bool,
    last_patrol_switch: Instant,

    show_settings: bool,
    settings_tab: SettingsTab,
    show_password: bool,

    rename_modal_channel: Option<usize>,
    rename_input: String,

    toast_message: Option<(String, Instant)>,
    scan_in_progress: Arc<Mutex<Option<Option<Vec<usize>>>>>,

    rss_mb: f32,
    last_sys_poll: Instant,
}

impl CameraViewerApp {
    pub fn new(cc: &eframe::CreationContext<'_>, env_path: PathBuf, mut env_cfg: NvrEnvConfig) -> Self {
        configure_dark_theme(&cc.egui_ctx);

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
            fullscreen_channel: None,
            all_paused: false,
            patrol_active: false,
            last_patrol_switch: Instant::now(),
            show_settings: is_unconfigured,
            settings_tab: SettingsTab::NvrAndCameras,
            show_password: false,
            rename_modal_channel: None,
            rename_input: String::new(),
            toast_message: initial_toast,
            scan_in_progress: Arc::new(Mutex::new(None)),
            rss_mb: read_process_rss_mb(),
            last_sys_poll: Instant::now(),
        }
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
        self.stream_mgr.set_fullscreen_focus(
            Some(channel),
            self.app_cfg.fullscreen_auto_hd,
            self.env_cfg.default_subtype,
        );
    }

    fn exit_fullscreen(&mut self) {
        self.fullscreen_channel = None;
        self.patrol_active = false;
        self.stream_mgr.set_fullscreen_focus(
            None,
            self.app_cfg.fullscreen_auto_hd,
            self.env_cfg.default_subtype,
        );
    }

    fn toggle_camera_hd_sd(&mut self, channel: usize) {
        if let Some(slot) = self.stream_mgr.slot(channel) {
            let current_target = slot.target_subtype.load(Ordering::Acquire);
            let next_sub = if current_target == 0 { 1 } else { 0 };
            slot.request_subtype(next_sub);
            let label = if next_sub == 0 { "HD (Main-Stream)" } else { "SD (Sub-Stream)" };
            self.show_toast(format!(
                "{} switched to {}",
                self.app_cfg.camera_display_name(channel),
                label
            ));
        }
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

        let out_dir = expand_tilde(&self.app_cfg.screenshot_dir);
        let _ = fs::create_dir_all(&out_dir);
        let ts = Local::now().format("%Y%m%d_%H%M%S");
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
            Rgba([11, 15, 23, 255]),
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
        let full_path = out_dir.join(format!("wall_{}cams_{}.png", self.app_cfg.num_cameras, ts));

        match canvas.save(&full_path) {
            Ok(()) => self.show_toast(format!("Saved {}-cam wall snapshot: {}", self.app_cfg.num_cameras, full_path.display())),
            Err(e) => self.show_toast(format!("Snapshot error: {}", e)),
        }
    }

    fn apply_and_save_settings(&mut self) {
        self.edit_env_cfg.num_cameras = self.edit_env_cfg.num_cameras.clamp(1, 32);
        self.env_cfg = self.edit_env_cfg.clone();
        self.sync_p2p_tunnel();
        self.app_cfg.num_cameras = self.env_cfg.num_cameras;
        self.app_cfg.normalize_channels();

        for ch in 1..=self.app_cfg.num_cameras {
            self.tiles.entry(ch).or_insert_with(TileUiState::new);
        }

        let _ = save_env_config(&self.env_path, &self.env_cfg);
        let _ = self.app_cfg.save();

        self.stream_mgr.update_env_and_restart(self.env_cfg.clone());
        self.fullscreen_channel = None;
        self.show_toast(format!(
            "Saved configuration ({} cameras on {}:{})",
            self.env_cfg.num_cameras,
            self.env_cfg.active_host(),
            self.env_cfg.active_port()
        ));
    }

    fn handle_keyboard_shortcuts(&mut self, ctx: &egui::Context) {
        if self.show_settings || self.rename_modal_channel.is_some() {
            if ctx.input(|i| i.key_pressed(Key::Escape)) {
                self.show_settings = false;
                self.rename_modal_channel = None;
            }
            return;
        }

        ctx.input(|i| {
            if i.key_pressed(Key::Escape) && self.fullscreen_channel.is_some() {
                self.exit_fullscreen();
            } else if i.key_pressed(Key::F11) {
                if self.fullscreen_channel.is_some() {
                    self.exit_fullscreen();
                } else if let Some(&first_ch) = self.app_cfg.channel_order.first() {
                    self.enter_fullscreen(first_ch);
                }
            } else if i.key_pressed(Key::ArrowRight) || i.key_pressed(Key::CloseBracket) {
                if self.fullscreen_channel.is_some() {
                    self.cycle_fullscreen(1);
                }
            } else if i.key_pressed(Key::ArrowLeft) || i.key_pressed(Key::OpenBracket) {
                if self.fullscreen_channel.is_some() {
                    self.cycle_fullscreen(-1);
                }
            } else if i.key_pressed(Key::R) {
                self.stream_mgr.resync_all();
                self.show_toast("Re-synced all camera streams to live edge");
            } else if i.key_pressed(Key::P) || i.key_pressed(Key::Space) {
                self.all_paused = !self.all_paused;
                self.stream_mgr.set_all_paused(self.all_paused);
                self.show_toast(if self.all_paused {
                    "Paused all streams"
                } else {
                    "Resumed all streams"
                });
            } else if i.key_pressed(Key::S) {
                self.save_composite_wall_snapshot();
            } else if i.key_pressed(Key::M) {
                self.patrol_active = !self.patrol_active;
                self.last_patrol_switch = Instant::now();
                if self.patrol_active && self.fullscreen_channel.is_none() {
                    if let Some(&first_ch) = self.app_cfg.channel_order.first() {
                        self.enter_fullscreen(first_ch);
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

            // Direct camera hotkeys: 1..9 -> Cam 1..9, 0 -> Cam 10, - -> Cam 11, = -> Cam 12
            let hotkey_map = [
                (Key::Num1, 1usize),
                (Key::Num2, 2),
                (Key::Num3, 3),
                (Key::Num4, 4),
                (Key::Num5, 5),
                (Key::Num6, 6),
                (Key::Num7, 7),
                (Key::Num8, 8),
                (Key::Num9, 9),
                (Key::Num0, 10),
                (Key::Minus, 11),
                (Key::Equals, 12),
            ];
            for (k, ch) in hotkey_map {
                if i.key_pressed(k) && ch <= self.app_cfg.num_cameras {
                    if self.fullscreen_channel == Some(ch) {
                        self.exit_fullscreen();
                    } else {
                        self.enter_fullscreen(ch);
                    }
                }
            }
        });
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
            let tile = self.tiles.entry(ch).or_insert_with(TileUiState::new);

            if seq > 0 && seq != tile.last_uploaded_seq {
                if let Ok(guard) = slot.buffer.lock() {
                    let w = guard.width as usize;
                    let h = guard.height as usize;
                    if w > 0 && h > 0 && guard.rgba.len() == w * h * 4 {
                        let color_img =
                            ColorImage::from_rgba_unmultiplied([w, h], &guard.rgba);
                        match &mut tile.texture {
                            Some(tex) if tile.tex_width == guard.width && tile.tex_height == guard.height => {
                                tex.set(color_img, TextureOptions::LINEAR);
                            }
                            _ => {
                                tile.texture = Some(ctx.load_texture(
                                    format!("cam_tex_{}", ch),
                                    color_img,
                                    TextureOptions::LINEAR,
                                ));
                                tile.tex_width = guard.width;
                                tile.tex_height = guard.height;
                            }
                        }
                        tile.last_uploaded_seq = seq;
                    }
                }
            }
        }
    }

    fn render_top_command_bar(&mut self, ui: &mut Ui) {
        let total = self.app_cfg.num_cameras;
        let mut live_count = 0usize;
        let mut total_drained = 0u64;
        for ch in 1..=total {
            if let Some(slot) = self.stream_mgr.slot(ch) {
                if slot.status() == StreamStatus::Live {
                    live_count += 1;
                }
                total_drained += slot.drained_burst_frames.load(Ordering::Relaxed);
            }
        }

        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;

            // Brand / App Title
            ui.label(
                RichText::new("NVR COMMAND CENTER")
                    .font(FontId::new(14.0, FontFamily::Proportional))
                    .strong()
                    .color(Color32::from_rgb(240, 246, 252)),
            );

            ui.separator();

            // Live Telemetry Pill
            let status_color = if live_count == total {
                Color32::from_rgb(16, 185, 129)
            } else if live_count > 0 {
                Color32::from_rgb(245, 158, 11)
            } else {
                Color32::from_rgb(239, 68, 68)
            };

            egui::Frame::none()
                .fill(Color32::from_rgb(22, 29, 43))
                .stroke(Stroke::new(1.0_f32, status_color.gamma_multiply(0.5)))
                .rounding(Rounding::same(6.0))
                .inner_margin(egui::Margin::symmetric(8.0, 4.0))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(format!(
                            "{}/{} Live  |  Sync <80ms (Drained: {})  |  RAM: {:.0} MB",
                            live_count, total, total_drained, self.rss_mb
                        ))
                        .size(12.0)
                        .color(status_color),
                    );
                });

            // 1-Click Network Mode Switcher: LAN <-> Cloud P2P (Any Internet) <-> Tailscale
            let net_label = if self.env_cfg.use_cloud_p2p {
                format!("Cloud P2P ({})", self.p2p_mgr.status_text())
            } else if self.env_cfg.use_tailscale {
                format!("Tailscale ({})", self.env_cfg.active_host())
            } else {
                format!("LAN ({})", self.env_cfg.active_host())
            };
            let net_btn = egui::Button::new(
                RichText::new(net_label)
                    .size(12.0)
                    .strong()
                    .color(Color32::WHITE),
            )
            .fill(if self.env_cfg.use_cloud_p2p {
                Color32::from_rgb(8, 145, 178)
            } else if self.env_cfg.use_tailscale {
                Color32::from_rgb(79, 70, 229)
            } else {
                Color32::from_rgb(30, 41, 59)
            });

            if ui
                .add(net_btn)
                .on_hover_text("Click to switch between Local LAN Direct and Cloud P2P (works on any internet connection)")
                .clicked()
            {
                if !self.env_cfg.p2p_serial.trim().is_empty() {
                    self.env_cfg.use_cloud_p2p = !self.env_cfg.use_cloud_p2p;
                    if self.env_cfg.use_cloud_p2p {
                        self.env_cfg.use_tailscale = false;
                    }
                    self.edit_env_cfg.use_cloud_p2p = self.env_cfg.use_cloud_p2p;
                    self.edit_env_cfg.use_tailscale = self.env_cfg.use_tailscale;
                    self.sync_p2p_tunnel();
                    let _ = save_env_config(&self.env_path, &self.env_cfg);
                    self.stream_mgr.update_env_and_restart(self.env_cfg.clone());
                    let mode_desc = if self.env_cfg.use_cloud_p2p {
                        "Cloud P2P Tunnel (works on any internet connection)"
                    } else {
                        "Local LAN Direct"
                    };
                    self.show_toast(format!("Switched to {}", mode_desc));
                } else if !self.env_cfg.tailscale_host.trim().is_empty() {
                    self.env_cfg.use_tailscale = !self.env_cfg.use_tailscale;
                    self.edit_env_cfg.use_tailscale = self.env_cfg.use_tailscale;
                    let _ = save_env_config(&self.env_path, &self.env_cfg);
                    self.stream_mgr.update_env_and_restart(self.env_cfg.clone());
                    self.show_toast(format!("Switched to {}", self.env_cfg.active_host()));
                } else {
                    self.show_settings = true;
                    self.settings_tab = SettingsTab::NvrAndCameras;
                    self.show_toast("Configure Cloud P2P Serial or Tailscale IP in Settings");
                }
            }

            ui.separator();

            // Layout Switcher Pills
            for mode in ["3x4", "4x3", "2x6", "Auto"] {
                let norm = mode.to_lowercase();
                let selected = self.app_cfg.layout_mode.eq_ignore_ascii_case(&norm)
                    && self.fullscreen_channel.is_none();
                let btn = egui::Button::new(
                    RichText::new(mode).size(12.0).color(if selected {
                        Color32::WHITE
                    } else {
                        Color32::from_rgb(180, 192, 210)
                    }),
                )
                .fill(if selected {
                    Color32::from_rgb(37, 99, 235)
                } else {
                    Color32::from_rgb(26, 34, 50)
                });
                if ui.add(btn).clicked() {
                    self.exit_fullscreen();
                    self.app_cfg.layout_mode = norm;
                    let _ = self.app_cfg.save();
                }
            }

            ui.separator();

            // Global SD / HD Switcher
            if ui
                .button(RichText::new("All SD").size(12.0))
                .on_hover_text("Switch all cameras to low-bandwidth Sub-Stream (SD)")
                .clicked()
            {
                self.env_cfg.default_subtype = 1;
                self.edit_env_cfg.default_subtype = 1;
                self.stream_mgr.set_all_subtype(1);
                self.show_toast("Switched all cameras to SD Sub-Stream");
            }
            if ui
                .button(RichText::new("All HD").size(12.0))
                .on_hover_text("Switch all cameras to 720p/1080p Main-Stream (HD)")
                .clicked()
            {
                self.env_cfg.default_subtype = 0;
                self.edit_env_cfg.default_subtype = 0;
                self.stream_mgr.set_all_subtype(0);
                self.show_toast("Switched all cameras to HD Main-Stream");
            }

            ui.separator();

            // Quick Actions
            if ui
                .button(RichText::new("Re-Sync (R)").size(12.0))
                .on_hover_text("Flush all buffers and re-lock all cameras to live edge")
                .clicked()
            {
                self.stream_mgr.resync_all();
                self.show_toast("Flushed all buffers -> Locked to live edge");
            }

            let pause_label = if self.all_paused {
                "Resume (P)"
            } else {
                "Pause (P)"
            };
            if ui.button(RichText::new(pause_label).size(12.0)).clicked() {
                self.all_paused = !self.all_paused;
                self.stream_mgr.set_all_paused(self.all_paused);
            }

            if ui
                .button(RichText::new("Snapshot (S)").size(12.0))
                .on_hover_text("Save PNG snapshot (12-cam wall in Grid View, or HD frame in Fullscreen)")
                .clicked()
            {
                self.save_composite_wall_snapshot();
            }

            let patrol_btn = egui::Button::new(
                RichText::new(if self.patrol_active {
                    "Patrol: ON"
                } else {
                    "Patrol (M)"
                })
                .size(12.0),
            )
            .fill(if self.patrol_active {
                Color32::from_rgb(16, 185, 129)
            } else {
                Color32::from_rgb(26, 34, 50)
            });
            if ui
                .add(patrol_btn)
                .on_hover_text("Auto-cycle Fullscreen across all cameras")
                .clicked()
            {
                self.patrol_active = !self.patrol_active;
                self.last_patrol_switch = Instant::now();
                if self.patrol_active && self.fullscreen_channel.is_none() {
                    if let Some(&first_ch) = self.app_cfg.channel_order.first() {
                        self.enter_fullscreen(first_ch);
                    }
                }
            }

            // Single Screen <-> Multi-View Grid Toggle + Single View Controls
            if let Some(fs_ch) = self.fullscreen_channel {
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("Multi-View Grid (Esc)")
                                .size(12.0)
                                .strong()
                                .color(Color32::WHITE),
                        )
                        .fill(Color32::from_rgb(37, 99, 235)),
                    )
                    .on_hover_text("Return to 12-camera Multi-View Grid")
                    .clicked()
                {
                    self.exit_fullscreen();
                }

                if ui
                    .button(RichText::new("< Prev").size(12.0))
                    .on_hover_text("Previous Camera (Left Arrow)")
                    .clicked()
                {
                    self.cycle_fullscreen(-1);
                }
                if ui
                    .button(RichText::new("Next >").size(12.0))
                    .on_hover_text("Next Camera (Right Arrow)")
                    .clicked()
                {
                    self.cycle_fullscreen(1);
                }

                let cur_sub = self
                    .stream_mgr
                    .slot(fs_ch)
                    .map(|s| s.target_subtype.load(Ordering::Relaxed))
                    .unwrap_or(1);
                let hd_btn_label = if cur_sub == 0 {
                    format!("Cam {}: HD (Click for SD)", fs_ch)
                } else {
                    format!("Cam {}: SD (Click for HD)", fs_ch)
                };
                let hd_btn = egui::Button::new(
                    RichText::new(hd_btn_label).size(12.0).strong().color(Color32::WHITE),
                )
                .fill(if cur_sub == 0 {
                    Color32::from_rgb(2, 132, 199)
                } else {
                    Color32::from_rgb(30, 41, 59)
                });
                if ui
                    .add(hd_btn)
                    .on_hover_text("Toggle this camera between SD and HD")
                    .clicked()
                {
                    self.toggle_camera_hd_sd(fs_ch);
                }
            } else if ui
                .button(RichText::new("Single View (F11)").size(12.0))
                .on_hover_text("Switch to Single Screen View (F11)")
                .clicked()
            {
                if let Some(&first_ch) = self.app_cfg.channel_order.first() {
                    self.enter_fullscreen(first_ch);
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("Buy Me a Coffee")
                                .size(12.0)
                                .strong()
                                .color(Color32::from_rgb(255, 221, 87)),
                        )
                        .fill(Color32::from_rgb(45, 35, 15)),
                    )
                    .on_hover_text(BUY_ME_A_COFFEE_URL)
                    .clicked()
                {
                    open_url(BUY_ME_A_COFFEE_URL);
                }

                if ui
                    .button(RichText::new("Settings").size(12.0).strong())
                    .clicked()
                {
                    self.edit_env_cfg = self.env_cfg.clone();
                    self.show_settings = !self.show_settings;
                }
            });
        });
    }

    fn render_camera_tile(&mut self, ui: &mut Ui, channel: usize, rect: Rect, is_fullscreen: bool) {
        // Compute interactive HUD button rectangles first so clicks on HUD buttons don't trigger tile zoom/fullscreen
        let btn_h = 22.0;
        let sd_hd_w = 40.0;
        let view_btn_w = 54.0;
        let view_btn_rect = Rect::from_min_size(
            Pos2::new(rect.max.x - view_btn_w - 6.0, rect.max.y - btn_h - 6.0),
            Vec2::new(view_btn_w, btn_h),
        );
        let sd_hd_rect = Rect::from_min_size(
            Pos2::new(view_btn_rect.min.x - sd_hd_w - 5.0, rect.max.y - btn_h - 6.0),
            Vec2::new(sd_hd_w, btn_h),
        );

        let response = ui.allocate_rect(rect, Sense::click_and_drag());
        let painter = ui.painter_at(rect);

        // Tile background & border
        painter.rect_filled(rect, Rounding::same(4.0), Color32::from_rgb(12, 16, 24));

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

        let pointer_pos = ui.input(|i| i.pointer.interact_pos());
        let over_sd_hd_btn = pointer_pos.map_or(false, |p| sd_hd_rect.contains(p));
        let over_view_btn = pointer_pos.map_or(false, |p| view_btn_rect.contains(p));
        let over_hud_btn = over_sd_hd_btn || over_view_btn;

        if over_hud_btn {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }

        // Handle 1-click on per-camera [SD/HD] or [Single/Grid] button
        let mut hud_clicked = false;
        if response.clicked() {
            if over_sd_hd_btn {
                self.toggle_camera_hd_sd(channel);
                hud_clicked = true;
            } else if over_view_btn {
                if is_fullscreen {
                    self.exit_fullscreen();
                } else {
                    self.enter_fullscreen(channel);
                }
                hud_clicked = true;
            }
        }

        // Handle UV digital zoom & pan + double-click Single View
        {
            let tile = self.tiles.entry(channel).or_insert_with(TileUiState::new);
            if response.hovered() {
                let scroll_y = ui.input(|i| i.smooth_scroll_delta.y);
                if scroll_y.abs() > 0.1 {
                    let factor = if scroll_y > 0.0 { 1.15 } else { 1.0 / 1.15 };
                    tile.zoom = (tile.zoom * factor).clamp(1.0, 5.0);
                    if tile.zoom <= 1.01 {
                        tile.pan_uv = Vec2::ZERO;
                    }
                }
            }
            if !over_hud_btn && response.dragged_by(PointerButton::Primary) && tile.zoom > 1.01 {
                let delta = response.drag_delta();
                let z = tile.zoom;
                tile.pan_uv.x = (tile.pan_uv.x - delta.x / (rect.width().max(1.0) * z))
                    .clamp(-0.5, 0.5);
                tile.pan_uv.y = (tile.pan_uv.y - delta.y / (rect.height().max(1.0) * z))
                    .clamp(-0.5, 0.5);
                ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
            }
            if !hud_clicked && !over_hud_btn && response.double_clicked() {
                if is_fullscreen {
                    self.exit_fullscreen();
                } else {
                    self.enter_fullscreen(channel);
                }
            }
        }

        // Paint GPU texture with UV zoom/pan rectangle
        let (has_texture, zoom_val, uv_rect) = {
            let tile = self.tiles.get(&channel).unwrap();
            (tile.texture.is_some(), tile.zoom, tile.uv_rect())
        };

        if has_texture {
            if let Some(tex) = self.tiles.get(&channel).and_then(|t| t.texture.as_ref()) {
                painter.image(tex.id(), rect, uv_rect, Color32::WHITE);
            }
        } else {
            // Center status placeholder when waiting for first frame
            let center = rect.center();
            painter.text(
                center - Vec2::new(0.0, 8.0),
                Align2::CENTER_CENTER,
                self.app_cfg.camera_display_name(channel),
                FontId::new(15.0, FontFamily::Proportional),
                Color32::from_rgb(148, 163, 184),
            );
            painter.text(
                center + Vec2::new(0.0, 12.0),
                Align2::CENTER_CENTER,
                status.label(),
                FontId::new(12.0, FontFamily::Proportional),
                 Color32::from_rgb(100, 116, 139),
            );
        }

        // Subtle border (highlighted on hover)
        let border_color = if response.hovered() {
            Color32::from_rgb(59, 130, 246)
        } else {
            Color32::from_rgb(30, 41, 59)
        };
        painter.rect_stroke(
            rect,
            Rounding::same(4.0),
            Stroke::new(1.0_f32, border_color),
        );

        // Top-Left Translucent HUD Pill (Status Dot + Camera Name + SD/HD Badge)
        let cam_name = self.app_cfg.camera_display_name(channel);
        let qual_tag = if active_sub == 0 { "HD" } else { "SD" };
        let hud_text = format!("  {}  [{}]", cam_name, qual_tag);
        let pill_rect = Rect::from_min_size(
            rect.min + Vec2::new(6.0, 6.0),
            Vec2::new((hud_text.len() as f32 * 7.2).clamp(95.0, rect.width() - 12.0), 22.0),
        );
        painter.rect_filled(
            pill_rect,
            Rounding::same(5.0),
            Color32::from_rgba_unmultiplied(10, 14, 22, 195),
        );

        let dot_color = match status {
            StreamStatus::Live => Color32::from_rgb(16, 185, 129),
            StreamStatus::Connecting | StreamStatus::Reconnecting => Color32::from_rgb(245, 158, 11),
            StreamStatus::Paused => Color32::from_rgb(148, 163, 184),
            StreamStatus::Error => Color32::from_rgb(239, 68, 68),
        };
        painter.circle_filled(
            Pos2::new(pill_rect.min.x + 10.0, pill_rect.center().y),
            4.0,
            dot_color,
        );
        painter.text(
            Pos2::new(pill_rect.min.x + 18.0, pill_rect.center().y),
            Align2::LEFT_CENTER,
            format!("{}  {}", cam_name, qual_tag),
            FontId::new(12.0, FontFamily::Proportional),
            Color32::from_rgb(241, 245, 249),
        );

        // Top-Right Translucent Telemetry Pill (FPS + Zoom)
        let right_text = if zoom_val > 1.02 {
            format!("{:.0} FPS | {:.1}x", fps, zoom_val)
        } else if status == StreamStatus::Live {
            format!("{:.0} FPS", fps)
        } else {
            status.label().to_string()
        };
        let r_width = (right_text.len() as f32 * 7.2).max(54.0);
        let right_pill = Rect::from_min_size(
            Pos2::new(rect.max.x - r_width - 6.0, rect.min.y + 6.0),
            Vec2::new(r_width, 22.0),
        );
        painter.rect_filled(
            right_pill,
            Rounding::same(5.0),
            Color32::from_rgba_unmultiplied(10, 14, 22, 185),
        );
        painter.text(
            right_pill.center(),
            Align2::CENTER_CENTER,
            right_text,
            FontId::new(11.5, FontFamily::Proportional),
            Color32::from_rgb(203, 213, 225),
        );

        // Bottom-Right 1-Click Per-Camera [SD/HD] and [Single/Grid] Toggle Buttons
        let sd_hd_fill = if target_sub == 0 {
            if over_sd_hd_btn {
                Color32::from_rgb(14, 165, 233)
            } else {
                Color32::from_rgb(2, 132, 199)
            }
        } else if over_sd_hd_btn {
            Color32::from_rgb(51, 65, 85)
        } else {
            Color32::from_rgba_unmultiplied(15, 23, 42, 215)
        };
        painter.rect_filled(sd_hd_rect, Rounding::same(5.0), sd_hd_fill);
        painter.rect_stroke(
            sd_hd_rect,
            Rounding::same(5.0),
            Stroke::new(
                1.0_f32,
                if target_sub == 0 || over_sd_hd_btn {
                    Color32::from_rgb(56, 189, 248)
                } else {
                    Color32::from_rgb(71, 85, 105)
                },
            ),
        );
        painter.text(
            sd_hd_rect.center(),
            Align2::CENTER_CENTER,
            if target_sub == 0 { "HD" } else { "SD" },
            FontId::new(11.5, FontFamily::Proportional),
            Color32::WHITE,
        );

        let view_fill = if over_view_btn {
            Color32::from_rgb(37, 99, 235)
        } else {
            Color32::from_rgba_unmultiplied(15, 23, 42, 215)
        };
        painter.rect_filled(view_btn_rect, Rounding::same(5.0), view_fill);
        painter.rect_stroke(
            view_btn_rect,
            Rounding::same(5.0),
            Stroke::new(
                1.0_f32,
                if over_view_btn {
                    Color32::from_rgb(96, 165, 250)
                } else {
                    Color32::from_rgb(71, 85, 105)
                },
            ),
        );
        painter.text(
            view_btn_rect.center(),
            Align2::CENTER_CENTER,
            if is_fullscreen { "Grid" } else { "Single" },
            FontId::new(11.5, FontFamily::Proportional),
            Color32::WHITE,
        );

        // Right-Click Context Menu
        let all_channels = self.app_cfg.channel_order.clone();
        response.context_menu(|ui| {
            ui.label(RichText::new(format!("{} (Channel {})", cam_name, channel)).strong());
            ui.separator();

            if ui
                .button(if is_fullscreen {
                    "Exit Fullscreen (Esc)"
                } else {
                    "Fullscreen (Double-Click)"
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

            let target_sub = self
                .stream_mgr
                .slot(channel)
                .map(|s| s.target_subtype.load(Ordering::Relaxed))
                .unwrap_or(1);
            if ui
                .button(if target_sub == 0 {
                    "Switch to SD (Sub-Stream)"
                } else {
                    "Switch to HD (Main-Stream)"
                })
                .clicked()
            {
                ui.close_menu();
                if let Some(slot) = self.stream_mgr.slot(channel) {
                    slot.request_subtype(if target_sub == 0 { 1 } else { 0 });
                }
            }

            if ui.button("Save Camera Snapshot (PNG)").clicked() {
                ui.close_menu();
                self.save_single_snapshot(channel);
            }

            if ui.button("Re-Sync / Reconnect Camera").clicked() {
                ui.close_menu();
                if let Some(slot) = self.stream_mgr.slot(channel) {
                    slot.trigger_resync();
                }
            }

            if zoom_val > 1.01 {
                if ui.button("Reset Digital Zoom (1.0x)").clicked() {
                    ui.close_menu();
                    if let Some(t) = self.tiles.get_mut(&channel) {
                        t.reset_zoom();
                    }
                }
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
                self.show_toast("Auto-Scan completed: 0 channels responded (check NVR IP/credentials)");
            }
        }

        let mut open = self.show_settings;
        egui::Window::new("NVR Command Center - Settings & Configurator")
            .open(&mut open)
            .resizable(true)
            .default_width(560.0)
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
                        "2. Performance & Camera Names",
                    );
                    ui.selectable_value(
                        &mut self.settings_tab,
                        SettingsTab::ShortcutsAndSupport,
                        "3. Shortcuts & Support",
                    );
                });
                ui.separator();

                match self.settings_tab {
                    SettingsTab::NvrAndCameras => {
                        egui::Frame::none()
                            .fill(Color32::from_rgb(22, 31, 48))
                            .stroke(Stroke::new(1.0_f32, Color32::from_rgb(56, 189, 248).gamma_multiply(0.45)))
                            .rounding(Rounding::same(6.0))
                            .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new("Quick Setup Guide:")
                                        .size(12.0)
                                        .strong()
                                        .color(Color32::from_rgb(56, 189, 248)),
                                );
                                ui.label(
                                    RichText::new(
                                        "- LAN Host / IP: Found on your NVR monitor under Menu -> Network -> TCP/IP (e.g. 192.168.1.108)\n\
                                         - Cloud P2P Serial (Optional): Found under Menu -> Network -> P2P (Status: Online) or NVR S/N sticker for viewing on any internet connection without port forwarding.",
                                    )
                                    .size(11.5)
                                    .color(Color32::from_rgb(203, 213, 225)),
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
                                        egui::TextEdit::singleline(&mut self.edit_env_cfg.p2p_serial)
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
                                    ui.add(
                                        egui::Slider::new(&mut self.edit_env_cfg.num_cameras, 1..=32),
                                    );
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
                            .color(Color32::from_rgb(148, 163, 184)),
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

                                ui.label("Fullscreen Auto-HD:");
                                ui.checkbox(
                                    &mut self.app_cfg.fullscreen_auto_hd,
                                    "Warm-swap focused Fullscreen camera to HD (subtype=0)",
                                );
                                ui.end_row();

                                ui.label("Patrol Cycle Interval (sec):");
                                ui.add(
                                    egui::Slider::new(&mut self.app_cfg.patrol_interval_secs, 3..=60),
                                );
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
                        ui.label(RichText::new("Keyboard Shortcuts").strong());
                        ui.label("- 1-9, 0, -, = : Toggle Fullscreen for Cameras 1 to 12");
                        ui.label("- Left / Right Arrows (or [ / ]) : Previous / Next Camera in Fullscreen");
                        ui.label("- Double-Click Tile or F11 / Esc : Enter or Exit Fullscreen");
                        ui.label("- Mouse Wheel / Click + Drag : GPU UV Digital Zoom (1x-5x) & Pan");
                        ui.label("- R : Instant Re-Sync (Flush all buffers to live edge)");
                        ui.label("- P or Space : Pause / Resume all camera streams");
                        ui.label("- S : Save PNG Snapshot (full 12-cam composite wall or single HD frame)");
                        ui.label("- M : Toggle Patrol Mode (auto-rotate Fullscreen)");
                        ui.separator();
                        ui.label(RichText::new("Support Open-Source Development").strong());
                        ui.horizontal(|ui| {
                            if ui.button("Buy Me a Coffee").clicked() {
                                open_url(BUY_ME_A_COFFEE_URL);
                            }
                            if ui.button("GitHub Sponsors").clicked() {
                                open_url(GITHUB_SPONSORS_URL);
                            }
                            if ui.button("GitHub Repository").clicked() {
                                open_url(GITHUB_REPO_URL);
                            }
                        });
                    }
                }

                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("Save & Apply (.env 0600)")
                                    .strong()
                                    .color(Color32::WHITE),
                            )
                            .fill(Color32::from_rgb(37, 99, 235)),
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

        self.handle_keyboard_shortcuts(ctx);
        self.update_gpu_textures(ctx);

        egui::TopBottomPanel::top("command_center_top_bar")
            .frame(
                egui::Frame::none()
                    .fill(Color32::from_rgb(11, 15, 23))
                    .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(30, 41, 59))),
            )
            .show(ctx, |ui| {
                self.render_top_command_bar(ui);
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(Color32::from_rgb(7, 10, 16))
                    .inner_margin(egui::Margin::same(6.0)),
            )
            .show(ctx, |ui| {
                let avail = ui.available_rect_before_wrap();
                if let Some(fs_ch) = self.fullscreen_channel {
                    self.render_camera_tile(ui, fs_ch, avail, true);
                } else {
                    let (rows, cols) = self.app_cfg.grid_dimensions();
                    let gap = 4.0f32;
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
                    if ts.elapsed() < Duration::from_millis(3200) {
                        let painter = ui.painter();
                        let width = (msg.len() as f32 * 7.5 + 28.0).clamp(180.0, avail.width() - 20.0);
                        let toast_rect = Rect::from_center_size(
                            Pos2::new(avail.center().x, avail.max.y - 26.0),
                            Vec2::new(width, 30.0),
                        );
                        painter.rect_filled(
                            toast_rect,
                            Rounding::same(6.0),
                            Color32::from_rgba_unmultiplied(15, 23, 42, 235),
                        );
                        painter.rect_stroke(
                            toast_rect,
                            Rounding::same(6.0),
                            Stroke::new(1.0_f32, Color32::from_rgb(59, 130, 246)),
                        );
                        painter.text(
                            toast_rect.center(),
                            Align2::CENTER_CENTER,
                            msg,
                            FontId::new(12.5, FontFamily::Proportional),
                            Color32::WHITE,
                        );
                    } else {
                        self.toast_message = None;
                    }
                }
            });

        self.render_settings_modal(ctx);
        self.render_rename_modal(ctx);

        // Synchronized presentation tick (~30 Hz UI check, zero texture upload unless frame_seq changed)
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

fn configure_dark_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(11, 15, 23);
    visuals.window_fill = Color32::from_rgb(15, 23, 42);
    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(15, 23, 42);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(26, 34, 50);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(37, 99, 235);
    visuals.widgets.active.bg_fill = Color32::from_rgb(29, 78, 216);
    ctx.set_visuals(visuals);
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

fn open_url(url: &str) {
    let _ = Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
