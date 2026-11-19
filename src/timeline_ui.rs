use crate::config::AppConfig;
use crate::nvr_api::{
    epoch_ms_to_local_naive, local_naive_to_epoch_ms, DayTimelineIndex, NvrEventKind, RecordedSpan,
};
use crate::playback::{
    matches_event_filter, PlaybackManager, PlaybackStatus, SmartSkipMode, MAX_CLIP_EXPORT_MS,
};
use crate::stream::StreamStatus;
use crate::wayland_pinch::PinchFrameSnapshot;
use chrono::{Datelike, Duration as ChronoDuration, Local, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontFamily, FontId, Key, LayerId, Order, PointerButton,
    Pos2, Rect, Response, Rounding, Sense, Stroke, TextureHandle, Ui, Vec2,
};

pub const TIMELINE_DOCK_HEIGHT: f32 = 98.0;
pub const EVENT_DRAWER_WIDTH: f32 = 248.0;
pub const LIVE_PIP_SIZE: Vec2 = Vec2::new(196.0, 110.0);

pub const MIN_WINDOW_SPAN_MS: i64 = 120_000; // 2 minutes
pub const MAX_WINDOW_SPAN_MS: i64 = 86_400_000; // 24 hours
pub const DEFAULT_WINDOW_SPAN_MS: i64 = 3_600_000; // 1 hour
pub const MIN_EVENT_WIDTH_PX: f32 = 3.5;

// Obsidian Studio Palette
pub const OBSIDIAN_BG: Color32 = Color32::from_rgb(9, 9, 11);
pub const GRAPHITE_SURFACE: Color32 = Color32::from_rgb(17, 17, 20);
pub const ZINC_ELEVATED: Color32 = Color32::from_rgb(24, 24, 27);
pub const ZINC_HOVER: Color32 = Color32::from_rgb(39, 39, 46);
pub const ZINC_DIM: Color32 = Color32::from_rgb(82, 82, 91);
pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(244, 244, 245);
pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(212, 212, 216);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(161, 161, 170);

pub const STATUS_ICE: Color32 = Color32::from_rgb(56, 189, 248); // #38BDF8
pub const STATUS_EMERALD: Color32 = Color32::from_rgb(16, 185, 129); // #10B981
pub const STATUS_AMBER: Color32 = Color32::from_rgb(245, 158, 11); // #F59E0B
pub const STATUS_CORAL: Color32 = Color32::from_rgb(239, 68, 68); // #EF4444
pub const STATUS_VIOLET: Color32 = Color32::from_rgb(167, 139, 250); // #A78BFA

#[inline]
pub fn hairline_stroke() -> Color32 {
    Color32::from_rgba_unmultiplied(255, 255, 255, 22)
}

#[inline]
fn continuous_coverage_fill() -> Color32 {
    Color32::from_rgba_unmultiplied(16, 185, 129, 55)
}

#[inline]
fn current_wall_ms() -> i64 {
    local_naive_to_epoch_ms(Local::now().naive_local()).max(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LivePipCorner {
    #[default]
    TopRight,
    BottomLeft,
    TopLeft,
    Hidden,
}

impl LivePipCorner {
    pub fn next_visible_corner(self) -> Self {
        match self {
            Self::TopRight => Self::BottomLeft,
            Self::BottomLeft => Self::TopLeft,
            Self::TopLeft | Self::Hidden => Self::TopRight,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::TopRight => "Top-Right",
            Self::BottomLeft => "Bottom-Left",
            Self::TopLeft => "Top-Left",
            Self::Hidden => "Hidden",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivePipAction {
    ReturnToLive,
    CycleCorner,
    Hide,
}

pub fn compute_live_pip_rect(
    viewport_rect: Rect,
    corner: LivePipCorner,
    avoid_bottom_left_radar: bool,
) -> Option<Rect> {
    if corner == LivePipCorner::Hidden
        || viewport_rect.width() < 320.0
        || viewport_rect.height() < 200.0
    {
        return None;
    }

    let w = LIVE_PIP_SIZE.x;
    let h = LIVE_PIP_SIZE.y;
    let margin = 12.0;
    let top_hud_clearance = 34.0;

    let min = match corner {
        LivePipCorner::TopRight => Pos2::new(
            (viewport_rect.max.x - margin - w).max(viewport_rect.min.x + margin),
            (viewport_rect.min.y + top_hud_clearance).min(viewport_rect.max.y - margin - h),
        ),
        LivePipCorner::TopLeft => Pos2::new(
            viewport_rect.min.x + margin,
            (viewport_rect.min.y + top_hud_clearance).min(viewport_rect.max.y - margin - h),
        ),
        LivePipCorner::BottomLeft => {
            let radar_offset = if avoid_bottom_left_radar { 68.0 } else { 0.0 };
            Pos2::new(
                viewport_rect.min.x + margin,
                (viewport_rect.max.y - margin - h - radar_offset)
                    .max(viewport_rect.min.y + top_hud_clearance),
            )
        }
        LivePipCorner::Hidden => return None,
    };

    Some(Rect::from_min_size(min, LIVE_PIP_SIZE))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveBracketDrag {
    In,
    Out,
}

fn pick_bracket_drag_target(
    origin: Pos2,
    track_min_y: f32,
    in_x: Option<f32>,
    out_x: Option<f32>,
    drag_delta_x: f32,
) -> Option<ActiveBracketDrag> {
    if origin.y < track_min_y {
        return None;
    }
    let d_in = in_x.map(|ix| (origin.x - ix).abs()).filter(|&d| d <= 7.0);
    let d_out = out_x.map(|ox| (origin.x - ox).abs()).filter(|&d| d <= 7.0);
    match (d_in, d_out) {
        (Some(di), Some(do_)) => {
            if (di - do_).abs() <= 0.5 {
                if drag_delta_x < 0.0 {
                    Some(ActiveBracketDrag::In)
                } else {
                    Some(ActiveBracketDrag::Out)
                }
            } else if di < do_ {
                Some(ActiveBracketDrag::In)
            } else {
                Some(ActiveBracketDrag::Out)
            }
        }
        (Some(_), None) => Some(ActiveBracketDrag::In),
        (None, Some(_)) => Some(ActiveBracketDrag::Out),
        (None, None) => None,
    }
}

#[derive(Debug, Clone)]
pub struct TimelineUiState {
    pub window_span_ms: i64,
    pub view_center_ms: i64,
    pub follow_playhead: bool,
    pub selected_date: NaiveDate,
    pub event_filter: Option<NvrEventKind>,
    pub clip_in_ms: Option<i64>,
    pub clip_out_ms: Option<i64>,
    pub show_event_drawer: bool,
    pub show_calendar_popup: bool,
    pub show_time_jump_popup: bool,
    pub dragging_playhead: bool,
    pub drawer_newest_first: bool,
    pub pip_corner: LivePipCorner,
    pub auto_scroll_to_active_event: bool,

    calendar_ym: (i32, u32),
    calendar_cursor_date: Option<NaiveDate>,
    dragging_bracket: Option<ActiveBracketDrag>,
    panning_window: bool,
    last_scrub_ms: Option<i64>,
    last_emitted_scrub_ms: Option<i64>,
    time_jump_input: String,
    time_jump_error: Option<String>,
    time_jump_needs_focus: bool,
}

impl Default for TimelineUiState {
    fn default() -> Self {
        let now_local = Local::now().naive_local();
        let now_ms = local_naive_to_epoch_ms(now_local);
        Self::new(now_ms.saturating_sub(300_000))
    }
}

impl TimelineUiState {
    pub fn new(initial_epoch_ms: i64) -> Self {
        let dt = epoch_ms_to_local_naive(initial_epoch_ms);
        let date = dt.date();
        Self {
            window_span_ms: DEFAULT_WINDOW_SPAN_MS,
            view_center_ms: initial_epoch_ms,
            follow_playhead: true,
            selected_date: date,
            event_filter: None,
            clip_in_ms: None,
            clip_out_ms: None,
            show_event_drawer: false,
            show_calendar_popup: false,
            show_time_jump_popup: false,
            dragging_playhead: false,
            drawer_newest_first: true,
            pip_corner: LivePipCorner::TopRight,
            auto_scroll_to_active_event: true,
            calendar_ym: (date.year(), date.month()),
            calendar_cursor_date: Some(date),
            dragging_bracket: None,
            panning_window: false,
            last_scrub_ms: None,
            last_emitted_scrub_ms: None,
            time_jump_input: dt.format("%H:%M:%S").to_string(),
            time_jump_error: None,
            time_jump_needs_focus: false,
        }
    }

    pub fn reset_for_open(&mut self, start_epoch_ms: i64) {
        let dt = epoch_ms_to_local_naive(start_epoch_ms);
        let date = dt.date();
        self.view_center_ms = start_epoch_ms;
        self.follow_playhead = true;
        self.selected_date = date;
        self.calendar_ym = (dt.year(), dt.month());
        self.calendar_cursor_date = Some(date);
        self.clip_in_ms = None;
        self.clip_out_ms = None;
        self.show_calendar_popup = false;
        self.show_time_jump_popup = false;
        self.dragging_playhead = false;
        self.dragging_bracket = None;
        self.panning_window = false;
        self.last_scrub_ms = None;
        self.last_emitted_scrub_ms = None;
        self.time_jump_input = dt.format("%H:%M:%S").to_string();
        self.time_jump_error = None;
        self.time_jump_needs_focus = false;
        self.auto_scroll_to_active_event = true;
    }

    pub fn sync_playhead(&mut self, playhead_epoch_ms: i64, is_playing: bool) {
        if self.is_actively_dragging() {
            return;
        }
        let playhead_date = epoch_ms_to_local_naive(playhead_epoch_ms).date();
        if self.follow_playhead {
            self.selected_date = playhead_date;
            if is_playing {
                self.view_center_ms = playhead_epoch_ms;
            } else {
                let half = self.window_span_ms / 2;
                let margin = (self.window_span_ms / 10).max(1_000);
                if playhead_epoch_ms < self.view_center_ms - half + margin
                    || playhead_epoch_ms > self.view_center_ms + half - margin
                {
                    self.view_center_ms = playhead_epoch_ms;
                }
            }
        }
    }

    pub fn recenter_on_playhead(&mut self, playhead_epoch_ms: i64) {
        self.follow_playhead = true;
        self.view_center_ms = playhead_epoch_ms;
        self.selected_date = epoch_ms_to_local_naive(playhead_epoch_ms).date();
    }

    pub fn visible_range_ms(&self) -> (i64, i64) {
        let span = self.window_span_ms.clamp(MIN_WINDOW_SPAN_MS, MAX_WINDOW_SPAN_MS);
        let half = span / 2;
        let start = self.view_center_ms.saturating_sub(half);
        (start, start.saturating_add(span))
    }

    pub fn calendar_ym(&self) -> (i32, u32) {
        self.calendar_ym
    }

    pub fn zoom_by_span_factor(&mut self, span_factor: f64, anchor_epoch_ms: i64) {
        let now_wall_ms = current_wall_ms();
        let max_center_ms = now_wall_ms.saturating_add(MAX_WINDOW_SPAN_MS / 2);
        let (new_center, new_span) = zoom_window_around_anchor(
            self.view_center_ms,
            self.window_span_ms,
            anchor_epoch_ms,
            span_factor.clamp(0.05, 20.0),
        );
        self.view_center_ms = new_center.clamp(0, max_center_ms);
        self.window_span_ms = new_span;
        self.follow_playhead = false;
    }

    pub fn zoom_around_playhead_preserving_follow(
        &mut self,
        span_factor: f64,
        playhead_epoch_ms: i64,
    ) {
        let now_wall_ms = current_wall_ms();
        let max_center_ms = now_wall_ms.saturating_add(MAX_WINDOW_SPAN_MS / 2);
        let clamped_factor = span_factor.clamp(0.05, 20.0);
        if self.follow_playhead {
            let old_span = self.window_span_ms.clamp(MIN_WINDOW_SPAN_MS, MAX_WINDOW_SPAN_MS);
            let new_span = ((old_span as f64 * clamped_factor).round() as i64)
                .clamp(MIN_WINDOW_SPAN_MS, MAX_WINDOW_SPAN_MS);
            self.window_span_ms = new_span;
            self.view_center_ms = playhead_epoch_ms.clamp(0, max_center_ms);
            self.follow_playhead = true;
        } else {
            self.zoom_by_span_factor(clamped_factor, playhead_epoch_ms);
        }
    }

    pub fn open_time_jump_popover(&mut self, playhead_epoch_ms: i64) {
        let dt = epoch_ms_to_local_naive(playhead_epoch_ms);
        self.show_time_jump_popup = true;
        self.show_calendar_popup = false;
        self.time_jump_input = dt.format("%H:%M:%S").to_string();
        self.time_jump_error = None;
        self.time_jump_needs_focus = true;
    }

    pub fn is_any_popup_open(&self) -> bool {
        self.show_calendar_popup || self.show_time_jump_popup
    }

    pub fn is_actively_dragging(&self) -> bool {
        self.dragging_playhead || self.dragging_bracket.is_some() || self.panning_window
    }

    pub fn cancel_active_drags(&mut self) {
        self.dragging_playhead = false;
        self.dragging_bracket = None;
        self.panning_window = false;
        self.last_scrub_ms = None;
        self.last_emitted_scrub_ms = None;
    }

    pub fn set_clip_in(&mut self, epoch_ms: i64) {
        self.set_clip_in_clamped(epoch_ms, current_wall_ms());
    }

    pub fn set_clip_in_clamped(&mut self, epoch_ms: i64, now_wall_ms: i64) {
        let wall_cap = now_wall_ms.max(0);
        let clamped = epoch_ms.clamp(0, wall_cap.saturating_sub(500).max(0));
        self.clip_in_ms = Some(clamped);
        let auto_out = (clamped + 15_000).min(wall_cap).max(clamped + 500);
        match self.clip_out_ms {
            Some(out_ms) if out_ms <= clamped + 500 => {
                self.clip_out_ms = Some(auto_out);
            }
            Some(out_ms) if out_ms > wall_cap && wall_cap >= clamped + 500 => {
                self.clip_out_ms = Some(wall_cap);
            }
            None => {
                self.clip_out_ms = Some(auto_out);
            }
            _ => {}
        }
    }

    pub fn set_clip_out(&mut self, epoch_ms: i64) {
        self.set_clip_out_clamped(epoch_ms, current_wall_ms());
    }

    pub fn set_clip_out_clamped(&mut self, epoch_ms: i64, now_wall_ms: i64) {
        let wall_cap = now_wall_ms.max(0);
        let min_out = 500.min(wall_cap);
        let clamped = epoch_ms.clamp(min_out, wall_cap);
        self.clip_out_ms = Some(clamped);
        match self.clip_in_ms {
            Some(in_ms) if in_ms >= clamped.saturating_sub(500) => {
                self.clip_in_ms = Some((clamped - 15_000).max(0));
            }
            None => {
                self.clip_in_ms = Some((clamped - 15_000).max(0));
            }
            _ => {}
        }
    }

    pub fn clear_clip_range(&mut self) {
        self.clip_in_ms = None;
        self.clip_out_ms = None;
    }

    pub fn resolved_clip_range(&self, fallback_playhead_ms: i64) -> (i64, i64) {
        let wall_cap = current_wall_ms();
        match (self.clip_in_ms, self.clip_out_ms) {
            (Some(a), Some(b)) => {
                if b >= a + 500 {
                    (a, b)
                } else if a >= b + 500 {
                    (b, a)
                } else {
                    (a, (a + 15_000).min(wall_cap.max(a + 500)))
                }
            }
            (Some(a), None) => (a, (a + 15_000).min(wall_cap.max(a + 500))),
            (None, Some(b)) => ((b - 15_000).max(0), b),
            (None, None) => {
                let start = (fallback_playhead_ms - 10_000).max(0);
                let end = (fallback_playhead_ms + 20_000).min(wall_cap.max(start + 1_000));
                (start, end)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TimelineAction {
    ExitToLive,
    ToggleSync2x2,
    SelectChannel(usize),
    ChangeDate(NaiveDate),
    Seek { epoch_ms: i64, immediate: bool },
    TogglePause,
    StepFrame(i32),
    SetSpeed(f32),
    CycleSmartSkip,
    JumpPrevEvent,
    JumpNextEvent,
    ExportClip { in_ms: i64, out_ms: i64 },
    CancelClipExport,
    RefreshTimeline,
}

pub fn event_kind_color(kind: NvrEventKind) -> Color32 {
    match kind {
        NvrEventKind::Continuous => STATUS_EMERALD,
        NvrEventKind::Human => STATUS_ICE,
        NvrEventKind::Vehicle => STATUS_VIOLET,
        NvrEventKind::Intrusion | NvrEventKind::Alarm => STATUS_CORAL,
        NvrEventKind::Motion => STATUS_AMBER,
    }
}

pub fn event_kind_label(kind: NvrEventKind) -> &'static str {
    match kind {
        NvrEventKind::Continuous => "Continuous",
        NvrEventKind::Human => "Human",
        NvrEventKind::Vehicle => "Vehicle",
        NvrEventKind::Intrusion => "Intrusion",
        NvrEventKind::Alarm => "Alarm",
        NvrEventKind::Motion => "Motion",
    }
}

pub fn format_duration_compact(duration_ms: i64) -> String {
    let secs = (duration_ms.max(0) + 500) / 1000;
    if secs < 60 {
        format!("{}s", secs.max(1))
    } else if secs < 3600 {
        let m = secs / 60;
        let s = secs % 60;
        if s == 0 {
            format!("{}m", m)
        } else {
            format!("{}m {:02}s", m, s)
        }
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{}h {:02}m", h, m)
    }
}

pub fn format_span_badge(span_ms: i64) -> String {
    if span_ms >= 3_600_000 {
        let h = span_ms as f64 / 3_600_000.0;
        if (h - h.round()).abs() < 0.08 {
            format!("{}h", h.round() as i64)
        } else {
            format!("{:.1}h", h)
        }
    } else {
        let m = (span_ms as f64 / 60_000.0).max(1.0);
        if (m - m.round()).abs() < 0.08 {
            format!("{}m", m.round() as i64)
        } else {
            format!("{:.1}m", m)
        }
    }
}

/// Parses `HH:MM:SS` or `HH:MM` (with optional `.d` tenths) on `date` and clamps to `[0, now_wall_ms]`.
pub fn parse_time_jump_hms(
    input: &str,
    date: NaiveDate,
    now_wall_ms: i64,
) -> Result<i64, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Enter time as HH:MM:SS or HH:MM".to_string());
    }

    let (hms_part, millis) = if let Some((left, frac)) = trimmed.split_once('.') {
        if frac.is_empty() || frac.len() > 3 || !frac.bytes().all(|b| b.is_ascii_digit()) {
            return Err("Invalid fractional seconds (use HH:MM:SS)".to_string());
        }
        let scale = match frac.len() {
            1 => 100,
            2 => 10,
            _ => 1,
        };
        let m = frac
            .parse::<u32>()
            .map_err(|_| "Invalid fractional seconds".to_string())?
            * scale;
        (left, m)
    } else {
        (trimmed, 0)
    };

    let parts: Vec<&str> = hms_part.split(':').collect();
    if parts.len() != 2 && parts.len() != 3 {
        return Err("Use HH:MM:SS or HH:MM (00:00:00 - 23:59:59)".to_string());
    }
    for p in &parts {
        if p.is_empty() || p.len() > 2 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return Err("Use HH:MM:SS or HH:MM (00:00:00 - 23:59:59)".to_string());
        }
    }

    let hour: u32 = parts[0]
        .parse()
        .map_err(|_| "Invalid hour (00 - 23)".to_string())?;
    let min: u32 = parts[1]
        .parse()
        .map_err(|_| "Invalid minute (00 - 59)".to_string())?;
    let sec: u32 = if parts.len() == 3 {
        parts[2]
            .parse()
            .map_err(|_| "Invalid second (00 - 59)".to_string())?
    } else {
        0
    };

    if hour > 23 || min > 59 || sec > 59 {
        return Err("Time out of range (00:00:00 - 23:59:59)".to_string());
    }

    let Some(time) = NaiveTime::from_hms_milli_opt(hour, min, sec, millis) else {
        return Err("Invalid wall-clock time".to_string());
    };
    let dt = NaiveDateTime::new(date, time);
    let raw_ms = local_naive_to_epoch_ms(dt);
    Ok(raw_ms.clamp(0, now_wall_ms.max(0)))
}

/// Converts `epoch_ms` to screen X coordinate using `f64` math so large `1.79e12` timestamps
/// retain sub-millisecond precision even at `2m` (`120_000ms`) zoom.
#[inline]
pub fn epoch_ms_to_x(
    epoch_ms: i64,
    rect_min_x: f32,
    rect_width: f32,
    view_start_ms: i64,
    window_span_ms: i64,
) -> f32 {
    let span = window_span_ms.max(1) as f64;
    let frac = (epoch_ms - view_start_ms) as f64 / span;
    rect_min_x + (frac * rect_width.max(1.0) as f64) as f32
}

/// Converts screen X coordinate back to `epoch_ms` using `f64` math.
#[inline]
pub fn x_to_epoch_ms(
    x: f32,
    rect_min_x: f32,
    rect_width: f32,
    view_start_ms: i64,
    window_span_ms: i64,
) -> i64 {
    let w = rect_width.max(1.0) as f64;
    let frac = (((x - rect_min_x) as f64) / w).clamp(0.0, 1.0);
    view_start_ms + (frac * window_span_ms.max(1) as f64).round() as i64
}

/// Cursor-anchored zoom: `zoom_factor < 1.0` zooms IN (smaller time span), `zoom_factor > 1.0`
/// zooms OUT (larger time span), keeping `anchor_epoch_ms` pinned at the exact same pixel!
pub fn zoom_window_around_anchor(
    view_center_ms: i64,
    window_span_ms: i64,
    anchor_epoch_ms: i64,
    zoom_factor: f64,
) -> (i64, i64) {
    let old_span = window_span_ms.clamp(MIN_WINDOW_SPAN_MS, MAX_WINDOW_SPAN_MS);
    let old_start = view_center_ms - old_span / 2;
    let anchor_frac = ((anchor_epoch_ms - old_start) as f64 / old_span as f64).clamp(0.0, 1.0);

    let new_span = ((old_span as f64 * zoom_factor).round() as i64)
        .clamp(MIN_WINDOW_SPAN_MS, MAX_WINDOW_SPAN_MS);
    let new_start = anchor_epoch_ms - (anchor_frac * new_span as f64).round() as i64;
    let new_center = new_start + new_span / 2;
    (new_center, new_span)
}

/// Sub-pixel event dilation: clamps any visible event span to at least `min_width_px` (`3.5px`)
/// so even a 1-2s motion/human trigger remains bright, visible, and hoverable at 24h zoom.
pub fn compute_dilated_span_px(
    start_ms: i64,
    end_ms: i64,
    rect_min_x: f32,
    rect_max_x: f32,
    view_start_ms: i64,
    window_span_ms: i64,
    min_width_px: f32,
) -> Option<(f32, f32)> {
    let rect_max_x = rect_max_x.max(rect_min_x);
    let view_end_ms = view_start_ms.saturating_add(window_span_ms);
    let eff_end_ms = end_ms.max(start_ms.saturating_add(500));
    if eff_end_ms < view_start_ms || start_ms > view_end_ms {
        return None;
    }
    let width = (rect_max_x - rect_min_x).max(1.0);
    let raw_x0 = epoch_ms_to_x(start_ms, rect_min_x, width, view_start_ms, window_span_ms);
    let raw_x1 = epoch_ms_to_x(eff_end_ms, rect_min_x, width, view_start_ms, window_span_ms);

    let mut x0 = raw_x0.clamp(rect_min_x, rect_max_x);
    let mut x1 = raw_x1.clamp(rect_min_x, rect_max_x);
    let cur_w = x1 - x0;
    if cur_w < min_width_px {
        let mid = (0.5 * (raw_x0 + raw_x1)).clamp(rect_min_x, rect_max_x);
        let half = 0.5 * min_width_px;
        x0 = (mid - half).max(rect_min_x);
        x1 = (x0 + min_width_px).min(rect_max_x);
        if x1 - x0 < min_width_px {
            x0 = (x1 - min_width_px).max(rect_min_x);
        }
    }
    if x1 <= rect_min_x || x0 >= rect_max_x {
        None
    } else {
        Some((x0, x1))
    }
}

fn ruler_steps_for_span(span_ms: i64) -> (i64, i64, bool) {
    if span_ms > 43_200_000 {
        // 12h..24h -> major 2h, minor 30m
        (7_200_000, 1_800_000, false)
    } else if span_ms > 14_400_000 {
        // 4h..12h -> major 1h, minor 15m
        (3_600_000, 900_000, false)
    } else if span_ms > 5_400_000 {
        // 1.5h..4h -> major 30m, minor 5m
        (1_800_000, 300_000, false)
    } else if span_ms > 1_800_000 {
        // 30m..1.5h -> major 10m, minor 2m
        (600_000, 120_000, false)
    } else if span_ms > 600_000 {
        // 10m..30m -> major 5m, minor 1m
        (300_000, 60_000, false)
    } else if span_ms > 240_000 {
        // 4m..10m -> major 1m, minor 15s
        (60_000, 15_000, true)
    } else {
        // 2m..4m -> major 15s, minor 5s
        (15_000, 5_000, true)
    }
}

#[derive(Clone, Copy)]
enum ChipTone {
    Neutral,
    Ice,
    Emerald,
    Amber,
    Coral,
    Violet,
}

impl ChipTone {
    fn palette(self, active: bool, hovered: bool) -> (Color32, Color32, Color32) {
        if !active {
            let fill = if hovered { ZINC_HOVER } else { ZINC_ELEVATED };
            let stroke = if hovered {
                Color32::from_rgba_unmultiplied(255, 255, 255, 48)
            } else {
                hairline_stroke()
            };
            return (fill, stroke, TEXT_SECONDARY);
        }
        match self {
            Self::Neutral => (
                ZINC_HOVER,
                Color32::from_rgba_unmultiplied(255, 255, 255, 70),
                TEXT_PRIMARY,
            ),
            Self::Ice => (
                Color32::from_rgba_unmultiplied(56, 189, 248, 42),
                STATUS_ICE,
                TEXT_PRIMARY,
            ),
            Self::Emerald => (
                Color32::from_rgba_unmultiplied(16, 185, 129, 46),
                STATUS_EMERALD,
                TEXT_PRIMARY,
            ),
            Self::Amber => (
                Color32::from_rgba_unmultiplied(245, 158, 11, 44),
                STATUS_AMBER,
                Color32::from_rgb(254, 243, 199),
            ),
            Self::Coral => (
                Color32::from_rgba_unmultiplied(239, 68, 68, 44),
                STATUS_CORAL,
                Color32::from_rgb(254, 226, 226),
            ),
            Self::Violet => (
                Color32::from_rgba_unmultiplied(167, 139, 250, 44),
                STATUS_VIOLET,
                TEXT_PRIMARY,
            ),
        }
    }
}

fn draw_compact_pill(
    ui: &mut Ui,
    label: &str,
    active: bool,
    tone: ChipTone,
    height: f32,
    pad_x: f32,
) -> Response {
    draw_compact_pill_with_dot(ui, label, None, active, tone, height, pad_x)
}

fn draw_compact_pill_with_dot(
    ui: &mut Ui,
    label: &str,
    dot_col: Option<Color32>,
    active: bool,
    tone: ChipTone,
    height: f32,
    pad_x: f32,
) -> Response {
    let font = FontId::new(11.5, FontFamily::Proportional);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), font.clone(), TEXT_PRIMARY);
    let dot_extra = if dot_col.is_some() { 9.5 } else { 0.0 };
    let width = (galley.size().x + pad_x * 2.0 + dot_extra).max(22.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let (fill, stroke, text_col) = tone.palette(active, resp.hovered());
    let p = ui.painter();
    p.rect_filled(rect, Rounding::same(6.0), fill);
    p.rect_stroke(rect, Rounding::same(6.0), Stroke::new(1.0_f32, stroke));
    if let Some(dc) = dot_col {
        let content_w = dot_extra + galley.size().x;
        let start_x = rect.center().x - 0.5 * content_w;
        p.circle_filled(Pos2::new(start_x + 3.0, rect.center().y), 3.0, dc);
        p.text(
            Pos2::new(start_x + dot_extra, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            font,
            text_col,
        );
    } else {
        p.text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            font,
            text_col,
        );
    }
    resp
}

fn draw_capsule_group<R>(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::none()
        .fill(ZINC_ELEVATED)
        .rounding(Rounding::same(6.0))
        .stroke(Stroke::new(1.0_f32, hairline_stroke()))
        .inner_margin(egui::Margin::symmetric(2.0, 2.0))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            add_contents(ui)
        })
        .inner
}

fn draw_capsule_item(ui: &mut Ui, label: &str, selected: bool, tone: ChipTone) -> Response {
    draw_capsule_item_ex(ui, label, None, selected, true, tone)
}

fn draw_capsule_item_enabled(
    ui: &mut Ui,
    label: &str,
    selected: bool,
    enabled: bool,
    tone: ChipTone,
) -> Response {
    draw_capsule_item_ex(ui, label, None, selected, enabled, tone)
}

fn draw_capsule_item_with_dot(
    ui: &mut Ui,
    label: &str,
    dot_col: Option<Color32>,
    selected: bool,
    tone: ChipTone,
) -> Response {
    draw_capsule_item_ex(ui, label, dot_col, selected, true, tone)
}

fn draw_capsule_item_ex(
    ui: &mut Ui,
    label: &str,
    dot_col: Option<Color32>,
    selected: bool,
    enabled: bool,
    tone: ChipTone,
) -> Response {
    let font = FontId::new(11.0, FontFamily::Proportional);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), font.clone(), TEXT_PRIMARY);
    let dot_extra = if dot_col.is_some() { 9.0 } else { 0.0 };
    let width = (galley.size().x + 11.0 + dot_extra).max(20.0);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 22.0), sense);
    if enabled && resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }

    let p = ui.painter();
    let text_col = if !enabled {
        ZINC_DIM
    } else if selected {
        let (fill, stroke, tc) = tone.palette(true, resp.hovered());
        p.rect_filled(rect, Rounding::same(4.5), fill);
        p.rect_stroke(rect, Rounding::same(4.5), Stroke::new(1.0_f32, stroke));
        tc
    } else {
        if resp.hovered() {
            p.rect_filled(rect, Rounding::same(4.5), ZINC_HOVER);
        }
        if resp.hovered() {
            TEXT_PRIMARY
        } else {
            TEXT_MUTED
        }
    };

    if let Some(dc) = dot_col {
        let content_w = dot_extra + galley.size().x;
        let start_x = rect.center().x - 0.5 * content_w;
        p.circle_filled(Pos2::new(start_x + 3.0, rect.center().y), 3.0, dc);
        p.text(
            Pos2::new(start_x + dot_extra, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            font,
            text_col,
        );
    } else {
        p.text(rect.center(), Align2::CENTER_CENTER, label, font, text_col);
    }

    resp
}

