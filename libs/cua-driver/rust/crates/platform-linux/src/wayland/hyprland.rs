//! Hyprland-specific window metadata and input targeting.
//!
//! Generic Wayland protocols intentionally omit process ids and global window
//! geometry. Hyprland exposes both through its local `hyprctl -j` IPC surface,
//! which lets cua-driver keep the normal `(pid, window_id)` contract while
//! still capturing and targeting native Wayland windows precisely.

use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::x11::WindowInfo;

const FOCUS_TIMEOUT: Duration = Duration::from_millis(500);
const FOCUS_POLL_INTERVAL: Duration = Duration::from_millis(15);
const SURROGATE_ID_MIN: u64 = 0x8000_0000;

#[derive(Clone, Debug, Deserialize)]
struct HyprWorkspace {
    #[serde(default)]
    id: i32,
}

#[derive(Clone, Debug, Deserialize)]
struct HyprClient {
    address: String,
    #[serde(default)]
    mapped: bool,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    at: [i32; 2],
    #[serde(default)]
    size: [i32; 2],
    #[serde(default)]
    class: String,
    #[serde(default, rename = "initialClass")]
    initial_class: String,
    #[serde(default)]
    title: String,
    #[serde(default, rename = "initialTitle")]
    initial_title: String,
    #[serde(default, rename = "stableId")]
    stable_id: String,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    workspace: Option<HyprWorkspace>,
    #[serde(default)]
    pinned: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct HyprMonitor {
    #[serde(default)]
    name: String,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    #[serde(default)]
    x: i32,
    #[serde(default)]
    y: i32,
    #[serde(default = "default_scale")]
    scale: f64,
    #[serde(default)]
    transform: i32,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    disabled: bool,
    #[serde(default, rename = "activeWorkspace")]
    active_workspace: Option<HyprWorkspace>,
    #[serde(default, rename = "specialWorkspace")]
    special_workspace: Option<HyprWorkspace>,
}

#[derive(Clone, Debug, Deserialize)]
struct HyprCursor {
    x: i32,
    y: i32,
}

#[derive(Clone, Debug, Deserialize)]
struct HyprActiveWindow {
    #[serde(default)]
    address: String,
}

#[derive(Clone, Debug)]
pub(crate) struct PointerTarget {
    pub output_name: String,
    pub output_width: u32,
    pub output_height: u32,
    pub output_x: u32,
    pub output_y: u32,
    pub screen_x: f64,
    pub screen_y: f64,
    pub capture_x: f64,
    pub capture_y: f64,
    pub capture_to_output_scale: f64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DesktopCaptureSpace {
    pub origin_x: i32,
    pub origin_y: i32,
    pub scale: f64,
    pub width: u32,
    pub height: u32,
}

#[derive(Default)]
struct IdRegistry {
    by_address: HashMap<String, u32>,
    // Historical registrations stay here as tombstones. Hyprland addresses
    // are allocator pointers and can be reused after a window closes; keeping
    // the old identity prevents a stale id from silently retargeting the new
    // window that inherited the same address.
    by_id: HashMap<u32, WindowIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WindowIdentity {
    address: String,
    stable_id: String,
    pid: Option<u32>,
    initial_class: String,
    initial_title: String,
}

impl From<&HyprClient> for WindowIdentity {
    fn from(client: &HyprClient) -> Self {
        Self {
            address: client.address.clone(),
            stable_id: client.stable_id.clone(),
            pid: client.pid,
            initial_class: if client.initial_class.is_empty() {
                client.class.clone()
            } else {
                client.initial_class.clone()
            },
            // Never fall back to the mutable current title: tab/document title
            // changes must not invalidate an otherwise live window id.
            initial_title: client.initial_title.clone(),
        }
    }
}

impl IdRegistry {
    fn register(&mut self, identity: WindowIdentity) -> u32 {
        let candidate = surrogate_seed(&identity.address);
        self.register_with_candidate(identity, candidate)
    }

    fn register_with_candidate(&mut self, identity: WindowIdentity, candidate: u32) -> u32 {
        if let Some(id) = self.by_address.get(&identity.address).copied() {
            if self.by_id.get(&id) == Some(&identity) {
                return id;
            }
            self.by_address.remove(&identity.address);
        }

        // Keep Hyprland ids in the upper half of the u32 range so they do not
        // resemble the small foreign-toplevel protocol ids used by the generic
        // backend. Linear probing makes the truncated hash collision-safe.
        let mut id = candidate | 0x8000_0000;
        while self.by_id.contains_key(&id) {
            id = id.wrapping_add(1) | 0x8000_0000;
        }
        self.by_address.insert(identity.address.clone(), id);
        self.by_id.insert(id, identity);
        id
    }

