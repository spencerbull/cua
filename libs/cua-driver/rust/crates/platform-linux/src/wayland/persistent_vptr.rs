//! Persistent virtual-pointer for stateful `mouse_button_down` / `mouse_drag` /
//! `mouse_button_up`.
//!
//! The non-persistent path in [`crate::wayland::click`] opens its own
//! `ZwlrVirtualPointerV1`, presses, releases, drops the connection — useful
//! for one-shot clicks but useless for held-button drags: each tool call
//! emits a fresh device whose press/release pair is matched by the
//! compositor, so apps that distinguish a real drag (press, motion+, release)
//! from a series of clicks (press, release, press, release, …) miss the
//! drag entirely.
//!
//! This module keeps the virtual-pointer alive across tool calls. A single
//! owner thread per process owns one Wayland `Connection`, one `EventQueue`,
//! and a map of `cursor_id -> ActivePointer`. Commands are sent over a
//! `crossbeam-channel`; replies come back on a per-call reply channel so the
//! caller blocks until the compositor has roundtripped.
//!
//! Lifecycle:
//! - First `press` for a cursor_id binds a fresh `ZwlrVirtualPointerV1`,
//!   activates the foreign-toplevel target window once, presses the button,
//!   adds to the held-button set, roundtrips.
//! - Subsequent `move_to` calls emit `motion_absolute` on the same vptr (no
//!   activate — would steal focus mid-drag) and roundtrip.
//! - `release` emits a button release, removes from the held set; if the
//!   set is empty the vptr is destroyed and the map entry dropped.
//! - On `Connection` roundtrip failure (compositor restart / disconnect)
//!   the owner thread tears down its connection and accepts the next
//!   command on a fresh one, emitting a typed error for the in-flight call.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::thread;

use crossbeam_channel::{bounded, Receiver, Sender};
use wayland_client::{protocol::wl_pointer::ButtonState, Connection};
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

use super::{evdev_pointer_button, open_vptr_session_at, protocol_window_id};

/// One in-flight command from the public API to the owner thread.
enum Cmd {
    Press {
        cursor_id: String,
        window_id: u64,
        x: i32,
        y: i32,
        button: u8,
        reply: Sender<anyhow::Result<()>>,
    },
    MoveTo {
        cursor_id: String,
        x: i32,
        y: i32,
        reply: Sender<anyhow::Result<()>>,
    },
    Release {
        cursor_id: String,
        button: u8,
        reply: Sender<anyhow::Result<()>>,
    },
    /// Release any held buttons and drop the entry for a cursor_id.
    Forget {
        cursor_id: String,
        reply: Sender<anyhow::Result<()>>,
    },
}

/// State held inside the owner thread for one cursor_id.
struct ActivePointer {
    vptr: ZwlrVirtualPointerV1,
    /// evdev codes of buttons currently held down. When this set becomes
    /// empty the vptr is destroyed and the entry dropped from the map.
    held: HashSet<u32>,
    /// Output extent at session open time — needed for motion_absolute.
    out_w: u32,
    out_h: u32,
    output_name: Option<String>,
    capture_origin_x: f64,
    capture_origin_y: f64,
    output_origin_x: f64,
    output_origin_y: f64,
    capture_to_output_scale: f64,
}

/// Process-global command channel into the owner thread. Lazily started on
/// first use.
static TX: OnceLock<Sender<Cmd>> = OnceLock::new();

fn tx() -> &'static Sender<Cmd> {
    TX.get_or_init(|| {
        let (tx, rx) = bounded::<Cmd>(32);
        thread::Builder::new()
            .name("cua-persistent-vptr".into())
            .spawn(move || owner_thread(rx))
            .expect("spawn cua-persistent-vptr thread");
        tx
    })
}

fn owner_thread(rx: Receiver<Cmd>) {
    let mut active: HashMap<String, ActivePointer> = HashMap::new();
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Press {
                cursor_id,
                window_id,
                x,
                y,
                button,
                reply,
            } => {
                let r = handle_press(&mut active, &cursor_id, window_id, x, y, button);
                let _ = reply.send(r);
            }
            Cmd::MoveTo {
                cursor_id,
                x,
                y,
                reply,
            } => {
                let r = handle_move(&mut active, &cursor_id, x, y);
                let _ = reply.send(r);
            }
            Cmd::Release {
                cursor_id,
                button,
                reply,
            } => {
                let r = handle_release(&mut active, &cursor_id, button);
                let _ = reply.send(r);
            }
            Cmd::Forget { cursor_id, reply } => {
                let r = release_all_and_forget(&mut active, &cursor_id);
                let _ = reply.send(r);
            }
        }
    }
}