fn draw_export_progress_capsule_item(
    ui: &mut Ui,
    label: &str,
    progress_frac: f32,
) -> Response {
    let font = FontId::new(11.0, FontFamily::Proportional);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), font.clone(), TEXT_PRIMARY);
    let width = (galley.size().x + 13.0).max(58.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 22.0), Sense::hover());

    let p = ui.painter();
    p.rect_filled(
        rect,
        Rounding::same(4.5),
        Color32::from_rgba_unmultiplied(245, 158, 11, 24),
    );
    let fill_w = (rect.width() * progress_frac.clamp(0.03, 1.0)).clamp(2.0, rect.width().max(2.0));
    let prog_rect = Rect::from_min_size(rect.min, Vec2::new(fill_w, rect.height()));
    p.rect_filled(
        prog_rect,
        Rounding::same(4.5),
        Color32::from_rgba_unmultiplied(245, 158, 11, 88),
    );
    p.rect_stroke(rect, Rounding::same(4.5), Stroke::new(1.0_f32, STATUS_AMBER));
    p.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        font,
        Color32::from_rgb(254, 243, 199),
    );
    resp
}

/// Renders the `98px` Obsidian Studio Bottom Timeline Dock either docked (`TopBottomPanel::bottom`)
/// or as a zero-relayout `Order::Foreground` floating overlay (`floating_overlay == true`).
pub fn render_timeline_dock(
    ctx: &egui::Context,
    state: &mut TimelineUiState,
    playback: &PlaybackManager,
    app_cfg: &AppConfig,
    is_cloud_p2p: bool,
    floating_overlay: bool,
    frame_pinch: PinchFrameSnapshot,
) -> Vec<TimelineAction> {
    let mut actions = Vec::new();
    let dock_frame = egui::Frame::none()
        .fill(GRAPHITE_SURFACE)
        .stroke(Stroke::new(1.0_f32, hairline_stroke()))
        .inner_margin(egui::Margin {
            left: 10.0,
            right: 10.0,
            top: 4.0,
            bottom: 4.0,
        });

    if !floating_overlay {
        egui::TopBottomPanel::bottom("obsidian_playback_timeline_dock")
            .exact_height(TIMELINE_DOCK_HEIGHT)
            .frame(dock_frame)
            .show(ctx, |ui| {
                actions = render_timeline_dock_ui(
                    ui,
                    state,
                    playback,
                    app_cfg,
                    is_cloud_p2p,
                    frame_pinch,
                );
            });
    } else {
        let screen = ctx.screen_rect();
        let avail_x = ctx.available_rect();
        let dock_y = (screen.max.y - TIMELINE_DOCK_HEIGHT).max(screen.min.y);
        let outer_rect = Rect::from_min_size(
            Pos2::new(avail_x.min.x, dock_y),
            Vec2::new(avail_x.width().max(120.0), TIMELINE_DOCK_HEIGHT),
        );
        egui::Area::new(egui::Id::new("obsidian_playback_timeline_dock_float"))
            .order(Order::Foreground)
            .fixed_pos(outer_rect.min)
            .show(ctx, |ui| {
                let (allocated_outer, _) =
                    ui.allocate_exact_size(outer_rect.size(), Sense::hover());
                let p = ui.painter();
                p.rect_filled(
                    allocated_outer,
                    Rounding::ZERO,
                    Color32::from_rgba_unmultiplied(17, 17, 20, 248),
                );
                p.rect_stroke(
                    allocated_outer,
                    Rounding::ZERO,
                    Stroke::new(1.0_f32, hairline_stroke()),
                );
                let inner_rect = Rect::from_min_max(
                    Pos2::new(allocated_outer.min.x + 10.0, allocated_outer.min.y + 4.0),
                    Pos2::new(
                        (allocated_outer.max.x - 10.0).max(allocated_outer.min.x + 12.0),
                        (allocated_outer.max.y - 4.0).max(allocated_outer.min.y + 12.0),
                    ),
                );
                ui.allocate_new_ui(egui::UiBuilder::new().max_rect(inner_rect), |ui| {
                    actions = render_timeline_dock_ui(
                        ui,
                        state,
                        playback,
                        app_cfg,
                        is_cloud_p2p,
                        frame_pinch,
                    );
                });
            });
    }

    actions
}