    fn sync(&mut self, clients: &[HyprClient]) {
        let live: HashMap<&str, WindowIdentity> = clients
            .iter()
            .map(|client| (client.address.as_str(), WindowIdentity::from(client)))
            .collect();
        self.by_address.retain(|address, id| {
            live.get(address.as_str())
                .is_some_and(|identity| self.by_id.get(id) == Some(identity))
        });

        // Registration order must not depend on `hyprctl`'s client ordering,
        // otherwise a hash collision could assign different ids between calls.
        let mut sorted: Vec<WindowIdentity> = live.into_values().collect();
        sorted.sort_unstable_by(|left, right| left.address.cmp(&right.address));
        for identity in sorted {
            self.register(identity);
        }
    }

    fn identity(&self, id: u64) -> Option<&WindowIdentity> {
        let id = u32::try_from(id).ok()?;
        self.by_id.get(&id)
    }
}

static IDS: OnceLock<Mutex<IdRegistry>> = OnceLock::new();

fn ids() -> &'static Mutex<IdRegistry> {
    IDS.get_or_init(|| Mutex::new(IdRegistry::default()))
}

fn surrogate_seed(address: &str) -> u32 {
    // FNV-1a is tiny, deterministic, and sufficient here because IdRegistry
    // resolves the rare 32-bit collision before exposing an id.
    let mut hash = 0x811c_9dc5_u32;
    for byte in address.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

pub(crate) fn is_surrogate_window_id(window_id: u64) -> bool {
    (SURROGATE_ID_MIN..=u64::from(u32::MAX)).contains(&window_id)
}

fn default_scale() -> f64 {
    1.0
}

fn desktop_mentions_hyprland(value: &str) -> bool {
    value
        .split([':', ';'])
        .any(|part| part.trim().eq_ignore_ascii_case("hyprland"))
}

fn session_markers_indicate_hyprland(
    has_signature: bool,
    desktop: Option<&str>,
    nested_wayland: bool,
) -> bool {
    // `ensure_nested_session` deliberately points WAYLAND_DISPLAY at a private
    // compositor but leaves the host's Hyprland variables inherited. Treating
    // that private socket as the host compositor would enumerate host windows
    // and then inject into the nested output. The supported nested mode is a
    // distinct backend and must not use Hyprland IPC.
    !nested_wayland && (has_signature || desktop.is_some_and(desktop_mentions_hyprland))
}

pub(crate) fn is_session() -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    static AVAILABLE: AtomicBool = AtomicBool::new(false);
    if AVAILABLE.load(Ordering::Relaxed) {
        return true;
    }

    let desktop = std::env::var("XDG_CURRENT_DESKTOP").ok();
    if !session_markers_indicate_hyprland(
        std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some(),
        desktop.as_deref(),
        std::env::var_os("CUA_WAYLAND_NEST").is_some(),
    ) {
        return false;
    }

    // Cache only a successful probe. During session startup hyprctl can lag
    // the environment markers briefly; caching that transient false would pin
    // the daemon to X11 until restart. Once selected, later IPC failures fail
    // closed in the Hyprland path rather than mixing window-id namespaces.
    let available = hyprctl_json::<Vec<HyprMonitor>>(&["monitors"])
        .map(|monitors| monitors.iter().any(|monitor| !monitor.disabled))
        .unwrap_or(false);
    if available {
        AVAILABLE.store(true, Ordering::Relaxed);
    }
    available
}

fn hyprctl_json<T: for<'de> Deserialize<'de>>(args: &[&str]) -> anyhow::Result<T> {
    let output = Command::new("hyprctl").arg("-j").args(args).output()?;
    if !output.status.success() {
        anyhow::bail!(
            "hyprctl -j {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| anyhow::anyhow!("invalid hyprctl -j {} response: {error}", args.join(" ")))
}

fn clients() -> anyhow::Result<Vec<HyprClient>> {
    let clients: Vec<HyprClient> = hyprctl_json(&["clients"])?;
    ids()
        .lock()
        .map_err(|_| anyhow::anyhow!("Hyprland window-id registry is poisoned"))?
        .sync(&clients);
    Ok(clients)
}

fn monitors() -> anyhow::Result<Vec<HyprMonitor>> {
    hyprctl_json(&["monitors"])
}

fn active_window() -> anyhow::Result<HyprActiveWindow> {
    hyprctl_json(&["activewindow"])
}

fn id_for_client(client: &HyprClient) -> anyhow::Result<u32> {
    Ok(ids()
        .lock()
        .map_err(|_| anyhow::anyhow!("Hyprland window-id registry is poisoned"))?
        .register(WindowIdentity::from(client)))
}

fn identity_for_id(window_id: u64) -> anyhow::Result<WindowIdentity> {
    ids()
        .lock()
        .map_err(|_| anyhow::anyhow!("Hyprland window-id registry is poisoned"))?
        .identity(window_id)
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown Hyprland window_id {window_id}; call list_windows again to refresh it"
            )
        })
}

fn client_for_window(window_id: u64) -> anyhow::Result<HyprClient> {
    let clients = clients()?;
    let identity = identity_for_id(window_id)?;
    clients
        .into_iter()
        .find(|client| WindowIdentity::from(client) == identity)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "stale Hyprland window_id {window_id}; its original window no longer exists"
            )
        })
}

