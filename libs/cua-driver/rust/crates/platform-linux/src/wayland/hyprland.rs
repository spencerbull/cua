//! Hyprland compositor metadata, capture, focus, and output targeting.
//!
//! Generic Wayland protocols deliberately omit process ids, global window
//! geometry, monitor layout, and the real cursor position. Hyprland exposes
//! those through its authenticated per-user IPC socket via `hyprctl -j`.

use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::x11::WindowInfo;

const FOCUS_TIMEOUT: Duration = Duration::from_millis(500);
const FOCUS_POLL_INTERVAL: Duration = Duration::from_millis(15);
const SURROGATE_ID_MIN: u64 = 0x8000_0000;

#[derive(Clone, Debug, Default, Deserialize)]
struct Workspace {
    id: i32,
}

#[derive(Clone, Debug, Deserialize)]
struct Client {
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
    workspace: Option<Workspace>,
    #[serde(default)]
    pinned: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct Monitor {
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
    active_workspace: Option<Workspace>,
    #[serde(default, rename = "specialWorkspace")]
    special_workspace: Option<Workspace>,
}

#[derive(Deserialize)]
struct CursorPosition {
    x: i32,
    y: i32,
}

#[derive(Deserialize)]
struct ActiveWindow {
    #[serde(default)]
    address: String,
    #[serde(default, rename = "stableId")]
    stable_id: String,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    class: String,
    #[serde(default, rename = "initialClass")]
    initial_class: String,
    #[serde(default, rename = "initialTitle")]
    initial_title: String,
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
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DesktopCaptureSpace {
    pub origin_x: i32,
    pub origin_y: i32,
    pub scale: f64,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WindowIdentity {
    address: String,
    stable_id: String,
    pid: Option<u32>,
    initial_class: String,
    initial_title: String,
}

impl ActiveWindow {
    fn identity(&self) -> Option<WindowIdentity> {
        (!self.address.trim().is_empty()).then(|| WindowIdentity {
            address: self.address.clone(),
            stable_id: self.stable_id.clone(),
            pid: self.pid,
            initial_class: if self.initial_class.is_empty() {
                self.class.clone()
            } else {
                self.initial_class.clone()
            },
            initial_title: self.initial_title.clone(),
        })
    }
}

impl From<&Client> for WindowIdentity {
    fn from(client: &Client) -> Self {
        Self {
            address: client.address.clone(),
            stable_id: client.stable_id.clone(),
            pid: client.pid,
            initial_class: if client.initial_class.is_empty() {
                client.class.clone()
            } else {
                client.initial_class.clone()
            },
            initial_title: client.initial_title.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct FocusLease {
    target: WindowIdentity,
    prior: Option<WindowIdentity>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FocusRestoreOutcome {
    Restored,
    NoPriorFocus,
    ActiveFocusChanged,
    PriorWindowUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum FocusRestoreDecision {
    Restore(WindowIdentity),
    NoPriorFocus,
    ActiveFocusChanged,
    PriorWindowUnavailable,
}

#[derive(Default)]
struct IdRegistry {
    by_address: HashMap<String, u32>,
    // Keep historical registrations as tombstones: Hyprland addresses are
    // allocator pointers and may be reused after a window closes.
    by_id: HashMap<u32, WindowIdentity>,
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
        let mut id = candidate | 0x8000_0000;
        while self.by_id.contains_key(&id) {
            id = id.wrapping_add(1) | 0x8000_0000;
        }
        self.by_address.insert(identity.address.clone(), id);
        self.by_id.insert(id, identity);
        id
    }

    fn sync(&mut self, clients: &[Client]) {
        let live = clients
            .iter()
            .map(|client| (client.address.as_str(), WindowIdentity::from(client)))
            .collect::<HashMap<_, _>>();
        self.by_address.retain(|address, id| {
            live.get(address.as_str())
                .is_some_and(|identity| self.by_id.get(id) == Some(identity))
        });
        let mut identities = live.into_values().collect::<Vec<_>>();
        identities.sort_unstable_by(|left, right| left.address.cmp(&right.address));
        for identity in identities {
            self.register(identity);
        }
    }

    fn identity(&self, window_id: u64) -> Option<&WindowIdentity> {
        self.by_id.get(&u32::try_from(window_id).ok()?)
    }
}

fn ids() -> &'static Mutex<IdRegistry> {
    static IDS: OnceLock<Mutex<IdRegistry>> = OnceLock::new();
    IDS.get_or_init(|| Mutex::new(IdRegistry::default()))
}

fn surrogate_seed(address: &str) -> u32 {
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
    // A private CUA_WAYLAND_NEST socket must not accidentally enumerate and
    // focus windows in the inherited host Hyprland session.
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
    // Cache only success so a daemon started before Hyprland IPC is ready can
    // recover without a restart.
    let available = monitors()
        .map(|monitors| monitors.iter().any(|monitor| !monitor.disabled))
        .unwrap_or(false);
    if available {
        AVAILABLE.store(true, Ordering::Relaxed);
    }
    available
}

fn hyprctl_json<T: for<'de> Deserialize<'de>>(args: &[&str]) -> anyhow::Result<T> {
    let output = Command::new("hyprctl")
        .arg("-j")
        .args(args)
        .stdin(Stdio::null())
        .output()?;
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

fn clients() -> anyhow::Result<Vec<Client>> {
    let clients: Vec<Client> = hyprctl_json(&["clients"])?;
    ids()
        .lock()
        .map_err(|_| anyhow::anyhow!("Hyprland window-id registry is poisoned"))?
        .sync(&clients);
    Ok(clients)
}

fn monitors() -> anyhow::Result<Vec<Monitor>> {
    hyprctl_json(&["monitors"])
}

fn active_window() -> anyhow::Result<ActiveWindow> {
    hyprctl_json(&["activewindow"])
}

fn id_for_client(client: &Client) -> anyhow::Result<u32> {
    Ok(ids()
        .lock()
        .map_err(|_| anyhow::anyhow!("Hyprland window-id registry is poisoned"))?
        .register(WindowIdentity::from(client)))
}

fn client_for_window(window_id: u64) -> anyhow::Result<Client> {
    let clients = clients()?;
    let identity = ids()
        .lock()
        .map_err(|_| anyhow::anyhow!("Hyprland window-id registry is poisoned"))?
        .identity(window_id)
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown Hyprland window_id {window_id}; call list_windows to refresh it"
            )
        })?;
    clients
        .into_iter()
        .find(|client| WindowIdentity::from(client) == identity)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "stale Hyprland window_id {window_id}; its original window no longer exists"
            )
        })
}

fn active_workspace_ids(monitors: &[Monitor]) -> HashSet<i32> {
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

fn client_is_internal(client: &Client) -> bool {
    client.pid == Some(std::process::id())
        || client.class.starts_with("Cua.AgentCursorOverlay.")
        || client.initial_class.starts_with("Cua.AgentCursorOverlay.")
        || client.title.starts_with("Cua.AgentCursorOverlay.")
        || client.initial_title.starts_with("Cua.AgentCursorOverlay.")
}

fn client_is_enumerable(client: &Client) -> bool {
    client.mapped && !client_is_internal(client)
}

fn client_is_visible(client: &Client, workspaces: &HashSet<i32>) -> bool {
    client_is_enumerable(client)
        && !client.hidden
        && client.workspace.as_ref().is_some_and(|workspace| {
            workspaces.is_empty() || client.pinned || workspaces.contains(&workspace.id)
        })
}

fn ensure_visible(client: &Client, monitors: &[Monitor]) -> anyhow::Result<()> {
    if client_is_visible(client, &active_workspace_ids(monitors)) {
        Ok(())
    } else {
        anyhow::bail!(
            "Hyprland window is not visible on an active workspace; refusing a stale/off-workspace target"
        )
    }
}

pub(crate) fn list_windows(filter_pid: Option<u32>) -> anyhow::Result<Vec<WindowInfo>> {
    let clients = clients()?;
    let workspaces = active_workspace_ids(&monitors()?);
    let mut windows = Vec::new();
    for client in clients.iter().filter(|client| {
        client_is_enumerable(client) && filter_pid.is_none_or(|pid| client.pid == Some(pid))
    }) {
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
            app_name: client.class.clone(),
            title,
            is_on_screen: client_is_visible(client, &workspaces),
            z_index: None,
            x: client.at[0],
            y: client.at[1],
            width: client.size[0].max(0) as u32,
            height: client.size[1].max(0) as u32,
        });
    }
    windows.sort_by(|left, right| {
        left.pid
            .cmp(&right.pid)
            .then_with(|| right.is_on_screen.cmp(&left.is_on_screen))
            .then(left.y.cmp(&right.y))
            .then(left.x.cmp(&right.x))
            .then(left.title.cmp(&right.title))
    });
    Ok(windows)
}