/// Renders the contents of the `98px` Timeline Dock inside an existing `Ui`.
pub fn render_timeline_dock_ui(
    ui: &mut Ui,
    state: &mut TimelineUiState,
    playback: &PlaybackManager,
    app_cfg: &AppConfig,
    is_cloud_p2p: bool,
    frame_pinch: PinchFrameSnapshot,
) -> Vec<TimelineAction> {
    let mut actions = Vec::new();
    let now_wall_ms = current_wall_ms();
    let playhead_ms = playback.playhead_epoch_ms();
    let is_paused = playback.is_paused();
    let is_playing = playback.is_active() && !is_paused;
    state.sync_playhead(playhead_ms, is_playing);

    let primary_ch = playback.primary_channel().unwrap_or(1);
    let primary_cam_name = app_cfg.camera_display_name(primary_ch);
    let total_cams = app_cfg.num_cameras.max(1).max(primary_ch);
    let timeline = playback.day_timeline(primary_ch, state.selected_date);
    let timeline_loading = playback.is_timeline_loading(primary_ch, state.selected_date);

    let full_rect = ui.max_rect();
    let top_row_rect =
        Rect::from_min_size(full_rect.min, Vec2::new(full_rect.width().max(1.0), 36.0));
    let canvas_min = Pos2::new(full_rect.min.x, top_row_rect.max.y + 4.0);
    let canvas_rect = Rect::from_min_max(
        canvas_min,
        Pos2::new(
            full_rect.max.x.max(canvas_min.x + 1.0),
            full_rect.max.y.max(canvas_min.y + 1.0),
        ),
    );

    let mut calendar_anchor_rect: Option<Rect> = None;
    let mut clock_anchor_rect: Option<Rect> = None;
    let avail_w = top_row_rect.width();

    // =========================================================================
    // TOP ROW (36px band): Badge + Clock + Calendar + Cam + Mode | Transport | Filters + Export + Live
    // =========================================================================
    let mut left_end_x = top_row_rect.min.x;
    ui.allocate_new_ui(
        egui::UiBuilder::new()
            .max_rect(top_row_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
        |ui| {
            ui.spacing_mut().item_spacing.x = if avail_w < 1220.0 { 4.0 } else { 5.0 };

            // 1. Dynamic Playback Status Badge (LOAD.. / PAUSED / NO REC / ERROR / HISTORY)
            let slot_status = playback
                .slot(primary_ch)
                .map(|s| s.status())
                .unwrap_or(PlaybackStatus::Idle);
            let slot_fps = playback
                .slot(primary_ch)
                .map(|s| s.fps_x10.load(std::sync::atomic::Ordering::Relaxed) as f32 / 10.0)
                .unwrap_or(0.0);
            let slot_err = playback
                .slot(primary_ch)
                .and_then(|s| s.last_error.lock().ok().and_then(|g| g.clone()));

            let show_history_text = avail_w >= 960.0;
            let badge_w = if show_history_text { 76.0 } else { 22.0 };
            let (badge_rect, badge_resp) =
                ui.allocate_exact_size(Vec2::new(badge_w, 26.0), Sense::hover());
            {
                let (badge_txt, base_col, text_col, should_pulse) = match slot_status {
                    PlaybackStatus::Buffering => (
                        "LOAD..",
                        STATUS_ICE,
                        Color32::from_rgb(224, 242, 254),
                        true,
                    ),
                    PlaybackStatus::Paused => (
                        "PAUSED",
                        STATUS_AMBER,
                        Color32::from_rgb(254, 243, 199),
                        false,
                    ),
                    PlaybackStatus::EndOfRecording => ("NO REC", ZINC_DIM, TEXT_SECONDARY, false),
                    PlaybackStatus::Error | PlaybackStatus::AuthError => (
                        "ERROR",
                        STATUS_CORAL,
                        Color32::from_rgb(254, 226, 226),
                        false,
                    ),
                    PlaybackStatus::Playing | PlaybackStatus::Idle => (
                        "HISTORY",
                        STATUS_AMBER,
                        Color32::from_rgb(254, 243, 199),
                        slot_status == PlaybackStatus::Playing,
                    ),
                };
                if slot_status == PlaybackStatus::Buffering {
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(90));
                }
                let pulse = if should_pulse {
                    ((ui.input(|i| i.time) * 4.2).sin() as f32 * 0.5 + 0.5).clamp(0.0, 1.0)
                } else {
                    0.55
                };
                let fill_alpha = (28.0 + pulse * 26.0) as u8;
                let fill = Color32::from_rgba_unmultiplied(
                    base_col.r(),
                    base_col.g(),
                    base_col.b(),
                    fill_alpha,
                );
                let p = ui.painter();
                p.rect_filled(badge_rect, Rounding::same(6.0), fill);
                p.rect_stroke(
                    badge_rect,
                    Rounding::same(6.0),
                    Stroke::new(1.0_f32, base_col),
                );
                let dot_x = if show_history_text {
                    badge_rect.min.x + 10.0
                } else {
                    badge_rect.center().x
                };
                p.circle_filled(Pos2::new(dot_x, badge_rect.center().y), 3.5, base_col);
                if show_history_text {
                    p.text(
                        Pos2::new(badge_rect.min.x + 18.0, badge_rect.center().y),
                        Align2::LEFT_CENTER,
                        badge_txt,
                        FontId::new(10.5, FontFamily::Proportional),
                        text_col,
                    );
                }
            }
            let mut status_tip = format!(
                "Ch {:02} - {}\nStatus: {} ({:.1} fps)",
                primary_ch,
                primary_cam_name,
                slot_status.label(),
                slot_fps
            );
            if let Some(err) = slot_err {
                status_tip.push_str("\nNote: ");
                status_tip.push_str(&err);
            }
            badge_resp.on_hover_text(status_tip);

            // 2. Interactive Digital Playhead Clock (Click -> Direct Time Jump Popover!)
            let dt_now = epoch_ms_to_local_naive(playhead_ms);
            let tenths = (playhead_ms.rem_euclid(1000)) / 100;
            let (clock_str, clock_w) = if avail_w >= 1560.0 {
                (
                    format!(
                        "{}  {}.{}",
                        dt_now.format("%Y-%m-%d"),
                        dt_now.format("%H:%M:%S"),
                        tenths
                    ),
                    152.0,
                )
            } else if avail_w >= 920.0 {
                (format!("{}.{}", dt_now.format("%H:%M:%S"), tenths), 80.0)
            } else {
                (format!("{}", dt_now.format("%H:%M:%S")), 66.0)
            };
            let (clock_rect, clock_resp) =
                ui.allocate_exact_size(Vec2::new(clock_w, 26.0), Sense::click());
            if clock_resp.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            {
                let p = ui.painter();
                let active_or_hov = state.show_time_jump_popup || clock_resp.hovered();
                let fill = if state.show_time_jump_popup {
                    Color32::from_rgba_unmultiplied(56, 189, 248, 30)
                } else if clock_resp.hovered() {
                    ZINC_ELEVATED
                } else {
                    OBSIDIAN_BG
                };
                let stroke_col = if active_or_hov {
                    STATUS_ICE
                } else {
                    hairline_stroke()
                };
                p.rect_filled(clock_rect, Rounding::same(6.0), fill);
                p.rect_stroke(
                    clock_rect,
                    Rounding::same(6.0),
                    Stroke::new(1.0_f32, stroke_col),
                );
                p.text(
                    clock_rect.center(),
                    Align2::CENTER_CENTER,
                    &clock_str,
                    FontId::new(11.5, FontFamily::Monospace),
                    TEXT_PRIMARY,
                );
            }
            clock_anchor_rect = Some(clock_rect);
            if clock_resp
                .on_hover_text("Playhead timestamp (Click to jump to HH:MM:SS or quick-jump)")
                .clicked()
            {
                if state.show_time_jump_popup {
                    state.show_time_jump_popup = false;
                } else {
                    state.open_time_jump_popover(playhead_ms);
                }
            }

            // 3. Calendar Pill (< Nov 18 > or compact Nov 18 below 920px)
            let show_day_step_arrows = avail_w >= 920.0;
            let cal_resp = draw_capsule_group(ui, |ui| {
                if show_day_step_arrows
                    && draw_capsule_item(ui, "<", false, ChipTone::Neutral)
                        .on_hover_text("Previous Day")
                        .clicked()
                {
                    if let Some(prev_d) = state.selected_date.pred_opt() {
                        state.selected_date = prev_d;
                        state.calendar_ym = (prev_d.year(), prev_d.month());
                        state.calendar_cursor_date = Some(prev_d);
                        state.clear_clip_range();
                        actions.push(TimelineAction::ChangeDate(prev_d));
                    }
                }
                let date_lbl = state.selected_date.format("%b %d").to_string();
                let mid = draw_capsule_item(
                    ui,
                    &date_lbl,
                    state.show_calendar_popup,
                    ChipTone::Amber,
                )
                .on_hover_text("Open Calendar (green dots show recorded days)");
                if mid.clicked() {
                    state.show_calendar_popup = !state.show_calendar_popup;
                    if state.show_calendar_popup {
                        state.show_time_jump_popup = false;
                        state.calendar_ym =
                            (state.selected_date.year(), state.selected_date.month());
                        state.calendar_cursor_date = Some(state.selected_date);
                    }
                }
                if show_day_step_arrows {
                    let today = Local::now().date_naive();
                    let can_next = state.selected_date < today;
                    let next_tip = if can_next {
                        "Next Day"
                    } else {
                        "Already viewing today"
                    };
                    let next_btn =
                        draw_capsule_item_enabled(ui, ">", false, can_next, ChipTone::Neutral)
                            .on_hover_text(next_tip);
                    if can_next && next_btn.clicked() {
                        if let Some(next_d) = state.selected_date.succ_opt() {
                            state.selected_date = next_d;
                            state.calendar_ym = (next_d.year(), next_d.month());
                            state.calendar_cursor_date = Some(next_d);
                            state.clear_clip_range();
                            actions.push(TimelineAction::ChangeDate(next_d));
                        }
                    }
                }
                mid.rect
            });
            calendar_anchor_rect = Some(cal_resp);

            // 4. Compact Camera Selector Dropdown Pill (Upward Popup + Custom Camera Names)
            let cam_popup_id = ui.make_persistent_id("obsidian_pb_cam_selector_popup");
            let cam_lbl = format!("Ch {} v", primary_ch);
            let cam_popup_open = ui.memory(|m| m.is_popup_open(cam_popup_id));
            let cam_btn =
                draw_compact_pill(ui, &cam_lbl, cam_popup_open, ChipTone::Ice, 26.0, 7.0);
            if cam_btn.clicked() {
                ui.memory_mut(|m| m.toggle_popup(cam_popup_id));
            }
            egui::popup_above_or_below_widget(
                ui,
                cam_popup_id,
                &cam_btn,
                egui::AboveOrBelow::Above,
                egui::PopupCloseBehavior::CloseOnClickOutside,
                |ui| {
                    ui.set_min_width(168.0);
                    egui::ScrollArea::vertical()
                        .max_height(250.0)
                        .show(ui, |ui| {
                            let sync_active = playback.is_sync_2x2();
                            let active_chs = playback.active_channels();
                            for ch in 1..=total_cams {
                                let in_sync = sync_active && active_chs.contains(&ch);
                                let disp = app_cfg.camera_display_name(ch);
                                let item_lbl = if in_sync {
                                    format!("Ch {:02} - {} (Sync)", ch, disp)
                                } else {
                                    format!("Ch {:02} - {}", ch, disp)
                                };
                                if ui.selectable_label(ch == primary_ch, item_lbl).clicked() {
                                    state.auto_scroll_to_active_event = true;
                                    actions.push(TimelineAction::SelectChannel(ch));
                                    ui.memory_mut(|m| m.close_popup());
                                }
                            }
                        });
                },
            );
            cam_btn.on_hover_text(format!(
                "Active: Ch {:02} - {}\nClick to switch playback camera (1..{})",
                primary_ch, primary_cam_name, total_cams
            ));

            // 5. Mode Toggle ([1-Cam 1080p | 2x2 Sync]) when not on Cloud P2P
            if !is_cloud_p2p {
                let sync_on = playback.is_sync_2x2();
                draw_capsule_group(ui, |ui| {
                    let single_lbl = if avail_w >= 1680.0 {
                        "1-Cam 1080p"
                    } else if avail_w >= 1020.0 {
                        "1-Cam"
                    } else {
                        "1x"
                    };
                    if draw_capsule_item(ui, single_lbl, !sync_on, ChipTone::Ice)
                        .on_hover_text(
                            "Single-Camera 1920x1080 Playback (crisp 8x Box Zoom + Live PiP)",
                        )
                        .clicked()
                        && sync_on
                    {
                        actions.push(TimelineAction::ToggleSync2x2);
                    }
                    let sync_lbl = if avail_w >= 1680.0 {
                        "2x2 Sync"
                    } else if avail_w >= 1020.0 {
                        "2x2"
                    } else {
                        "2x"
                    };
                    if draw_capsule_item(ui, sync_lbl, sync_on, ChipTone::Ice)
                        .on_hover_text("4-Camera 2x2 Synchronized 960x540 Playback")
                        .clicked()
                        && !sync_on
                    {
                        actions.push(TimelineAction::ToggleSync2x2);
                    }
                });

                // Inline C1..C4 quick-focus chips on wide screens when in 2x2 Sync
                if sync_on && playback.active_channels().len() > 1 && avail_w >= 1780.0 {
                    draw_capsule_group(ui, |ui| {
                        for &ch in playback.active_channels() {
                            let lbl = format!("C{}", ch);
                            let ch_name = app_cfg.camera_display_name(ch);
                            if draw_capsule_item(ui, &lbl, ch == primary_ch, ChipTone::Ice)
                                .on_hover_text(format!(
                                    "Focus timeline events on Ch {:02} - {}",
                                    ch, ch_name
                                ))
                                .clicked()
                            {
                                state.auto_scroll_to_active_event = true;
                                actions.push(TimelineAction::SelectChannel(ch));
                            }
                        }
                    });
                }
            } else if avail_w >= 1680.0 {
                let _ = draw_compact_pill(ui, "P2P 1-Cam", false, ChipTone::Ice, 24.0, 7.0)
                    .on_hover_text(
                        "Cloud P2P Relay active: capped at 1 stream by NvrResourceGovernor",
                    );
            }

            // 6. Quick-Jump Rewind Pills (shown when >= 1340px so 1280px never crowds!) & Transport
            if avail_w >= 1340.0 {
                draw_capsule_group(ui, |ui| {
                    for (lbl, delta_ms, tip) in [
                        ("-5m", -300_000_i64, "Rewind 5 minutes"),
                        ("-1m", -60_000_i64, "Rewind 1 minute"),
                        ("-15s", -15_000_i64, "Rewind 15 seconds"),
                    ] {
                        if draw_capsule_item(ui, lbl, false, ChipTone::Neutral)
                            .on_hover_text(tip)
                            .clicked()
                        {
                            actions.push(TimelineAction::Seek {
                                epoch_ms: (playhead_ms + delta_ms).clamp(0, now_wall_ms),
                                immediate: true,
                            });
                        }
                    }
                });
            }

            draw_capsule_group(ui, |ui| {
                let prev_evt_lbl = if avail_w >= 1300.0 { "< Evt" } else { "<E" };
                if draw_capsule_item(ui, prev_evt_lbl, false, ChipTone::Ice)
                    .on_hover_text("Jump to Previous Event ([)")
                    .clicked()
                {
                    state.auto_scroll_to_active_event = true;
                    actions.push(TimelineAction::JumpPrevEvent);
                }
                if avail_w >= 860.0 {
                    let step_back_lbl = if avail_w >= 1300.0 { "|< ," } else { "," };
                    if draw_capsule_item(ui, step_back_lbl, false, ChipTone::Neutral)
                        .on_hover_text("Step Back 1 Decoded Frame (,) - 0ms RAM ring buffer")
                        .clicked()
                    {
                        actions.push(TimelineAction::StepFrame(-1));
                    }
                }
                let play_lbl = if is_paused { "Play" } else { "Pause" };
                let play_tone = if is_paused {
                    ChipTone::Amber
                } else {
                    ChipTone::Emerald
                };
                if draw_capsule_item(ui, play_lbl, true, play_tone)
                    .on_hover_text("Play / Pause Playback (Space / K / P)")
                    .clicked()
                {
                    actions.push(TimelineAction::TogglePause);
                }
                if avail_w >= 860.0 {
                    let step_fwd_lbl = if avail_w >= 1300.0 { ". >|" } else { "." };
                    if draw_capsule_item(ui, step_fwd_lbl, false, ChipTone::Neutral)
                        .on_hover_text("Step Forward 1 Decoded Frame (.) - 0ms RAM ring buffer")
                        .clicked()
                    {
                        actions.push(TimelineAction::StepFrame(1));
                    }
                }
                let next_evt_lbl = if avail_w >= 1300.0 { "Evt >" } else { "E>" };
                if draw_capsule_item(ui, next_evt_lbl, false, ChipTone::Ice)
                    .on_hover_text("Jump to Next Event (])")
                    .clicked()
                {
                    state.auto_scroll_to_active_event = true;
                    actions.push(TimelineAction::JumpNextEvent);
                }
            });

            // 7. Speed Selector (inline segmented on ultra-wide >= 1960px, upward popup capsule otherwise)
            let cur_speed = playback.speed();
            let eff_speed = playback.effective_speed();
            if avail_w >= 1960.0 {
                draw_capsule_group(ui, |ui| {
                    for (sp, lbl) in [
                        (0.5_f32, "0.5x"),
                        (1.0, "1x"),
                        (2.0, "2x"),
                        (4.0, "4x"),
                        (8.0, "8x"),
                        (16.0, "16x"),
                    ] {
                        let sel = (cur_speed - sp).abs() < 0.05 && cur_speed > 0.0;
                        if draw_capsule_item(ui, lbl, sel, ChipTone::Ice)
                            .on_hover_text(format!("Set playback speed to {}", lbl))
                            .clicked()
                        {
                            actions.push(TimelineAction::SetSpeed(sp));
                        }
                    }
                    let is_rev = cur_speed < 0.0;
                    let rev_lbl = if is_rev {
                        format!("{:.0}x", cur_speed)
                    } else {
                        "Rev".to_string()
                    };
                    if draw_capsule_item(ui, &rev_lbl, is_rev, ChipTone::Violet)
                        .on_hover_text("Reverse keyframe playback (-1x -> -2x -> -4x)")
                        .clicked()
                    {
                        let next_rev = if (cur_speed - (-1.0)).abs() < 0.05 {
                            -2.0
                        } else if (cur_speed - (-2.0)).abs() < 0.05 {
                            -4.0
                        } else {
                            -1.0
                        };
                        actions.push(TimelineAction::SetSpeed(next_rev));
                    }
                });
            } else {
                let speed_popup_id = ui.make_persistent_id("obsidian_pb_speed_popup");
                let sp_txt = if (eff_speed - cur_speed).abs() > 0.1 && avail_w >= 1120.0 {
                    format!("{:.1}x->{}x v", cur_speed, eff_speed as i32)
                } else if (cur_speed - cur_speed.round()).abs() < 0.05 {
                    format!("{:.0}x v", cur_speed)
                } else {
                    format!("{:.1}x v", cur_speed)
                };
                let sp_btn = draw_compact_pill(
                    ui,
                    &sp_txt,
                    (cur_speed - 1.0).abs() > 0.05,
                    if cur_speed < 0.0 {
                        ChipTone::Violet
                    } else {
                        ChipTone::Ice
                    },
                    26.0,
                    7.0,
                );
                if sp_btn.clicked() {
                    ui.memory_mut(|m| m.toggle_popup(speed_popup_id));
                }
                egui::popup_above_or_below_widget(
                    ui,
                    speed_popup_id,
                    &sp_btn,
                    egui::AboveOrBelow::Above,
                    egui::PopupCloseBehavior::CloseOnClickOutside,
                    |ui| {
                        ui.set_min_width(108.0);
                        for (sp, lbl) in [
                            (0.5_f32, "0.5x Slow"),
                            (1.0, "1.0x Normal"),
                            (2.0, "2.0x Fast"),
                            (4.0, "4.0x Fast"),
                            (8.0, "8.0x Keyframe"),
                            (16.0, "16.0x Turbo"),
                            (-1.0, "-1.0x Reverse"),
                            (-2.0, "-2.0x Reverse"),
                            (-4.0, "-4.0x Reverse"),
                        ] {
                            let sel = (cur_speed - sp).abs() < 0.05;
                            if ui.selectable_label(sel, lbl).clicked() {
                                actions.push(TimelineAction::SetSpeed(sp));
                                ui.memory_mut(|m| m.close_popup());
                            }
                        }
                    },
                );
                sp_btn.on_hover_text("Playback Speed (0.5x..16x + Reverse -1x..-4x)");
            }

            left_end_x = ui.min_rect().max.x;
        },
    );

    // 8. Right-Aligned Cluster bounded strictly to `right_rect` with dynamic `ui.available_width()` checks!
    let right_min_x = (left_end_x + 6.0).min(top_row_rect.max.x);
    let right_rect = Rect::from_min_max(
        Pos2::new(right_min_x, top_row_rect.min.y),
        top_row_rect.max,
    );

    ui.allocate_new_ui(
        egui::UiBuilder::new()
            .max_rect(right_rect)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
        |ui| {
            ui.set_clip_rect(right_rect.intersect(ui.clip_rect()));
            ui.spacing_mut().item_spacing.x = if avail_w < 1020.0 { 4.0 } else { 5.0 };

            // 8a. Prominent Emerald [Live ->] Exit Button (always shown)
            let live_lbl = if avail_w >= 960.0 { "Live ->" } else { "Live" };
            let live_pad = if avail_w >= 960.0 { 9.0 } else { 7.0 };
            if draw_compact_pill(ui, live_lbl, true, ChipTone::Emerald, 26.0, live_pad)
                .on_hover_text("Exit History & Return Instantly to Live Wall (0ms - Esc)")
                .clicked()
            {
                actions.push(TimelineAction::ExitToLive);
            }

            // 8b. [Events (N)] Drawer Toggle (always shown)
            let filtered_count = timeline
                .as_ref()
                .map(|tl| {
                    tl.events
                        .iter()
                        .filter(|ev| matches_event_filter(ev.kind, state.event_filter))
                        .count()
                })
                .unwrap_or(0);
            let ev_label = if avail_w < 1060.0 {
                if timeline_loading {
                    "Evt(..)".to_string()
                } else {
                    format!("Evt({})", filtered_count)
                }
            } else if timeline_loading {
                "Events (...)".to_string()
            } else {
                format!("Events ({})", filtered_count)
            };
            if draw_compact_pill(
                ui,
                &ev_label,
                state.show_event_drawer,
                ChipTone::Ice,
                26.0,
                if avail_w < 960.0 { 6.0 } else { 8.0 },
            )
            .on_hover_text("Toggle 'What Happened' Event Feed Drawer (E)")
            .clicked()
            {
                state.show_event_drawer = !state.show_event_drawer;
                if state.show_event_drawer {
                    state.auto_scroll_to_active_event = true;
                }
            }

            // 8c. Clip Trim & Export Capsule ([In] [Out] [x?] [Export 2m 15s / Export 45% + X]) (always shown)
            let exp_state = playback.export_state();
            let has_clip_bounds = state.clip_in_ms.is_some() || state.clip_out_ms.is_some();
            draw_capsule_group(ui, |ui| {
                if exp_state.in_progress {
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(120));
                    if draw_capsule_item(ui, "X", true, ChipTone::Coral)
                        .on_hover_text("Cancel active MP4 clip export")
                        .clicked()
                    {
                        actions.push(TimelineAction::CancelClipExport);
                    }
                    let pct = (exp_state.progress_frac * 100.0).clamp(1.0, 99.0);
                    let prog_lbl = if avail_w < 920.0 {
                        format!("{:.0}%", pct)
                    } else {
                        format!("Export {:.0}%", pct)
                    };
                    let mb = exp_state.bytes_written as f64 / (1024.0 * 1024.0);
                    let stage_txt = if exp_state.stage.is_empty() {
                        "Exporting MP4..."
                    } else {
                        exp_state.stage.as_str()
                    };
                    draw_export_progress_capsule_item(ui, &prog_lbl, exp_state.progress_frac)
                        .on_hover_text(format!(
                            "{}\nWritten: {:.2} MB ({:.0}%)",
                            stage_txt, mb, pct
                        ));
                } else {
                    let (in_ms, out_ms) = state.resolved_clip_range(playhead_ms);
                    let raw_dur = (out_ms.abs_diff(in_ms).min(i64::MAX as u64)) as i64;
                    let over_cap = has_clip_bounds && raw_dur > MAX_CLIP_EXPORT_MS;
                    let capped_dur = raw_dur.min(MAX_CLIP_EXPORT_MS);

                    let exp_lbl = if has_clip_bounds {
                        if over_cap {
                            "Export 2h MAX".to_string()
                        } else {
                            format!("Export {}", format_duration_compact(capped_dur))
                        }
                    } else if avail_w < 880.0 {
                        "Clip".to_string()
                    } else {
                        "Export MP4".to_string()
                    };
                    let exp_tone = if over_cap {
                        ChipTone::Amber
                    } else {
                        ChipTone::Emerald
                    };
                    let exp_tip = if has_clip_bounds {
                        if over_cap {
                            format!(
                                "Selected range ({}) exceeds the 2-hour single-clip cap.\nClick to export the first 2h 00m from [In] to {} (-c:v copy +faststart, Ctrl+E).",
                                format_duration_compact(raw_dur),
                                app_cfg.recording_dir
                            )
                        } else {
                            format!(
                                "Export {} MP4 clip [{} -> {}] to {} (-c:v copy +faststart, Ctrl+E)",
                                format_duration_compact(capped_dur),
                                epoch_ms_to_local_naive(in_ms).format("%H:%M:%S"),
                                epoch_ms_to_local_naive(out_ms).format("%H:%M:%S"),
                                app_cfg.recording_dir
                            )
                        }
                    } else {
                        format!(
                            "Export 30s MP4 clip around playhead (-10s..+20s) to {}, or mark [In] / [Out] first (Ctrl+E)",
                            app_cfg.recording_dir
                        )
                    };

                    if draw_capsule_item(ui, &exp_lbl, has_clip_bounds, exp_tone)
                        .on_hover_text(exp_tip)
                        .clicked()
                    {
                        actions.push(TimelineAction::ExportClip { in_ms, out_ms });
                    }
                    if has_clip_bounds
                        && draw_capsule_item(ui, "x", false, ChipTone::Neutral)
                            .on_hover_text("Clear In/Out clip trim handles")
                            .clicked()
                    {
                        state.clear_clip_range();
                    }
                }

                if draw_capsule_item(ui, "Out", state.clip_out_ms.is_some(), ChipTone::Ice)
                    .on_hover_text("Mark Clip Out-Point at Playhead (O)")
                    .clicked()
                {
                    state.set_clip_out(playhead_ms);
                }
                if draw_capsule_item(ui, "In", state.clip_in_ms.is_some(), ChipTone::Ice)
                    .on_hover_text("Mark Clip In-Point at Playhead (I)")
                    .clicked()
                {
                    state.set_clip_in(playhead_ms);
                }
            });

            // 8d. Optional [PiP] restore pill when Live PiP is hidden in 1-Cam LAN/Tailscale mode
            if state.pip_corner == LivePipCorner::Hidden
                && !is_cloud_p2p
                && !playback.is_sync_2x2()
                && ui.available_width() >= 52.0
                && draw_compact_pill(ui, "PiP", false, ChipTone::Emerald, 26.0, 7.0)
                    .on_hover_text("Restore Live Picture-in-Picture corner window")
                    .clicked()
            {
                state.pip_corner = LivePipCorner::TopRight;
            }

            // 8e. Optional Smart Skip Pill (never bare "Off": always "Smart: ..." or "Skip: ...")
            if ui.available_width() >= 96.0 {
                let skip_mode = playback.smart_skip_mode();
                let skip_lbl = if ui.available_width() >= 150.0 && avail_w >= 1460.0 {
                    format!("Smart: {}", skip_mode.label())
                } else {
                    match skip_mode {
                        SmartSkipMode::Off => "Skip: Off".to_string(),
                        SmartSkipMode::FastGlide8x => "Skip: 8x".to_string(),
                        SmartSkipMode::InstantJump => "Skip: Jump".to_string(),
                    }
                };
                let skip_tone = match skip_mode {
                    SmartSkipMode::Off => ChipTone::Neutral,
                    SmartSkipMode::FastGlide8x => ChipTone::Amber,
                    SmartSkipMode::InstantJump => ChipTone::Ice,
                };
                if draw_compact_pill(
                    ui,
                    &skip_lbl,
                    skip_mode != SmartSkipMode::Off,
                    skip_tone,
                    26.0,
                    7.0,
                )
                .on_hover_text(
                    "Cycle Smart Skip (G): Off -> 8x Fast-Glide (auto-1x around events) -> Instant Event Jump",
                )
                .clicked()
                {
                    actions.push(TimelineAction::CycleSmartSkip);
                }
            }

            // 8f. Optional Event Filter Controls: 5 inline legend-dot chips (>= 295px) or upward Filter v dropdown (>= 86px)
            if ui.available_width() >= 295.0 {
                draw_capsule_group(ui, |ui| {
                    if draw_capsule_item_with_dot(
                        ui,
                        "Motion",
                        Some(STATUS_AMBER),
                        state.event_filter == Some(NvrEventKind::Motion),
                        ChipTone::Amber,
                    )
                    .on_hover_text("Filter Motion Events")
                    .clicked()
                    {
                        state.event_filter = if state.event_filter == Some(NvrEventKind::Motion) {
                            None
                        } else {
                            Some(NvrEventKind::Motion)
                        };
                        state.auto_scroll_to_active_event = true;
                    }
                    let alarm_active = matches!(
                        state.event_filter,
                        Some(NvrEventKind::Intrusion | NvrEventKind::Alarm)
                    );
                    if draw_capsule_item_with_dot(
                        ui,
                        "Alarm",
                        Some(STATUS_CORAL),
                        alarm_active,
                        ChipTone::Coral,
                    )
                    .on_hover_text("Filter Intrusion & Alarm Events")
                    .clicked()
                    {
                        state.event_filter = if alarm_active {
                            None
                        } else {
                            Some(NvrEventKind::Intrusion)
                        };
                        state.auto_scroll_to_active_event = true;
                    }
                    if draw_capsule_item_with_dot(
                        ui,
                        "Vehicle",
                        Some(STATUS_VIOLET),
                        state.event_filter == Some(NvrEventKind::Vehicle),
                        ChipTone::Violet,
                    )
                    .on_hover_text("Filter Smart IVS Vehicle Events")
                    .clicked()
                    {
                        state.event_filter = if state.event_filter == Some(NvrEventKind::Vehicle) {
                            None
                        } else {
                            Some(NvrEventKind::Vehicle)
                        };
                        state.auto_scroll_to_active_event = true;
                    }
                    if draw_capsule_item_with_dot(
                        ui,
                        "Human",
                        Some(STATUS_ICE),
                        state.event_filter == Some(NvrEventKind::Human),
                        ChipTone::Ice,
                    )
                    .on_hover_text("Filter Smart IVS Human Events")
                    .clicked()
                    {
                        state.event_filter = if state.event_filter == Some(NvrEventKind::Human) {
                            None
                        } else {
                            Some(NvrEventKind::Human)
                        };
                        state.auto_scroll_to_active_event = true;
                    }
                    if draw_capsule_item(
                        ui,
                        "All",
                        state.event_filter.is_none(),
                        ChipTone::Neutral,
                    )
                    .on_hover_text("Show All Event Types")
                    .clicked()
                    {
                        state.event_filter = None;
                        state.auto_scroll_to_active_event = true;
                    }
                });
            } else if ui.available_width() >= 86.0 {
                let filter_popup_id = ui.make_persistent_id("obsidian_ev_filter_popup");
                let (flt_lbl, flt_dot) = if ui.available_width() < 120.0 {
                    match state.event_filter {
                        None => ("Filter v", None),
                        Some(NvrEventKind::Human) => ("Hum v", Some(STATUS_ICE)),
                        Some(NvrEventKind::Vehicle) => ("Veh v", Some(STATUS_VIOLET)),
                        Some(NvrEventKind::Intrusion) | Some(NvrEventKind::Alarm) => {
                            ("Alm v", Some(STATUS_CORAL))
                        }
                        Some(NvrEventKind::Motion) => ("Mot v", Some(STATUS_AMBER)),
                        Some(NvrEventKind::Continuous) => ("Filter v", None),
                    }
                } else {
                    match state.event_filter {
                        None => ("Filter: All v", None),
                        Some(NvrEventKind::Human) => ("Human v", Some(STATUS_ICE)),
                        Some(NvrEventKind::Vehicle) => ("Vehicle v", Some(STATUS_VIOLET)),
                        Some(NvrEventKind::Intrusion) | Some(NvrEventKind::Alarm) => {
                            ("Alarm v", Some(STATUS_CORAL))
                        }
                        Some(NvrEventKind::Motion) => ("Motion v", Some(STATUS_AMBER)),
                        Some(NvrEventKind::Continuous) => ("Filter: All v", None),
                    }
                };
                let flt_btn = draw_compact_pill_with_dot(
                    ui,
                    flt_lbl,
                    flt_dot,
                    state.event_filter.is_some(),
                    ChipTone::Ice,
                    26.0,
                    7.0,
                );
                if flt_btn.clicked() {
                    ui.memory_mut(|m| m.toggle_popup(filter_popup_id));
                }
                egui::popup_above_or_below_widget(
                    ui,
                    filter_popup_id,
                    &flt_btn,
                    egui::AboveOrBelow::Above,
                    egui::PopupCloseBehavior::CloseOnClickOutside,
                    |ui| {
                        ui.set_min_width(138.0);
                        for (opt, name, dot) in [
                            (None, "All Events", None),
                            (Some(NvrEventKind::Human), "Human", Some(STATUS_ICE)),
                            (Some(NvrEventKind::Vehicle), "Vehicle", Some(STATUS_VIOLET)),
                            (
                                Some(NvrEventKind::Intrusion),
                                "Intrusion / Alarm",
                                Some(STATUS_CORAL),
                            ),
                            (Some(NvrEventKind::Motion), "Motion", Some(STATUS_AMBER)),
                        ] {
                            let is_sel = match (state.event_filter, opt) {
                                (
                                    Some(NvrEventKind::Intrusion | NvrEventKind::Alarm),
                                    Some(NvrEventKind::Intrusion),
                                ) => true,
                                (a, b) => a == b,
                            };
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                let (dot_r, _) =
                                    ui.allocate_exact_size(Vec2::new(8.0, 16.0), Sense::hover());
                                if let Some(dc) = dot {
                                    ui.painter().circle_filled(dot_r.center(), 3.2, dc);
                                }
                                if ui.selectable_label(is_sel, name).clicked() {
                                    state.event_filter = opt;
                                    state.auto_scroll_to_active_event = true;
                                    ui.memory_mut(|m| m.close_popup());
                                }
                            });
                        }
                    },
                );
                flt_btn.on_hover_text("Filter timeline & drawer events");
            }
        },
    );

    // =========================================================================
    // BOTTOM ROW (52px Multi-Scale Interactive Timeline Canvas)
    // =========================================================================
    render_timeline_canvas(
        ui,
        full_rect,
        canvas_rect,
        state,
        playback,
        primary_ch,
        timeline.as_ref(),
        timeline_loading,
        playhead_ms,
        frame_pinch,
        &mut actions,
    );

    // =========================================================================
    // FLOATING CALENDAR POPUP WITH RECORDED-DAY GREEN DOTS & KEYBOARD NAV
    // =========================================================================
    if state.show_calendar_popup {
        if let Some(anchor) = calendar_anchor_rect {
            render_calendar_popup(
                ui.ctx(),
                anchor,
                state,
                playback,
                app_cfg,
                primary_ch,
                &mut actions,
            );
        }
    }

    // =========================================================================
    // FLOATING DIRECT TIME JUMP POPOVER (HH:MM:SS + QUICK-JUMP CHIPS)
    // =========================================================================
    if state.show_time_jump_popup {
        if let Some(anchor) = clock_anchor_rect {
            render_time_jump_popover(
                ui.ctx(),
                anchor,
                state,
                timeline.as_ref(),
                playhead_ms,
                now_wall_ms,
                &mut actions,
            );
        }
    }

    actions
}