fn monitor_logical_size(monitor: &HyprMonitor) -> (u32, u32) {
    let scale = monitor.scale.max(f64::EPSILON);
    let (width, height) = monitor_physical_size(monitor);
    (
        ((width as f64) / scale).round().max(1.0) as u32,
        ((height as f64) / scale).round().max(1.0) as u32,
    )
}

fn monitor_physical_size(monitor: &HyprMonitor) -> (u32, u32) {
    if monitor.transform.rem_euclid(2) == 1 {
        (monitor.height, monitor.width)
    } else {
        (monitor.width, monitor.height)
    }
}

fn pointer_coordinates_for_monitor(
    monitor: &HyprMonitor,
    screen_x: f64,
    screen_y: f64,
) -> (u32, u32, u32, u32) {
    // Hyprland normalizes motion_absolute against these extents and maps the
    // resulting x/y fractions directly into the bound monitor's logicalBox.
    // A transformed output therefore needs transformed extents, but the point
    // itself must stay on the logical x/y axes; rotating it into framebuffer
    // coordinates here would rotate the pointer a second time.
    let scale = monitor.scale.max(f64::EPSILON);
    let (output_width, output_height) = monitor_physical_size(monitor);
    let output_width = output_width.max(1);
    let output_height = output_height.max(1);
    let output_x = ((screen_x - monitor.x as f64) * scale)
        .round()
        .clamp(0.0, output_width.saturating_sub(1) as f64) as u32;
    let output_y = ((screen_y - monitor.y as f64) * scale)
        .round()
        .clamp(0.0, output_height.saturating_sub(1) as f64) as u32;
    (output_width, output_height, output_x, output_y)
}

fn monitor_for_screen_point(
    monitors: &[HyprMonitor],
    screen_x: f64,
    screen_y: f64,
) -> Option<&HyprMonitor> {
    monitors
        .iter()
        .filter(|monitor| !monitor.disabled)
        .find(|monitor| {
            let (width, height) = monitor_logical_size(monitor);
            screen_x >= monitor.x as f64
                && screen_y >= monitor.y as f64
                && screen_x < monitor.x.saturating_add(width as i32) as f64
                && screen_y < monitor.y.saturating_add(height as i32) as f64
        })
}

fn global_pointer_target_for(
    monitors: &[HyprMonitor],
    screen_x: f64,
    screen_y: f64,
) -> anyhow::Result<PointerTarget> {
    let monitor = monitor_for_screen_point(monitors, screen_x, screen_y).ok_or_else(|| {
        anyhow::anyhow!(
            "screen point ({screen_x:.1}, {screen_y:.1}) is outside every enabled Hyprland output"
        )
    })?;
    let (output_width, output_height, output_x, output_y) =
        pointer_coordinates_for_monitor(monitor, screen_x, screen_y);
    Ok(PointerTarget {
        output_name: monitor.name.clone(),
        output_width,
        output_height,
        output_x,
        output_y,
        screen_x,
        screen_y,
        capture_x: 0.0,
        capture_y: 0.0,
        capture_to_output_scale: 1.0,
    })
}

fn desktop_capture_space_for(monitors: &[HyprMonitor]) -> anyhow::Result<DesktopCaptureSpace> {
    let enabled: Vec<&HyprMonitor> = monitors
        .iter()
        .filter(|monitor| !monitor.disabled)
        .collect();
    if enabled.is_empty() {
        anyhow::bail!("hyprctl monitors returned no enabled monitors");
    }
    let origin_x = enabled.iter().map(|monitor| monitor.x).min().unwrap_or(0);
    let origin_y = enabled.iter().map(|monitor| monitor.y).min().unwrap_or(0);
    let max_x = enabled
        .iter()
        .map(|monitor| {
            let (width, _) = monitor_logical_size(monitor);
            monitor.x.saturating_add(width as i32)
        })
        .max()
        .unwrap_or(origin_x);
    let max_y = enabled
        .iter()
        .map(|monitor| {
            let (_, height) = monitor_logical_size(monitor);
            monitor.y.saturating_add(height as i32)
        })
        .max()
        .unwrap_or(origin_y);
    // `grim` renders a multi-output region at the highest intersecting output
    // scale. This is the conversion from its PNG pixels back to Hyprland's
    // global logical coordinate space.
    let scale = enabled
        .iter()
        .map(|monitor| monitor.scale.max(f64::EPSILON))
        .fold(f64::EPSILON, f64::max);
    Ok(DesktopCaptureSpace {
        origin_x,
        origin_y,
        scale,
        width: (((max_x - origin_x).max(1) as f64) * scale).round() as u32,
        height: (((max_y - origin_y).max(1) as f64) * scale).round() as u32,
    })
}

