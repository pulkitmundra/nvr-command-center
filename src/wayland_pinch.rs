//! Native Wayland `zwp_pointer_gestures_v1` touchpad two-finger pinch-to-zoom bridge.
//!
//! `winit 0.30.x` (used by `eframe 0.29`) only emits `WindowEvent::PinchGesture` on macOS/iOS
//! and ignores `zwp_pointer_gestures_v1` on Linux Wayland. By attaching a guest Wayland
//! `EventQueue` (`Backend::from_foreign_display`) onto `eframe`'s active `wl_display`, we
//! receive real-time `zwp_pointer_gesture_pinch_v1` (`Begin` / `Update` / `End`) events from
//! GNOME Mutter / KDE KWin / Sway with zero extra compositor connections.

use eframe::egui;

#[derive(Debug, Clone, Copy)]
pub struct PinchFrameSnapshot {
    /// Multiplicative zoom factor accumulated since the previous frame (`1.0` = neutral).
    pub scale_delta: f32,
    /// Logical surface delta (`dx`, `dy`) of the pinch center since the previous frame.
    pub pan_delta: egui::Vec2,
    /// True during an active pinch or within a brief cooldown window so stray pre/post-pinch
    /// two-finger scroll events never fight the pinch direction.
    pub suppress_axis_scroll: bool,
}