/// Computes `(span_zoom_factor, pan_dx_px)` from Wayland touchpad `zwp_pointer_gesture_pinch_v1`,
/// touchscreen multi-touch pinch, `Event::Zoom`, and axis scroll (`MouseWheel` suppressed during pinch).
/// `span_zoom_factor < 1.0` zooms IN (narrower time window), `> 1.0` zooms OUT (wider time window).
pub fn compute_timeline_zoom_and_pan(
    events: &[egui::Event],
    touch_zoom: Option<f32>,
    pinch: PinchFrameSnapshot,
    canvas_hovered: bool,
) -> (f64, f64) {
    let mut span_factor = 1.0_f64;
    let mut pan_dx_px = 0.0_f64;

    if pinch.scale_delta.is_finite() && pinch.scale_delta > 0.01 {
        // Timeline covers a 720x range (24h <-> 2m); powf(2.2) gives effortless 3-pinch traversal
        // while keeping 1% micro-pinches silky smooth (~2.2% per frame).
        span_factor /= (pinch.scale_delta as f64).powf(2.2);
    }
    if pinch.pan_delta.x.is_finite() && pinch.pan_delta.x.abs() > 0.05 {
        pan_dx_px += pinch.pan_delta.x as f64;
    }
    if let Some(tz) = touch_zoom {
        if tz.is_finite() && tz > 0.01 {
            span_factor /= (tz as f64).powf(1.8);
        }
    }

    if canvas_hovered {
        for ev in events {
            match ev {
                egui::Event::Zoom(z) if z.is_finite() && *z > 0.01 => {
                    span_factor /= *z as f64;
                }
                egui::Event::MouseWheel { unit, delta, .. } if !pinch.suppress_axis_scroll => {
                    let scroll_y = if delta.y.abs() >= delta.x.abs() {
                        delta.y
                    } else {
                        0.0
                    };
                    if scroll_y.abs() > 0.01 {
                        match unit {
                            egui::MouseWheelUnit::Point => {
                                span_factor *= (-scroll_y as f64 * 0.0085).clamp(-1.4, 1.4).exp();
                            }
                            egui::MouseWheelUnit::Line => {
                                span_factor *= 0.80_f64.powf((scroll_y as f64).clamp(-4.0, 4.0));
                            }
                            egui::MouseWheelUnit::Page => {
                                span_factor *= 0.55_f64.powf((scroll_y as f64).clamp(-2.0, 2.0));
                            }
                        }
                    }
                    // Horizontal trackpad two-finger swipe or mouse tilt-wheel pans timeline smoothly
                    let scaled_dx = match unit {
                        egui::MouseWheelUnit::Point => delta.x as f64,
                        egui::MouseWheelUnit::Line => delta.x as f64 * 28.0,
                        egui::MouseWheelUnit::Page => delta.x as f64 * 120.0,
                    };
                    if scaled_dx.abs() > 0.25 && delta.x.abs() > delta.y.abs() * 1.2 {
                        pan_dx_px += scaled_dx;
                    }
                }
                _ => {}
            }
        }
    }

    (span_factor.clamp(0.05, 20.0), pan_dx_px)
}