pub(crate) fn window(window_id: u64) -> anyhow::Result<WindowInfo> {
    let client = client_for_window(window_id)?;
    let monitors = monitors()?;
    ensure_visible(&client, &monitors)?;
    Ok(WindowInfo {
        xid: window_id,
        pid: client.pid,
        app_name: client.class.clone(),
        title: client.title.clone(),
        is_on_screen: true,
        z_index: None,
        x: client.at[0],
        y: client.at[1],
        width: client.size[0].max(0) as u32,
        height: client.size[1].max(0) as u32,
    })
}

pub(crate) fn window_origin(window_id: u64) -> anyhow::Result<(i32, i32)> {
    let window = window(window_id)?;
    Ok((window.x, window.y))
}

pub(crate) fn window_origin_for_pid(pid: u32) -> anyhow::Result<Option<(i32, i32)>> {
    let clients = clients()?;
    let workspaces = active_workspace_ids(&monitors()?);
    let active = active_window().ok();
    let matches = clients
        .iter()
        .filter(|client| client.pid == Some(pid) && client_is_visible(client, &workspaces))
        .collect::<Vec<_>>();
    if let Some(active) = active {
        if let Some(client) = matches
            .iter()
            .find(|client| client.address.eq_ignore_ascii_case(&active.address))
        {
            return Ok(Some((client.at[0], client.at[1])));
        }
    }
    Ok(match matches.as_slice() {
        [client] => Some((client.at[0], client.at[1])),
        _ => None,
    })
}

