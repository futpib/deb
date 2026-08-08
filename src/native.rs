use crate::profile::{EngineDirectories, ProfileDirectories};
use shell_protocol::{
    MAX_PACKET_BYTES, ProtocolError, Transport, configure_child_command,
    wire::{self, Capability, Engine},
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    error::Error,
    ffi::CString,
    os::fd::{IntoRawFd, OwnedFd},
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::{
        Mutex, MutexGuard, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, RecvTimeoutError, TryRecvError},
    },
    thread::{JoinHandle, sleep},
    time::{Duration, Instant},
};
use x11rb::{protocol::xproto::Window, rust_connection::RustConnection};

unsafe extern "C" {
    fn deb_browser_surface_submit(
        surface_id: *const libc::c_char,
        browser_id: u64,
        lease_id: u64,
        layer: i32,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        drm_format: u32,
        modifier: u64,
        flip_y: i32,
        plane_count: u32,
        fds: *const i32,
        strides: *const u32,
        offsets: *const u64,
        acquire_fence_fd: i32,
    );
    fn deb_browser_surface_clear(surface_id: *const libc::c_char, layer: i32);
    fn deb_browser_surface_bind(surface_id: *const libc::c_char, browser_id: u64, generation: u64);
    fn deb_browser_surface_forget(browser_id: u64);
    fn deb_browser_surface_set_cursor(
        browser_id: u64,
        cef_type: i32,
        custom_bgra: *const u8,
        custom_bgra_length: usize,
        width: u32,
        height: u32,
        hotspot_x: i32,
        hotspot_y: i32,
        image_scale_factor: f32,
    );
}

struct FrameReleaseTarget {
    sender: mpsc::Sender<FrameLeaseEvent>,
    browser_id: u64,
    frame_id: u64,
    layer: wire::SurfaceLayer,
    surface_generation: u64,
}

enum FrameLeaseEvent {
    Presented(u64, u64),
    Released(u64, u64),
}

fn frame_leases() -> &'static Mutex<HashMap<u64, FrameReleaseTarget>> {
    static LEASES: OnceLock<Mutex<HashMap<u64, FrameReleaseTarget>>> = OnceLock::new();
    LEASES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_frame_leases(operation: &str) -> MutexGuard<'static, HashMap<u64, FrameReleaseTarget>> {
    match frame_leases().lock() {
        Ok(leases) => leases,
        Err(error) => {
            eprintln!(
                "deb: failure: frame lease {operation}: frame lease registry lock was poisoned"
            );
            error.into_inner()
        }
    }
}

fn register_frame_lease(
    sender: &mpsc::Sender<FrameLeaseEvent>,
    browser_id: u64,
    frame_id: u64,
    layer: wire::SurfaceLayer,
    surface_generation: u64,
) -> u64 {
    static NEXT_LEASE: AtomicU64 = AtomicU64::new(1);
    let lease_id = NEXT_LEASE.fetch_add(1, Ordering::Relaxed);
    lock_frame_leases("registration").insert(
        lease_id,
        FrameReleaseTarget {
            sender: sender.clone(),
            browser_id,
            frame_id,
            layer,
            surface_generation,
        },
    );
    lease_id
}