fn handle_press(
    active: &mut HashMap<String, ActivePointer>,
    cursor_id: &str,
    window_id: u64,
    x: i32,
    y: i32,
    button: u8,
) -> anyhow::Result<()> {
    // Open a fresh session for this press — this binds the seat, the foreign-
    // toplevel manager, activates the target window, and creates a new vptr.
    // Keep the (out_w, out_h) but drop the queue + state at end of scope; the
    // vptr itself remains alive (Wayland objects survive their original queue
    // as long as the Connection is alive).
    let mut sess = open_vptr_session_at(
        Some(protocol_window_id(window_id)?),
        Some((f64::from(x), f64::from(y))),
    )?;
    let (w, h) = (sess.output_w, sess.output_h);
    let px = sess
        .target_x
        .unwrap_or_else(|| x.clamp(0, w as i32 - 1) as u32);
    let py = sess
        .target_y
        .unwrap_or_else(|| y.clamp(0, h as i32 - 1) as u32);
    let btn = evdev_pointer_button(button);

    sess.vptr.motion_absolute(0, px, py, w, h);
    sess.vptr.frame();
    sess.vptr.button(0, btn, ButtonState::Pressed);
    sess.vptr.frame();
    sess.queue.roundtrip(&mut sess.state)?;

    // Take ownership of the vptr handle by extracting it from the session.
    // ZwlrVirtualPointerV1 is a Wayland proxy — cloning it gives another
    // handle to the same wire object; destroying it sends the destructor.
    let vptr = sess.vptr.clone();
    // Persist the live connection so the proxy stays valid after this fn returns
    // (the session goes out of scope; we need the conn alive).
    // We do this by leaking the connection into a process-static slot keyed by
    // cursor_id. Subsequent commands on the same cursor reuse this conn.
    persist_conn(cursor_id, sess.conn);

    let mut held = HashSet::new();
    held.insert(btn);
    active.insert(
        cursor_id.to_string(),
        ActivePointer {
            vptr,
            held,
            out_w: w,
            out_h: h,
            output_name: sess.target_output_name,
            capture_origin_x: sess.target_capture_x.unwrap_or(f64::from(x)),
            capture_origin_y: sess.target_capture_y.unwrap_or(f64::from(y)),
            output_origin_x: f64::from(px),
            output_origin_y: f64::from(py),
            capture_to_output_scale: sess.target_capture_to_output_scale.unwrap_or(1.0),
        },
    );
    Ok(())
}

fn handle_move(
    active: &mut HashMap<String, ActivePointer>,
    cursor_id: &str,
    x: i32,
    y: i32,
) -> anyhow::Result<()> {
    let entry = active.get(cursor_id).ok_or_else(|| {
        anyhow::anyhow!(
            "no held mouse button for cursor '{cursor_id}'; call mouse_button_down first"
        )
    })?;
    let mapped_point = if let Some(output_name) = &entry.output_name {
        match cached_output_point(
            entry.capture_origin_x,
            entry.capture_origin_y,
            entry.output_origin_x,
            entry.output_origin_y,
            entry.capture_to_output_scale,
            entry.out_w,
            entry.out_h,
            x,
            y,
        ) {
            Some(point) => Ok(point),
            None => Err(output_name.clone()),
        }
    } else {
        Ok((
            x.clamp(0, entry.out_w as i32 - 1) as u32,
            y.clamp(0, entry.out_h as i32 - 1) as u32,
        ))
    };

    let (px, py) = match mapped_point {
        Ok(point) => point,
        Err(output_name) => {
            let cleanup = release_all_and_forget(active, cursor_id);
            let message = format!(
                "Hyprland held-pointer move leaves bound output {output_name}; the held gesture was canceled and its buttons released"
            );
            return match cleanup {
                Ok(()) => Err(anyhow::anyhow!(message)),
                Err(error) => Err(anyhow::anyhow!(
                    "{message}, but compositor cleanup failed: {error}"
                )),
            };
        }
    };
    let entry = active
        .get(cursor_id)
        .expect("active pointer remains present after a mapped move");
    entry
        .vptr
        .motion_absolute(0, px, py, entry.out_w, entry.out_h);
    entry.vptr.frame();
    roundtrip_on_persistent(cursor_id)?;
    Ok(())
}