fn monitor_physical_size(monitor: &Monitor) -> (u32, u32) {
    if monitor.transform.rem_euclid(2) == 1 {
        (monitor.height, monitor.width)
    } else {
        (monitor.width, monitor.height)
    }
}

fn monitor_logical_size(monitor: &Monitor) -> (u32, u32) {
    let scale = monitor.scale.max(f64::EPSILON);
    let (width, height) = monitor_physical_size(monitor);
    (
        ((f64::from(width)) / scale).round().max(1.0) as u32,
        ((f64::from(height)) / scale).round().max(1.0) as u32,
    )
}

fn monitor_for_point(monitors: &[Monitor], x: f64, y: f64) -> Option<&Monitor> {
    monitors
        .iter()
        .filter(|monitor| !monitor.disabled)
        .find(|monitor| {
            let (width, height) = monitor_logical_size(monitor);
            x >= f64::from(monitor.x)
                && y >= f64::from(monitor.y)
                && x < f64::from(monitor.x.saturating_add(width as i32))
                && y < f64::from(monitor.y.saturating_add(height as i32))
        })
}

fn pointer_target_for(
    monitors: &[Monitor],
    screen_x: f64,
    screen_y: f64,
) -> anyhow::Result<PointerTarget> {
    let monitor = monitor_for_point(monitors, screen_x, screen_y).ok_or_else(|| {
        anyhow::anyhow!(
            "screen point ({screen_x:.1}, {screen_y:.1}) is outside every enabled Hyprland output"
        )
    })?;
    let scale = monitor.scale.max(f64::EPSILON);
    let (output_width, output_height) = monitor_physical_size(monitor);
    let output_x = ((screen_x - f64::from(monitor.x)) * scale)
        .round()
        .clamp(0.0, output_width.saturating_sub(1) as f64) as u32;
    let output_y = ((screen_y - f64::from(monitor.y)) * scale)
        .round()
        .clamp(0.0, output_height.saturating_sub(1) as f64) as u32;
    Ok(PointerTarget {
        output_name: monitor.name.clone(),
        output_width: output_width.max(1),
        output_height: output_height.max(1),
        output_x,
        output_y,
        screen_x,
        screen_y,
    })
}

pub(crate) fn global_pointer_target(screen_x: f64, screen_y: f64) -> anyhow::Result<PointerTarget> {
    pointer_target_for(&monitors()?, screen_x, screen_y)
}

fn capture_scale_for_client(client: &Client, monitors: &[Monitor]) -> f64 {
    let right = client.at[0].saturating_add(client.size[0].max(1));
    let bottom = client.at[1].saturating_add(client.size[1].max(1));
    monitors
        .iter()
        .filter(|monitor| !monitor.disabled)
        .filter(|monitor| {
            let (width, height) = monitor_logical_size(monitor);
            let monitor_right = monitor.x.saturating_add(width as i32);
            let monitor_bottom = monitor.y.saturating_add(height as i32);
            client.at[0] < monitor_right
                && right > monitor.x
                && client.at[1] < monitor_bottom
                && bottom > monitor.y
        })
        .map(|monitor| monitor.scale.max(f64::EPSILON))
        .reduce(f64::max)
        .unwrap_or(1.0)
}

pub(crate) fn window_capture_to_screen(
    window_id: u64,
    capture_x: i32,
    capture_y: i32,
) -> anyhow::Result<(i32, i32)> {
    let client = client_for_window(window_id)?;
    let monitors = monitors()?;
    ensure_visible(&client, &monitors)?;
    validate_window_capture_point_for(&client, &monitors, capture_x, capture_y)?;
    Ok(window_capture_to_screen_for(
        &client, &monitors, capture_x, capture_y,
    ))
}