#[allow(clippy::too_many_arguments)]
fn render_timeline_canvas(
    ui: &mut Ui,
    dock_rect: Rect,
    canvas_rect: Rect,
    state: &mut TimelineUiState,
    playback: &PlaybackManager,
    primary_ch: usize,
    timeline: Option<&DayTimelineIndex>,
    timeline_loading: bool,
    playhead_ms: i64,
    frame_pinch: PinchFrameSnapshot,
    actions: &mut Vec<TimelineAction>,
) {
    let canvas_rect = Rect::from_min_max(
        canvas_rect.min,
        Pos2::new(
            canvas_rect.max.x.max(canvas_rect.min.x + 1.0),
            canvas_rect.max.y.max(canvas_rect.min.y + 1.0),
        ),
    );
    let now_wall_ms = current_wall_ms();
    let max_center_ms = now_wall_ms.saturating_add(MAX_WINDOW_SPAN_MS / 2);

    let canvas_id = ui.make_persistent_id("obsidian_timeline_scrub_canvas");
    let resp = ui.interact(canvas_rect, canvas_id, Sense::click_and_drag());

    let ruler_min = canvas_rect.min;
    let ruler_rect = Rect::from_min_max(
        ruler_min,
        Pos2::new(
            canvas_rect.max.x.max(ruler_min.x + 1.0),
            (canvas_rect.min.y + 18.0).max(ruler_min.y + 1.0),
        ),
    );
    let track_min = Pos2::new(canvas_rect.min.x + 1.0, ruler_rect.max.y);
    let track_rect = Rect::from_min_max(
        track_min,
        Pos2::new(
            (canvas_rect.max.x - 1.0).max(track_min.x + 1.0),
            (canvas_rect.max.y - 2.0).max(track_min.y + 1.0),
        ),
    );
    let cov_min = Pos2::new(track_rect.min.x, track_rect.min.y + 4.0);
    let cov_bar_rect = Rect::from_min_max(
        cov_min,
        Pos2::new(
            track_rect.max.x.max(cov_min.x + 1.0),
            (track_rect.max.y - 3.0).max(cov_min.y + 1.0),
        ),
    );

    // Top-right ruler HUD buttons: [<- Follow / Follow -> / Center]?  [- (18x15.5)] [1h] [+ (18x15.5)]
    let zoom_in_rect = Rect::from_min_size(
        Pos2::new(canvas_rect.max.x - 22.0, ruler_rect.min.y + 1.25),
        Vec2::new(18.0, 15.5),
    );
    let zoom_span_rect = Rect::from_min_size(
        Pos2::new(zoom_in_rect.min.x - 2.0 - 38.0, ruler_rect.min.y + 1.25),
        Vec2::new(38.0, 15.5),
    );
    let zoom_out_rect = Rect::from_min_size(
        Pos2::new(zoom_span_rect.min.x - 2.0 - 18.0, ruler_rect.min.y + 1.25),
        Vec2::new(18.0, 15.5),
    );
    let recenter_pill_rect = if !state.follow_playhead {
        Some(Rect::from_min_size(
            Pos2::new(zoom_out_rect.min.x - 5.0 - 66.0, ruler_rect.min.y + 1.25),
            Vec2::new(66.0, 15.5),
        ))
    } else {
        None
    };

    // Interact ruler controls & optional [Retry] pill cleanly on top so they never misfire a scrub!
    let zoom_out_resp = ui.interact(
        zoom_out_rect,
        ui.make_persistent_id("obsidian_tl_zoom_out_btn"),
        Sense::click(),
    );
    let zoom_span_resp = ui.interact(
        zoom_span_rect,
        ui.make_persistent_id("obsidian_tl_zoom_span_btn"),
        Sense::click(),
    );
    let zoom_in_resp = ui.interact(
        zoom_in_rect,
        ui.make_persistent_id("obsidian_tl_zoom_in_btn"),
        Sense::click(),
    );
    let recenter_resp = recenter_pill_rect.map(|rc_rect| {
        ui.interact(
            rc_rect,
            ui.make_persistent_id("obsidian_tl_recenter_btn"),
            Sense::click(),
        )
    });

    let retry_error_note = timeline
        .filter(|tl| !tl.api_supported)
        .and_then(|tl| tl.note.as_deref());
    let retry_pill_rect = if retry_error_note.is_some() {
        Some(Rect::from_center_size(
            Pos2::new(
                (cov_bar_rect.center().x + 128.0).min(cov_bar_rect.max.x - 32.0),
                cov_bar_rect.center().y,
            ),
            Vec2::new(46.0, 17.0),
        ))
    } else {
        None
    };
    let retry_resp = retry_pill_rect.map(|rr| {
        ui.interact(
            rr,
            ui.make_persistent_id("obsidian_tl_retry_index_btn"),
            Sense::click(),
        )
    });

    let zoom_preset_popup_id = ui.make_persistent_id("obsidian_tl_zoom_preset_popup");
    if zoom_span_resp.clicked() {
        ui.memory_mut(|m| m.toggle_popup(zoom_preset_popup_id));
    }
    egui::popup_above_or_below_widget(
        ui,
        zoom_preset_popup_id,
        &zoom_span_resp,
        egui::AboveOrBelow::Above,
        egui::PopupCloseBehavior::CloseOnClickOutside,
        |ui| {
            ui.set_min_width(116.0);
            for (span_ms, lbl) in [
                (120_000_i64, "2m"),
                (300_000, "5m"),
                (900_000, "15m"),
                (1_800_000, "30m"),
                (DEFAULT_WINDOW_SPAN_MS, "1h (Default)"),
                (14_400_000, "4h"),
                (43_200_000, "12h"),
                (86_400_000, "24h"),
            ] {
                let is_sel = state.window_span_ms.abs_diff(span_ms) < 15_000;
                if ui.selectable_label(is_sel, lbl).clicked() {
                    let factor = span_ms as f64 / state.window_span_ms.max(1) as f64;
                    state.zoom_around_playhead_preserving_follow(factor, playhead_ms);
                    state.window_span_ms = span_ms;
                    ui.memory_mut(|m| m.close_popup());
                }
            }
        },
    );

    if zoom_out_resp.clicked() {
        state.zoom_around_playhead_preserving_follow(1.6, playhead_ms);
    }
    if zoom_in_resp.clicked() {
        state.zoom_around_playhead_preserving_follow(1.0 / 1.6, playhead_ms);
    }
    if let Some(ref rc_r) = recenter_resp {
        if rc_r.clicked() {
            state.recenter_on_playhead(playhead_ms);
        }
    }
    if let Some(ref rt_r) = retry_resp {
        if rt_r.clicked() {
            actions.push(TimelineAction::RefreshTimeline);
        }
    }

    let point_on_ruler_btn = |p: Pos2| {
        zoom_in_rect.contains(p)
            || zoom_span_rect.contains(p)
            || zoom_out_rect.contains(p)
            || recenter_pill_rect.is_some_and(|r| r.contains(p))
            || retry_pill_rect.is_some_and(|r| r.contains(p))
    };

    // 1. Cursor-anchored Wayland touchpad pinch, touchscreen pinch, and wheel zoom/pan
    let hover_or_interact =
        ui.input(|i| i.pointer.hover_pos().or_else(|| i.pointer.interact_pos()));
    let popup_open = ui.ctx().memory(|m| m.any_popup_open())
        || ui.ctx().is_context_menu_open()
        || state.is_any_popup_open();
    let dock_hovered =
        !popup_open && hover_or_interact.is_some_and(|p| dock_rect.expand(2.0).contains(p));
    let canvas_area_hovered =
        !popup_open && hover_or_interact.is_some_and(|p| canvas_rect.contains(p));
    let pinch_active = (frame_pinch.scale_delta - 1.0).abs() > 0.0002
        || frame_pinch.pan_delta.x.abs() > 0.05
        || frame_pinch.suppress_axis_scroll;

    if canvas_area_hovered || (dock_hovered && pinch_active) {
        let (zoom_factor, pan_dx_px) = ui.input(|i| {
            compute_timeline_zoom_and_pan(
                &i.events,
                i.multi_touch().map(|t| t.zoom_delta),
                frame_pinch,
                canvas_area_hovered,
            )
        });

        if pan_dx_px.abs() > 0.05 {
            let ms_per_px = state.window_span_ms as f64 / canvas_rect.width().max(1.0) as f64;
            let shift_ms = (-pan_dx_px * ms_per_px).round() as i64;
            state.view_center_ms = state
                .view_center_ms
                .saturating_add(shift_ms)
                .clamp(0, max_center_ms);
            state.follow_playhead = false;
        }

        if (zoom_factor - 1.0).abs() > 0.0003 {
            let (vs, _) = state.visible_range_ms();
            let anchor_ms = hover_or_interact
                .map(|p| {
                    let clamped_x = p.x.clamp(canvas_rect.min.x, canvas_rect.max.x);
                    x_to_epoch_ms(
                        clamped_x,
                        canvas_rect.min.x,
                        canvas_rect.width(),
                        vs,
                        state.window_span_ms,
                    )
                })
                .unwrap_or(playhead_ms);
            let (new_center, new_span) = zoom_window_around_anchor(
                state.view_center_ms,
                state.window_span_ms,
                anchor_ms,
                zoom_factor,
            );
            state.view_center_ms = new_center.clamp(0, max_center_ms);
            state.window_span_ms = new_span;
            state.follow_playhead = false;
        }
    }

    // 2. Middle-drag or Right-drag horizontal window pan
    let (
        middle_down,
        secondary_down,
        primary_down,
        primary_released,
        shift_held,
        pointer_pos,
        latest_pos,
        press_origin,
        drag_delta,
    ) = ui.input(|i| {
        (
            i.pointer.button_down(PointerButton::Middle),
            i.pointer.button_down(PointerButton::Secondary),
            i.pointer.button_down(PointerButton::Primary),
            i.pointer.button_released(PointerButton::Primary),
            i.modifiers.shift,
            i.pointer.interact_pos(),
            i.pointer.latest_pos(),
            i.pointer.press_origin(),
            i.pointer.delta(),
        )
    });

    if resp.hovered() && (middle_down || secondary_down) {
        state.panning_window = true;
    }
    if !middle_down && !secondary_down {
        state.panning_window = false;
    }
    if state.panning_window {
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        if drag_delta.x.abs() > 0.01 {
            let ms_per_px = state.window_span_ms as f64 / canvas_rect.width().max(1.0) as f64;
            let delta_ms = (-drag_delta.x as f64 * ms_per_px).round() as i64;
            state.view_center_ms = state
                .view_center_ms
                .saturating_add(delta_ms)
                .clamp(0, max_center_ms);
            state.follow_playhead = false;
        }
    }

    let (view_start_ms, view_end_ms) = state.visible_range_ms();
    let canvas_w = canvas_rect.width().max(1.0);

    // Gather multi-day timelines across [view_start_ms, view_end_ms] so midnight pans/zooms never blank out!
    let mut range_timelines =
        playback.day_timelines_for_range(primary_ch, view_start_ms, view_end_ms);
    if let Some(sel_tl) = timeline {
        if !range_timelines.iter().any(|t| t.date == sel_tl.date)
            && sel_tl.date
                >= epoch_ms_to_local_naive(view_start_ms).date()
            && sel_tl.date <= epoch_ms_to_local_naive(view_end_ms).date()
        {
            range_timelines.push(sel_tl.clone());
        }
    }
    let all_range_events: Vec<&RecordedSpan> = range_timelines
        .iter()
        .filter(|tl| tl.api_supported)
        .flat_map(|tl| tl.events.iter())
        .collect();

    // 3. Bracket handle hit-testing & dragging + Playhead scrubbing + Double-click ruler reset
    let has_clip = state.clip_in_ms.is_some() || state.clip_out_ms.is_some();
    let (resolved_in_ms, resolved_out_ms) = if has_clip {
        let (im, om) = state.resolved_clip_range(playhead_ms);
        (Some(im), Some(om))
    } else {
        (None, None)
    };
    let in_x_opt = resolved_in_ms
        .map(|ms| {
            epoch_ms_to_x(
                ms,
                canvas_rect.min.x,
                canvas_w,
                view_start_ms,
                state.window_span_ms,
            )
        })
        .filter(|ix| (track_rect.min.x..=track_rect.max.x).contains(ix));
    let out_x_opt = resolved_out_ms
        .map(|ms| {
            epoch_ms_to_x(
                ms,
                canvas_rect.min.x,
                canvas_w,
                view_start_ms,
                state.window_span_ms,
            )
        })
        .filter(|ox| (track_rect.min.x..=track_rect.max.x).contains(ox));

    let ruler_btn_hovered = zoom_out_resp.hovered()
        || zoom_span_resp.hovered()
        || zoom_in_resp.hovered()
        || recenter_resp.as_ref().is_some_and(|r| r.hovered())
        || retry_resp.as_ref().is_some_and(|r| r.hovered());

    if ruler_btn_hovered {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    } else if let Some(pos) = resp.hover_pos() {
        let near_in =
            in_x_opt.is_some_and(|ix| (pos.x - ix).abs() <= 7.0 && pos.y >= track_rect.min.y);
        let near_out =
            out_x_opt.is_some_and(|ox| (pos.x - ox).abs() <= 7.0 && pos.y >= track_rect.min.y);
        if near_in || near_out || state.dragging_bracket.is_some() {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        } else if !state.panning_window {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
    }

    let clicked_hud_btn = zoom_out_resp.clicked()
        || zoom_span_resp.clicked()
        || zoom_in_resp.clicked()
        || recenter_resp.as_ref().is_some_and(|r| r.clicked())
        || retry_resp.as_ref().is_some_and(|r| r.clicked());
    let started_on_ruler_btn = press_origin.is_some_and(point_on_ruler_btn);

    let double_clicked_ruler = !clicked_hud_btn
        && resp.double_clicked()
        && pointer_pos
            .or(latest_pos)
            .is_some_and(|p| ruler_rect.contains(p) && !point_on_ruler_btn(p));
    if double_clicked_ruler {
        state.window_span_ms = DEFAULT_WINDOW_SPAN_MS;
        state.recenter_on_playhead(playhead_ms);
    }

    if !clicked_hud_btn && !double_clicked_ruler && !state.panning_window {
        if resp.drag_started_by(PointerButton::Primary) && !started_on_ruler_btn {
            if let Some(origin) = press_origin.or(pointer_pos) {
                if let Some(target_bracket) = pick_bracket_drag_target(
                    origin,
                    track_rect.min.y,
                    in_x_opt,
                    out_x_opt,
                    resp.drag_delta().x,
                ) {
                    if has_clip {
                        let (im, om) = state.resolved_clip_range(playhead_ms);
                        if state.clip_in_ms.is_none() {
                            state.clip_in_ms = Some(im);
                        }
                        if state.clip_out_ms.is_none() {
                            state.clip_out_ms = Some(om);
                        }
                    }
                    state.dragging_bracket = Some(target_bracket);
                } else {
                    state.dragging_playhead = true;
                    state.last_emitted_scrub_ms = None;
                }
            }
        }

        if let Some(bracket) = state.dragging_bracket {
            if primary_down {
                if let Some(pos) = pointer_pos.or(latest_pos) {
                    let target_ms = x_to_epoch_ms(
                        pos.x,
                        canvas_rect.min.x,
                        canvas_w,
                        view_start_ms,
                        state.window_span_ms,
                    )
                    .clamp(0, now_wall_ms);
                    match bracket {
                        ActiveBracketDrag::In => {
                            let max_in = state
                                .clip_out_ms
                                .map(|o| o.saturating_sub(500))
                                .unwrap_or_else(|| now_wall_ms.saturating_sub(500))
                                .max(0);
                            state.clip_in_ms = Some(target_ms.clamp(0, max_in));
                        }
                        ActiveBracketDrag::Out => {
                            let wall_cap = now_wall_ms.max(0);
                            let min_out = state
                                .clip_in_ms
                                .map(|i| i.saturating_add(500))
                                .unwrap_or(500)
                                .clamp(0, wall_cap);
                            state.clip_out_ms = Some(target_ms.clamp(min_out, wall_cap));
                        }
                    }
                }
            }
            if primary_released || !primary_down {
                state.dragging_bracket = None;
            }
        } else if state.dragging_playhead {
            if let Some(pos) = pointer_pos.or(latest_pos) {
                let mut target_ms = x_to_epoch_ms(
                    pos.x,
                    canvas_rect.min.x,
                    canvas_w,
                    view_start_ms,
                    state.window_span_ms,
                );
                if shift_held {
                    if let Some((snapped, _, _)) = snap_to_nearby_event(
                        pos.x,
                        &all_range_events,
                        state.event_filter,
                        canvas_rect.min.x,
                        canvas_w,
                        view_start_ms,
                        state.window_span_ms,
                    ) {
                        target_ms = snapped;
                    }
                }
                target_ms = target_ms.clamp(0, now_wall_ms);
                state.last_scrub_ms = Some(target_ms);
                if primary_down {
                    let should_emit = state
                        .last_emitted_scrub_ms
                        .map(|prev| target_ms.abs_diff(prev) >= 30)
                        .unwrap_or(true);
                    if should_emit {
                        state.last_emitted_scrub_ms = Some(target_ms);
                        actions.push(TimelineAction::Seek {
                            epoch_ms: target_ms,
                            immediate: false,
                        });
                    }
                }
            }
            if primary_released || !primary_down {
                state.dragging_playhead = false;
                state.last_emitted_scrub_ms = None;
                if let Some(final_ms) = state.last_scrub_ms.take() {
                    actions.push(TimelineAction::Seek {
                        epoch_ms: final_ms.clamp(0, now_wall_ms),
                        immediate: true,
                    });
                }
            }
        } else if resp.clicked_by(PointerButton::Primary) && !started_on_ruler_btn {
            if let Some(pos) = pointer_pos {
                if !point_on_ruler_btn(pos) {
                    let mut target_ms = x_to_epoch_ms(
                        pos.x,
                        canvas_rect.min.x,
                        canvas_w,
                        view_start_ms,
                        state.window_span_ms,
                    );
                    if shift_held {
                        if let Some((snapped, _, _)) = snap_to_nearby_event(
                            pos.x,
                            &all_range_events,
                            state.event_filter,
                            canvas_rect.min.x,
                            canvas_w,
                            view_start_ms,
                            state.window_span_ms,
                        ) {
                            target_ms = snapped;
                        }
                    }
                    target_ms = target_ms.clamp(0, now_wall_ms);
                    actions.push(TimelineAction::Seek {
                        epoch_ms: target_ms,
                        immediate: true,
                    });
                }
            }
        }
    }

    // Re-read visible_range_ms in case ruler [-]/[+]/Follow/double-click mutated view_center_ms/window_span_ms
    let (view_start_ms, view_end_ms) = state.visible_range_ms();

    let painter = ui.painter_at(canvas_rect);
    painter.rect_filled(canvas_rect, Rounding::same(6.0), OBSIDIAN_BG);
    painter.rect_stroke(
        canvas_rect,
        Rounding::same(6.0),
        Stroke::new(1.0_f32, hairline_stroke()),
    );
    painter.rect_filled(
        ruler_rect,
        Rounding {
            nw: 6.0,
            ne: 6.0,
            sw: 0.0,
            se: 0.0,
        },
        Color32::from_rgb(14, 14, 18),
    );
    painter.line_segment(
        [
            Pos2::new(canvas_rect.min.x, ruler_rect.max.y),
            Pos2::new(canvas_rect.max.x, ruler_rect.max.y),
        ],
        Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 16)),
    );

    // 4. Adaptive Time-Ruler Ticks (galley-measured anti-collision + midnight date labels!)
    let (major_step_ms, minor_step_ms, show_secs) = ruler_steps_for_span(state.window_span_ms);
    let anchor_midnight_dt = epoch_ms_to_local_naive(view_start_ms)
        .date()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let anchor_midnight_ms = local_naive_to_epoch_ms(anchor_midnight_dt);
    let right_hud_min_x = recenter_pill_rect
        .map(|r| r.min.x)
        .unwrap_or(zoom_out_rect.min.x);

    let first_minor_idx = (view_start_ms - anchor_midnight_ms).div_euclid(minor_step_ms);
    let mut tick_ms = anchor_midnight_ms + first_minor_idx * minor_step_ms;
    let mut last_label_max_x = f32::NEG_INFINITY;

    while tick_ms <= view_end_ms {
        if tick_ms >= view_start_ms {
            let tx = epoch_ms_to_x(
                tick_ms,
                canvas_rect.min.x,
                canvas_w,
                view_start_ms,
                state.window_span_ms,
            );
            let is_major = (tick_ms - anchor_midnight_ms).rem_euclid(major_step_ms) == 0;
            let t_dt = epoch_ms_to_local_naive(tick_ms);
            let is_midnight = t_dt.hour() == 0 && t_dt.minute() == 0 && t_dt.second() == 0;

            if is_major || is_midnight {
                painter.line_segment(
                    [
                        Pos2::new(tx, ruler_rect.max.y - if is_midnight { 9.0 } else { 6.0 }),
                        Pos2::new(tx, ruler_rect.max.y),
                    ],
                    Stroke::new(
                        1.0_f32,
                        if is_midnight {
                            STATUS_ICE
                        } else {
                            Color32::from_rgba_unmultiplied(255, 255, 255, 65)
                        },
                    ),
                );
                painter.line_segment(
                    [Pos2::new(tx, track_rect.min.y), Pos2::new(tx, track_rect.max.y)],
                    Stroke::new(
                        1.0_f32,
                        if is_midnight {
                            Color32::from_rgba_unmultiplied(56, 189, 248, 38)
                        } else {
                            Color32::from_rgba_unmultiplied(255, 255, 255, 10)
                        },
                    ),
                );
                if tx > canvas_rect.min.x + 6.0 {
                    let lbl = if is_midnight {
                        format!("{}", t_dt.format("%b %d 00:00"))
                    } else if show_secs {
                        format!(
                            "{:02}:{:02}:{:02}",
                            t_dt.hour(),
                            t_dt.minute(),
                            t_dt.second()
                        )
                    } else {
                        format!("{:02}:{:02}", t_dt.hour(), t_dt.minute())
                    };
                    let lbl_col = if is_midnight {
                        TEXT_SECONDARY
                    } else {
                        TEXT_MUTED
                    };
                    let galley = painter.layout_no_wrap(
                        lbl,
                        FontId::new(9.5, FontFamily::Monospace),
                        lbl_col,
                    );
                    let lbl_x = tx + 3.0;
                    let lbl_end_x = lbl_x + galley.size().x;
                    if tx >= last_label_max_x + 8.0 && lbl_end_x + 6.0 < right_hud_min_x {
                        let lbl_y = ruler_rect.center().y - 0.5 * galley.size().y;
                        painter.galley(Pos2::new(lbl_x, lbl_y), galley, lbl_col);
                        last_label_max_x = lbl_end_x;
                    }
                }
            } else {
                painter.line_segment(
                    [
                        Pos2::new(tx, ruler_rect.max.y - 3.5),
                        Pos2::new(tx, ruler_rect.max.y),
                    ],
                    Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 28)),
                );
            }
        }
        tick_ms = tick_ms.saturating_add(minor_step_ms);
    }

    // Paint ruler [- (18x15.5)] [1h] [+ (18x15.5)] + directional [<- Follow / Follow -> / Center]
    for (btn_rect, btn_resp, sym, tip) in [
        (zoom_out_rect, &zoom_out_resp, "-", "Zoom Out Timeline (-)"),
        (zoom_in_rect, &zoom_in_resp, "+", "Zoom In Timeline (+)"),
    ] {
        let is_hov = btn_resp.hovered();
        painter.rect_filled(
            btn_rect,
            Rounding::same(3.5),
            if is_hov { ZINC_HOVER } else { ZINC_ELEVATED },
        );
        painter.rect_stroke(
            btn_rect,
            Rounding::same(3.5),
            Stroke::new(
                1.0_f32,
                if is_hov { STATUS_ICE } else { hairline_stroke() },
            ),
        );
        painter.text(
            btn_rect.center(),
            Align2::CENTER_CENTER,
            sym,
            FontId::new(11.0, FontFamily::Monospace),
            if is_hov { TEXT_PRIMARY } else { TEXT_SECONDARY },
        );
        btn_resp.clone().on_hover_text(tip);
    }

    {
        let span_open = ui.memory(|m| m.is_popup_open(zoom_preset_popup_id));
        let span_hov = zoom_span_resp.hovered() || span_open;
        painter.rect_filled(
            zoom_span_rect,
            Rounding::same(3.5),
            if span_hov {
                Color32::from_rgba_unmultiplied(56, 189, 248, 36)
            } else {
                ZINC_ELEVATED
            },
        );
        painter.rect_stroke(
            zoom_span_rect,
            Rounding::same(3.5),
            Stroke::new(
                1.0_f32,
                if span_hov { STATUS_ICE } else { hairline_stroke() },
            ),
        );
        painter.text(
            zoom_span_rect.center(),
            Align2::CENTER_CENTER,
            format_span_badge(state.window_span_ms),
            FontId::new(9.5, FontFamily::Monospace),
            if span_hov { TEXT_PRIMARY } else { STATUS_ICE },
        );
        zoom_span_resp.clone().on_hover_text(
            "Zoom Window Presets (Click to select 2m..24h | Double-click ruler to reset to 1h)",
        );
    }

    if let (Some(rc_rect), Some(ref rc_r)) = (recenter_pill_rect, &recenter_resp) {
        let rc_hov = rc_r.hovered();
        let rc_lbl = if playhead_ms < view_start_ms {
            "<- Follow"
        } else if playhead_ms > view_end_ms {
            "Follow ->"
        } else {
            "Center"
        };
        painter.rect_filled(
            rc_rect,
            Rounding::same(4.0),
            if rc_hov {
                Color32::from_rgba_unmultiplied(56, 189, 248, 76)
            } else {
                Color32::from_rgba_unmultiplied(56, 189, 248, 38)
            },
        );
        painter.rect_stroke(
            rc_rect,
            Rounding::same(4.0),
            Stroke::new(
                1.0_f32,
                if rc_hov { TEXT_PRIMARY } else { STATUS_ICE },
            ),
        );
        painter.text(
            rc_rect.center(),
            Align2::CENTER_CENTER,
            rc_lbl,
            FontId::new(9.5, FontFamily::Proportional),
            TEXT_PRIMARY,
        );
        rc_r.clone()
            .on_hover_text("Re-center timeline on playhead and resume Follow mode");
    }

    // 5. Continuous Coverage Track + Cached RAM Underline + Specificity-Sorted Event Layering!
    painter.rect_filled(
        cov_bar_rect,
        Rounding::same(3.0),
        Color32::from_rgb(16, 16, 20),
    );

    let hover_pos = resp.hover_pos();
    let active_scrub_x = if state.dragging_playhead {
        pointer_pos
            .or(latest_pos)
            .map(|p| p.x.clamp(canvas_rect.min.x, canvas_rect.max.x))
    } else {
        hover_pos
            .filter(|_| !ruler_btn_hovered)
            .map(|p| p.x.clamp(canvas_rect.min.x, canvas_rect.max.x))
    };

    let mut overlapping_events: Vec<&RecordedSpan> = Vec::new();
    let any_api_supported = range_timelines.iter().any(|t| t.api_supported)
        || timeline.is_some_and(|t| t.api_supported);

    if any_api_supported {
        let mut painted_cov_count = 0usize;
        let mut total_unfiltered_events = 0usize;

        for tl in &range_timelines {
            if !tl.api_supported {
                continue;
            }
            total_unfiltered_events += tl.events.len();
            for cov in &tl.coverage {
                if let Some((x0, x1)) = compute_dilated_span_px(
                    cov.start_ms,
                    cov.end_ms,
                    track_rect.min.x,
                    track_rect.max.x,
                    view_start_ms,
                    state.window_span_ms,
                    1.5,
                ) {
                    painted_cov_count += 1;
                    let r = Rect::from_min_max(
                        Pos2::new(x0, cov_bar_rect.min.y),
                        Pos2::new(x1, cov_bar_rect.max.y),
                    );
                    painter.rect_filled(r, Rounding::same(2.0), continuous_coverage_fill());
                }
            }
        }

        // Collect all visible filtered events across days and sort ASCENDING by specificity_rank()
        // so Motion (1) paints first and Alarm/Intrusion/Vehicle/Human (2..5) paint cleanly ON TOP!
        let mut visible_events: Vec<(&RecordedSpan, f32, f32)> = Vec::new();
        for tl in &range_timelines {
            if !tl.api_supported {
                continue;
            }
            for ev in &tl.events {
                if !matches_event_filter(ev.kind, state.event_filter) {
                    continue;
                }
                if let Some((x0, x1)) = compute_dilated_span_px(
                    ev.start_ms,
                    ev.end_ms,
                    track_rect.min.x,
                    track_rect.max.x,
                    view_start_ms,
                    state.window_span_ms,
                    MIN_EVENT_WIDTH_PX,
                ) {
                    visible_events.push((ev, x0, x1));
                }
            }
        }
        sort_events_for_painting(&mut visible_events);

        for &(ev, x0, x1) in &visible_events {
            let is_hit = active_scrub_x.is_some_and(|hx| {
                let y_ok = state.dragging_playhead
                    || hover_pos
                        .is_some_and(|hp| hp.y >= track_rect.min.y && hp.y <= track_rect.max.y);
                y_ok && hx >= x0 - 2.0 && hx <= x1 + 2.0
            });
            if is_hit {
                overlapping_events.push(ev);
            }
            let col = event_kind_color(ev.kind);
            let ev_rect = Rect::from_min_max(
                Pos2::new(x0, cov_bar_rect.min.y + 1.0),
                Pos2::new(x1, cov_bar_rect.max.y - 1.0),
            );
            let alpha = if is_hit { 248 } else { 208 };
            let fill = Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), alpha);
            painter.rect_filled(ev_rect, Rounding::same(2.0), fill);
        }

        // Sort overlapping hovered events DESCENDING by specificity_rank() so Human/Vehicle leads!
        overlapping_events.sort_by(|a, b| {
            b.kind
                .specificity_rank()
                .cmp(&a.kind.specificity_rank())
                .then_with(|| a.start_ms.cmp(&b.start_ms))
        });

        // Cached 60s MemfdSegmentCache instant-scrub underline along bottom of cov_bar_rect
        for (c_start, c_end) in
            playback.cached_ranges_for_channel(primary_ch, view_start_ms, view_end_ms)
        {
            let cx0 = epoch_ms_to_x(
                c_start,
                canvas_rect.min.x,
                canvas_w,
                view_start_ms,
                state.window_span_ms,
            )
            .clamp(track_rect.min.x, track_rect.max.x);
            let cx1 = epoch_ms_to_x(
                c_end,
                canvas_rect.min.x,
                canvas_w,
                view_start_ms,
                state.window_span_ms,
            )
            .clamp(track_rect.min.x, track_rect.max.x);
            if cx1 > cx0 + 0.5 {
                painter.line_segment(
                    [
                        Pos2::new(cx0, cov_bar_rect.max.y - 1.0),
                        Pos2::new(cx1, cov_bar_rect.max.y - 1.0),
                    ],
                    Stroke::new(
                        2.0_f32,
                        Color32::from_rgba_unmultiplied(
                            STATUS_ICE.r(),
                            STATUS_ICE.g(),
                            STATUS_ICE.b(),
                            165,
                        ),
                    ),
                );
            }
        }

        // Empty day state when loaded, api_supported, and zero recordings/events visible
        let sel_day_empty = timeline
            .is_some_and(|tl| tl.api_supported && tl.coverage.is_empty() && tl.events.is_empty());
        if !timeline_loading
            && sel_day_empty
            && painted_cov_count == 0
            && total_unfiltered_events == 0
        {
            painter.text(
                cov_bar_rect.center(),
                Align2::CENTER_CENTER,
                "No recordings found on this date",
                FontId::new(10.0, FontFamily::Proportional),
                TEXT_MUTED,
            );
        }
    } else if let Some(tl) = timeline {
        // !tl.api_supported: distinguish CGI/auth/network error (with [Retry] pill) vs clean fallback
        painter.rect_filled(
            cov_bar_rect,
            Rounding::same(3.0),
            Color32::from_rgba_unmultiplied(56, 189, 248, 18),
        );
        if let Some(ref err_note) = tl.note {
            let short_err = truncate_single_line(err_note, 48);
            let msg_center_x = retry_pill_rect
                .map(|rr| (rr.min.x - 8.0).min(cov_bar_rect.center().x + 92.0))
                .unwrap_or(cov_bar_rect.center().x);
            painter.text(
                Pos2::new(msg_center_x, cov_bar_rect.center().y),
                Align2::RIGHT_CENTER,
                format!("Index unavailable: {}", short_err),
                FontId::new(10.0, FontFamily::Proportional),
                STATUS_AMBER,
            );
            if let (Some(rr), Some(ref rt_r)) = (retry_pill_rect, &retry_resp) {
                let hov = rt_r.hovered();
                painter.rect_filled(
                    rr,
                    Rounding::same(4.0),
                    if hov {
                        Color32::from_rgba_unmultiplied(56, 189, 248, 64)
                    } else {
                        ZINC_ELEVATED
                    },
                );
                painter.rect_stroke(
                    rr,
                    Rounding::same(4.0),
                    Stroke::new(1.0_f32, STATUS_ICE),
                );
                painter.text(
                    rr.center(),
                    Align2::CENTER_CENTER,
                    "Retry",
                    FontId::new(9.5, FontFamily::Proportional),
                    TEXT_PRIMARY,
                );
                rt_r.clone()
                    .on_hover_text(format!("Retry NVR timeline query\nDetails: {}", err_note));
            }
        } else {
            painter.text(
                cov_bar_rect.center(),
                Align2::CENTER_CENTER,
                "Direct Timeline Scrubbing Ready (Click anywhere to seek)",
                FontId::new(10.0, FontFamily::Proportional),
                ZINC_DIM,
            );
        }
    } else if timeline_loading {
        painter.text(
            cov_bar_rect.center(),
            Align2::CENTER_CENTER,
            "Indexing NVR recorded spans...",
            FontId::new(10.0, FontFamily::Proportional),
            TEXT_MUTED,
        );
    }

    // 6. Ice-Blue [In .. Out] Clip Selection Bracket (>= 4.0px visual width + off-screen < IN / OUT >)
    let mut bracket_hover_tip: Option<String> = None;
    if has_clip {
        let (in_ms, out_ms) = state.resolved_clip_range(playhead_ms);
        let dur_compact = format_duration_compact(out_ms.abs_diff(in_ms) as i64);
        let raw_ix = epoch_ms_to_x(
            in_ms,
            canvas_rect.min.x,
            canvas_w,
            view_start_ms,
            state.window_span_ms,
        );
        let raw_ox = epoch_ms_to_x(
            out_ms,
            canvas_rect.min.x,
            canvas_w,
            view_start_ms,
            state.window_span_ms,
        );

        if raw_ox >= track_rect.min.x && raw_ix <= track_rect.max.x {
            let mut ix = raw_ix.clamp(track_rect.min.x, track_rect.max.x);
            let mut ox = raw_ox.clamp(track_rect.min.x, track_rect.max.x);
            if ox - ix < 4.0 {
                let mid = 0.5 * (ix + ox);
                ix = (mid - 2.0).max(track_rect.min.x);
                ox = (ix + 4.0).min(track_rect.max.x);
                if ox - ix < 4.0 {
                    ix = (ox - 4.0).max(track_rect.min.x);
                }
            }
            if ox > ix {
                let sel_rect = Rect::from_min_max(
                    Pos2::new(ix, track_rect.min.y + 1.0),
                    Pos2::new(ox, track_rect.max.y - 1.0),
                );
                painter.rect_filled(
                    sel_rect,
                    Rounding::same(2.0),
                    Color32::from_rgba_unmultiplied(56, 189, 248, 42),
                );
                painter.rect_stroke(sel_rect, Rounding::same(2.0), Stroke::new(1.2_f32, STATUS_ICE));
            }
        }

        for (raw_hx, is_left) in [(raw_ix, true), (raw_ox, false)] {
            if (track_rect.min.x..=track_rect.max.x).contains(&raw_hx) {
                painter.line_segment(
                    [
                        Pos2::new(raw_hx, track_rect.min.y),
                        Pos2::new(raw_hx, track_rect.max.y),
                    ],
                    Stroke::new(2.4_f32, STATUS_ICE),
                );
                let cap_dx = if is_left { 5.0 } else { -5.0 };
                painter.line_segment(
                    [
                        Pos2::new(raw_hx, track_rect.min.y),
                        Pos2::new(raw_hx + cap_dx, track_rect.min.y),
                    ],
                    Stroke::new(2.0_f32, STATUS_ICE),
                );
                painter.line_segment(
                    [
                        Pos2::new(raw_hx, track_rect.max.y),
                        Pos2::new(raw_hx + cap_dx, track_rect.max.y),
                    ],
                    Stroke::new(2.0_f32, STATUS_ICE),
                );
            }
        }

        // Off-screen "< IN" / "OUT >" edge indicators when brackets are panned outside visible window
        if raw_ix < track_rect.min.x {
            let badge = Rect::from_min_size(
                Pos2::new(track_rect.min.x + 3.0, track_rect.center().y - 7.0),
                Vec2::new(28.0, 14.0),
            );
            painter.rect_filled(
                badge,
                Rounding::same(3.0),
                Color32::from_rgba_unmultiplied(9, 9, 11, 215),
            );
            painter.rect_stroke(badge, Rounding::same(3.0), Stroke::new(1.0_f32, STATUS_ICE));
            painter.text(
                badge.center(),
                Align2::CENTER_CENTER,
                "< IN",
                FontId::new(8.5, FontFamily::Monospace),
                STATUS_ICE,
            );
        }
        if raw_ox > track_rect.max.x {
            let badge = Rect::from_min_size(
                Pos2::new(track_rect.max.x - 35.0, track_rect.center().y - 7.0),
                Vec2::new(32.0, 14.0),
            );
            painter.rect_filled(
                badge,
                Rounding::same(3.0),
                Color32::from_rgba_unmultiplied(9, 9, 11, 215),
            );
            painter.rect_stroke(badge, Rounding::same(3.0), Stroke::new(1.0_f32, STATUS_ICE));
            painter.text(
                badge.center(),
                Align2::CENTER_CENTER,
                "OUT >",
                FontId::new(8.5, FontFamily::Monospace),
                STATUS_ICE,
            );
        }

        // Bracket handle hover / drag tooltips
        if state.dragging_bracket == Some(ActiveBracketDrag::In)
            || hover_pos.is_some_and(|hp| {
                hp.y >= track_rect.min.y
                    && (track_rect.min.x..=track_rect.max.x).contains(&raw_ix)
                    && (hp.x - raw_ix).abs() <= 7.0
            })
        {
            bracket_hover_tip = Some(format!(
                "Drag Clip IN: {} (Duration: {})",
                epoch_ms_to_local_naive(in_ms).format("%H:%M:%S"),
                dur_compact
            ));
        } else if state.dragging_bracket == Some(ActiveBracketDrag::Out)
            || hover_pos.is_some_and(|hp| {
                hp.y >= track_rect.min.y
                    && (track_rect.min.x..=track_rect.max.x).contains(&raw_ox)
                    && (hp.x - raw_ox).abs() <= 7.0
            })
        {
            bracket_hover_tip = Some(format!(
                "Drag Clip OUT: {} (Duration: {})",
                epoch_ms_to_local_naive(out_ms).format("%H:%M:%S"),
                dur_compact
            ));
        }
    }

    // 7. Future Unrecorded Cutoff Shade + Crisp Emerald "NOW" Divider!
    if now_wall_ms < view_end_ms {
        let raw_nx = epoch_ms_to_x(
            now_wall_ms,
            canvas_rect.min.x,
            canvas_w,
            view_start_ms,
            state.window_span_ms,
        );
        let nx = raw_nx.max(track_rect.min.x);
        if nx < track_rect.max.x {
            let fut_rect = Rect::from_min_max(
                Pos2::new(nx, track_rect.min.y),
                Pos2::new(track_rect.max.x, track_rect.max.y),
            );
            painter.rect_filled(
                fut_rect,
                Rounding::ZERO,
                Color32::from_rgba_unmultiplied(9, 9, 11, 165),
            );
        }
        if now_wall_ms >= view_start_ms && (track_rect.min.x..=track_rect.max.x).contains(&raw_nx) {
            let mut y = track_rect.min.y;
            while y < track_rect.max.y {
                let y2 = (y + 3.5).min(track_rect.max.y);
                painter.line_segment(
                    [Pos2::new(raw_nx, y), Pos2::new(raw_nx, y2)],
                    Stroke::new(1.2_f32, STATUS_EMERALD),
                );
                y += 6.0;
            }
            let now_pill_x = if raw_nx + 28.0 <= track_rect.max.x {
                raw_nx + 3.0
            } else {
                (raw_nx - 27.0).max(track_rect.min.x + 2.0)
            };
            let now_pill = Rect::from_min_size(
                Pos2::new(now_pill_x, track_rect.min.y + 2.0),
                Vec2::new(24.0, 12.0),
            );
            painter.rect_filled(
                now_pill,
                Rounding::same(3.0),
                Color32::from_rgba_unmultiplied(9, 9, 11, 215),
            );
            painter.text(
                now_pill.center(),
                Align2::CENTER_CENTER,
                "NOW",
                FontId::new(8.0, FontFamily::Monospace),
                STATUS_EMERALD,
            );
        }
    }

    // 8. Magnetic Shift-Snap Ring/Line + Hover/Drag Ghost Needle & Rich Overlap Tooltip
    if let Some(hx) = active_scrub_x {
        if shift_held {
            if let Some((_, snap_x, snap_kind)) = snap_to_nearby_event(
                hx,
                &all_range_events,
                state.event_filter,
                canvas_rect.min.x,
                canvas_w,
                view_start_ms,
                state.window_span_ms,
            ) {
                let snap_col = event_kind_color(snap_kind);
                painter.line_segment(
                    [
                        Pos2::new(snap_x, ruler_rect.min.y),
                        Pos2::new(snap_x, track_rect.max.y),
                    ],
                    Stroke::new(1.8_f32, snap_col),
                );
                let ring_center = Pos2::new(snap_x, cov_bar_rect.center().y);
                painter.circle_filled(
                    ring_center,
                    6.0,
                    Color32::from_rgba_unmultiplied(snap_col.r(), snap_col.g(), snap_col.b(), 48),
                );
                painter.circle_stroke(ring_center, 5.0, Stroke::new(1.6_f32, snap_col));
            }
        }

        painter.line_segment(
            [
                Pos2::new(hx, ruler_rect.min.y),
                Pos2::new(hx, track_rect.max.y),
            ],
            Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 78)),
        );

        let cursor_ms = if state.dragging_playhead {
            state.last_scrub_ms.unwrap_or_else(|| {
                x_to_epoch_ms(
                    hx,
                    canvas_rect.min.x,
                    canvas_w,
                    view_start_ms,
                    state.window_span_ms,
                )
            })
        } else {
            x_to_epoch_ms(
                hx,
                canvas_rect.min.x,
                canvas_w,
                view_start_ms,
                state.window_span_ms,
            )
        };

        let rec_tag = if cursor_ms > now_wall_ms {
            " (Future)"
        } else if any_api_supported {
            let in_cov = range_timelines.iter().filter(|t| t.api_supported).any(|t| {
                t.coverage
                    .iter()
                    .any(|c| cursor_ms >= c.start_ms - 1_000 && cursor_ms <= c.end_ms + 1_000)
                    || t.events
                        .iter()
                        .any(|e| cursor_ms >= e.start_ms - 1_000 && cursor_ms <= e.end_ms + 1_000)
            });
            if in_cov {
                " (Recorded)"
            } else {
                " (Unrecorded Gap)"
            }
        } else if timeline.is_some_and(|t| !t.api_supported && t.note.is_some()) {
            " (Direct Seek)"
        } else {
            ""
        };

        let tip_text = if let Some(b_tip) = bracket_hover_tip {
            b_tip
        } else if !overlapping_events.is_empty() {
            let prim = overlapping_events[0];
            let prim_dt = epoch_ms_to_local_naive(prim.start_ms);
            let show_date =
                state.window_span_ms >= 14_400_000 || prim_dt.date() != state.selected_date;
            let ts_prefix = if show_date {
                format!("{} {}", prim_dt.format("%b %d"), prim_dt.format("%H:%M:%S"))
            } else {
                format!("{}", prim_dt.format("%H:%M:%S"))
            };
            let joined_ev = overlapping_events
                .iter()
                .take(4)
                .map(|e| {
                    format!(
                        "{} ({})",
                        event_kind_label(e.kind),
                        format_duration_compact(e.end_ms - e.start_ms)
                    )
                })
                .collect::<Vec<_>>()
                .join(" + ");
            format!(
                "{} - {}{}\nClick/Drag: Scrub | Shift+Click/Drag: Snap to Event",
                ts_prefix, joined_ev, rec_tag
            )
        } else {
            let c_dt = epoch_ms_to_local_naive(cursor_ms);
            let show_date =
                state.window_span_ms >= 14_400_000 || c_dt.date() != state.selected_date;
            let ts_str = if show_date {
                format!("{} {}", c_dt.format("%b %d"), c_dt.format("%H:%M:%S"))
            } else {
                format!("{}", c_dt.format("%H:%M:%S"))
            };
            let err_extra = retry_error_note
                .map(|e| format!("\nIndex warning: {}", truncate_single_line(e, 90)))
                .unwrap_or_default();
            format!(
                "{}{}\nScroll: Zoom | Click/Drag: Scrub | Shift+Click/Drag: Snap to Event | Right-Drag: Pan{}",
                ts_str, rec_tag, err_extra
            )
        };

        if resp.hovered() {
            resp.clone().on_hover_text(tip_text);
        } else if state.dragging_playhead {
            // Keep scrub preview timestamp pill visible during active drag even if pointer drifts vertically!
            let c_dt = epoch_ms_to_local_naive(cursor_ms);
            let pill_str = if state.window_span_ms >= 14_400_000
                || c_dt.date() != state.selected_date
            {
                format!(
                    "{} {}{}",
                    c_dt.format("%b %d"),
                    c_dt.format("%H:%M:%S"),
                    rec_tag
                )
            } else {
                format!("{}{}", c_dt.format("%H:%M:%S"), rec_tag)
            };
            let tip_painter = ui.ctx().layer_painter(LayerId::new(
                Order::Tooltip,
                egui::Id::new("obsidian_scrub_drag_preview_pill"),
            ));
            let font = FontId::new(11.0, FontFamily::Monospace);
            let galley = tip_painter.layout_no_wrap(pill_str, font, TEXT_PRIMARY);
            let pw = galley.size().x + 14.0;
            let ph = 22.0;
            let min_px = canvas_rect.min.x + 2.0;
            let px = (hx - 0.5 * pw).clamp(min_px, (canvas_rect.max.x - pw - 2.0).max(min_px));
            let py = (dock_rect.min.y - ph - 6.0).max(8.0);
            let pill_rect = Rect::from_min_size(Pos2::new(px, py), Vec2::new(pw, ph));
            tip_painter.rect_filled(
                pill_rect,
                Rounding::same(5.0),
                Color32::from_rgba_unmultiplied(9, 9, 11, 238),
            );
            tip_painter.rect_stroke(
                pill_rect,
                Rounding::same(5.0),
                Stroke::new(1.2_f32, STATUS_AMBER),
            );
            tip_painter.galley(
                Pos2::new(
                    pill_rect.center().x - 0.5 * galley.size().x,
                    pill_rect.center().y - 0.5 * galley.size().y,
                ),
                galley,
                TEXT_PRIMARY,
            );
        }
    }

    // 9. Bright Playhead Needle
    let disp_playhead_ms = if state.dragging_playhead {
        state.last_scrub_ms.unwrap_or(playhead_ms)
    } else {
        playhead_ms
    };
    if disp_playhead_ms >= view_start_ms && disp_playhead_ms <= view_end_ms {
        let px = epoch_ms_to_x(
            disp_playhead_ms,
            canvas_rect.min.x,
            canvas_w,
            view_start_ms,
            state.window_span_ms,
        );
        painter.line_segment(
            [
                Pos2::new(px, ruler_rect.min.y + 2.0),
                Pos2::new(px, canvas_rect.max.y),
            ],
            Stroke::new(2.0_f32, STATUS_AMBER),
        );
        let cap_pts = vec![
            Pos2::new(px - 5.0, ruler_rect.min.y + 1.0),
            Pos2::new(px + 5.0, ruler_rect.min.y + 1.0),
            Pos2::new(px, ruler_rect.min.y + 8.0),
        ];
        painter.add(egui::Shape::convex_polygon(
            cap_pts,
            STATUS_AMBER,
            Stroke::NONE,
        ));
    }
}