fn capture_scale_for_client(client: &HyprClient, monitors: &[HyprMonitor]) -> f64 {
    let client_right = client.at[0].saturating_add(client.size[0].max(1));
    let client_bottom = client.at[1].saturating_add(client.size[1].max(1));
    monitors
        .iter()
        .filter(|monitor| !monitor.disabled)
        .filter(|monitor| {
            let (width, height) = monitor_logical_size(monitor);
            let right = monitor.x.saturating_add(width as i32);
            let bottom = monitor.y.saturating_add(height as i32);
            client.at[0] < right
                && client_right > monitor.x
                && client.at[1] < bottom
                && client_bottom > monitor.y
        })
        // grim renders geometry intersecting multiple outputs at the highest
        // intersecting output scale.
        .map(|monitor| monitor.scale.max(f64::EPSILON))
        .reduce(f64::max)
        .unwrap_or(1.0)
}

fn active_workspace_ids(monitors: &[HyprMonitor]) -> HashSet<i32> {
    monitors
        .iter()
        .filter(|monitor| !monitor.disabled)
        .flat_map(|monitor| {
            [
                monitor
                    .active_workspace
                    .as_ref()
                    .map(|workspace| workspace.id),
                monitor
                    .special_workspace
                    .as_ref()
                    .map(|workspace| workspace.id)
                    .filter(|id| *id != 0),
            ]
            .into_iter()
            .flatten()
        })
        .collect()
}

fn client_is_internal(client: &HyprClient) -> bool {
    client.pid == Some(std::process::id())
        || client.class.starts_with("Cua.AgentCursorOverlay.")
        || client.initial_class.starts_with("Cua.AgentCursorOverlay.")
        || client.title.starts_with("Cua.AgentCursorOverlay.")
        || client.initial_title.starts_with("Cua.AgentCursorOverlay.")
}

fn client_is_visible(client: &HyprClient, active_workspaces: &HashSet<i32>) -> bool {
    client.mapped
        && !client.hidden
        && !client_is_internal(client)
        && client.workspace.as_ref().is_some_and(|workspace| {
            active_workspaces.is_empty()
                || client.pinned
                || active_workspaces.contains(&workspace.id)
        })
}

fn ensure_client_is_visible(client: &HyprClient) -> anyhow::Result<()> {
    let monitors = monitors()?;
    ensure_client_is_visible_on(client, &monitors)
}

fn ensure_client_is_visible_on(
    client: &HyprClient,
    monitors: &[HyprMonitor],
) -> anyhow::Result<()> {
    if client_is_visible(client, &active_workspace_ids(monitors)) {
        Ok(())
    } else {
        anyhow::bail!(
            "Hyprland window is not visible on an active workspace; refusing to capture or focus a stale/off-workspace target"
        )
    }
}

fn windows_from_clients(
    clients: &[HyprClient],
    active_workspaces: &HashSet<i32>,
    filter_pid: Option<u32>,
) -> anyhow::Result<Vec<WindowInfo>> {
    let mut windows = Vec::new();
    for client in clients {
        if !client_is_visible(client, active_workspaces) {
            continue;
        }
        if filter_pid.is_some_and(|pid| client.pid != Some(pid)) {
            continue;
        }

        let title = match (
            client.title.trim().is_empty(),
            client.class.trim().is_empty(),
        ) {
            (true, true) => client.address.clone(),
            (false, true) => client.title.clone(),
            (true, false) => format!("[{}]", client.class),
            (false, false) => format!("{} [{}]", client.title, client.class),
        };
        windows.push(WindowInfo {
            xid: u64::from(id_for_client(client)?),
            pid: client.pid,
            title,
            x: client.at[0],
            y: client.at[1],
            width: client.size[0].max(0) as u32,
            height: client.size[1].max(0) as u32,
        });
    }
    windows.sort_by(|left, right| {
        left.pid
            .cmp(&right.pid)
            .then(left.y.cmp(&right.y))
            .then(left.x.cmp(&right.x))
            .then(left.title.cmp(&right.title))
    });
    Ok(windows)
}

pub(crate) fn list_windows(filter_pid: Option<u32>) -> anyhow::Result<Vec<WindowInfo>> {
    let clients = clients()?;
    let active_workspaces = active_workspace_ids(&monitors()?);
    windows_from_clients(&clients, &active_workspaces, filter_pid)
}

pub(crate) fn window_origin(window_id: u64) -> anyhow::Result<(i32, i32)> {
    let client = client_for_window(window_id)?;
    ensure_client_is_visible(&client)?;
    Ok((client.at[0], client.at[1]))
}