fn window_capture_extent_for(client: &Client, monitors: &[Monitor]) -> (u32, u32) {
    let scale = capture_scale_for_client(client, monitors);
    // grim's region capture floors fractional physical extents. Match the
    // actual screenshot contract and keep the right/bottom edge exclusive.
    let width = (f64::from(client.size[0].max(1)) * scale).floor().max(1.0) as u32;
    let height = (f64::from(client.size[1].max(1)) * scale).floor().max(1.0) as u32;
    (width, height)
}

fn validate_window_capture_point_for(
    client: &Client,
    monitors: &[Monitor],
    capture_x: i32,
    capture_y: i32,
) -> anyhow::Result<()> {
    let (width, height) = window_capture_extent_for(client, monitors);
    anyhow::ensure!(
        capture_x >= 0
            && capture_y >= 0
            && (capture_x as u32) < width
            && (capture_y as u32) < height,
        "window capture point ({capture_x}, {capture_y}) is outside the target screenshot's exclusive bounds 0..{width} x 0..{height}"
    );
    Ok(())
}

/// Resolve a window-capture pixel to an output-bound pointer target from one
/// fresh compositor snapshot. This is the final pre-input gate: identity,
/// visibility, geometry, capture bounds, and output membership are all checked
/// together so a move/resize cannot turn a window-scoped action into a click on
/// an adjacent surface.
pub(crate) fn window_capture_pointer_target(
    window_id: u64,
    capture_x: i32,
    capture_y: i32,
) -> anyhow::Result<PointerTarget> {
    let client = client_for_window(window_id)?;
    let monitors = monitors()?;
    ensure_visible(&client, &monitors)?;
    validate_window_capture_point_for(&client, &monitors, capture_x, capture_y)?;
    let (screen_x, screen_y) =
        window_capture_to_screen_for(&client, &monitors, capture_x, capture_y);
    pointer_target_for(&monitors, f64::from(screen_x), f64::from(screen_y))
}

/// Resolve both endpoints from one compositor snapshot so a move/resize cannot
/// splice two different window geometries into one held gesture.
pub(crate) fn window_capture_drag_targets(
    window_id: u64,
    from: (i32, i32),
    to: (i32, i32),
) -> anyhow::Result<(PointerTarget, PointerTarget)> {
    let client = client_for_window(window_id)?;
    let monitors = monitors()?;
    ensure_visible(&client, &monitors)?;
    validate_window_capture_point_for(&client, &monitors, from.0, from.1)?;
    validate_window_capture_point_for(&client, &monitors, to.0, to.1)?;
    let from = window_capture_to_screen_for(&client, &monitors, from.0, from.1);
    let to = window_capture_to_screen_for(&client, &monitors, to.0, to.1);
    Ok((
        pointer_target_for(&monitors, f64::from(from.0), f64::from(from.1))?,
        pointer_target_for(&monitors, f64::from(to.0), f64::from(to.1))?,
    ))
}

pub(crate) fn window_center_pointer_target(window_id: u64) -> anyhow::Result<PointerTarget> {
    let client = client_for_window(window_id)?;
    let monitors = monitors()?;
    ensure_visible(&client, &monitors)?;
    pointer_target_for(
        &monitors,
        f64::from(client.at[0]) + f64::from(client.size[0].max(1)) / 2.0,
        f64::from(client.at[1]) + f64::from(client.size[1].max(1)) / 2.0,
    )
}

fn window_capture_to_screen_for(
    client: &Client,
    monitors: &[Monitor],
    capture_x: i32,
    capture_y: i32,
) -> (i32, i32) {
    let scale = capture_scale_for_client(client, monitors);
    (
        client.at[0].saturating_add((f64::from(capture_x) / scale).round() as i32),
        client.at[1].saturating_add((f64::from(capture_y) / scale).round() as i32),
    )
}

pub(crate) fn screen_to_window_capture(
    window_id: u64,
    screen_x: f64,
    screen_y: f64,
) -> anyhow::Result<(f64, f64)> {
    let client = client_for_window(window_id)?;
    let monitors = monitors()?;
    ensure_visible(&client, &monitors)?;
    let scale = capture_scale_for_client(&client, &monitors);
    Ok((
        (screen_x - f64::from(client.at[0])) * scale,
        (screen_y - f64::from(client.at[1])) * scale,
    ))
}