fn sort_events_for_painting(events: &mut [(&RecordedSpan, f32, f32)]) {
    events.sort_by(|(a, _, _), (b, _, _)| {
        a.kind
            .specificity_rank()
            .cmp(&b.kind.specificity_rank())
            .then_with(|| a.start_ms.cmp(&b.start_ms))
    });
}

fn truncate_single_line(s: &str, max_chars: usize) -> String {
    let first_line = s.lines().next().unwrap_or(s).trim();
    if first_line.chars().count() <= max_chars {
        first_line.to_string()
    } else {
        let taken: String = first_line
            .chars()
            .take(max_chars.saturating_sub(3))
            .collect();
        format!("{}...", taken)
    }
}

fn snap_to_nearby_event(
    cursor_x: f32,
    events: &[&RecordedSpan],
    filter: Option<NvrEventKind>,
    rect_min_x: f32,
    rect_width: f32,
    view_start_ms: i64,
    window_span_ms: i64,
) -> Option<(i64, f32, NvrEventKind)> {
    let mut best: Option<(f32, u8, i64, f32, NvrEventKind)> = None;
    for &ev in events {
        if !matches_event_filter(ev.kind, filter) {
            continue;
        }
        let ex = epoch_ms_to_x(
            ev.start_ms,
            rect_min_x,
            rect_width,
            view_start_ms,
            window_span_ms,
        );
        let dist = (cursor_x - ex).abs();
        let rank = ev.kind.specificity_rank();
        if dist <= 14.0
            && best
                .map(|(d, r, _, _, _)| dist < d - 1.5 || ((dist - d).abs() <= 1.5 && rank > r))
                .unwrap_or(true)
        {
            best = Some((dist, rank, (ev.start_ms - 2_000).max(0), ex, ev.kind));
        }
    }
    best.map(|(_, _, ms, ex, kind)| (ms, ex, kind))
}

fn render_time_jump_popover(
    ctx: &egui::Context,
    anchor_rect: Rect,
    state: &mut TimelineUiState,
    timeline: Option<&DayTimelineIndex>,
    playhead_ms: i64,
    now_wall_ms: i64,
    actions: &mut Vec<TimelineAction>,
) {
    let popup_w = 248.0;
    let pos = Pos2::new(
        anchor_rect.min.x.max(8.0),
        (anchor_rect.min.y - 8.0).max(168.0),
    );
    let mut close_popup = false;

    if ctx.input(|i| i.key_pressed(Key::Escape)) {
        state.show_time_jump_popup = false;
        return;
    }

    let enter_pressed = ctx.input(|i| i.key_pressed(Key::Enter));

    let area_resp = egui::Area::new(egui::Id::new("obsidian_timeline_time_jump_popup"))
        .order(Order::Foreground)
        .pivot(Align2::LEFT_BOTTOM)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            egui::Frame::none()
                .fill(GRAPHITE_SURFACE)
                .rounding(Rounding::same(8.0))
                .stroke(Stroke::new(
                    1.0_f32,
                    Color32::from_rgba_unmultiplied(56, 189, 248, 85),
                ))
                .inner_margin(egui::Margin::same(10.0))
                .show(ui, |ui| {
                    ui.set_width(popup_w - 20.0);

                    // Header row
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("Jump to Time")
                                .size(12.0)
                                .color(TEXT_PRIMARY)
                                .strong(),
                        );
                        ui.label(
                            egui::RichText::new(format!(
                                "({})",
                                state.selected_date.format("%b %d")
                            ))
                            .size(10.5)
                            .color(TEXT_MUTED),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if draw_compact_pill(ui, "X", false, ChipTone::Neutral, 20.0, 6.0)
                                .on_hover_text("Close (Esc)")
                                .clicked()
                            {
                                close_popup = true;
                            }
                        });
                    });

                    ui.add_space(6.0);

                    let mut submit_now = false;
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        let prev_txt = state.time_jump_input.clone();
                        let mut te_out = egui::TextEdit::singleline(&mut state.time_jump_input)
                            .desired_width(166.0)
                            .font(FontId::new(12.5, FontFamily::Monospace))
                            .hint_text("HH:MM:SS")
                            .show(ui);

                        if state.time_jump_needs_focus {
                            te_out.response.request_focus();
                            let len = state.time_jump_input.chars().count();
                            te_out.state.cursor.set_char_range(Some(
                                egui::text::CCursorRange::two(
                                    egui::text::CCursor::new(0),
                                    egui::text::CCursor::new(len),
                                ),
                            ));
                            te_out.state.store(ui.ctx(), te_out.response.id);
                            state.time_jump_needs_focus = false;
                        }

                        if state.time_jump_input != prev_txt && !enter_pressed {
                            state.time_jump_error = None;
                        }

                        if draw_compact_pill(ui, "Go", true, ChipTone::Emerald, 24.0, 9.0)
                            .on_hover_text("Jump to timestamp (Enter)")
                            .clicked()
                            || enter_pressed
                        {
                            submit_now = true;
                        }
                    });

                    if submit_now {
                        match parse_time_jump_hms(
                            &state.time_jump_input,
                            state.selected_date,
                            now_wall_ms,
                        ) {
                            Ok(target_ms) => {
                                state.recenter_on_playhead(target_ms);
                                actions.push(TimelineAction::Seek {
                                    epoch_ms: target_ms,
                                    immediate: true,
                                });
                                state.time_jump_error = None;
                                close_popup = true;
                            }
                            Err(err_msg) => {
                                state.time_jump_error = Some(err_msg);
                                state.time_jump_needs_focus = true;
                            }
                        }
                    }

                    if let Some(ref err) = state.time_jump_error {
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new(err).size(10.0).color(STATUS_CORAL));
                    }

                    ui.add_space(7.0);
                    ui.separator();
                    ui.add_space(5.0);

                    // Quick-jump chips
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = Vec2::new(4.0, 4.0);

                        if draw_compact_pill(ui, "Start of Day", false, ChipTone::Neutral, 21.0, 6.5)
                            .on_hover_text("Jump to 00:00:00 on selected date")
                            .clicked()
                        {
                            let start_dt = state.selected_date.and_hms_opt(0, 0, 0).unwrap();
                            let target_ms =
                                local_naive_to_epoch_ms(start_dt).clamp(0, now_wall_ms);
                            state.recenter_on_playhead(target_ms);
                            actions.push(TimelineAction::Seek {
                                epoch_ms: target_ms,
                                immediate: true,
                            });
                            close_popup = true;
                        }

                        if draw_compact_pill(ui, "-1h", false, ChipTone::Neutral, 21.0, 6.5)
                            .on_hover_text("Rewind 1 hour from playhead")
                            .clicked()
                        {
                            let target_ms = (playhead_ms - 3_600_000).clamp(0, now_wall_ms);
                            state.recenter_on_playhead(target_ms);
                            actions.push(TimelineAction::Seek {
                                epoch_ms: target_ms,
                                immediate: true,
                            });
                            close_popup = true;
                        }

                        if draw_compact_pill(ui, "-15m", false, ChipTone::Neutral, 21.0, 6.5)
                            .on_hover_text("Rewind 15 minutes from playhead")
                            .clicked()
                        {
                            let target_ms = (playhead_ms - 900_000).clamp(0, now_wall_ms);
                            state.recenter_on_playhead(target_ms);
                            actions.push(TimelineAction::Seek {
                                epoch_ms: target_ms,
                                immediate: true,
                            });
                            close_popup = true;
                        }

                        let latest_event_ms = timeline.and_then(|tl| {
                            tl.events
                                .iter()
                                .filter(|ev| matches_event_filter(ev.kind, state.event_filter))
                                .max_by_key(|ev| ev.start_ms)
                                .or_else(|| tl.events.iter().max_by_key(|ev| ev.start_ms))
                                .map(|ev| (ev.start_ms - 2_000).clamp(0, now_wall_ms))
                        });
                        if let Some(ev_ms) = latest_event_ms {
                            if draw_compact_pill(ui, "Last Event", false, ChipTone::Ice, 21.0, 6.5)
                                .on_hover_text("Jump to most recent event on selected date")
                                .clicked()
                            {
                                state.recenter_on_playhead(ev_ms);
                                state.auto_scroll_to_active_event = true;
                                actions.push(TimelineAction::Seek {
                                    epoch_ms: ev_ms,
                                    immediate: true,
                                });
                                close_popup = true;
                            }
                        }

                        if draw_compact_pill(
                            ui,
                            "Recenter Timeline",
                            false,
                            ChipTone::Ice,
                            21.0,
                            6.5,
                        )
                        .on_hover_text("Center timeline window on playhead and follow")
                        .clicked()
                        {
                            state.recenter_on_playhead(playhead_ms);
                            close_popup = true;
                        }
                    });
                });
        });

    let clicked_outside = ctx.input(|i| {
        i.pointer.any_pressed()
            && i.pointer
                .interact_pos()
                .or_else(|| i.pointer.latest_pos())
                .is_some_and(|p| {
                    !area_resp.response.rect.contains(p) && !anchor_rect.contains(p)
                })
    });
    if close_popup || clicked_outside {
        state.show_time_jump_popup = false;
    }
}

fn draw_fixed_header_btn(
    ui: &mut Ui,
    width: f32,
    height: f32,
    label: &str,
    enabled: bool,
) -> Response {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, height), sense);
    if enabled && resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let fill = if !enabled {
        OBSIDIAN_BG
    } else if resp.hovered() {
        ZINC_HOVER
    } else {
        ZINC_ELEVATED
    };
    let stroke_col = if enabled && resp.hovered() {
        Color32::from_rgba_unmultiplied(255, 255, 255, 56)
    } else {
        hairline_stroke()
    };
    let text_col = if !enabled {
        ZINC_DIM
    } else if resp.hovered() {
        TEXT_PRIMARY
    } else {
        TEXT_SECONDARY
    };
    let p = ui.painter();
    p.rect_filled(rect, Rounding::same(5.0), fill);
    p.rect_stroke(rect, Rounding::same(5.0), Stroke::new(1.0_f32, stroke_col));
    p.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::new(11.0, FontFamily::Proportional),
        text_col,
    );
    resp
}