fn window_origin_for_pid_from_clients(
    clients: &[HyprClient],
    active_workspaces: &HashSet<i32>,
    pid: u32,
    active_address: Option<&str>,
) -> Option<(i32, i32)> {
    let matching: Vec<&HyprClient> = clients
        .iter()
        .filter(|client| client.pid == Some(pid) && client_is_visible(client, active_workspaces))
        .collect();

    if let Some(active_address) = active_address {
        if let Some(active) = matching
            .iter()
            .find(|client| client.address.eq_ignore_ascii_case(active_address))
        {
            return Some((active.at[0], active.at[1]));
        }
    }

    match matching.as_slice() {
        [only] => Some((only.at[0], only.at[1])),
        _ => None,
    }
}

pub(crate) fn window_origin_for_pid(pid: u32) -> anyhow::Result<Option<(i32, i32)>> {
    let clients = clients()?;
    let active_workspaces = active_workspace_ids(&monitors()?);
    let active = active_window().ok();
    Ok(window_origin_for_pid_from_clients(
        &clients,
        &active_workspaces,
        pid,
        active.as_ref().map(|window| window.address.as_str()),
    ))
}

pub(crate) fn window_scale(window_id: u64) -> anyhow::Result<f64> {
    let client = client_for_window(window_id)?;
    Ok(capture_scale_for_client(&client, &monitors()?))
}

pub(crate) fn window_metadata(window_id: u64) -> anyhow::Result<((i32, i32), f64)> {
    let client = client_for_window(window_id)?;
    let monitors = monitors()?;
    Ok((
        (client.at[0], client.at[1]),
        capture_scale_for_client(&client, &monitors),
    ))
}

pub(crate) fn window_capture_center(window_id: u64) -> anyhow::Result<(f64, f64)> {
    let client = client_for_window(window_id)?;
    let monitors = monitors()?;
    ensure_client_is_visible_on(&client, &monitors)?;
    let scale = capture_scale_for_client(&client, &monitors);
    Ok((
        (client.size[0].max(1) as f64) * scale / 2.0,
        (client.size[1].max(1) as f64) * scale / 2.0,
    ))
}

pub(crate) fn capture_to_screen(
    window_id: u64,
    capture_x: f64,
    capture_y: f64,
) -> anyhow::Result<(f64, f64)> {
    let target = pointer_target(window_id, capture_x, capture_y)?;
    Ok((target.screen_x, target.screen_y))
}

fn pointer_target_for_layout(
    client: &HyprClient,
    monitors: &[HyprMonitor],
    capture_x: f64,
    capture_y: f64,
) -> anyhow::Result<PointerTarget> {
    let capture_scale = capture_scale_for_client(client, monitors);
    let max_capture_x = ((client.size[0].max(1) as f64) * capture_scale - 1.0).max(0.0);
    let max_capture_y = ((client.size[1].max(1) as f64) * capture_scale - 1.0).max(0.0);
    let capture_x = capture_x.clamp(0.0, max_capture_x);
    let capture_y = capture_y.clamp(0.0, max_capture_y);
    let screen_x = client.at[0] as f64 + capture_x / capture_scale;
    let screen_y = client.at[1] as f64 + capture_y / capture_scale;
    let monitor = monitor_for_screen_point(monitors, screen_x, screen_y).ok_or_else(|| {
        anyhow::anyhow!(
            "window point maps to ({screen_x:.1}, {screen_y:.1}), outside every enabled Hyprland output"
        )
    })?;
    let output_scale = monitor.scale.max(f64::EPSILON);
    let (output_width, output_height, output_x, output_y) =
        pointer_coordinates_for_monitor(monitor, screen_x, screen_y);
    Ok(PointerTarget {
        output_name: monitor.name.clone(),
        output_width,
        output_height,
        output_x,
        output_y,
        screen_x,
        screen_y,
        capture_x,
        capture_y,
        capture_to_output_scale: output_scale / capture_scale,
    })
}

pub(crate) fn pointer_target(
    window_id: u64,
    capture_x: f64,
    capture_y: f64,
) -> anyhow::Result<PointerTarget> {
    let client = client_for_window(window_id)?;
    let monitors = monitors()?;
    ensure_client_is_visible_on(&client, &monitors)?;
    pointer_target_for_layout(&client, &monitors, capture_x, capture_y)
}

pub(crate) fn default_pointer_target(window_id: u64) -> anyhow::Result<PointerTarget> {
    let client = client_for_window(window_id)?;
    let monitors = monitors()?;
    ensure_client_is_visible_on(&client, &monitors)?;
    let scale = capture_scale_for_client(&client, &monitors);
    pointer_target_for_layout(
        &client,
        &monitors,
        (client.size[0].max(1) as f64) * scale / 2.0,
        (client.size[1].max(1) as f64) * scale / 2.0,
    )
}

pub(crate) fn global_pointer_target(screen_x: f64, screen_y: f64) -> anyhow::Result<PointerTarget> {
    global_pointer_target_for(&monitors()?, screen_x, screen_y)
}