fn desktop_capture_space_for(monitors: &[Monitor]) -> anyhow::Result<DesktopCaptureSpace> {
    let enabled = monitors
        .iter()
        .filter(|monitor| !monitor.disabled)
        .collect::<Vec<_>>();
    if enabled.is_empty() {
        anyhow::bail!("hyprctl monitors returned no enabled monitors");
    }
    let origin_x = enabled.iter().map(|monitor| monitor.x).min().unwrap_or(0);
    let origin_y = enabled.iter().map(|monitor| monitor.y).min().unwrap_or(0);
    let max_x = enabled
        .iter()
        .map(|monitor| {
            monitor
                .x
                .saturating_add(monitor_logical_size(monitor).0 as i32)
        })
        .max()
        .unwrap_or(origin_x);
    let max_y = enabled
        .iter()
        .map(|monitor| {
            monitor
                .y
                .saturating_add(monitor_logical_size(monitor).1 as i32)
        })
        .max()
        .unwrap_or(origin_y);
    // grim composes a region at the highest intersecting output scale.
    let scale = enabled
        .iter()
        .map(|monitor| monitor.scale.max(f64::EPSILON))
        .fold(f64::EPSILON, f64::max);
    Ok(DesktopCaptureSpace {
        origin_x,
        origin_y,
        scale,
        width: (f64::from((max_x - origin_x).max(1)) * scale).round() as u32,
        height: (f64::from((max_y - origin_y).max(1)) * scale).round() as u32,
    })
}

pub(crate) fn desktop_capture_space() -> anyhow::Result<DesktopCaptureSpace> {
    desktop_capture_space_for(&monitors()?)
}

pub(crate) fn desktop_capture_to_screen(
    capture_x: i32,
    capture_y: i32,
) -> anyhow::Result<(i32, i32)> {
    let space = desktop_capture_space()?;
    Ok(desktop_capture_to_screen_for(space, capture_x, capture_y))
}

fn desktop_capture_to_screen_for(
    space: DesktopCaptureSpace,
    capture_x: i32,
    capture_y: i32,
) -> (i32, i32) {
    (
        space
            .origin_x
            .saturating_add((f64::from(capture_x) / space.scale).round() as i32),
        space
            .origin_y
            .saturating_add((f64::from(capture_y) / space.scale).round() as i32),
    )
}

pub(crate) fn focus_window(window_id: u64) -> anyhow::Result<()> {
    let client = client_for_window(window_id)?;
    ensure_visible(&client, &monitors()?)?;
    focus_identity(&WindowIdentity::from(&client))
}

fn live_identities(clients: &[Client]) -> Vec<WindowIdentity> {
    clients.iter().map(WindowIdentity::from).collect()
}

fn verified_prior_identity(
    active: Option<WindowIdentity>,
    target: &WindowIdentity,
    live: &[WindowIdentity],
) -> Option<WindowIdentity> {
    active.filter(|identity| identity != target && live.iter().any(|item| item == identity))
}

fn focus_restore_decision(
    lease: &FocusLease,
    active: Option<&WindowIdentity>,
    live: &[WindowIdentity],
) -> FocusRestoreDecision {
    if active != Some(&lease.target) {
        return FocusRestoreDecision::ActiveFocusChanged;
    }
    let Some(prior) = lease.prior.as_ref() else {
        return FocusRestoreDecision::NoPriorFocus;
    };
    match live
        .iter()
        .find(|identity| identity.address.eq_ignore_ascii_case(&prior.address))
    {
        Some(identity) if identity == prior => FocusRestoreDecision::Restore(prior.clone()),
        _ => FocusRestoreDecision::PriorWindowUnavailable,
    }
}

fn begin_temporary_focus_for_client(target: Client) -> anyhow::Result<FocusLease> {
    ensure_visible(&target, &monitors()?)?;
    let target = WindowIdentity::from(&target);
    let live = live_identities(&clients()?);
    anyhow::ensure!(
        live.iter().any(|identity| identity == &target),
        "Hyprland target window changed identity before it could be focused"
    );
    // An empty address is a valid "no active window" response. IPC or parse
    // failure is not: focusing without a trustworthy prior identity would be
    // fail-open and could permanently steal the user's focus.
    let prior = verified_prior_identity(active_window()?.identity(), &target, &live);
    let lease = FocusLease { target, prior };
    focus_with_rollback(lease, focus_identity, |lease| {
        restore_temporary_focus(Some(lease))
    })
}

fn focus_with_rollback<F, R>(lease: FocusLease, focus: F, rollback: R) -> anyhow::Result<FocusLease>
where
    F: FnOnce(&WindowIdentity) -> anyhow::Result<()>,
    R: FnOnce(FocusLease) -> anyhow::Result<FocusRestoreOutcome>,
{
    match focus(&lease.target) {
        Ok(()) => Ok(lease),
        Err(focus_error) => match rollback(lease.clone()) {
            Ok(outcome) => Err(focus_error.context(format!(
                "guarded rollback after the failed focus attempt completed with {outcome:?}"
            ))),
            Err(rollback_error) => Err(focus_error.context(format!(
                "guarded rollback after the failed focus attempt also failed: {rollback_error}"
            ))),
        },
    }
}