#[allow(clippy::too_many_arguments)]
fn render_calendar_popup(
    ctx: &egui::Context,
    anchor_rect: Rect,
    state: &mut TimelineUiState,
    playback: &PlaybackManager,
    app_cfg: &AppConfig,
    channel: usize,
    actions: &mut Vec<TimelineAction>,
) {
    let popup_w = 236.0;
    let pos = Pos2::new(
        anchor_rect.min.x.max(8.0),
        (anchor_rect.min.y - 8.0).max(290.0),
    );
    let today = Local::now().date_naive();

    // Keyboard navigation inside Calendar Popup
    let mut close_popup = false;
    let mut cursor_date = state.calendar_cursor_date.unwrap_or(state.selected_date);
    let (esc_pressed, enter_pressed, day_delta) = ctx.input(|i| {
        let mut d = 0_i64;
        if i.key_pressed(Key::ArrowLeft) {
            d -= 1;
        }
        if i.key_pressed(Key::ArrowRight) {
            d += 1;
        }
        if i.key_pressed(Key::ArrowUp) {
            d -= 7;
        }
        if i.key_pressed(Key::ArrowDown) {
            d += 7;
        }
        (
            i.key_pressed(Key::Escape),
            i.key_pressed(Key::Enter),
            d,
        )
    });

    if esc_pressed {
        state.show_calendar_popup = false;
        return;
    }
    if day_delta != 0 {
        let next_cand = (cursor_date + ChronoDuration::days(day_delta)).min(today);
        cursor_date = next_cand;
        state.calendar_cursor_date = Some(cursor_date);
        state.calendar_ym = (cursor_date.year(), cursor_date.month());
    }
    if enter_pressed {
        let chosen = cursor_date.min(today);
        state.selected_date = chosen;
        state.calendar_ym = (chosen.year(), chosen.month());
        state.calendar_cursor_date = Some(chosen);
        state.clear_clip_range();
        actions.push(TimelineAction::ChangeDate(chosen));
        state.show_calendar_popup = false;
        return;
    }

    let (year, month) = state.calendar_ym;
    let recorded_days = playback
        .recorded_days_in_month(channel, year, month)
        .unwrap_or_default();
    let month_loading = playback.is_month_days_loading(channel, year, month);
    let month_err = playback.month_days_error(channel, year, month);

    let area_resp = egui::Area::new(egui::Id::new("obsidian_timeline_calendar_popup"))
        .order(Order::Foreground)
        .pivot(Align2::LEFT_BOTTOM)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            egui::Frame::none()
                .fill(GRAPHITE_SURFACE)
                .rounding(Rounding::same(8.0))
                .stroke(Stroke::new(
                    1.0_f32,
                    Color32::from_rgba_unmultiplied(255, 255, 255, 38),
                ))
                .inner_margin(egui::Margin::same(10.0))
                .show(ui, |ui| {
                    // Inner width strictly 216.0px (popup_w 236.0 - 20.0 margin)
                    ui.set_width(popup_w - 20.0);

                    // Fixed-width Month Header: `<` (24px) + Month/Year (132px) + `>` (24px) + `X` (22px) + 3*4px spacing = 214px <= 216px!
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        if draw_fixed_header_btn(ui, 24.0, 22.0, "<", true)
                            .on_hover_text("Previous month")
                            .clicked()
                        {
                            if month == 1 {
                                state.calendar_ym = (year - 1, 12);
                            } else {
                                state.calendar_ym = (year, month - 1);
                            }
                        }

                        let month_name = match month {
                            1 => "January",
                            2 => "February",
                            3 => "March",
                            4 => "April",
                            5 => "May",
                            6 => "June",
                            7 => "July",
                            8 => "August",
                            9 => "September",
                            10 => "October",
                            11 => "November",
                            _ => "December",
                        };
                        let (lbl_rect, _) =
                            ui.allocate_exact_size(Vec2::new(132.0, 22.0), Sense::hover());
                        ui.painter().text(
                            lbl_rect.center(),
                            Align2::CENTER_CENTER,
                            format!("{} {}", month_name, year),
                            FontId::new(12.0, FontFamily::Proportional),
                            TEXT_PRIMARY,
                        );

                        let can_next_month = (year, month) < (today.year(), today.month());
                        let next_m_tip = if can_next_month {
                            "Next month"
                        } else {
                            "Current month"
                        };
                        if draw_fixed_header_btn(ui, 24.0, 22.0, ">", can_next_month)
                            .on_hover_text(next_m_tip)
                            .clicked()
                            && can_next_month
                        {
                            if month == 12 {
                                state.calendar_ym = (year + 1, 1);
                            } else {
                                state.calendar_ym = (year, month + 1);
                            }
                        }

                        if draw_fixed_header_btn(ui, 22.0, 22.0, "X", true)
                            .on_hover_text("Close Calendar (Esc)")
                            .clicked()
                        {
                            close_popup = true;
                        }
                    });

                    // Camera subtitle
                    let cam_disp = truncate_single_line(&app_cfg.camera_display_name(channel), 28);
                    ui.add_space(3.0);
                    ui.label(
                        egui::RichText::new(format!("Ch {:02} - {}", channel, cam_disp))
                            .size(10.0)
                            .color(TEXT_MUTED),
                    );
                    ui.add_space(4.0);

                    // Weekday & day grid
                    egui::Grid::new("obsidian_cal_dow_grid")
                        .spacing(Vec2::new(2.0, 3.0))
                        .min_col_width(28.0)
                        .show(ui, |ui| {
                            for dow in ["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"] {
                                let (r, _) =
                                    ui.allocate_exact_size(Vec2::new(28.0, 16.0), Sense::hover());
                                ui.painter().text(
                                    r.center(),
                                    Align2::CENTER_CENTER,
                                    dow,
                                    FontId::new(10.0, FontFamily::Proportional),
                                    TEXT_MUTED,
                                );
                            }
                            ui.end_row();

                            let first_day = NaiveDate::from_ymd_opt(year, month, 1)
                                .unwrap_or(state.selected_date);
                            let lead_blanks = first_day.weekday().num_days_from_sunday() as usize;
                            let days_in_m = days_in_month(year, month);

                            let mut col_idx = 0usize;
                            for _ in 0..lead_blanks {
                                ui.allocate_exact_size(Vec2::new(28.0, 24.0), Sense::hover());
                                col_idx += 1;
                            }
                            for day in 1..=days_in_m {
                                let Some(cand_date) = NaiveDate::from_ymd_opt(year, month, day)
                                else {
                                    continue;
                                };
                                let is_future = cand_date > today;
                                let is_sel = cand_date == state.selected_date;
                                let is_kbd_focus =
                                    cand_date == cursor_date && cursor_date != state.selected_date;
                                let is_today = cand_date == today;
                                let has_rec = recorded_days.contains(&day);

                                let sense = if is_future {
                                    Sense::hover()
                                } else {
                                    Sense::click()
                                };
                                let (cell_rect, cell_resp) =
                                    ui.allocate_exact_size(Vec2::new(28.0, 24.0), sense);
                                if !is_future && cell_resp.hovered() {
                                    ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                                }

                                let p = ui.painter();
                                if is_sel {
                                    p.rect_filled(
                                        cell_rect,
                                        Rounding::same(5.0),
                                        Color32::from_rgba_unmultiplied(245, 158, 11, 55),
                                    );
                                    p.rect_stroke(
                                        cell_rect,
                                        Rounding::same(5.0),
                                        Stroke::new(1.2_f32, STATUS_AMBER),
                                    );
                                } else if cell_resp.hovered() && !is_future {
                                    p.rect_filled(cell_rect, Rounding::same(5.0), ZINC_HOVER);
                                }

                                if is_kbd_focus {
                                    p.rect_stroke(
                                        cell_rect,
                                        Rounding::same(5.0),
                                        Stroke::new(
                                            1.0_f32,
                                            Color32::from_rgba_unmultiplied(255, 255, 255, 120),
                                        ),
                                    );
                                }

                                // Keep `today` Ice ring ALWAYS visible even when hovered or selected!
                                if is_today {
                                    let ring_rect = if is_sel {
                                        cell_rect.shrink(1.8)
                                    } else {
                                        cell_rect
                                    };
                                    p.rect_stroke(
                                        ring_rect,
                                        Rounding::same(4.0),
                                        Stroke::new(1.1_f32, STATUS_ICE),
                                    );
                                }

                                let txt_col = if is_future {
                                    ZINC_DIM
                                } else if is_sel || has_rec {
                                    TEXT_PRIMARY
                                } else {
                                    TEXT_MUTED
                                };
                                p.text(
                                    cell_rect.center() - Vec2::new(0.0, 2.0),
                                    Align2::CENTER_CENTER,
                                    format!("{}", day),
                                    FontId::new(11.0, FontFamily::Proportional),
                                    txt_col,
                                );

                                if has_rec {
                                    p.circle_filled(
                                        Pos2::new(cell_rect.center().x, cell_rect.max.y - 3.5),
                                        2.3,
                                        STATUS_EMERALD,
                                    );
                                }

                                if !is_future && cell_resp.clicked() {
                                    state.selected_date = cand_date;
                                    state.calendar_cursor_date = Some(cand_date);
                                    state.clear_clip_range();
                                    actions.push(TimelineAction::ChangeDate(cand_date));
                                    close_popup = true;
                                }

                                col_idx += 1;
                                #[allow(clippy::manual_is_multiple_of)]
                                if col_idx % 7 == 0 && day < days_in_m {
                                    ui.end_row();
                                }
                            }
                        });

                    // Footer row: [Today] jump pill + green dot legend / loading / error retry
                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if draw_compact_pill(
                            ui,
                            "Today",
                            state.selected_date == today,
                            ChipTone::Ice,
                            20.0,
                            7.0,
                        )
                        .on_hover_text("Jump to today's recordings")
                        .clicked()
                        {
                            state.selected_date = today;
                            state.calendar_ym = (today.year(), today.month());
                            state.calendar_cursor_date = Some(today);
                            state.clear_clip_range();
                            actions.push(TimelineAction::ChangeDate(today));
                            close_popup = true;
                        }

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if month_loading {
                                ui.label(
                                    egui::RichText::new("Scanning...")
                                        .size(10.0)
                                        .color(TEXT_MUTED),
                                );
                            } else if let Some(ref err) = month_err {
                                if draw_compact_pill(ui, "Retry", true, ChipTone::Coral, 20.0, 6.5)
                                    .on_hover_text(format!(
                                        "Failed to query recorded days: {}\nClick to retry",
                                        err
                                    ))
                                    .clicked()
                                {
                                    actions.push(TimelineAction::RefreshTimeline);
                                }
                                ui.label(
                                    egui::RichText::new("Scan err")
                                        .size(9.5)
                                        .color(STATUS_AMBER),
                                );
                            } else {
                                ui.label(
                                    egui::RichText::new("Recorded")
                                        .size(10.0)
                                        .color(TEXT_MUTED),
                                );
                                let (dot_r, _) =
                                    ui.allocate_exact_size(Vec2::new(8.0, 14.0), Sense::hover());
                                ui.painter()
                                    .circle_filled(dot_r.center(), 2.6, STATUS_EMERALD);
                            }
                        });
                    });
                });
        });

    let clicked_outside = ctx.input(|i| {
        i.pointer.any_pressed()
            && i.pointer
                .interact_pos()
                .or_else(|| i.pointer.latest_pos())
                .is_some_and(|p| {
                    !area_resp.response.rect.contains(p) && !anchor_rect.contains(p)
                })
    });
    if close_popup || clicked_outside {
        state.show_calendar_popup = false;
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (ny, nm) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let next_first =
        NaiveDate::from_ymd_opt(ny, nm, 1).unwrap_or_else(|| NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
    next_first.pred_opt().map(|d| d.day()).unwrap_or(30)
}

/// Renders the resizable Right-Side Collapsible "What Happened" Event Drawer with inline filtering,
/// active-event auto-scroll, and 1-click `[Trim]` / `[MP4]` event card actions.
pub fn render_event_drawer(
    ctx: &egui::Context,
    state: &mut TimelineUiState,
    playback: &PlaybackManager,
    app_cfg: &AppConfig,
    playhead_epoch_ms: i64,
) -> Vec<TimelineAction> {
    let mut actions = Vec::new();
    if !state.show_event_drawer {
        return actions;
    }
    let now_wall_ms = current_wall_ms();
    let primary_ch = playback.primary_channel().unwrap_or(1);
    let cam_name = app_cfg.camera_display_name(primary_ch);
    let timeline = playback.day_timeline(primary_ch, state.selected_date);
    let timeline_loading = playback.is_timeline_loading(primary_ch, state.selected_date);

    egui::SidePanel::right("obsidian_timeline_event_drawer")
        .resizable(true)
        .default_width(EVENT_DRAWER_WIDTH)
        .width_range(200.0..=380.0)
        .frame(
            egui::Frame::none()
                .fill(GRAPHITE_SURFACE)
                .stroke(Stroke::new(1.0_f32, hairline_stroke()))
                .inner_margin(egui::Margin::same(10.0)),
        )
        .show(ctx, |ui| {
            // Header
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("What Happened")
                        .size(13.0)
                        .color(TEXT_PRIMARY)
                        .strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    if draw_compact_pill(ui, "X", false, ChipTone::Neutral, 21.0, 6.0)
                        .on_hover_text("Close Event Drawer (E)")
                        .clicked()
                    {
                        state.show_event_drawer = false;
                    }
                    let sort_lbl = if state.drawer_newest_first {
                        "Newest"
                    } else {
                        "Oldest"
                    };
                    if draw_compact_pill(ui, sort_lbl, false, ChipTone::Neutral, 21.0, 6.0)
                        .on_hover_text("Toggle chronological vs newest-first event order")
                        .clicked()
                    {
                        state.drawer_newest_first = !state.drawer_newest_first;
                    }
                    if draw_compact_pill(ui, "Now", false, ChipTone::Ice, 21.0, 6.0)
                        .on_hover_text("Scroll to event nearest current playhead")
                        .clicked()
                    {
                        state.auto_scroll_to_active_event = true;
                    }
                });
            });

            ui.add_space(3.0);
            ui.label(
                egui::RichText::new(format!(
                    "{} (Ch {}) - {}",
                    truncate_single_line(&cam_name, 22),
                    primary_ch,
                    state.selected_date.format("%b %d, %Y")
                ))
                .size(10.5)
                .color(TEXT_MUTED),
            );

            // Per-type counts & compact inline filter bar inside drawer (works even on narrow <1000px windows!)
            let (mut all_cnt, mut hum_cnt, mut veh_cnt, mut alm_cnt, mut mot_cnt) =
                (0usize, 0usize, 0usize, 0usize, 0usize);
            if let Some(ref tl) = timeline {
                if tl.api_supported {
                    for ev in &tl.events {
                        match ev.kind {
                            NvrEventKind::Continuous => {}
                            NvrEventKind::Human => {
                                all_cnt += 1;
                                hum_cnt += 1;
                            }
                            NvrEventKind::Vehicle => {
                                all_cnt += 1;
                                veh_cnt += 1;
                            }
                            NvrEventKind::Intrusion | NvrEventKind::Alarm => {
                                all_cnt += 1;
                                alm_cnt += 1;
                            }
                            NvrEventKind::Motion => {
                                all_cnt += 1;
                                mot_cnt += 1;
                            }
                        }
                    }
                }
            }

            ui.add_space(5.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(3.5, 3.5);
                if draw_compact_pill(
                    ui,
                    &format!("All {}", all_cnt),
                    state.event_filter.is_none(),
                    ChipTone::Neutral,
                    20.0,
                    5.5,
                )
                .on_hover_text("Show all events")
                .clicked()
                {
                    state.event_filter = None;
                    state.auto_scroll_to_active_event = true;
                }
                if draw_compact_pill_with_dot(
                    ui,
                    &format!("Hum {}", hum_cnt),
                    Some(STATUS_ICE),
                    state.event_filter == Some(NvrEventKind::Human),
                    ChipTone::Ice,
                    20.0,
                    5.0,
                )
                .on_hover_text("Filter Human events")
                .clicked()
                {
                    state.event_filter = if state.event_filter == Some(NvrEventKind::Human) {
                        None
                    } else {
                        Some(NvrEventKind::Human)
                    };
                    state.auto_scroll_to_active_event = true;
                }
                if draw_compact_pill_with_dot(
                    ui,
                    &format!("Veh {}", veh_cnt),
                    Some(STATUS_VIOLET),
                    state.event_filter == Some(NvrEventKind::Vehicle),
                    ChipTone::Violet,
                    20.0,
                    5.0,
                )
                .on_hover_text("Filter Vehicle events")
                .clicked()
                {
                    state.event_filter = if state.event_filter == Some(NvrEventKind::Vehicle) {
                        None
                    } else {
                        Some(NvrEventKind::Vehicle)
                    };
                    state.auto_scroll_to_active_event = true;
                }
                let alm_sel = matches!(
                    state.event_filter,
                    Some(NvrEventKind::Intrusion | NvrEventKind::Alarm)
                );
                if draw_compact_pill_with_dot(
                    ui,
                    &format!("Alm {}", alm_cnt),
                    Some(STATUS_CORAL),
                    alm_sel,
                    ChipTone::Coral,
                    20.0,
                    5.0,
                )
                .on_hover_text("Filter Intrusion & Alarm events")
                .clicked()
                {
                    state.event_filter = if alm_sel {
                        None
                    } else {
                        Some(NvrEventKind::Intrusion)
                    };
                    state.auto_scroll_to_active_event = true;
                }
                if draw_compact_pill_with_dot(
                    ui,
                    &format!("Mot {}", mot_cnt),
                    Some(STATUS_AMBER),
                    state.event_filter == Some(NvrEventKind::Motion),
                    ChipTone::Amber,
                    20.0,
                    5.0,
                )
                .on_hover_text("Filter Motion events")
                .clicked()
                {
                    state.event_filter = if state.event_filter == Some(NvrEventKind::Motion) {
                        None
                    } else {
                        Some(NvrEventKind::Motion)
                    };
                    state.auto_scroll_to_active_event = true;
                }
            });

            ui.add_space(5.0);
            ui.separator();
            ui.add_space(4.0);

            if timeline.is_none() || (timeline_loading && timeline.is_none()) {
                ui.vertical_centered(|ui| {
                    ui.add_space(36.0);
                    ui.label(
                        egui::RichText::new("Loading day events...")
                            .size(11.5)
                            .color(TEXT_MUTED),
                    );
                });
                return;
            }
            let tl = timeline.as_ref().unwrap();

            if !tl.api_supported {
                ui.vertical_centered(|ui| {
                    ui.add_space(24.0);
                    ui.label(
                        egui::RichText::new("Event index unavailable")
                            .size(11.5)
                            .color(TEXT_SECONDARY),
                    );
                    if let Some(ref note) = tl.note {
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new(note).size(10.0).color(TEXT_MUTED));
                    }
                    ui.add_space(10.0);
                    if draw_compact_pill(ui, "Retry", true, ChipTone::Ice, 24.0, 10.0)
                        .on_hover_text("Retry querying day timeline from NVR")
                        .clicked()
                    {
                        actions.push(TimelineAction::RefreshTimeline);
                    }
                });
                return;
            }

            let mut events: Vec<&RecordedSpan> = tl
                .events
                .iter()
                .filter(|ev| matches_event_filter(ev.kind, state.event_filter))
                .collect();

            if state.drawer_newest_first {
                events.sort_by_key(|a| std::cmp::Reverse(a.start_ms));
            } else {
                events.sort_by_key(|a| a.start_ms);
            }

            if events.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(32.0);
                    if let Some(flt_kind) = state.event_filter {
                        let kind_name = if matches!(
                            flt_kind,
                            NvrEventKind::Intrusion | NvrEventKind::Alarm
                        ) {
                            "Alarm"
                        } else {
                            event_kind_label(flt_kind)
                        };
                        ui.label(
                            egui::RichText::new(format!(
                                "No {} events on this date ({} total)",
                                kind_name, all_cnt
                            ))
                            .size(11.0)
                            .color(TEXT_MUTED),
                        );
                        ui.add_space(8.0);
                        if draw_compact_pill(
                            ui,
                            "Clear Filter",
                            true,
                            ChipTone::Ice,
                            23.0,
                            9.0,
                        )
                        .clicked()
                        {
                            state.event_filter = None;
                            state.auto_scroll_to_active_event = true;
                        }
                    } else {
                        ui.label(
                            egui::RichText::new("No events recorded on this date")
                                .size(11.5)
                                .color(TEXT_MUTED),
                        );
                    }
                });
                return;
            }

            // Identify active or nearest event index for on-demand / auto-scroll
            let current_idx = events.iter().position(|ev| {
                playhead_epoch_ms >= ev.start_ms.saturating_sub(2_500)
                    && playhead_epoch_ms <= ev.end_ms.saturating_add(1_000)
            });
            let scroll_target_idx = current_idx.or_else(|| {
                events
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, ev)| ev.start_ms.abs_diff(playhead_epoch_ms))
                    .map(|(idx, _)| idx)
            });

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 5.0;
                    let ptr_pos = ui.input(|i| {
                        i.pointer
                            .hover_pos()
                            .or_else(|| i.pointer.interact_pos())
                    });

                    for (idx, ev) in events.into_iter().enumerate() {
                        let card_w = ui.available_width().max(176.0);
                        let (card_rect, card_resp) =
                            ui.allocate_exact_size(Vec2::new(card_w, 44.0), Sense::click());

                        let is_current = playhead_epoch_ms >= ev.start_ms.saturating_sub(2_500)
                            && playhead_epoch_ms <= ev.end_ms.saturating_add(1_000);
                        if state.auto_scroll_to_active_event
                            && (is_current || scroll_target_idx == Some(idx))
                        {
                            card_resp.scroll_to_me(Some(egui::Align::Center));
                            state.auto_scroll_to_active_event = false;
                        }

                        let card_hov = card_resp.hovered();
                        if card_hov {
                            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                        }

                        let mp4_rect = Rect::from_min_size(
                            Pos2::new(card_rect.max.x - 38.0, card_rect.max.y - 20.0),
                            Vec2::new(32.0, 16.0),
                        );
                        let trim_rect = Rect::from_min_size(
                            Pos2::new(mp4_rect.min.x - 3.0 - 34.0, card_rect.max.y - 20.0),
                            Vec2::new(34.0, 16.0),
                        );
                        let trim_hov = card_hov && ptr_pos.is_some_and(|p| trim_rect.contains(p));
                        let mp4_hov = card_hov && ptr_pos.is_some_and(|p| mp4_rect.contains(p));

                        let kind_col = event_kind_color(ev.kind);
                        let fill = if is_current {
                            Color32::from_rgba_unmultiplied(
                                kind_col.r(),
                                kind_col.g(),
                                kind_col.b(),
                                34,
                            )
                        } else if card_hov {
                            ZINC_HOVER
                        } else {
                            ZINC_ELEVATED
                        };
                        let stroke_col = if is_current {
                            kind_col
                        } else if card_hov {
                            Color32::from_rgba_unmultiplied(255, 255, 255, 44)
                        } else {
                            hairline_stroke()
                        };

                        let p = ui.painter();
                        p.rect_filled(card_rect, Rounding::same(6.0), fill);
                        p.rect_stroke(
                            card_rect,
                            Rounding::same(6.0),
                            Stroke::new(1.0_f32, stroke_col),
                        );

                        // Color dot + HH:MM:SS timestamp
                        let dt_start = epoch_ms_to_local_naive(ev.start_ms);
                        let dt_end = epoch_ms_to_local_naive(ev.end_ms);
                        let ts_str = dt_start.format("%H:%M:%S").to_string();
                        p.circle_filled(
                            Pos2::new(card_rect.min.x + 11.0, card_rect.min.y + 14.0),
                            3.8,
                            kind_col,
                        );
                        p.text(
                            Pos2::new(card_rect.min.x + 20.0, card_rect.min.y + 14.0),
                            Align2::LEFT_CENTER,
                            &ts_str,
                            FontId::new(11.5, FontFamily::Monospace),
                            TEXT_PRIMARY,
                        );

                        // Duration pill on top-right
                        let dur_str = format_duration_compact(ev.end_ms - ev.start_ms);
                        p.text(
                            Pos2::new(card_rect.max.x - 8.0, card_rect.min.y + 14.0),
                            Align2::RIGHT_CENTER,
                            &dur_str,
                            FontId::new(10.5, FontFamily::Monospace),
                            TEXT_MUTED,
                        );

                        // Kind badge on bottom-left
                        p.text(
                            Pos2::new(card_rect.min.x + 20.0, card_rect.min.y + 31.0),
                            Align2::LEFT_CENTER,
                            event_kind_label(ev.kind),
                            FontId::new(10.5, FontFamily::Proportional),
                            kind_col,
                        );

                        // Bottom-right: [Trim] + [MP4] micro-buttons on hover, "Jump ->" otherwise
                        if card_hov {
                            p.rect_filled(
                                trim_rect,
                                Rounding::same(3.5),
                                if trim_hov {
                                    Color32::from_rgba_unmultiplied(56, 189, 248, 68)
                                } else {
                                    OBSIDIAN_BG
                                },
                            );
                            p.rect_stroke(
                                trim_rect,
                                Rounding::same(3.5),
                                Stroke::new(
                                    1.0_f32,
                                    if trim_hov { STATUS_ICE } else { hairline_stroke() },
                                ),
                            );
                            p.text(
                                trim_rect.center(),
                                Align2::CENTER_CENTER,
                                "Trim",
                                FontId::new(9.5, FontFamily::Proportional),
                                if trim_hov { TEXT_PRIMARY } else { STATUS_ICE },
                            );

                            p.rect_filled(
                                mp4_rect,
                                Rounding::same(3.5),
                                if mp4_hov {
                                    Color32::from_rgba_unmultiplied(16, 185, 129, 72)
                                } else {
                                    OBSIDIAN_BG
                                },
                            );
                            p.rect_stroke(
                                mp4_rect,
                                Rounding::same(3.5),
                                Stroke::new(
                                    1.0_f32,
                                    if mp4_hov {
                                        STATUS_EMERALD
                                    } else {
                                        hairline_stroke()
                                    },
                                ),
                            );
                            p.text(
                                mp4_rect.center(),
                                Align2::CENTER_CENTER,
                                "MP4",
                                FontId::new(9.5, FontFamily::Proportional),
                                if mp4_hov {
                                    TEXT_PRIMARY
                                } else {
                                    STATUS_EMERALD
                                },
                            );
                        } else {
                            p.text(
                                Pos2::new(card_rect.max.x - 8.0, card_rect.min.y + 31.0),
                                Align2::RIGHT_CENTER,
                                "Jump ->",
                                FontId::new(10.0, FontFamily::Proportional),
                                ZINC_DIM,
                            );
                        }

                        // Right-click context menu for 1-click Jump, Trim, and MP4 Export
                        card_resp.context_menu(|ui| {
                            ui.set_min_width(168.0);
                            if ui.button("Jump to Event (-2s)").clicked() {
                                actions.push(TimelineAction::Seek {
                                    epoch_ms: (ev.start_ms - 2_000).clamp(0, now_wall_ms),
                                    immediate: true,
                                });
                                ui.close_menu();
                            }
                            if ui.button("Trim Clip [In..Out] (+/-2s)").clicked() {
                                let in_t = (ev.start_ms - 2_000).max(0);
                                let out_t = (ev.end_ms + 2_000).min(now_wall_ms).max(in_t + 500);
                                state.set_clip_in_clamped(in_t, now_wall_ms);
                                state.set_clip_out_clamped(out_t, now_wall_ms);
                                actions.push(TimelineAction::Seek {
                                    epoch_ms: in_t.min(now_wall_ms),
                                    immediate: true,
                                });
                                ui.close_menu();
                            }
                            if ui.button("Export Event MP4 (+/-3s)").clicked() {
                                let in_t = (ev.start_ms - 3_000).max(0);
                                let out_t = (ev.end_ms + 3_000).min(now_wall_ms).max(in_t + 1_000);
                                actions.push(TimelineAction::ExportClip {
                                    in_ms: in_t,
                                    out_ms: out_t,
                                });
                                ui.close_menu();
                            }
                        });

                        let stream_qual = if ev.main_stream { "Main HD" } else { "Sub SD" };
                        let size_info = if ev.byte_len > 0 {
                            format!(" - {:.1} MB", ev.byte_len as f64 / (1024.0 * 1024.0))
                        } else {
                            String::new()
                        };
                        let action_hint = if trim_hov {
                            "Click [Trim] to mark [In..Out] (+/-2s) and seek to event"
                        } else if mp4_hov {
                            "Click [MP4] to export event clip immediately (+/-3s)"
                        } else {
                            "Click to jump 2s before event | Right-click for Trim / MP4"
                        };
                        let card_tip = format!(
                            "{} -> {} ({}) - {}\nStream: {}{}\n{}",
                            dt_start.format("%H:%M:%S"),
                            dt_end.format("%H:%M:%S"),
                            dur_str,
                            event_kind_label(ev.kind),
                            stream_qual,
                            size_info,
                            action_hint
                        );

                        if card_resp.on_hover_text(card_tip).clicked() {
                            let click_p = ui.input(|i| i.pointer.interact_pos());
                            if click_p.is_some_and(|cp| trim_rect.contains(cp)) {
                                let in_t = (ev.start_ms - 2_000).max(0);
                                let out_t = (ev.end_ms + 2_000).min(now_wall_ms).max(in_t + 500);
                                state.set_clip_in_clamped(in_t, now_wall_ms);
                                state.set_clip_out_clamped(out_t, now_wall_ms);
                                actions.push(TimelineAction::Seek {
                                    epoch_ms: in_t.min(now_wall_ms),
                                    immediate: true,
                                });
                            } else if click_p.is_some_and(|cp| mp4_rect.contains(cp)) {
                                let in_t = (ev.start_ms - 3_000).max(0);
                                let out_t = (ev.end_ms + 3_000).min(now_wall_ms).max(in_t + 1_000);
                                actions.push(TimelineAction::ExportClip {
                                    in_ms: in_t,
                                    out_ms: out_t,
                                });
                            } else {
                                actions.push(TimelineAction::Seek {
                                    epoch_ms: (ev.start_ms - 2_000).clamp(0, now_wall_ms),
                                    immediate: true,
                                });
                            }
                        }
                    }
                });
        });

    actions
}