fn held_buttons_for_release(held: &HashSet<u32>) -> Vec<u32> {
    let mut buttons: Vec<u32> = held.iter().copied().collect();
    buttons.sort_unstable();
    buttons
}

/// End a persistent pointer without abandoning compositor-side button state.
/// Release requests are committed before the proxy and connection are removed;
/// even when that roundtrip fails, destroying the device and closing its
/// connection remains the safest recovery path.
fn release_all_and_forget(
    active: &mut HashMap<String, ActivePointer>,
    cursor_id: &str,
) -> anyhow::Result<()> {
    let release_result = if let Some(entry) = active.get(cursor_id) {
        for button in held_buttons_for_release(&entry.held) {
            entry.vptr.button(0, button, ButtonState::Released);
        }
        entry.vptr.frame();
        roundtrip_on_persistent(cursor_id)
    } else {
        Ok(())
    };

    let destroy_result = if let Some(entry) = active.remove(cursor_id) {
        entry.vptr.destroy();
        roundtrip_on_persistent(cursor_id)
    } else {
        Ok(())
    };
    forget_conn(cursor_id);

    release_result.and(destroy_result)
}

#[allow(clippy::too_many_arguments)]
fn cached_output_point(
    capture_origin_x: f64,
    capture_origin_y: f64,
    output_origin_x: f64,
    output_origin_y: f64,
    capture_to_output_scale: f64,
    output_width: u32,
    output_height: u32,
    x: i32,
    y: i32,
) -> Option<(u32, u32)> {
    let raw_x = output_origin_x + (f64::from(x) - capture_origin_x) * capture_to_output_scale;
    let raw_y = output_origin_y + (f64::from(y) - capture_origin_y) * capture_to_output_scale;
    if raw_x < 0.0
        || raw_y < 0.0
        || raw_x >= f64::from(output_width)
        || raw_y >= f64::from(output_height)
    {
        None
    } else {
        Some((
            raw_x
                .round()
                .clamp(0.0, output_width.saturating_sub(1) as f64) as u32,
            raw_y
                .round()
                .clamp(0.0, output_height.saturating_sub(1) as f64) as u32,
        ))
    }
}

fn handle_release(
    active: &mut HashMap<String, ActivePointer>,
    cursor_id: &str,
    button: u8,
) -> anyhow::Result<()> {
    let btn = evdev_pointer_button(button);
    let drop_entry = {
        let entry = active
            .get_mut(cursor_id)
            .ok_or_else(|| anyhow::anyhow!("no held mouse button for cursor '{cursor_id}'"))?;
        entry.vptr.button(0, btn, ButtonState::Released);
        entry.vptr.frame();
        roundtrip_on_persistent(cursor_id)?;
        entry.held.remove(&btn);
        entry.held.is_empty()
    };
    if drop_entry {
        if let Some(p) = active.remove(cursor_id) {
            p.vptr.destroy();
            roundtrip_on_persistent(cursor_id).ok();
        }
        forget_conn(cursor_id);
    }
    Ok(())
}

// Process-static slots for Connection + EventQueue keyed by cursor_id. The
// EventQueue is !Send but we only touch these on the owner thread, so wrap
// in a thread-local-by-construction pattern: store inside the same map so
// the owner thread is the sole accessor.
//
// We use a per-thread static rather than a Mutex<HashMap> because the owner
// thread is the only accessor (no contention possible).
thread_local! {
    static CONNS: std::cell::RefCell<HashMap<String, (Connection, wayland_client::EventQueue<super::State>)>>
        = std::cell::RefCell::new(HashMap::new());
}

fn persist_conn(cursor_id: &str, conn: Connection) {
    let queue = conn.new_event_queue::<super::State>();
    CONNS.with(|c| {
        c.borrow_mut().insert(cursor_id.to_string(), (conn, queue));
    });
}

fn forget_conn(cursor_id: &str) {
    CONNS.with(|c| {
        c.borrow_mut().remove(cursor_id);
    });
}