pub(crate) fn begin_temporary_focus(window_id: u64) -> anyhow::Result<FocusLease> {
    begin_temporary_focus_for_client(client_for_window(window_id)?)
}

pub(crate) fn validate_temporary_focus(lease: Option<&FocusLease>) -> anyhow::Result<()> {
    let Some(lease) = lease else {
        return Ok(());
    };
    let active = active_window()?.identity();
    anyhow::ensure!(
        active.as_ref() == Some(&lease.target),
        "Hyprland active focus changed before pointer dispatch; refusing input to a different surface"
    );
    Ok(())
}

pub(crate) fn restore_temporary_focus(
    lease: Option<FocusLease>,
) -> anyhow::Result<FocusRestoreOutcome> {
    let Some(lease) = lease else {
        return Ok(FocusRestoreOutcome::NoPriorFocus);
    };
    let active = active_window()?.identity();
    let live = live_identities(&clients()?);
    match focus_restore_decision(&lease, active.as_ref(), &live) {
        FocusRestoreDecision::Restore(prior) => {
            // Revalidate the complete identity immediately before dispatch.
            // Hyprland addresses are allocator-derived and can be reused.
            focus_identity(&prior)?;
            Ok(FocusRestoreOutcome::Restored)
        }
        FocusRestoreDecision::NoPriorFocus => Ok(FocusRestoreOutcome::NoPriorFocus),
        FocusRestoreDecision::ActiveFocusChanged => Ok(FocusRestoreOutcome::ActiveFocusChanged),
        FocusRestoreDecision::PriorWindowUnavailable => {
            Ok(FocusRestoreOutcome::PriorWindowUnavailable)
        }
    }
}

fn focus_identity(identity: &WindowIdentity) -> anyhow::Result<()> {
    let current = live_identities(&clients()?)
        .into_iter()
        .find(|candidate| candidate.address.eq_ignore_ascii_case(&identity.address));
    anyhow::ensure!(
        current.as_ref() == Some(identity),
        "refusing to focus stale or reused Hyprland address {}",
        identity.address
    );

    let selector = format!("address:{}", identity.address);
    // Hyprland 0.55 replaced the legacy `dispatch focuswindow <selector>`
    // CLI surface with Lua dispatchers. Prefer the current form, but retain
    // the legacy fallback so the fork still works on pre-0.55 sessions.
    let lua_selector = serde_json::to_string(&selector)?;
    let lua_dispatch = format!("hl.dsp.focus({{ window = {lua_selector} }})");
    let lua_output = Command::new("hyprctl")
        .args(["dispatch", &lua_dispatch])
        .stdin(Stdio::null())
        .output()?;
    if !lua_output.status.success() {
        let legacy_output = Command::new("hyprctl")
            .args(["dispatch", "focuswindow", &selector])
            .stdin(Stdio::null())
            .output()?;
        if legacy_output.status.success() {
            return wait_for_focused_identity(identity);
        }
        anyhow::bail!(
            "Hyprland focus dispatch failed (Lua: {}; legacy: {})",
            command_error(&lua_output),
            command_error(&legacy_output)
        );
    }
    wait_for_focused_identity(identity)
}

fn command_error(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    if detail.is_empty() {
        format!("exit status {}", output.status)
    } else {
        detail.to_owned()
    }
}

fn wait_for_focused_identity(identity: &WindowIdentity) -> anyhow::Result<()> {
    let deadline = Instant::now() + FOCUS_TIMEOUT;
    loop {
        if let Ok(active) = active_window() {
            if active.identity().as_ref() == Some(identity) {
                return Ok(());
            }
            if active.address.eq_ignore_ascii_case(&identity.address) {
                anyhow::bail!(
                    "Hyprland address {} was reused by a different window while focusing",
                    identity.address
                );
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "Hyprland did not focus window {} within 500ms",
                identity.address
            );
        }
        std::thread::sleep(FOCUS_POLL_INTERVAL);
    }
}

pub(crate) fn with_focused_window<T>(
    pid: u32,
    window_id: u64,
    body: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let target = client_for_window(window_id)?;
    anyhow::ensure!(
        target.pid == Some(pid),
        "Hyprland window {window_id} is not owned by pid {pid}"
    );
    with_focused_client(target, body)
}

pub(crate) fn with_focused_window_id<T>(
    window_id: u64,
    body: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    with_focused_client(client_for_window(window_id)?, body)
}