/// Paints the movable, hideable, aspect-fitted `196x110` Single-View Live Picture-in-Picture
/// corner window without allocating a competing `ui.interact` widget so the underlying camera
/// tile retains unbroken hover, HUD, and right-click context menu behavior.
#[allow(clippy::too_many_arguments)]
pub fn render_live_pip_overlay(
    ui: &mut Ui,
    pip_rect: Rect,
    live_texture: Option<&TextureHandle>,
    tex_w: u32,
    tex_h: u32,
    aspect_fit_16_9: bool,
    live_status: StreamStatus,
    hovered_pos: Option<Pos2>,
    clicked_pos: Option<Pos2>,
) -> Option<LivePipAction> {
    let hovered = hovered_pos.is_some_and(|p| pip_rect.contains(p));
    if hovered {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }

    let hide_btn_rect = Rect::from_min_size(
        Pos2::new(pip_rect.max.x - 6.0 - 18.0, pip_rect.min.y + 6.0),
        Vec2::new(18.0, 18.0),
    );
    let move_btn_rect = Rect::from_min_size(
        Pos2::new(hide_btn_rect.min.x - 4.0 - 18.0, pip_rect.min.y + 6.0),
        Vec2::new(18.0, 18.0),
    );
    let hov_hide = hovered && hovered_pos.is_some_and(|p| hide_btn_rect.contains(p));
    let hov_move = hovered && hovered_pos.is_some_and(|p| move_btn_rect.contains(p));

    let painter = ui.painter_at(pip_rect.expand(6.0));

    // Subtle dark drop shadow behind glass PiP frame
    let shadow_rect = pip_rect.translate(Vec2::new(0.0, 3.0)).expand(1.5);
    painter.rect_filled(
        shadow_rect,
        Rounding::same(8.0),
        Color32::from_rgba_unmultiplied(0, 0, 0, 140),
    );

    painter.rect_filled(pip_rect, Rounding::same(7.0), OBSIDIAN_BG);

    let inner_video_rect = pip_rect.shrink(1.5);
    if let Some(tex) = live_texture {
        let eff_w = if tex_w > 0 {
            tex_w as f32
        } else {
            tex.size()[0].max(1) as f32
        };
        let eff_h = if tex_h > 0 {
            tex_h as f32
        } else {
            tex.size()[1].max(1) as f32
        };
        let src_aspect = (eff_w / eff_h).clamp(0.2, 5.0);
        let dst_w = inner_video_rect.width().max(1.0);
        let dst_h = inner_video_rect.height().max(1.0);
        let dst_aspect = dst_w / dst_h;

        let (draw_rect, uv_rect) = if aspect_fit_16_9 {
            // Letterbox / pillarbox inside inner_video_rect preserving exact source aspect ratio
            let (fit_w, fit_h) = if src_aspect > dst_aspect {
                (dst_w, (dst_w / src_aspect).max(1.0))
            } else {
                ((dst_h * src_aspect).max(1.0), dst_h)
            };
            (
                Rect::from_center_size(inner_video_rect.center(), Vec2::new(fit_w, fit_h)),
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            )
        } else {
            // Aspect-preserving center-crop UV (never stretches non-16:9 streams!)
            let uv = if src_aspect > dst_aspect {
                let vis_frac = (dst_aspect / src_aspect).clamp(0.05, 1.0);
                let margin_u = 0.5 * (1.0 - vis_frac);
                Rect::from_min_max(Pos2::new(margin_u, 0.0), Pos2::new(1.0 - margin_u, 1.0))
            } else {
                let vis_frac = (src_aspect / dst_aspect).clamp(0.05, 1.0);
                let margin_v = 0.5 * (1.0 - vis_frac);
                Rect::from_min_max(Pos2::new(0.0, margin_v), Pos2::new(1.0, 1.0 - margin_v))
            };
            (inner_video_rect, uv)
        };

        painter.image(tex.id(), draw_rect, uv_rect, Color32::WHITE);
    } else {
        painter.rect_filled(
            inner_video_rect,
            Rounding::same(6.0),
            Color32::from_rgb(14, 14, 18),
        );
        let standby_lbl = match live_status {
            StreamStatus::Paused => "Live Paused",
            StreamStatus::Connecting | StreamStatus::Reconnecting => "Connecting...",
            StreamStatus::Error | StreamStatus::AuthError => "Live Offline",
            StreamStatus::Live => "Live Standby",
        };
        painter.text(
            inner_video_rect.center(),
            Align2::CENTER_CENTER,
            standby_lbl,
            FontId::new(11.0, FontFamily::Proportional),
            TEXT_MUTED,
        );
    }

    // Dark glass border (glows Emerald on hover)
    let border_col = if hov_hide {
        STATUS_CORAL
    } else if hov_move {
        STATUS_ICE
    } else if hovered {
        STATUS_EMERALD
    } else {
        Color32::from_rgba_unmultiplied(255, 255, 255, 58)
    };
    painter.rect_stroke(
        pip_rect,
        Rounding::same(7.0),
        Stroke::new(if hovered { 1.6_f32 } else { 1.1_f32 }, border_col),
    );

    // Top-left live status badge inside PiP
    let (badge_txt, badge_col, pulse_on) = match live_status {
        StreamStatus::Live => ("LIVE", STATUS_EMERALD, true),
        StreamStatus::Paused => ("PAUSED", STATUS_AMBER, false),
        StreamStatus::Connecting | StreamStatus::Reconnecting => ("SYNC..", STATUS_AMBER, true),
        StreamStatus::Error | StreamStatus::AuthError => ("OFFLINE", STATUS_CORAL, false),
    };
    let pulse = if pulse_on {
        ((ui.input(|i| i.time) * 4.5).sin() as f32 * 0.5 + 0.5).clamp(0.0, 1.0)
    } else {
        0.75
    };
    let dot_alpha = (165.0 + pulse * 90.0) as u8;
    let badge_font = FontId::new(10.0, FontFamily::Proportional);
    let badge_galley =
        painter.layout_no_wrap(badge_txt.to_string(), badge_font.clone(), TEXT_PRIMARY);
    let pill_w = (badge_galley.size().x + 22.0).max(48.0);
    let live_pill = Rect::from_min_size(pip_rect.min + Vec2::new(6.0, 6.0), Vec2::new(pill_w, 18.0));
    painter.rect_filled(
        live_pill,
        Rounding::same(4.5),
        Color32::from_rgba_unmultiplied(9, 9, 11, 185),
    );
    painter.circle_filled(
        Pos2::new(live_pill.min.x + 8.5, live_pill.center().y),
        3.2,
        Color32::from_rgba_unmultiplied(badge_col.r(), badge_col.g(), badge_col.b(), dot_alpha),
    );
    painter.text(
        Pos2::new(live_pill.min.x + 16.0, live_pill.center().y),
        Align2::LEFT_CENTER,
        badge_txt,
        badge_font,
        TEXT_PRIMARY,
    );

    // Top-right [<>] Move Corner & [x] Hide PiP micro-buttons + bottom callout banner on hover
    if hovered {
        painter.rect_filled(
            move_btn_rect,
            Rounding::same(4.0),
            if hov_move {
                ZINC_HOVER
            } else {
                Color32::from_rgba_unmultiplied(9, 9, 11, 210)
            },
        );
        painter.rect_stroke(
            move_btn_rect,
            Rounding::same(4.0),
            Stroke::new(
                1.0_f32,
                if hov_move {
                    STATUS_ICE
                } else {
                    Color32::from_rgba_unmultiplied(255, 255, 255, 70)
                },
            ),
        );
        painter.text(
            move_btn_rect.center(),
            Align2::CENTER_CENTER,
            "<>",
            FontId::new(8.5, FontFamily::Monospace),
            if hov_move { STATUS_ICE } else { TEXT_PRIMARY },
        );

        painter.rect_filled(
            hide_btn_rect,
            Rounding::same(4.0),
            if hov_hide {
                Color32::from_rgba_unmultiplied(239, 68, 68, 72)
            } else {
                Color32::from_rgba_unmultiplied(9, 9, 11, 210)
            },
        );
        painter.rect_stroke(
            hide_btn_rect,
            Rounding::same(4.0),
            Stroke::new(
                1.0_f32,
                if hov_hide {
                    STATUS_CORAL
                } else {
                    Color32::from_rgba_unmultiplied(255, 255, 255, 70)
                },
            ),
        );
        painter.text(
            hide_btn_rect.center(),
            Align2::CENTER_CENTER,
            "x",
            FontId::new(9.5, FontFamily::Monospace),
            if hov_hide {
                Color32::from_rgb(254, 226, 226)
            } else {
                TEXT_PRIMARY
            },
        );

        let (banner_lbl, banner_col) = if hov_hide {
            ("Hide Live PiP", STATUS_CORAL)
        } else if hov_move {
            ("Move PiP corner", STATUS_ICE)
        } else {
            ("Click to return to Live (0ms)", STATUS_EMERALD)
        };
        let banner_rect = Rect::from_min_max(
            Pos2::new(pip_rect.min.x + 1.5, pip_rect.max.y - 22.0),
            Pos2::new(pip_rect.max.x - 1.5, pip_rect.max.y - 1.5),
        );
        painter.rect_filled(
            banner_rect,
            Rounding {
                nw: 0.0,
                ne: 0.0,
                sw: 5.5,
                se: 5.5,
            },
            Color32::from_rgba_unmultiplied(9, 9, 11, 215),
        );
        painter.text(
            banner_rect.center(),
            Align2::CENTER_CENTER,
            banner_lbl,
            FontId::new(10.0, FontFamily::Proportional),
            banner_col,
        );
    }

    if let Some(cp) = clicked_pos {
        if pip_rect.contains(cp) {
            if hide_btn_rect.contains(cp) {
                return Some(LivePipAction::Hide);
            }
            if move_btn_rect.contains(cp) {
                return Some(LivePipAction::CycleCorner);
            }
            return Some(LivePipAction::ReturnToLive);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_multiscale_time_to_pixel_and_pixel_to_time_roundtrip_high_epoch_precision() {
        // Real 2026 millisecond epoch (~1.795e12) at 2-minute (120_000ms) zoom
        let view_start_ms: i64 = 1_795_010_000_000;
        let span_ms: i64 = 120_000;
        let rect_min_x = 24.0_f32;
        let rect_w = 1200.0_f32;

        for offset_ms in [0_i64, 15_000, 42_300, 60_000, 99_900, 120_000] {
            let t = view_start_ms + offset_ms;
            let x = epoch_ms_to_x(t, rect_min_x, rect_w, view_start_ms, span_ms);
            let recovered = x_to_epoch_ms(x, rect_min_x, rect_w, view_start_ms, span_ms);
            assert!(
                (recovered - t).abs() <= 5,
                "precision drift too large: expected {}, got {}",
                t,
                recovered
            );
        }
    }

    #[test]
    fn test_cursor_anchored_timeline_zoom_pins_cursor_timestamp() {
        let rect_min_x = 10.0_f32;
        let rect_w = 1000.0_f32;
        let old_center: i64 = 1_795_010_000_000;
        let old_span: i64 = 86_400_000; // 24h
        let old_start = old_center - old_span / 2;

        // Cursor positioned at 30% across the timeline
        let cursor_x = rect_min_x + 300.0;
        let anchor_ms = x_to_epoch_ms(cursor_x, rect_min_x, rect_w, old_start, old_span);

        // Zoom IN by 4x (zoom_factor = 0.25 -> 6h span)
        let (new_center, new_span) =
            zoom_window_around_anchor(old_center, old_span, anchor_ms, 0.25);
        assert_eq!(new_span, 21_600_000);

        let new_start = new_center - new_span / 2;
        let mapped_x = epoch_ms_to_x(anchor_ms, rect_min_x, rect_w, new_start, new_span);
        assert!(
            (mapped_x - cursor_x).abs() < 0.05,
            "anchor drifted from {} to {}",
            cursor_x,
            mapped_x
        );

        // Clamping at extreme zoom-in (2m = 120_000ms) and zoom-out (24h = 86_400_000ms)
        let (_, min_clamped) = zoom_window_around_anchor(new_center, new_span, anchor_ms, 0.00001);
        assert_eq!(min_clamped, MIN_WINDOW_SPAN_MS);
        let (_, max_clamped) = zoom_window_around_anchor(new_center, new_span, anchor_ms, 1000.0);
        assert_eq!(max_clamped, MAX_WINDOW_SPAN_MS);
    }

    #[test]
    fn test_subpixel_event_dilation_at_24h_zoom() {
        let view_start: i64 = 1_795_000_000_000;
        let span_24h: i64 = 86_400_000;
        let min_x = 0.0_f32;
        let max_x = 1200.0_f32;

        // A 2-second (2000ms) Human trigger at 24h zoom across 1200px has natural width 0.027px,
        // which MUST be dilated to >= 3.5px!
        let ev_start = view_start + 43_200_000;
        let ev_end = ev_start + 2_000;
        let (x0, x1) = compute_dilated_span_px(
            ev_start,
            ev_end,
            min_x,
            max_x,
            view_start,
            span_24h,
            MIN_EVENT_WIDTH_PX,
        )
        .expect("visible event");
        assert!((x1 - x0) >= MIN_EVENT_WIDTH_PX - 1e-4);
        assert!(((0.5 * (x0 + x1)) - 600.0).abs() < 0.5);

        // Event completely outside visible window returns None
        assert!(compute_dilated_span_px(
            view_start - 50_000,
            view_start - 10_000,
            min_x,
            max_x,
            view_start,
            span_24h,
            MIN_EVENT_WIDTH_PX,
        )
        .is_none());
    }

    #[test]
    fn test_bracket_drag_hit_test_ignores_ruler_and_breaks_ties_by_direction() {
        let track_min_y = 60.0_f32;

        // Dragging playhead cap inside ruler_rect (origin.y < track_min_y) NEVER grabs brackets
        assert_eq!(
            pick_bracket_drag_target(
                Pos2::new(500.0, 48.0),
                track_min_y,
                Some(500.0),
                Some(502.0),
                -4.0
            ),
            None
        );

        // When both In and Out are sub-pixel close at 24h zoom, drag direction selects handle
        assert_eq!(
            pick_bracket_drag_target(
                Pos2::new(500.0, 72.0),
                track_min_y,
                Some(500.0),
                Some(500.2),
                -3.0
            ),
            Some(ActiveBracketDrag::In)
        );
        assert_eq!(
            pick_bracket_drag_target(
                Pos2::new(500.0, 72.0),
                track_min_y,
                Some(500.0),
                Some(500.2),
                3.0
            ),
            Some(ActiveBracketDrag::Out)
        );

        // When both within 7px, closer handle wins
        assert_eq!(
            pick_bracket_drag_target(
                Pos2::new(501.0, 72.0),
                track_min_y,
                Some(500.0),
                Some(505.0),
                4.0
            ),
            Some(ActiveBracketDrag::In)
        );
        assert_eq!(
            pick_bracket_drag_target(
                Pos2::new(504.0, 72.0),
                track_min_y,
                Some(500.0),
                Some(505.0),
                -4.0
            ),
            Some(ActiveBracketDrag::Out)
        );
    }

    #[test]
    fn test_clip_bounds_clamped_to_wall_clock_and_reset_on_open() {
        let now_ms: i64 = 1_795_010_000_000;
        let mut st = TimelineUiState::new(now_ms - 60_000);

        // Future timestamp beyond wallclock clamps cleanly to now_ms
        st.set_clip_out_clamped(now_ms + 300_000, now_ms);
        assert_eq!(st.clip_out_ms, Some(now_ms));
        assert_eq!(st.clip_in_ms, Some(now_ms - 15_000));

        // Negative timestamp clamps to >= 0
        st.set_clip_in_clamped(-50_000, now_ms);
        assert_eq!(st.clip_in_ms, Some(0));

        // reset_for_open clears stale clip_in_ms, clip_out_ms, popups, and scrub state
        st.open_time_jump_popover(now_ms - 60_000);
        assert!(st.is_any_popup_open());
        st.reset_for_open(now_ms - 120_000);
        assert_eq!(st.clip_in_ms, None);
        assert_eq!(st.clip_out_ms, None);
        assert!(!st.is_any_popup_open());
    }

    #[test]
    fn test_wayland_pinch_zoom_in_out_pan_and_scroll_suppression_on_timeline() {
        let stray_wheel = [egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: Vec2::new(15.0, -80.0),
            modifiers: egui::Modifiers::NONE,
        }];

        // 1. Pinch spread (scale_delta > 1.0) zooms IN (span_factor < 1.0) and suppresses stray wheel
        let pinch_in = PinchFrameSnapshot {
            scale_delta: 1.5,
            pan_delta: Vec2::new(12.0, 0.0),
            suppress_axis_scroll: true,
        };
        let (factor_in, pan_dx) = compute_timeline_zoom_and_pan(&stray_wheel, None, pinch_in, true);
        assert!(
            factor_in < 0.5,
            "expected zoom-in span factor < 0.5, got {}",
            factor_in
        );
        assert!((pan_dx - 12.0).abs() < 1e-6);

        // 2. Pinch close (scale_delta < 1.0) zooms OUT (span_factor > 1.0), even when hovering dock header
        let pinch_out = PinchFrameSnapshot {
            scale_delta: 0.65,
            pan_delta: Vec2::ZERO,
            suppress_axis_scroll: true,
        };
        let (factor_out, _) = compute_timeline_zoom_and_pan(&stray_wheel, None, pinch_out, false);
        assert!(
            factor_out > 2.0,
            "expected zoom-out span factor > 2.0, got {}",
            factor_out
        );

        // 3. Mouse tilt-wheel Line & Page horizontal scroll scales smoothly when not pinching
        let no_pinch = PinchFrameSnapshot {
            scale_delta: 1.0,
            pan_delta: Vec2::ZERO,
            suppress_axis_scroll: false,
        };
        let tilt_line = [egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: Vec2::new(1.0, 0.0),
            modifiers: egui::Modifiers::NONE,
        }];
        let (_, line_pan) = compute_timeline_zoom_and_pan(&tilt_line, None, no_pinch, true);
        assert!((line_pan - 28.0).abs() < 1e-6);

        let tilt_page = [egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Page,
            delta: Vec2::new(-1.0, 0.0),
            modifiers: egui::Modifiers::NONE,
        }];
        let (_, page_pan) = compute_timeline_zoom_and_pan(&tilt_page, None, no_pinch, true);
        assert!((page_pan - (-120.0)).abs() < 1e-6);
    }

    #[test]
    fn test_time_jump_hms_parser_valid_invalid_and_wall_clamping() {
        let date = NaiveDate::from_ymd_opt(2026, 11, 18).unwrap();
        let end_of_day_ms =
            local_naive_to_epoch_ms(date.and_hms_opt(23, 59, 59).unwrap()) + 10_000;

        let t_hms = parse_time_jump_hms("14:22:08", date, end_of_day_ms).expect("valid HH:MM:SS");
        assert_eq!(
            t_hms,
            local_naive_to_epoch_ms(date.and_hms_opt(14, 22, 8).unwrap())
        );

        let t_hm = parse_time_jump_hms("09:05", date, end_of_day_ms).expect("valid HH:MM");
        assert_eq!(
            t_hm,
            local_naive_to_epoch_ms(date.and_hms_opt(9, 5, 0).unwrap())
        );

        let t_start = parse_time_jump_hms("00:00:00", date, end_of_day_ms).expect("midnight");
        assert_eq!(
            t_start,
            local_naive_to_epoch_ms(date.and_hms_opt(0, 0, 0).unwrap())
        );

        // Clamping future timestamp on today to `now_wall_ms`
        let wall_15h = local_naive_to_epoch_ms(date.and_hms_opt(15, 0, 0).unwrap());
        let clamped =
            parse_time_jump_hms("21:30:00", date, wall_15h).expect("future clamped to wall");
        assert_eq!(clamped, wall_15h);

        // Invalid inputs rejected cleanly
        for bad in [
            "",
            "   ",
            "24:00:00",
            "12:60:00",
            "12:30:60",
            "12",
            "12:30:00:00",
            "ab:cd:ef",
            "-01:15",
            "12:05.1234",
        ] {
            assert!(
                parse_time_jump_hms(bad, date, end_of_day_ms).is_err(),
                "expected '{}' to fail parsing",
                bad
            );
        }
    }

    #[test]
    fn test_compute_live_pip_rect_corners_radar_avoidance_and_hidden() {
        let vp = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(1280.0, 720.0));

        let tr = compute_live_pip_rect(vp, LivePipCorner::TopRight, false).unwrap();
        assert_eq!(tr.size(), LIVE_PIP_SIZE);
        assert!((tr.min.x - (vp.max.x - 12.0 - LIVE_PIP_SIZE.x)).abs() < 1e-4);
        assert!((tr.min.y - (vp.min.y + 34.0)).abs() < 1e-4);

        let tl = compute_live_pip_rect(vp, LivePipCorner::TopLeft, false).unwrap();
        assert!((tl.min.x - (vp.min.x + 12.0)).abs() < 1e-4);
        assert!((tl.min.y - (vp.min.y + 34.0)).abs() < 1e-4);

        let bl_normal = compute_live_pip_rect(vp, LivePipCorner::BottomLeft, false).unwrap();
        assert!((bl_normal.min.x - (vp.min.x + 12.0)).abs() < 1e-4);
        assert!((bl_normal.min.y - (vp.max.y - 12.0 - LIVE_PIP_SIZE.y)).abs() < 1e-4);

        let bl_radar = compute_live_pip_rect(vp, LivePipCorner::BottomLeft, true).unwrap();
        assert!((bl_radar.min.x - (vp.min.x + 12.0)).abs() < 1e-4);
        assert!((bl_radar.min.y - (bl_normal.min.y - 68.0)).abs() < 1e-4);

        assert!(compute_live_pip_rect(vp, LivePipCorner::Hidden, false).is_none());
        let tiny_vp = Rect::from_min_size(Pos2::ZERO, Vec2::new(280.0, 180.0));
        assert!(compute_live_pip_rect(tiny_vp, LivePipCorner::TopRight, false).is_none());

        assert_eq!(
            LivePipCorner::TopRight.next_visible_corner(),
            LivePipCorner::BottomLeft
        );
        assert_eq!(
            LivePipCorner::BottomLeft.next_visible_corner(),
            LivePipCorner::TopLeft
        );
        assert_eq!(
            LivePipCorner::TopLeft.next_visible_corner(),
            LivePipCorner::TopRight
        );
        assert_eq!(
            LivePipCorner::Hidden.next_visible_corner(),
            LivePipCorner::TopRight
        );
    }

    #[test]
    fn test_zoom_around_playhead_preserving_follow_keeps_follow_state() {
        let playhead_ms = current_wall_ms().saturating_sub(180_000);
        let mut st = TimelineUiState::new(playhead_ms);
        assert!(st.follow_playhead);
        assert_eq!(st.window_span_ms, DEFAULT_WINDOW_SPAN_MS);

        // Zoom IN while following -> follow_playhead stays true and centers on playhead_ms
        st.zoom_around_playhead_preserving_follow(0.5, playhead_ms);
        assert!(st.follow_playhead);
        assert_eq!(st.window_span_ms, DEFAULT_WINDOW_SPAN_MS / 2);
        assert_eq!(st.view_center_ms, playhead_ms);

        // When follow_playhead is false (user panned), zooming keeps follow_playhead false
        st.follow_playhead = false;
        st.view_center_ms = playhead_ms - 600_000;
        st.zoom_around_playhead_preserving_follow(2.0, playhead_ms);
        assert!(!st.follow_playhead);
        assert_eq!(st.window_span_ms, DEFAULT_WINDOW_SPAN_MS);
    }

    #[test]
    fn test_event_specificity_ordering_paints_human_above_motion() {
        assert!(NvrEventKind::Human.specificity_rank() > NvrEventKind::Vehicle.specificity_rank());
        assert!(
            NvrEventKind::Vehicle.specificity_rank() > NvrEventKind::Intrusion.specificity_rank()
        );
        assert!(
            NvrEventKind::Intrusion.specificity_rank() > NvrEventKind::Motion.specificity_rank()
        );

        let mk = |kind: NvrEventKind, start_ms: i64| RecordedSpan {
            channel: 1,
            start_ms,
            end_ms: start_ms + 10_000,
            kind,
            file_path: None,
            byte_len: 0,
            main_stream: true,
        };
        let ev_human = mk(NvrEventKind::Human, 1_000);
        let ev_motion = mk(NvrEventKind::Motion, 1_000);
        let ev_vehicle = mk(NvrEventKind::Vehicle, 1_000);
        let ev_intrusion = mk(NvrEventKind::Intrusion, 1_000);

        let mut draw_list = vec![
            (&ev_human, 10.0, 20.0),
            (&ev_motion, 10.0, 20.0),
            (&ev_vehicle, 10.0, 20.0),
            (&ev_intrusion, 10.0, 20.0),
        ];
        sort_events_for_painting(&mut draw_list);

        // Ascending draw order: Motion painted first (bottom), Human painted last (top)!
        assert_eq!(draw_list[0].0.kind, NvrEventKind::Motion);
        assert_eq!(draw_list[1].0.kind, NvrEventKind::Intrusion);
        assert_eq!(draw_list[2].0.kind, NvrEventKind::Vehicle);
        assert_eq!(draw_list[3].0.kind, NvrEventKind::Human);
    }

    #[test]
    fn test_set_clip_in_clamped_handles_small_wall_ms() {
        let mut st = TimelineUiState::new(1);
        st.set_clip_in_clamped(1_000, 200);
        assert_eq!(st.clip_in_ms, Some(0));
        assert_eq!(st.clip_out_ms, Some(500));

        let mut st2 = TimelineUiState::new(1);
        st2.set_clip_in_clamped(50, 0);
        assert_eq!(st2.clip_in_ms, Some(0));
        assert_eq!(st2.clip_out_ms, Some(500));
    }

    #[test]
    fn test_is_actively_dragging_and_cancel_active_drags() {
        let mut st = TimelineUiState::new(1);
        assert!(!st.is_actively_dragging());

        st.dragging_playhead = true;
        st.last_scrub_ms = Some(10_000);
        st.last_emitted_scrub_ms = Some(10_000);
        assert!(st.is_actively_dragging());
        st.cancel_active_drags();
        assert!(!st.is_actively_dragging());
        assert_eq!(st.last_scrub_ms, None);
        assert_eq!(st.last_emitted_scrub_ms, None);

        st.dragging_bracket = Some(ActiveBracketDrag::In);
        assert!(st.is_actively_dragging());
        st.cancel_active_drags();
        assert!(!st.is_actively_dragging());

        st.panning_window = true;
        assert!(st.is_actively_dragging());
        st.cancel_active_drags();
        assert!(!st.is_actively_dragging());
    }
}