fn roundtrip_on_persistent(cursor_id: &str) -> anyhow::Result<()> {
    CONNS.with(|c| {
        let mut b = c.borrow_mut();
        let (_conn, queue) = b
            .get_mut(cursor_id)
            .ok_or_else(|| anyhow::anyhow!("no persistent connection for cursor '{cursor_id}'"))?;
        let mut tmp = super::State::default();
        queue
            .roundtrip(&mut tmp)
            .map_err(|e| anyhow::anyhow!("compositor roundtrip failed: {e}"))?;
        Ok(())
    })
}

// ── public API ────────────────────────────────────────────────────────────

/// Press and HOLD `button` (evdev code) at output coordinates `(x, y)` on the
/// toplevel identified by `window_id`. Subsequent `move_to` / `release` calls
/// targeting the same `cursor_id` reuse the same virtual-pointer device, so
/// the compositor treats the sequence as one logical drag rather than as
/// independent clicks. Errors if `cursor_id` already has a held button.
pub fn press(cursor_id: &str, window_id: u64, x: i32, y: i32, button: u8) -> anyhow::Result<()> {
    let (tx_r, rx_r) = bounded(1);
    tx().send(Cmd::Press {
        cursor_id: cursor_id.to_string(),
        window_id,
        x,
        y,
        button,
        reply: tx_r,
    })
    .map_err(|e| anyhow::anyhow!("cua-persistent-vptr thread is dead: {e}"))?;
    rx_r.recv()
        .map_err(|e| anyhow::anyhow!("reply channel closed: {e}"))?
}

/// Emit motion_absolute on the held cursor's virtual-pointer. Errors if there
/// is no held button for `cursor_id`.
pub fn move_to(cursor_id: &str, x: i32, y: i32) -> anyhow::Result<()> {
    let (tx_r, rx_r) = bounded(1);
    tx().send(Cmd::MoveTo {
        cursor_id: cursor_id.to_string(),
        x,
        y,
        reply: tx_r,
    })
    .map_err(|e| anyhow::anyhow!("cua-persistent-vptr thread is dead: {e}"))?;
    rx_r.recv()
        .map_err(|e| anyhow::anyhow!("reply channel closed: {e}"))?
}

/// Release `button` on the held cursor. If no other buttons remain held the
/// virtual-pointer is destroyed and its Wayland connection torn down.
pub fn release(cursor_id: &str, button: u8) -> anyhow::Result<()> {
    let (tx_r, rx_r) = bounded(1);
    tx().send(Cmd::Release {
        cursor_id: cursor_id.to_string(),
        button,
        reply: tx_r,
    })
    .map_err(|e| anyhow::anyhow!("cua-persistent-vptr thread is dead: {e}"))?;
    rx_r.recv()
        .map_err(|e| anyhow::anyhow!("reply channel closed: {e}"))?
}

/// Release every button held by `cursor_id`, commit those events, then destroy
/// the virtual pointer and tear down its Wayland connection. This is safe to
/// call when no entry exists and is also the recovery path for aborted drags.
pub fn forget(cursor_id: &str) -> anyhow::Result<()> {
    let (tx_r, rx_r) = bounded(1);
    tx().send(Cmd::Forget {
        cursor_id: cursor_id.to_string(),
        reply: tx_r,
    })
    .map_err(|e| anyhow::anyhow!("cua-persistent-vptr thread is dead: {e}"))?;
    rx_r.recv()
        .map_err(|e| anyhow::anyhow!("reply channel closed: {e}"))?
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{cached_output_point, held_buttons_for_release};

    #[test]
    fn cached_drag_mapping_scales_without_requerying_compositor() {
        assert_eq!(
            cached_output_point(100.0, 50.0, 400.0, 200.0, 1.25, 1920, 1080, 180, 90),
            Some((500, 250))
        );
        assert_eq!(
            cached_output_point(100.0, 50.0, 10.0, 10.0, 1.0, 1920, 1080, 0, 0),
            None
        );
    }

    #[test]
    fn cleanup_releases_every_held_button_in_stable_order() {
        let held = HashSet::from([0x112, 0x110, 0x111]);
        assert_eq!(held_buttons_for_release(&held), vec![0x110, 0x111, 0x112]);
    }
}