fn with_focused_client<T>(
    target: Client,
    body: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let lease = begin_temporary_focus_for_client(target)?;
    let result = body();
    let restore = restore_temporary_focus(Some(lease));
    match (result, restore) {
        (Ok(value), Ok(_)) => Ok(value),
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(restore)) => Err(error.context(format!(
            "the prior Hyprland focus also could not be restored: {restore}"
        ))),
    }
}

pub(crate) fn capture_window(window_id: u64) -> anyhow::Result<Vec<u8>> {
    let client = client_for_window(window_id)?;
    ensure_visible(&client, &monitors()?)?;
    let geometry = format!(
        "{},{} {}x{}",
        client.at[0],
        client.at[1],
        client.size[0].max(1),
        client.size[1].max(1)
    );
    run_grim(&["-g", &geometry, "-t", "png", "-"])
}

pub(crate) fn capture_desktop() -> anyhow::Result<Vec<u8>> {
    run_grim(&["-t", "png", "-"])
}

fn run_grim(args: &[&str]) -> anyhow::Result<Vec<u8>> {
    let output = Command::new("grim")
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "grim {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if output.stdout.is_empty() {
        anyhow::bail!("grim {} produced no output", args.join(" "));
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
    let cursor: CursorPosition = hyprctl_json(&["cursorpos"])?;
    Ok((cursor.x, cursor.y))
}