pub(crate) fn desktop_capture_space() -> anyhow::Result<DesktopCaptureSpace> {
    desktop_capture_space_for(&monitors()?)
}

pub(crate) fn desktop_capture_to_screen(
    capture_x: f64,
    capture_y: f64,
) -> anyhow::Result<(f64, f64)> {
    let space = desktop_capture_space()?;
    Ok((
        space.origin_x as f64 + capture_x / space.scale,
        space.origin_y as f64 + capture_y / space.scale,
    ))
}

pub(crate) fn focus_window(window_id: u64) -> anyhow::Result<()> {
    let client = client_for_window(window_id)?;
    ensure_client_is_visible(&client)?;
    let address = client.address;
    let selector = format!("address:{address}");
    let output = Command::new("hyprctl")
        .args(["dispatch", "focuswindow", &selector])
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "hyprctl dispatch focuswindow failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let deadline = Instant::now() + FOCUS_TIMEOUT;
    loop {
        if active_window()
            .map(|window| window.address.eq_ignore_ascii_case(&address))
            .unwrap_or(false)
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("Hyprland did not focus window {address} within 500ms");
        }
        std::thread::sleep(FOCUS_POLL_INTERVAL);
    }
}

pub(crate) fn capture_window(window_id: u64) -> anyhow::Result<Vec<u8>> {
    let client = client_for_window(window_id)?;
    ensure_client_is_visible(&client)?;
    let width = client.size[0].max(1);
    let height = client.size[1].max(1);
    let geometry = format!("{},{} {}x{}", client.at[0], client.at[1], width, height);
    let output = Command::new("grim")
        .args(["-g", &geometry, "-t", "png", "-"])
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "grim -g {geometry:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if output.stdout.is_empty() {
        anyhow::bail!("grim -g {geometry:?} produced no output");
    }
    Ok(output.stdout)
}