#[unsafe(no_mangle)]
pub extern "C" fn deb_release_dmabuf_lease(lease_id: u64) {
    let target = lock_frame_leases("release").remove(&lease_id);
    if let Some(target) = target
        && let Err(error) = target.sender.send(FrameLeaseEvent::Released(
            target.browser_id,
            target.frame_id,
        ))
    {
        eprintln!(
            "deb: failure: frame lease release: browser={} frame={}: {error}",
            target.browser_id, target.frame_id
        );
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn deb_present_dmabuf_lease(lease_id: u64) {
    let target = lock_frame_leases("presentation")
        .get(&lease_id)
        .and_then(|target| {
            (target.layer == wire::SurfaceLayer::View).then(|| {
                (
                    target.sender.clone(),
                    target.browser_id,
                    target.frame_id,
                    target.surface_generation,
                )
            })
        });
    if let Some((sender, browser_id, frame_id, surface_generation)) = target
        && let Err(error) = sender.send(FrameLeaseEvent::Presented(browser_id, surface_generation))
    {
        eprintln!(
            "deb: failure: frame lease presentation: browser={browser_id} frame={frame_id}: {error}"
        );
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn deb_rebind_dmabuf_lease(lease_id: u64, surface_generation: u64) {
    let mut leases = lock_frame_leases("rebind");
    if let Some(target) = leases.get_mut(&lease_id) {
        target.surface_generation = surface_generation;
    }
}

fn bind_qt_surface(surface_id: &str, browser_id: u64, generation: u64) {
    let surface_id = CString::new(surface_id).expect("internal surface ID contains no NUL bytes");
    unsafe {
        deb_browser_surface_bind(surface_id.as_ptr(), browser_id, generation);
    }
}

fn forget_qt_browser(browser_id: u64) {
    unsafe {
        deb_browser_surface_forget(browser_id);
    }
}

fn deliver_cursor(browser_id: u64, cursor: &wire::CursorChanged) -> NativeResult<()> {
    if cursor.cef_type > 49 {
        return Err(format!("cursor has invalid CEF type {}", cursor.cef_type).into());
    }
    if cursor.cef_type == 45 {
        let expected_length = usize::try_from(cursor.width)
            .ok()
            .and_then(|width| {
                usize::try_from(cursor.height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or("custom cursor dimensions overflow")?;
        if (!cursor.custom_bgra.is_empty() && cursor.custom_bgra.len() != expected_length)
            || !cursor.image_scale_factor.is_finite()
            || cursor.image_scale_factor <= 0.0
        {
            return Err("custom cursor metadata is invalid".into());
        }
    } else if !cursor.custom_bgra.is_empty() {
        return Err("standard cursor unexpectedly contains custom pixels".into());
    }
    let pixels = if cursor.custom_bgra.is_empty() {
        std::ptr::null()
    } else {
        cursor.custom_bgra.as_ptr()
    };
    unsafe {
        deb_browser_surface_set_cursor(
            browser_id,
            cursor.cef_type as i32,
            pixels,
            cursor.custom_bgra.len(),
            cursor.width,
            cursor.height,
            cursor.hotspot_x,
            cursor.hotspot_y,
            cursor.image_scale_factor,
        );
    }
    Ok(())
}

pub(crate) type NativeResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl NativeRect {
    pub fn new(x: i32, y: i32, width: i32, height: i32) -> Option<Self> {
        (width > 1 && height > 1).then_some(Self {
            x,
            y,
            width: width as u32,
            height: height as u32,
        })
    }

    fn viewport(self) -> wire::Viewport {
        wire::Viewport {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
            scale_factor: 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) enum CefBackend {
    Chromium,
    Firefox,
}

impl CefBackend {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Chromium => "Chromium",
            Self::Firefox => "Gecko",
        }
    }

    fn loader_directory(self, executable_directory: &Path) -> NativeResult<Option<PathBuf>> {
        if matches!(self, Self::Chromium) {
            return Ok(None);
        }
        let configured = std::env::var_os("DEB_FIREFOX_RUNTIME")
            .map(PathBuf::from)
            .unwrap_or_else(|| executable_directory.join("firefox-cef-runtime"));
        let directory = configured.canonicalize().map_err(|error| {
            format!(
                "cannot resolve {} at {}: {error}",
                self.name(),
                configured.display()
            )
        })?;
        if !directory.join("libcef.so").is_file() {
            return Err(format!("{} has no libcef.so", directory.display()).into());
        }
        Ok(Some(directory))
    }

    fn name(self) -> &'static str {
        match self {
            Self::Chromium => "Chromium libcef",
            Self::Firefox => "Firefox CEF adapter",
        }
    }

    fn engine(self) -> Engine {
        match self {
            Self::Chromium => Engine::Chromium,
            Self::Firefox => Engine::Gecko,
        }
    }

    pub(crate) fn directories(self, profile: &ProfileDirectories) -> &EngineDirectories {
        match self {
            Self::Chromium => &profile.chromium,
            Self::Firefox => &profile.firefox,
        }
    }
}

pub(crate) struct CefInstance {
    child: Child,
    transport: Transport,
    incoming: mpsc::Receiver<Result<shell_protocol::ReceivedPacket, ProtocolError>>,
    surfaces: HashMap<u64, String>,
    surface_generations: HashMap<u64, u64>,
    lease_sender: mpsc::Sender<FrameLeaseEvent>,
    lease_events: mpsc::Receiver<FrameLeaseEvent>,
    browser_id: u64,
    next_request_id: u64,
    pending_requests: HashMap<u64, (u64, &'static str)>,
    ready_browsers: HashSet<u64>,
    deferred_browser_requests: HashMap<u64, VecDeque<(wire::request::Operation, &'static str)>>,
    last_event_sequence: u64,
    protocol_closed: bool,
}

impl CefInstance {
    const MAX_PACKETS_PER_POLL: usize = 64;

    fn send_browser_request_now(
        &mut self,
        browser_id: u64,
        operation: wire::request::Operation,
        description: &'static str,
    ) -> NativeResult<()> {
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or("shell protocol request ID overflow")?;
        self.transport
            .send(&request_packet(request_id, browser_id, operation))?;
        self.pending_requests
            .insert(request_id, (browser_id, description));
        Ok(())
    }

    fn send_browser_request(
        &mut self,
        browser_id: u64,
        operation: wire::request::Operation,
        description: &'static str,
    ) -> NativeResult<()> {
        if self.ready_browsers.contains(&browser_id) {
            self.send_browser_request_now(browser_id, operation, description)
        } else {
            self.deferred_browser_requests
                .entry(browser_id)
                .or_default()
                .push_back((operation, description));
            Ok(())
        }
    }

    fn mark_browser_ready(&mut self, browser_id: u64) -> NativeResult<()> {
        self.ready_browsers.insert(browser_id);
        let Some(mut requests) = self.deferred_browser_requests.remove(&browser_id) else {
            return Ok(());
        };
        while let Some((operation, description)) = requests.pop_front() {
            self.send_browser_request_now(browser_id, operation, description)?;
        }
        Ok(())
    }

    fn send_browser_input(
        &mut self,
        browser_id: u64,
        operation: wire::request::Operation,
        description: &'static str,
    ) -> NativeResult<()> {
        if self.ready_browsers.contains(&browser_id) {
            self.transport
                .send(&request_packet(0, browser_id, operation))?;
        } else {
            self.deferred_browser_requests
                .entry(browser_id)
                .or_default()
                .push_back((operation, description));
        }
        Ok(())
    }

    fn send_process_request(
        &mut self,
        operation: wire::request::Operation,
        description: &'static str,
    ) -> NativeResult<()> {
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or("shell protocol request ID overflow")?;
        self.transport
            .send(&request_packet(request_id, 0, operation))?;
        self.pending_requests.insert(request_id, (0, description));
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_browser(
        &mut self,
        browser_id: u64,
        parent: Window,
        bounds: NativeRect,
        url: &str,
        profile_id: &str,
        directories: &EngineDirectories,
        surface_id: String,
    ) -> NativeResult<()> {
        self.ready_browsers.remove(&browser_id);
        self.deferred_browser_requests.remove(&browser_id);
        self.surfaces.insert(browser_id, surface_id.clone());
        let generation = self.surface_generations.entry(browser_id).or_default();
        *generation += 1;
        bind_qt_surface(&surface_id, browser_id, *generation);
        if let Err(error) = self.send_browser_request_now(
            browser_id,
            wire::request::Operation::CreateBrowser(wire::CreateBrowser {
                initial_url: url.to_owned(),
                surface: Some(wire::SurfaceTarget {
                    parent_window: u64::from(parent),
                    viewport: Some(bounds.viewport()),
                }),
                profile_id: profile_id.to_owned(),
                profile_data_path: protocol_path(&directories.data)?,
                profile_cache_path: protocol_path(&directories.cache)?,
            }),
            "browser creation",
        ) {
            self.surfaces.remove(&browser_id);
            self.surface_generations.remove(&browser_id);
            self.deferred_browser_requests.remove(&browser_id);
            forget_qt_browser(browser_id);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn navigate_browser(&mut self, browser_id: u64, url: &str) -> NativeResult<()> {
        self.send_browser_request(
            browser_id,
            wire::request::Operation::Navigate(wire::Navigate {
                url: url.to_owned(),
            }),
            "navigation",
        )
    }

    pub(crate) fn resize_browser(
        &mut self,
        browser_id: u64,
        bounds: NativeRect,
    ) -> NativeResult<()> {
        self.send_browser_request(
            browser_id,
            wire::request::Operation::Resize(wire::Resize {
                viewport: Some(bounds.viewport()),
            }),
            "resize",
        )
    }

    pub(crate) fn focus_browser(&mut self, browser_id: u64, focused: bool) -> NativeResult<()> {
        self.send_browser_request(
            browser_id,
            wire::request::Operation::SetFocus(wire::SetFocus { focused }),
            "focus",
        )
    }

    pub(crate) fn send_mouse_move(
        &mut self,
        browser_id: u64,
        x: i32,
        y: i32,
        modifiers: u32,
        leaving: bool,
    ) -> NativeResult<()> {
        self.send_browser_input(
            browser_id,
            wire::request::Operation::MouseMove(wire::MouseMove {
                x,
                y,
                modifiers,
                leaving,
            }),
            "mouse move",
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn send_mouse_click(
        &mut self,
        browser_id: u64,
        x: i32,
        y: i32,
        modifiers: u32,
        button: wire::MouseButton,
        mouse_up: bool,
        click_count: u32,
    ) -> NativeResult<()> {
        self.send_browser_input(
            browser_id,
            wire::request::Operation::MouseClick(wire::MouseClick {
                x,
                y,
                modifiers,
                button: button as i32,
                mouse_up,
                click_count,
            }),
            "mouse click",
        )
    }

    pub(crate) fn send_mouse_wheel(
        &mut self,
        browser_id: u64,
        x: i32,
        y: i32,
        modifiers: u32,
        delta_x: i32,
        delta_y: i32,
    ) -> NativeResult<()> {
        self.send_browser_input(
            browser_id,
            wire::request::Operation::MouseWheel(wire::MouseWheel {
                x,
                y,
                modifiers,
                delta_x,
                delta_y,
            }),
            "mouse wheel",
        )
    }

    pub(crate) fn send_key_event(
        &mut self,
        browser_id: u64,
        event: wire::KeyEvent,
    ) -> NativeResult<()> {
        self.send_browser_input(
            browser_id,
            wire::request::Operation::KeyEvent(event),
            "key event",
        )
    }

    pub(crate) fn send_touch_event(
        &mut self,
        browser_id: u64,
        event: wire::TouchEvent,
    ) -> NativeResult<()> {
        self.send_browser_input(
            browser_id,
            wire::request::Operation::TouchEvent(event),
            "touch event",
        )
    }

    pub(crate) fn set_browser_visible(
        &mut self,
        browser_id: u64,
        visible: bool,
    ) -> NativeResult<()> {
        self.send_browser_request(
            browser_id,
            wire::request::Operation::SetVisibility(wire::SetVisibility { visible }),
            "visibility",
        )
    }

    pub(crate) fn close_browser(&mut self, browser_id: u64, force: bool) -> NativeResult<()> {
        self.surfaces.remove(&browser_id);
        self.surface_generations.remove(&browser_id);
        forget_qt_browser(browser_id);
        self.send_browser_request(
            browser_id,
            wire::request::Operation::Close(wire::Close { force }),
            "close",
        )
    }

    pub(crate) fn read_browser_cookies(&mut self, browser_id: u64) -> NativeResult<()> {
        self.send_browser_request(
            browser_id,
            wire::request::Operation::ReadCookies(wire::ReadCookies {}),
            "cookie snapshot",
        )
    }

    pub(crate) fn set_browser_cookie(
        &mut self,
        browser_id: u64,
        cookie: wire::Cookie,
    ) -> NativeResult<()> {
        self.send_browser_request(
            browser_id,
            wire::request::Operation::SetCookie(wire::SetCookie {
                cookie: Some(cookie),
            }),
            "cookie set",
        )
    }

    pub(crate) fn delete_browser_cookie(
        &mut self,
        browser_id: u64,
        cookie: wire::Cookie,
    ) -> NativeResult<()> {
        self.send_browser_request(
            browser_id,
            wire::request::Operation::DeleteCookie(wire::DeleteCookie {
                cookie: Some(cookie),
            }),
            "cookie delete",
        )
    }

    pub(crate) fn bind_browser_surface(&mut self, browser_id: u64, surface_id: Option<String>) {
        match surface_id {
            Some(surface_id) => {
                if self.surfaces.get(&browser_id) != Some(&surface_id) {
                    self.surfaces.insert(browser_id, surface_id.clone());
                    let generation = self.surface_generations.entry(browser_id).or_default();
                    *generation += 1;
                    bind_qt_surface(&surface_id, browser_id, *generation);
                }
            }
            None => {
                self.surfaces.remove(&browser_id);
            }
        }
    }

    pub(crate) fn exited(&mut self) -> NativeResult<Option<std::process::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }

    pub(crate) fn drain_routed_notices(&mut self) -> Vec<RoutedNotice> {
        let mut notices = Vec::new();
        while let Ok(event) = self.lease_events.try_recv() {
            match event {
                FrameLeaseEvent::Presented(browser_id, surface_generation)
                    if self.surface_generations.get(&browser_id) == Some(&surface_generation) =>
                {
                    notices.push(RoutedNotice {
                        browser_id,
                        value: ProtocolNotice::FrameReady,
                    })
                }
                FrameLeaseEvent::Presented(_, _) => {}
                FrameLeaseEvent::Released(browser_id, frame_id) => {
                    if let Err(error) = self.transport.send(&wire::Packet {
                        request_id: 0,
                        body: Some(wire::packet::Body::FrameRelease(wire::FrameRelease {
                            browser_id,
                            frame_id,
                        })),
                    }) {
                        notices.push(RoutedNotice {
                            browser_id,
                            value: ProtocolNotice::ProtocolFailed(format!(
                                "frame release failed: {error}"
                            )),
                        });
                        self.protocol_closed = true;
                        return notices;
                    }
                }
            }
        }
        if self.protocol_closed {
            return notices;
        }
        for _ in 0..Self::MAX_PACKETS_PER_POLL {
            match self.incoming.try_recv() {
                Ok(Ok(received)) => {
                    if let Some(notice) = self.handle_packet(received) {
                        notices.push(notice);
                    }
                }
                Ok(Err(error)) => {
                    self.protocol_closed = true;
                    notices.push(RoutedNotice {
                        browser_id: 0,
                        value: ProtocolNotice::ProtocolFailed(error.to_string()),
                    });
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.protocol_closed = true;
                    notices.push(RoutedNotice {
                        browser_id: 0,
                        value: ProtocolNotice::ProtocolFailed("protocol reader stopped".to_owned()),
                    });
                    break;
                }
            }
        }
        notices
    }

    fn handle_packet(&mut self, received: shell_protocol::ReceivedPacket) -> Option<RoutedNotice> {
        let request_id = received.packet.request_id;
        let file_descriptors = received.file_descriptors;
        let (browser_id, value) = match received.packet.body {
            Some(wire::packet::Body::Response(response)) => {
                let Some((browser_id, description)) = self.pending_requests.remove(&request_id)
                else {
                    return Some(RoutedNotice {
                        browser_id: 0,
                        value: ProtocolNotice::ProtocolFailed(format!(
                            "unsolicited response for request {request_id}"
                        )),
                    });
                };
                let value = match response.result {
                    Some(wire::response::Result::Success(_)) => None,
                    Some(wire::response::Result::Error(error)) => {
                        Some(ProtocolNotice::CommandFailed(format!(
                            "{description} rejected [{}]: {}",
                            error.code, error.message
                        )))
                    }
                    None => Some(ProtocolNotice::ProtocolFailed(format!(
                        "{description} response has no result"
                    ))),
                };
                (browser_id, value)
            }
            Some(wire::packet::Body::Event(event)) => {
                if event.sequence <= self.last_event_sequence {
                    return Some(RoutedNotice {
                        browser_id: event.browser_id,
                        value: ProtocolNotice::ProtocolFailed(format!(
                            "event sequence {} followed {}",
                            event.sequence, self.last_event_sequence
                        )),
                    });
                }
                self.last_event_sequence = event.sequence;
                let browser_id = event.browser_id;
                let value = match event.value {
                    Some(wire::event::Value::SurfaceReady(_)) => {
                        match self.mark_browser_ready(browser_id) {
                            Ok(()) => Some(ProtocolNotice::SurfaceReady),
                            Err(error) => Some(ProtocolNotice::ProtocolFailed(format!(
                                "deferred browser command dispatch failed: {error}"
                            ))),
                        }
                    }
                    Some(wire::event::Value::LoadingChanged(loading)) => {
                        Some(ProtocolNotice::LoadingChanged(loading.loading))
                    }
                    Some(wire::event::Value::LoadFailed(failure)) => {
                        Some(ProtocolNotice::LoadFailed(format!(
                            "{} ({})",
                            failure.error_text, failure.error_code
                        )))
                    }
                    Some(wire::event::Value::BrowserClosed(_)) => {
                        self.ready_browsers.remove(&browser_id);
                        self.deferred_browser_requests.remove(&browser_id);
                        Some(ProtocolNotice::Closed)
                    }
                    Some(wire::event::Value::BrowserCrashed(crash)) => {
                        Some(ProtocolNotice::Crashed(crash.reason))
                    }
                    Some(wire::event::Value::NavigationCommitted(navigation)) => {
                        Some(ProtocolNotice::NavigationCommitted(navigation.url))
                    }
                    Some(wire::event::Value::TitleChanged(title)) => {
                        Some(ProtocolNotice::TitleChanged(title.title))
                    }
                    Some(wire::event::Value::CookieSnapshotEntry(entry)) => match entry.cookie {
                        Some(cookie) => Some(ProtocolNotice::CookieSnapshotEntry(cookie)),
                        None => Some(ProtocolNotice::ProtocolFailed(
                            "cookie snapshot entry has no cookie".to_owned(),
                        )),
                    },
                    Some(wire::event::Value::CookieSnapshotComplete(_)) => {
                        Some(ProtocolNotice::CookieSnapshotComplete)
                    }
                    Some(wire::event::Value::CookieChanged(change)) => match (
                        change.cookie,
                        wire::CookieChangeCause::try_from(change.cause),
                    ) {
                        (Some(cookie), Ok(cause)) => {
                            Some(ProtocolNotice::CookieChanged(cookie, cause))
                        }
                        (None, _) => Some(ProtocolNotice::ProtocolFailed(
                            "cookie change has no cookie".to_owned(),
                        )),
                        (_, Err(_)) => Some(ProtocolNotice::ProtocolFailed(format!(
                            "cookie change has invalid cause {}",
                            change.cause
                        ))),
                    },
                    Some(wire::event::Value::AcceleratedFrame(frame)) => {
                        let frame_id = frame.frame_id;
                        match self.deliver_frame(browser_id, frame, file_descriptors) {
                            Ok(()) => None,
                            Err(error) => {
                                let failure = match self.send_frame_release(browser_id, frame_id) {
                                    Ok(()) => error.to_string(),
                                    Err(release_error) => format!(
                                        "{error}; failed to release rejected frame {frame_id}: {release_error}"
                                    ),
                                };
                                Some(ProtocolNotice::ProtocolFailed(failure))
                            }
                        }
                    }
                    Some(wire::event::Value::SurfaceCleared(clear)) => {
                        if let (Some(surface_id), Ok(layer)) = (
                            self.surfaces.get(&browser_id),
                            wire::SurfaceLayer::try_from(clear.layer),
                        ) {
                            clear_qt_surface(surface_id, layer);
                        }
                        None
                    }
                    Some(wire::event::Value::CursorChanged(cursor)) => {
                        match deliver_cursor(browser_id, &cursor) {
                            Ok(()) => None,
                            Err(error) => Some(ProtocolNotice::ProtocolFailed(error.to_string())),
                        }
                    }
                    None => None,
                };
                (browser_id, value)
            }
            Some(_) => (
                0,
                Some(ProtocolNotice::ProtocolFailed(
                    "unexpected runtime packet type".to_owned(),
                )),
            ),
            None => (
                0,
                Some(ProtocolNotice::ProtocolFailed(
                    "runtime packet has no body".to_owned(),
                )),
            ),
        };
        value.map(|value| RoutedNotice { browser_id, value })
    }

    fn deliver_frame(
        &mut self,
        browser_id: u64,
        frame: wire::AcceleratedFrame,
        file_descriptors: Vec<OwnedFd>,
    ) -> NativeResult<()> {
        let layer = wire::SurfaceLayer::try_from(frame.layer)
            .map_err(|_| format!("frame has invalid surface layer {}", frame.layer))?;
        if !matches!(layer, wire::SurfaceLayer::View | wire::SurfaceLayer::Popup) {
            return Err("frame has unspecified surface layer".into());
        }
        if frame.frame_id == 0 || frame.width == 0 || frame.height == 0 {
            return Err("frame has invalid identity or dimensions".into());
        }
        let surface_id = self.surfaces.get(&browser_id).cloned().unwrap_or_default();
        if frame.planes.is_empty() || frame.planes.len() > 4 {
            return Err("frame has an invalid plane count".into());
        }
        let mut used_indices = HashSet::new();
        for plane in &frame.planes {
            if plane.fd_index as usize >= file_descriptors.len()
                || !used_indices.insert(plane.fd_index)
            {
                return Err("frame plane has an invalid or reused FD index".into());
            }
        }
        if frame.has_acquire_fence
            && (frame.acquire_fence_fd_index as usize >= file_descriptors.len()
                || !used_indices.insert(frame.acquire_fence_fd_index))
        {
            return Err("frame has an invalid or reused acquire-fence FD index".into());
        }
        let surface_id = CString::new(surface_id)?;
        let mut descriptors: Vec<Option<OwnedFd>> =
            file_descriptors.into_iter().map(Some).collect();
        let mut fds = Vec::with_capacity(frame.planes.len());
        let mut strides = Vec::with_capacity(frame.planes.len());
        let mut offsets = Vec::with_capacity(frame.planes.len());
        for plane in &frame.planes {
            let descriptor = descriptors
                .get_mut(plane.fd_index as usize)
                .and_then(Option::take)
                .expect("frame plane indices were validated");
            fds.push(descriptor.into_raw_fd());
            strides.push(plane.stride);
            offsets.push(plane.offset);
        }
        let acquire_fence_fd = if frame.has_acquire_fence {
            descriptors
                .get_mut(frame.acquire_fence_fd_index as usize)
                .and_then(Option::take)
                .expect("acquire-fence index was validated")
                .into_raw_fd()
        } else {
            -1
        };
        let lease_id = register_frame_lease(
            &self.lease_sender,
            browser_id,
            frame.frame_id,
            layer,
            *self
                .surface_generations
                .get(&browser_id)
                .ok_or("browser surface has no generation")?,
        );
        unsafe {
            deb_browser_surface_submit(
                surface_id.as_ptr(),
                browser_id,
                lease_id,
                frame.layer,
                frame.x,
                frame.y,
                frame.width,
                frame.height,
                frame.drm_format,
                frame.modifier,
                i32::from(frame.flip_y),
                fds.len() as u32,
                fds.as_ptr(),
                strides.as_ptr(),
                offsets.as_ptr(),
                acquire_fence_fd,
            );
        }
        Ok(())
    }

    fn send_frame_release(&self, browser_id: u64, frame_id: u64) -> NativeResult<()> {
        self.transport.send(&wire::Packet {
            request_id: 0,
            body: Some(wire::packet::Body::FrameRelease(wire::FrameRelease {
                browser_id,
                frame_id,
            })),
        })?;
        Ok(())
    }

    pub(crate) fn shutdown(mut self) {
        for browser_id in self.surface_generations.keys().copied() {
            forget_qt_browser(browser_id);
        }
        if let Err(error) = self.send_process_request(
            wire::request::Operation::Shutdown(wire::Shutdown {}),
            "shutdown",
        ) {
            eprintln!(
                "deb: failure: helper shutdown: browser={} process={}: {error}",
                self.browser_id,
                self.child.id()
            );
        }
        stop_child(&mut self.child);
    }

    pub(crate) fn initial_browser_id(&self) -> u64 {
        self.browser_id
    }

    pub(crate) fn protocol_closed(&self) -> bool {
        self.protocol_closed
    }
}

pub(crate) struct RoutedNotice {
    pub(crate) browser_id: u64,
    pub(crate) value: ProtocolNotice,
}

pub(crate) enum ProtocolNotice {
    CommandFailed(String),
    SurfaceReady,
    FrameReady,
    LoadingChanged(bool),
    NavigationCommitted(String),
    TitleChanged(String),
    LoadFailed(String),
    Closed,
    Crashed(String),
    ProtocolFailed(String),
    CookieSnapshotEntry(wire::Cookie),
    CookieSnapshotComplete,
    CookieChanged(wire::Cookie, wire::CookieChangeCause),
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_cef_browser(
    _connection: &RustConnection,
    parent: Window,
    bounds: NativeRect,
    url: &str,
    profile_id: &str,
    directories: &EngineDirectories,
    backend: CefBackend,
    browser_id: u64,
    surface_id: String,
) -> NativeResult<CefInstance> {
    let executable = std::env::current_exe()?;
    let executable_directory = executable
        .parent()
        .ok_or("application executable has no parent directory")?;
    let loader_directory = backend.loader_directory(executable_directory)?;
    let helper = match backend {
        CefBackend::Chromium => executable_directory.join("cef-renderer"),
        CefBackend::Firefox => loader_directory
            .as_ref()
            .ok_or("FirefoxCEF runtime directory is unavailable")?
            .join("cef-renderer"),
    };
    let mut command = Command::new(helper);
    if let Some(directory) = &loader_directory {
        command.env("LD_LIBRARY_PATH", directory);
        let mut preload = vec![directory.join("libmozglue-cef.so")];
        if let Some(existing) = std::env::var_os("LD_PRELOAD") {
            preload.extend(std::env::split_paths(&existing));
        }
        command.env("LD_PRELOAD", std::env::join_paths(preload)?);
        command.env("DEB_CEF_SINGLE_THREADED", "1");
        command.env(
            "FIREFOX_CEF_APP_INI",
            directory.join("browser/firefox-cef.ini"),
        );
        command.env("GDK_BACKEND", "x11");
        command.env("MOZ_ENABLE_WAYLAND", "0");
    }
    let (transport, child_transport) = Transport::pair()?;
    configure_child_command(&mut command, &child_transport);
    let mut child = command.spawn()?;
    drop(child_transport);
    let (incoming, _reader) = spawn_protocol_reader(transport.try_clone()?);
    let readiness_timeout = match backend {
        CefBackend::Chromium => Duration::from_secs(30),
        CefBackend::Firefox => Duration::from_secs(90),
    };
    let last_event_sequence = match initialize_helper(
        &transport,
        &incoming,
        &mut child,
        readiness_timeout,
        backend,
        parent,
        bounds,
        url,
        profile_id,
        directories,
        browser_id,
    ) {
        Ok(ready) => ready,
        Err(error) => {
            if let Err(kill_error) = child.kill() {
                eprintln!(
                    "deb: failure: helper startup cleanup: process={}: kill failed: {kill_error}",
                    child.id()
                );
            }
            if let Err(wait_error) = child.wait() {
                eprintln!(
                    "deb: failure: helper startup cleanup: process={}: wait failed: {wait_error}",
                    child.id()
                );
            }
            return Err(format!("{} did not become ready: {error}", backend.name()).into());
        }
    };
    let (lease_sender, lease_events) = mpsc::channel();
    bind_qt_surface(&surface_id, browser_id, 1);
    Ok(CefInstance {
        child,
        transport,
        incoming,
        surfaces: HashMap::from([(browser_id, surface_id)]),
        surface_generations: HashMap::from([(browser_id, 1)]),
        lease_sender,
        lease_events,
        browser_id,
        next_request_id: 3,
        pending_requests: HashMap::new(),
        ready_browsers: HashSet::from([browser_id]),
        deferred_browser_requests: HashMap::new(),
        last_event_sequence,
        protocol_closed: false,
    })
}

fn spawn_protocol_reader(
    transport: Transport,
) -> (
    mpsc::Receiver<Result<shell_protocol::ReceivedPacket, ProtocolError>>,
    JoinHandle<()>,
) {
    let (sender, receiver) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        loop {
            let received = transport.receive_with_fds();
            let failed = received.is_err();
            if sender.send(received).is_err() || failed {
                break;
            }
        }
    });
    (receiver, thread)
}

#[allow(clippy::too_many_arguments)]
fn initialize_helper(
    transport: &Transport,
    incoming: &mpsc::Receiver<Result<shell_protocol::ReceivedPacket, ProtocolError>>,
    child: &mut Child,
    timeout: Duration,
    backend: CefBackend,
    parent: Window,
    bounds: NativeRect,
    url: &str,
    profile_id: &str,
    directories: &EngineDirectories,
    browser_id: u64,
) -> NativeResult<u64> {
    transport.send(&wire::Packet {
        request_id: 1,
        body: Some(wire::packet::Body::Hello(wire::Hello {
            maximum_packet_bytes: MAX_PACKET_BYTES as u32,
            requested_capabilities: required_capabilities(),
        })),
    })?;
    let deadline = Instant::now() + timeout;
    let hello = receive_startup_packet(incoming, child, deadline)?;
    if !hello.file_descriptors.is_empty() {
        return Err("HelloReply unexpectedly carried file descriptors".into());
    }
    if hello.packet.request_id != 1 {
        return Err(format!(
            "HelloReply used request ID {}, expected 1",
            hello.packet.request_id
        )
        .into());
    }
    let reply = match hello.packet.body {
        Some(wire::packet::Body::HelloReply(reply)) => reply,
        Some(wire::packet::Body::Response(response)) => {
            return Err(format_response_error("protocol negotiation", response).into());
        }
        _ => return Err("helper did not answer Hello with HelloReply".into()),
    };
    validate_hello_reply(&reply, backend)?;

    transport.send(&request_packet(
        2,
        browser_id,
        wire::request::Operation::CreateBrowser(wire::CreateBrowser {
            initial_url: url.to_owned(),
            surface: Some(wire::SurfaceTarget {
                parent_window: u64::from(parent),
                viewport: Some(bounds.viewport()),
            }),
            profile_id: profile_id.to_owned(),
            profile_data_path: protocol_path(&directories.data)?,
            profile_cache_path: protocol_path(&directories.cache)?,
        }),
    ))?;

    let mut create_succeeded = false;
    let mut surface_ready = false;
    let mut last_event_sequence = 0;
    while !create_succeeded || !surface_ready {
        let received = receive_startup_packet(incoming, child, deadline)?;
        let request_id = received.packet.request_id;
        match received.packet.body {
            Some(wire::packet::Body::Response(response)) => {
                if request_id != 2 {
                    return Err(format!(
                        "startup response used request ID {}, expected 2",
                        request_id
                    )
                    .into());
                }
                match response.result {
                    Some(wire::response::Result::Success(_)) => create_succeeded = true,
                    _ => return Err(format_response_error("browser creation", response).into()),
                }
            }
            Some(wire::packet::Body::Event(event)) => {
                if event.browser_id != browser_id {
                    return Err(format!(
                        "startup event targets browser {}, expected {}",
                        event.browser_id, browser_id
                    )
                    .into());
                }
                if event.sequence <= last_event_sequence {
                    return Err(format!(
                        "startup event sequence {} followed {}",
                        event.sequence, last_event_sequence
                    )
                    .into());
                }
                last_event_sequence = event.sequence;
                match event.value {
                    Some(wire::event::Value::SurfaceReady(_)) => surface_ready = true,
                    Some(wire::event::Value::AcceleratedFrame(frame)) => {
                        transport.send(&wire::Packet {
                            request_id: 0,
                            body: Some(wire::packet::Body::FrameRelease(wire::FrameRelease {
                                browser_id,
                                frame_id: frame.frame_id,
                            })),
                        })?;
                    }
                    Some(wire::event::Value::BrowserCrashed(crash)) => {
                        return Err(
                            format!("browser crashed during startup: {}", crash.reason).into()
                        );
                    }
                    Some(wire::event::Value::BrowserClosed(_)) => {
                        return Err("browser closed during startup".into());
                    }
                    Some(_) => {}
                    None => {}
                }
            }
            _ => return Err("unexpected packet during browser startup".into()),
        }
    }
    Ok(last_event_sequence)
}

fn protocol_path(path: &Path) -> NativeResult<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("profile path is not valid UTF-8: {}", path.display()).into())
}

pub(crate) fn clear_qt_surface(surface_id: &str, layer: wire::SurfaceLayer) {
    if let Ok(surface_id) = CString::new(surface_id) {
        unsafe { deb_browser_surface_clear(surface_id.as_ptr(), layer as i32) };
    }
}

fn receive_startup_packet(
    incoming: &mpsc::Receiver<Result<shell_protocol::ReceivedPacket, ProtocolError>>,
    child: &mut Child,
    deadline: Instant,
) -> NativeResult<shell_protocol::ReceivedPacket> {
    while Instant::now() < deadline {
        match incoming.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(received)) => return Ok(received),
            Ok(Err(error)) => return Err(error.into()),
            Err(RecvTimeoutError::Disconnected) => {
                return Err("CEF protocol reader stopped".into());
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        if let Some(status) = child.try_wait()? {
            return Err(format!("CEF helper exited before readiness: {status}").into());
        }
    }
    Err("CEF helper readiness timed out".into())
}

fn validate_hello_reply(reply: &wire::HelloReply, backend: CefBackend) -> NativeResult<()> {
    let engine = Engine::try_from(reply.engine).unwrap_or(Engine::Unspecified);
    if engine != backend.engine() {
        return Err(format!("{} identified itself as {engine:?}", backend.name()).into());
    }
    if reply.cef_api_version == 0 || reply.maximum_packet_bytes == 0 {
        return Err("helper returned invalid CEF or packet-size metadata".into());
    }
    let advertised = reply.capabilities.iter().copied().collect::<HashSet<_>>();
    let missing = required_capabilities()
        .into_iter()
        .filter(|capability| !advertised.contains(capability))
        .filter_map(|capability| Capability::try_from(capability).ok())
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!("helper is missing required capabilities: {missing:?}").into());
    }
    Ok(())
}

fn required_capabilities() -> Vec<i32> {
    [
        Capability::BrowserLifecycle,
        Capability::Navigation,
        Capability::Resize,
        Capability::Focus,
        Capability::LoadingEvents,
        Capability::AcceleratedDmabufSurface,
        Capability::CookieSync,
        Capability::MultipleBrowsers,
        Capability::Visibility,
        Capability::RendererCrashEvents,
        Capability::PointerInput,
        Capability::KeyboardInput,
        Capability::CursorEvents,
        Capability::TouchInput,
    ]
    .into_iter()
    .map(|capability| capability as i32)
    .collect()
}

fn request_packet(
    request_id: u64,
    browser_id: u64,
    operation: wire::request::Operation,
) -> wire::Packet {
    wire::Packet {
        request_id,
        body: Some(wire::packet::Body::Request(wire::Request {
            browser_id,
            operation: Some(operation),
        })),
    }
}

fn format_response_error(context: &str, response: wire::Response) -> String {
    match response.result {
        Some(wire::response::Result::Error(error)) => {
            format!("{context} failed [{}]: {}", error.code, error.message)
        }
        Some(wire::response::Result::Success(_)) => {
            format!("{context} returned an unexpected success")
        }
        None => format!("{context} response has no result"),
    }
}

fn stop_child(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => sleep(Duration::from_millis(50)),
            Err(error) => {
                eprintln!(
                    "deb: failure: helper shutdown: process={}: status check failed: {error}",
                    child.id()
                );
                break;
            }
        }
    }
    if let Err(error) = child.kill() {
        eprintln!(
            "deb: failure: helper shutdown: process={}: kill failed: {error}",
            child.id()
        );
    }
    if let Err(error) = child.wait() {
        eprintln!(
            "deb: failure: helper shutdown: process={}: wait failed: {error}",
            child.id()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{CefBackend, NativeRect, validate_hello_reply};
    use shell_protocol::{MAX_PACKET_BYTES, wire};

    #[test]
    fn rejects_zero_sized_surfaces() {
        assert!(NativeRect::new(0, 0, 0, 100).is_none());
        assert!(NativeRect::new(0, 0, 100, 1).is_none());
    }

    #[test]
    fn converts_bounds_to_a_viewport() {
        let bounds = NativeRect::new(10, 20, 800, 600).unwrap();
        assert_eq!(
            bounds.viewport(),
            wire::Viewport {
                x: 10,
                y: 20,
                width: 800,
                height: 600,
                scale_factor: 1.0,
            }
        );
    }

    #[test]
    fn rejects_a_helper_with_the_wrong_engine() {
        let reply = wire::HelloReply {
            engine: wire::Engine::Gecko as i32,
            engine_version: "test".to_owned(),
            cef_api_version: 1,
            maximum_packet_bytes: MAX_PACKET_BYTES as u32,
            capabilities: super::required_capabilities(),
        };
        let error = validate_hello_reply(&reply, CefBackend::Chromium)
            .unwrap_err()
            .to_string();
        assert!(error.contains("identified itself as Gecko"));
    }

    #[test]
    fn rejects_a_helper_missing_a_required_capability() {
        let mut capabilities = super::required_capabilities();
        capabilities
            .retain(|capability| *capability != wire::Capability::AcceleratedDmabufSurface as i32);
        let reply = wire::HelloReply {
            engine: wire::Engine::Chromium as i32,
            engine_version: "test".to_owned(),
            cef_api_version: 1,
            maximum_packet_bytes: MAX_PACKET_BYTES as u32,
            capabilities,
        };
        let error = validate_hello_reply(&reply, CefBackend::Chromium)
            .unwrap_err()
            .to_string();
        assert!(error.contains("AcceleratedDmabufSurface"));
    }
}