pub(crate) fn app_class(window_id: u64) -> anyhow::Result<String> {
    Ok(client_for_window(window_id)?.class)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(name: &str, width: u32, height: u32, x: i32, y: i32, scale: f64) -> Monitor {
        Monitor {
            name: name.to_owned(),
            width,
            height,
            x,
            y,
            scale,
            transform: 0,
            focused: false,
            disabled: false,
            active_workspace: Some(Workspace { id: 1 }),
            special_workspace: None,
        }
    }

    fn client(address: &str, pid: u32, workspace: i32) -> Client {
        Client {
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
            workspace: Some(Workspace { id: workspace }),
            pinned: false,
        }
    }

    #[test]
    fn desktop_detection_handles_composite_values_and_nested_sessions() {
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
    fn surrogate_ids_are_stable_and_collision_safe() {
        let identity = WindowIdentity {
            address: "0x123456789".to_owned(),
            stable_id: "stable".to_owned(),
            pid: Some(10),
            initial_class: "app".to_owned(),
            initial_title: "title".to_owned(),
        };
        let mut registry = IdRegistry::default();
        let first = registry.register_with_candidate(identity.clone(), 7);
        assert_eq!(registry.register_with_candidate(identity.clone(), 7), first);
        let second = registry.register_with_candidate(
            WindowIdentity {
                address: "0x987654321".to_owned(),
                ..identity
            },
            7,
        );
        assert_ne!(first, second);
        assert!(is_surrogate_window_id(u64::from(first)));
    }

    #[test]
    fn visibility_filters_hidden_inactive_and_internal_clients() {
        let visible = client("0x1", 10, 2);
        let mut hidden = client("0x2", 10, 2);
        hidden.hidden = true;
        let inactive = client("0x3", 10, 3);
        let mut pinned = client("0x4", 10, 3);
        pinned.pinned = true;
        let mut internal = client("0x5", std::process::id(), 2);
        internal.class = "Cua.AgentCursorOverlay.default".to_owned();
        let workspaces = HashSet::from([2]);
        assert!(client_is_visible(&visible, &workspaces));
        assert!(!client_is_visible(&hidden, &workspaces));
        assert!(client_is_enumerable(&hidden));
        assert!(!client_is_visible(&inactive, &workspaces));
        assert!(client_is_enumerable(&inactive));
        assert!(client_is_visible(&pinned, &workspaces));
        assert!(!client_is_visible(&internal, &workspaces));
    }

    #[test]
    fn mixed_scale_layout_maps_global_points_to_named_outputs() {
        let monitors = [
            monitor("DP-1", 3840, 2160, 0, 0, 1.6),
            monitor("eDP-1", 2880, 1800, 300, 1350, 1.6),
        ];
        let target = pointer_target_for(&monitors, 500.0, 1000.0).unwrap();
        assert_eq!(target.output_name, "DP-1");
        assert_eq!((target.output_x, target.output_y), (800, 1600));
        let space = desktop_capture_space_for(&monitors).unwrap();
        assert_eq!((space.origin_x, space.origin_y), (0, 0));
        assert_eq!(space.scale, 1.6);
    }

    #[test]
    fn rotated_output_swaps_pointer_extents() {
        let mut portrait = monitor("DP-1", 1920, 1080, 0, 0, 1.0);
        portrait.transform = 1;
        let target = pointer_target_for(&[portrait], 270.0, 1440.0).unwrap();
        assert_eq!((target.output_width, target.output_height), (1080, 1920));
        assert_eq!((target.output_x, target.output_y), (270, 1440));
    }

    #[test]
    fn window_capture_pixels_round_trip_to_output_bound_coordinates() {
        let monitor = monitor("DP-2", 1920, 1080, -1280, 0, 1.5);
        let mut window = client("0x123", 10, 1);
        window.at = [-1000, 100];
        window.size = [800, 600];
        let (screen_x, screen_y) =
            window_capture_to_screen_for(&window, std::slice::from_ref(&monitor), 150, 75);
        assert_eq!((screen_x, screen_y), (-900, 150));
        let target = pointer_target_for(std::slice::from_ref(&monitor), -900.0, 150.0).unwrap();
        assert_eq!(target.output_name, "DP-2");
        assert_eq!((target.output_x, target.output_y), (570, 225));
    }

    #[test]
    fn window_capture_bounds_reject_adjacent_surface_escape() {
        let monitor = monitor("DP-2", 1920, 1080, 0, 0, 1.5);
        let mut window = client("0x123", 10, 1);
        window.at = [100, 100];
        window.size = [800, 600];
        let monitors = [monitor];
        assert_eq!(window_capture_extent_for(&window, &monitors), (1200, 900));
        for (x, y) in [(-1, 10), (10, -1), (1200, 10), (10, 900)] {
            assert!(validate_window_capture_point_for(&window, &monitors, x, y).is_err());
        }
        assert!(validate_window_capture_point_for(&window, &monitors, 1199, 899).is_ok());
    }

    #[test]
    fn desktop_capture_pixels_convert_to_global_logical_coordinates() {
        let space = DesktopCaptureSpace {
            origin_x: -1200,
            origin_y: 300,
            scale: 1.5,
            width: 3000,
            height: 1800,
        };
        assert_eq!(desktop_capture_to_screen_for(space, 450, 300), (-900, 500));
    }

    fn identity(address: &str, stable_id: &str) -> WindowIdentity {
        WindowIdentity {
            address: address.to_owned(),
            stable_id: stable_id.to_owned(),
            pid: Some(10),
            initial_class: "app".to_owned(),
            initial_title: "title".to_owned(),
        }
    }

    #[test]
    fn temporary_focus_restore_skips_user_focus_takeover() {
        let target = identity("0xabc", "target");
        let prior = identity("0xdef", "prior");
        let user_target = identity("0x987", "user");
        let lease = FocusLease {
            target,
            prior: Some(prior.clone()),
        };
        assert_eq!(
            focus_restore_decision(
                &lease,
                Some(&user_target),
                &[prior.clone(), user_target.clone()]
            ),
            FocusRestoreDecision::ActiveFocusChanged
        );
    }

    #[test]
    fn temporary_focus_restore_skips_reused_prior_address() {
        let target = identity("0xabc", "target");
        let prior = identity("0xdef", "prior");
        let reused = identity("0xdef", "replacement");
        let lease = FocusLease {
            target: target.clone(),
            prior: Some(prior),
        };
        assert_eq!(
            focus_restore_decision(&lease, Some(&target), &[target.clone(), reused]),
            FocusRestoreDecision::PriorWindowUnavailable
        );
        assert_eq!(
            focus_restore_decision(&lease, Some(&target), &[target.clone()]),
            FocusRestoreDecision::PriorWindowUnavailable
        );
    }

    #[test]
    fn temporary_focus_restore_requires_exact_live_prior_identity() {
        let target = identity("0xabc", "target");
        let prior = identity("0xdef", "prior");
        let lease = FocusLease {
            target: target.clone(),
            prior: Some(prior.clone()),
        };
        assert_eq!(
            focus_restore_decision(&lease, Some(&target), &[target.clone(), prior.clone()]),
            FocusRestoreDecision::Restore(prior)
        );
    }

    #[test]
    fn failed_focus_confirmation_runs_guarded_rollback() {
        let target = identity("0xabc", "target");
        let prior = identity("0xdef", "prior");
        let lease = FocusLease {
            target,
            prior: Some(prior),
        };
        let rolled_back = std::cell::Cell::new(false);
        let error = focus_with_rollback(
            lease,
            |_| anyhow::bail!("confirmation timed out"),
            |_| {
                rolled_back.set(true);
                Ok(FocusRestoreOutcome::Restored)
            },
        )
        .unwrap_err();
        assert!(rolled_back.get());
        assert!(error.to_string().contains("guarded rollback"));
    }
}