pub(crate) fn capture_desktop() -> anyhow::Result<Vec<u8>> {
    let output = Command::new("grim").args(["-t", "png", "-"]).output()?;
    if !output.status.success() {
        anyhow::bail!(
            "grim desktop capture failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if output.stdout.is_empty() {
        anyhow::bail!("grim desktop capture produced no output");
    }
    Ok(output.stdout)
}

pub(crate) fn screen_size() -> anyhow::Result<(u32, u32, f64, i32, i32, String)> {
    let monitors = monitors()?;
    let monitor = monitors
        .iter()
        .find(|monitor| monitor.focused && !monitor.disabled)
        .or_else(|| monitors.iter().find(|monitor| !monitor.disabled))
        .ok_or_else(|| anyhow::anyhow!("hyprctl monitors returned no enabled monitors"))?;
    let (width, height) = monitor_logical_size(monitor);
    Ok((
        width,
        height,
        monitor.scale.max(f64::EPSILON),
        monitor.x,
        monitor.y,
        monitor.name.clone(),
    ))
}

pub(crate) fn cursor_position() -> anyhow::Result<(i32, i32)> {
    let cursor: HyprCursor = hyprctl_json(&["cursorpos"])?;
    Ok((cursor.x, cursor.y))
}

pub(crate) fn app_class(window_id: u64) -> anyhow::Result<String> {
    Ok(client_for_window(window_id)?.class)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(address: &str, pid: u32, workspace: i32) -> HyprClient {
        HyprClient {
            address: address.to_owned(),
            mapped: true,
            hidden: false,
            at: [100, 200],
            size: [800, 600],
            class: "test-app".to_owned(),
            initial_class: "test-app".to_owned(),
            title: "Test".to_owned(),
            initial_title: "Test".to_owned(),
            stable_id: format!("stable-{address}"),
            pid: Some(pid),
            workspace: Some(HyprWorkspace { id: workspace }),
            pinned: false,
        }
    }

    fn monitor(
        name: &str,
        width: u32,
        height: u32,
        x: i32,
        y: i32,
        scale: f64,
        workspace: i32,
    ) -> HyprMonitor {
        HyprMonitor {
            name: name.to_owned(),
            width,
            height,
            x,
            y,
            scale,
            transform: 0,
            focused: false,
            disabled: false,
            active_workspace: Some(HyprWorkspace { id: workspace }),
            special_workspace: None,
        }
    }

    #[test]
    fn desktop_detection_handles_composite_values() {
        assert!(desktop_mentions_hyprland("Hyprland"));
        assert!(desktop_mentions_hyprland("X-Cinnamon:Hyprland"));
        assert!(!desktop_mentions_hyprland("GNOME"));
        assert!(session_markers_indicate_hyprland(
            true,
            Some("GNOME"),
            false
        ));
        assert!(!session_markers_indicate_hyprland(
            true,
            Some("Hyprland"),
            true
        ));
    }

    #[test]
    fn surrogate_ids_fit_u32_and_are_stable_for_large_addresses() {
        let mut registry = IdRegistry::default();
        let address = "0x563ae2e94be0";
        let identity = WindowIdentity {
            address: address.to_owned(),
            stable_id: "stable-1".to_owned(),
            pid: Some(10),
            initial_class: "test".to_owned(),
            initial_title: "Test".to_owned(),
        };
        let first = registry.register(identity.clone());
        let second = registry.register(identity.clone());
        assert_eq!(first, second);
        assert_ne!(first, 0);
        assert_eq!(registry.identity(u64::from(first)), Some(&identity));
    }

    #[test]
    fn surrogate_namespace_excludes_xwayland_xids() {
        assert!(!is_surrogate_window_id(0x00a0_0023));
        assert!(is_surrogate_window_id(0x8000_0000));
        assert!(is_surrogate_window_id(u64::from(u32::MAX)));
        assert!(!is_surrogate_window_id(u64::from(u32::MAX) + 1));
    }

    #[test]
    fn surrogate_id_collisions_probe_without_reassigning() {
        let mut registry = IdRegistry::default();
        let first_identity = WindowIdentity {
            address: "0x111".to_owned(),
            stable_id: "stable-1".to_owned(),
            pid: Some(1),
            initial_class: "first".to_owned(),
            initial_title: "First".to_owned(),
        };
        let second_identity = WindowIdentity {
            address: "0x222".to_owned(),
            stable_id: "stable-2".to_owned(),
            pid: Some(2),
            initial_class: "second".to_owned(),
            initial_title: "Second".to_owned(),
        };
        let first = registry.register_with_candidate(first_identity.clone(), 7);
        let second = registry.register_with_candidate(second_identity, 7);
        assert_ne!(first, second);
        assert_eq!(registry.register_with_candidate(first_identity, 99), first);
    }

    #[test]
    fn reused_address_gets_a_new_id_and_leaves_old_identity_stale() {
        let mut registry = IdRegistry::default();
        let old = WindowIdentity {
            address: "0xabc".to_owned(),
            stable_id: "stable-old".to_owned(),
            pid: Some(10),
            initial_class: "old".to_owned(),
            initial_title: "Old".to_owned(),
        };
        let new = WindowIdentity {
            address: "0xabc".to_owned(),
            stable_id: "stable-new".to_owned(),
            pid: Some(10),
            initial_class: "old".to_owned(),
            initial_title: "Old".to_owned(),
        };
        let old_id = registry.register(old.clone());
        registry.by_address.clear();
        let new_id = registry.register(new.clone());
        assert_ne!(old_id, new_id);
        assert_eq!(registry.identity(u64::from(old_id)), Some(&old));
        assert_eq!(registry.identity(u64::from(new_id)), Some(&new));
    }

    #[test]
    fn window_filtering_drops_hidden_unmapped_inactive_and_other_pids() {
        let mut visible = client("0x100000000", 10, 2);
        let mut hidden = client("0x200000000", 10, 2);
        hidden.hidden = true;
        let mut unmapped = client("0x300000000", 10, 2);
        unmapped.mapped = false;
        let inactive = client("0x400000000", 10, 3);
        let other_pid = client("0x500000000", 20, 2);
        let mut pinned = client("0x600000000", 10, 3);
        pinned.pinned = true;
        visible.title = "Visible".to_owned();

        let windows = windows_from_clients(
            &[visible, hidden, unmapped, inactive, other_pid, pinned],
            &HashSet::from([2]),
            Some(10),
        )
        .unwrap();
        assert_eq!(windows.len(), 2);
        assert!(windows
            .iter()
            .any(|window| window.title.contains("Visible")));
    }

    #[test]
    fn window_filtering_rejects_clients_without_workspace_metadata() {
        let mut missing_workspace = client("0x700000000", 10, 2);
        missing_workspace.workspace = None;
        missing_workspace.pinned = true;

        assert!(!client_is_visible(&missing_workspace, &HashSet::from([2])));
        assert!(!client_is_visible(&missing_workspace, &HashSet::new()));
    }

    #[test]
    fn pid_origin_prefers_active_window_and_rejects_ambiguity() {
        let mut first = client("0xabc", 10, 2);
        first.at = [10, 20];
        let mut second = client("0xdef", 10, 2);
        second.at = [30, 40];
        let other_pid = client("0x999", 20, 2);
        let active_workspaces = HashSet::from([2]);

        assert_eq!(
            window_origin_for_pid_from_clients(
                &[first.clone(), second.clone(), other_pid.clone()],
                &active_workspaces,
                10,
                Some("0xDEF"),
            ),
            Some((30, 40))
        );
        assert_eq!(
            window_origin_for_pid_from_clients(
                &[first.clone(), second, other_pid],
                &active_workspaces,
                10,
                None,
            ),
            None
        );
        assert_eq!(
            window_origin_for_pid_from_clients(&[first], &active_workspaces, 10, None),
            Some((10, 20))
        );
    }

    #[test]
    fn visible_special_workspace_is_included_and_cursor_overlay_is_hidden() {
        let mut primary = monitor("DP-1", 1920, 1080, 0, 0, 1.0, 2);
        primary.special_workspace = Some(HyprWorkspace { id: -98 });
        let special = client("0x700", 10, -98);
        let mut overlay = client("0x800", 11, 2);
        overlay.class = "Cua.AgentCursorOverlay.default".to_owned();
        overlay.initial_class = overlay.class.clone();

        let active = active_workspace_ids(&[primary]);
        let windows = windows_from_clients(&[special, overlay], &active, None).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].pid, Some(10));
    }

    #[test]
    fn mixed_scale_desktop_capture_round_trips_to_output_coordinates() {
        let monitors = [
            monitor("DP-1", 3840, 2160, 4025, 0, 1.5, 1),
            monitor("eDP-1", 2880, 1800, 4349, 1487, 1.6, 2),
        ];
        let space = desktop_capture_space_for(&monitors).unwrap();
        assert_eq!((space.origin_x, space.origin_y), (4025, 0));
        assert_eq!((space.width, space.height), (4096, 4179));
        assert_eq!(space.scale, 1.6);

        let screen = (4747.5, 1844.375);
        let capture = (
            (screen.0 - f64::from(space.origin_x)) * space.scale,
            (screen.1 - f64::from(space.origin_y)) * space.scale,
        );
        let round_trip = (
            f64::from(space.origin_x) + capture.0 / space.scale,
            f64::from(space.origin_y) + capture.1 / space.scale,
        );
        assert_eq!(round_trip, screen);

        let target = global_pointer_target_for(&monitors, screen.0, screen.1).unwrap();
        assert_eq!(target.output_name, "eDP-1");
        assert_eq!((target.output_x, target.output_y), (638, 572));
    }

    #[test]
    fn desktop_capture_preserves_output_scales_below_one() {
        let monitors = [monitor("DP-1", 1920, 1080, -2560, 0, 0.75, 1)];
        let space = desktop_capture_space_for(&monitors).unwrap();
        assert_eq!(space.scale, 0.75);
        assert_eq!((space.width, space.height), (1920, 1080));

        let target = global_pointer_target_for(&monitors, -1280.0, 720.0).unwrap();
        assert_eq!((target.output_x, target.output_y), (960, 540));
    }

    #[test]
    fn rotated_output_swaps_layout_and_pointer_extents() {
        let mut portrait = monitor("DP-1", 1920, 1080, 0, 0, 1.0, 1);
        portrait.transform = 1;
        assert_eq!(monitor_physical_size(&portrait), (1080, 1920));
        assert_eq!(monitor_logical_size(&portrait), (1080, 1920));

        let space = desktop_capture_space_for(&[portrait.clone()]).unwrap();
        assert_eq!((space.width, space.height), (1080, 1920));
        let target = global_pointer_target_for(&[portrait], 270.0, 1440.0).unwrap();
        assert_eq!((target.output_width, target.output_height), (1080, 1920));
        assert_eq!((target.output_x, target.output_y), (270, 1440));
    }

    #[test]
    fn all_output_transforms_preserve_logical_pointer_axes() {
        for transform in 0..8 {
            let mut output = monitor("DP-1", 400, 300, 0, 0, 1.0, 1);
            output.transform = transform;
            let (width, height) = monitor_physical_size(&output);
            let logical_x = f64::from(width) / 4.0;
            let logical_y = f64::from(height) * 3.0 / 4.0;

            let target = global_pointer_target_for(&[output], logical_x, logical_y).unwrap();
            assert_eq!(
                (target.output_width, target.output_height),
                (width, height),
                "transform {transform}"
            );
            assert_eq!(
                (target.output_x, target.output_y),
                (width / 4, height * 3 / 4),
                "transform {transform}"
            );
        }
    }

    #[test]
    fn pointer_target_accounts_for_origin_and_fractional_scale() {
        let client = HyprClient {
            address: "0x123".to_owned(),
            mapped: true,
            hidden: false,
            at: [-1000, 100],
            size: [800, 600],
            class: String::new(),
            initial_class: String::new(),
            title: String::new(),
            initial_title: String::new(),
            stable_id: "stable-pointer".to_owned(),
            pid: Some(1),
            workspace: Some(HyprWorkspace { id: 1 }),
            pinned: false,
        };
        let mut monitor = monitor("DP-2", 1920, 1080, -1280, 0, 1.5, 1);
        monitor.focused = true;

        let target = pointer_target_for_layout(&client, &[monitor], 150.0, 75.0).unwrap();
        assert_eq!((target.screen_x, target.screen_y), (-900.0, 150.0));
        assert_eq!((target.output_x, target.output_y), (570, 225));
        assert_eq!(target.output_name, "DP-2");
    }
}