impl Default for PinchFrameSnapshot {
    fn default() -> Self {
        Self {
            scale_delta: 1.0,
            pan_delta: egui::Vec2::ZERO,
            suppress_axis_scroll: false,
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::PinchFrameSnapshot;
    use eframe::egui;
    use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
    use std::collections::HashMap;
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};
    use wayland_backend::client::{Backend, ObjectId};
    use wayland_client::protocol::{wl_pointer, wl_registry, wl_seat};
    use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
    use wayland_protocols::wp::pointer_gestures::zv1::client::{
        zwp_pointer_gesture_pinch_v1::{self, ZwpPointerGesturePinchV1},
        zwp_pointer_gestures_v1::ZwpPointerGesturesV1,
    };

    const PINCH_AXIS_COOLDOWN: Duration = Duration::from_millis(90);

    #[derive(Debug)]
    struct SharedPinchAccum {
        scale_delta: f32,
        pan_delta: egui::Vec2,
        active: bool,
        last_pinch_at: Option<Instant>,
    }

    impl Default for SharedPinchAccum {
        fn default() -> Self {
            Self {
                scale_delta: 1.0,
                pan_delta: egui::Vec2::ZERO,
                active: false,
                last_pinch_at: None,
            }
        }
    }

    struct SeatPointerEntry {
        pointer: wl_pointer::WlPointer,
        pinch: Option<ZwpPointerGesturePinchV1>,
    }

    struct PinchDispatchState {
        gestures_mgr: Option<ZwpPointerGesturesV1>,
        seats: Vec<wl_seat::WlSeat>,
        pointers: HashMap<ObjectId, SeatPointerEntry>,
        last_scale: f64,
        shared: Arc<Mutex<SharedPinchAccum>>,
        egui_ctx: egui::Context,
    }

    impl PinchDispatchState {
        fn attach_missing_pinch_gestures(&mut self, qh: &QueueHandle<Self>) {
            let Some(ref mgr) = self.gestures_mgr else {
                return;
            };
            for entry in self.pointers.values_mut() {
                if entry.pinch.is_none() {
                    let pinch = mgr.get_pinch_gesture(&entry.pointer, qh, ());
                    entry.pinch = Some(pinch);
                }
            }
        }

        fn cleanup(&mut self) {
            for (_, mut entry) in self.pointers.drain() {
                if let Some(pinch) = entry.pinch.take() {
                    pinch.destroy();
                }
                if entry.pointer.version() >= 3 {
                    entry.pointer.release();
                }
            }
            for seat in self.seats.drain(..) {
                if seat.version() >= 5 {
                    seat.release();
                }
            }
            if let Some(mgr) = self.gestures_mgr.take() {
                if mgr.version() >= 2 {
                    mgr.release();
                }
            }
        }
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for PinchDispatchState {
        fn event(
            state: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _data: &(),
            conn: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
            {
                if interface == ZwpPointerGesturesV1::interface().name && state.gestures_mgr.is_none()
                {
                    let mgr =
                        registry.bind::<ZwpPointerGesturesV1, _, _>(name, version.clamp(1, 3), qh, ());
                    state.gestures_mgr = Some(mgr);
                    state.attach_missing_pinch_gestures(qh);
                    let _ = conn.flush();
                } else if interface == wl_seat::WlSeat::interface().name {
                    let seat =
                        registry.bind::<wl_seat::WlSeat, _, _>(name, version.clamp(1, 7), qh, ());
                    state.seats.push(seat);
                    let _ = conn.flush();
                }
            }
        }
    }

    impl Dispatch<wl_seat::WlSeat, ()> for PinchDispatchState {
        fn event(
            state: &mut Self,
            seat: &wl_seat::WlSeat,
            event: wl_seat::Event,
            _data: &(),
            conn: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wl_seat::Event::Capabilities {
                capabilities: WEnum::Value(caps),
            } = event
            {
                let seat_id = seat.id();
                if caps.contains(wl_seat::Capability::Pointer) {
                    if !state.pointers.contains_key(&seat_id) {
                        let pointer = seat.get_pointer(qh, ());
                        let pinch = state
                            .gestures_mgr
                            .as_ref()
                            .map(|mgr| mgr.get_pinch_gesture(&pointer, qh, ()));
                        state
                            .pointers
                            .insert(seat_id, SeatPointerEntry { pointer, pinch });
                        let _ = conn.flush();
                    }
                } else if let Some(mut removed) = state.pointers.remove(&seat_id) {
                    if let Some(pinch) = removed.pinch.take() {
                        pinch.destroy();
                    }
                    if removed.pointer.version() >= 3 {
                        removed.pointer.release();
                    }
                    let _ = conn.flush();
                }
            }
        }
    }

    impl Dispatch<wl_pointer::WlPointer, ()> for PinchDispatchState {
        fn event(
            _state: &mut Self,
            _proxy: &wl_pointer::WlPointer,
            _event: wl_pointer::Event,
            _data: &(),
            _conn: &Connection,
            _qh: &QueueHandle<Self>,
        ) {
            // Standard pointer events are already handled by winit on its own queue.
        }
    }

    impl Dispatch<ZwpPointerGesturesV1, ()> for PinchDispatchState {
        fn event(
            _state: &mut Self,
            _proxy: &ZwpPointerGesturesV1,
            _event: <ZwpPointerGesturesV1 as Proxy>::Event,
            _data: &(),
            _conn: &Connection,
            _qh: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ZwpPointerGesturePinchV1, ()> for PinchDispatchState {
        fn event(
            state: &mut Self,
            _proxy: &ZwpPointerGesturePinchV1,
            event: zwp_pointer_gesture_pinch_v1::Event,
            _data: &(),
            _conn: &Connection,
            _qh: &QueueHandle<Self>,
        ) {
            match event {
                zwp_pointer_gesture_pinch_v1::Event::Begin { .. } => {
                    state.last_scale = 1.0;
                    if let Ok(mut shared) = state.shared.lock() {
                        shared.active = true;
                        shared.last_pinch_at = Some(Instant::now());
                    }
                    state.egui_ctx.request_repaint();
                }
                zwp_pointer_gesture_pinch_v1::Event::Update {
                    dx, dy, scale, ..
                } => {
                    let rel = if state.last_scale > 0.001 && scale.is_finite() && scale > 0.001 {
                        (scale / state.last_scale) as f32
                    } else {
                        1.0
                    };
                    if scale.is_finite() && scale > 0.001 {
                        state.last_scale = scale;
                    }
                    if let Ok(mut shared) = state.shared.lock() {
                        shared.scale_delta =
                            (shared.scale_delta * rel.clamp(0.2, 5.0)).clamp(0.05, 20.0);
                        if dx.is_finite() && dy.is_finite() {
                            shared.pan_delta += egui::Vec2::new(dx as f32, dy as f32);
                        }
                        shared.active = true;
                        shared.last_pinch_at = Some(Instant::now());
                    }
                    state.egui_ctx.request_repaint();
                }
                zwp_pointer_gesture_pinch_v1::Event::End { .. } => {
                    state.last_scale = 1.0;
                    if let Ok(mut shared) = state.shared.lock() {
                        shared.active = false;
                        shared.last_pinch_at = Some(Instant::now());
                    }
                    state.egui_ctx.request_repaint();
                }
                _ => {}
            }
        }
    }

    pub struct WaylandPinchListener {
        shared: Arc<Mutex<SharedPinchAccum>>,
        stop_flag: Arc<AtomicBool>,
        wake_fd: i32,
        worker: Option<JoinHandle<()>>,
    }

    impl WaylandPinchListener {
        pub fn try_new(cc: &eframe::CreationContext<'_>) -> Option<Self> {
            let display_handle = cc.display_handle().ok()?;
            let RawDisplayHandle::Wayland(wl_handle) = display_handle.as_raw() else {
                return None;
            };
            let display_ptr = wl_handle.display.as_ptr();
            if display_ptr.is_null() {
                return None;
            }

            // SAFETY: `display_ptr` belongs to `eframe`/`winit`'s active Wayland display, which
            // outlives `CameraViewerApp` (and we explicitly join our worker thread in `Drop`).
            let backend = unsafe { Backend::from_foreign_display(display_ptr.cast()) };
            let conn = Connection::from_backend(backend);
            let mut event_queue = conn.new_event_queue::<PinchDispatchState>();
            let qh = event_queue.handle();

            let shared = Arc::new(Mutex::new(SharedPinchAccum::default()));
            let mut state = PinchDispatchState {
                gestures_mgr: None,
                seats: Vec::new(),
                pointers: HashMap::new(),
                last_scale: 1.0,
                shared: Arc::clone(&shared),
                egui_ctx: cc.egui_ctx.clone(),
            };

            let _registry = conn.display().get_registry(&qh, ());
            if event_queue.roundtrip(&mut state).is_err() {
                return None;
            }
            if state.gestures_mgr.is_none() {
                state.cleanup();
                let _ = conn.flush();
                return None;
            }
            // Second roundtrip processes `wl_seat::Capabilities` and attaches `get_pinch_gesture`.
            let _ = event_queue.roundtrip(&mut state);

            let wake_fd =
                unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
            if wake_fd < 0 {
                state.cleanup();
                let _ = conn.flush();
                return None;
            }

            let stop_flag = Arc::new(AtomicBool::new(false));
            let stop_thread = Arc::clone(&stop_flag);

            let worker = thread::Builder::new()
                .name("wayland-pinch".to_string())
                .spawn(move || {
                    while !stop_thread.load(Ordering::Acquire) {
                        if event_queue.dispatch_pending(&mut state).is_err() {
                            break;
                        }
                        let _ = conn.flush();

                        let Some(read_guard) = event_queue.prepare_read() else {
                            continue;
                        };

                        let wl_fd = read_guard.connection_fd().as_raw_fd();
                        let mut fds = [
                            libc::pollfd {
                                fd: wl_fd,
                                events: libc::POLLIN,
                                revents: 0,
                            },
                            libc::pollfd {
                                fd: wake_fd,
                                events: libc::POLLIN,
                                revents: 0,
                            },
                        ];

                        let poll_res = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
                        if stop_thread.load(Ordering::Acquire) || (fds[1].revents & libc::POLLIN) != 0
                        {
                            drop(read_guard);
                            break;
                        }
                        if poll_res < 0 {
                            let err = std::io::Error::last_os_error();
                            drop(read_guard);
                            if err.kind() == std::io::ErrorKind::Interrupted {
                                continue;
                            }
                            break;
                        }

                        if (fds[0].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP)) != 0 {
                            if let Err(wayland_backend::client::WaylandError::Io(ref io_err)) =
                                read_guard.read()
                            {
                                if io_err.kind() != std::io::ErrorKind::WouldBlock {
                                    break;
                                }
                            }
                            if event_queue.dispatch_pending(&mut state).is_err() {
                                break;
                            }
                        } else {
                            drop(read_guard);
                        }
                    }

                    state.cleanup();
                    let _ = conn.flush();
                })
                .ok()?;

            Some(Self {
                shared,
                stop_flag,
                wake_fd,
                worker: Some(worker),
            })
        }

        pub fn take_frame_snapshot(&self) -> PinchFrameSnapshot {
            let Ok(mut shared) = self.shared.lock() else {
                return PinchFrameSnapshot::default();
            };
            let scale_delta = shared.scale_delta;
            let pan_delta = shared.pan_delta;
            shared.scale_delta = 1.0;
            shared.pan_delta = egui::Vec2::ZERO;
            let suppress_axis_scroll = shared.active
                || shared
                    .last_pinch_at
                    .is_some_and(|t| t.elapsed() < PINCH_AXIS_COOLDOWN);
            PinchFrameSnapshot {
                scale_delta,
                pan_delta,
                suppress_axis_scroll,
            }
        }
    }

    impl Drop for WaylandPinchListener {
        fn drop(&mut self) {
            self.stop_flag.store(true, Ordering::Release);
            let one: u64 = 1;
            unsafe {
                let _ = libc::write(
                    self.wake_fd,
                    (&one as *const u64).cast(),
                    std::mem::size_of::<u64>(),
                );
            }
            if let Some(handle) = self.worker.take() {
                let _ = handle.join();
            }
            if self.wake_fd >= 0 {
                unsafe {
                    let _ = libc::close(self.wake_fd);
                }
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::PinchFrameSnapshot;

    pub struct WaylandPinchListener;

    impl WaylandPinchListener {
        pub fn try_new(_cc: &eframe::CreationContext<'_>) -> Option<Self> {
            None
        }

        pub fn take_frame_snapshot(&self) -> PinchFrameSnapshot {
            PinchFrameSnapshot::default()
        }
    }
}

pub use imp::WaylandPinchListener;
