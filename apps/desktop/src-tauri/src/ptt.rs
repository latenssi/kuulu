//! Passive global push-to-talk. An OS input hook (monio) observes keys and
//! mouse buttons without consuming them, so the game still gets the key:
//! low-level hooks on Windows, a listen-only event tap on macOS (needs the
//! Accessibility permission), XRecord on X11. Wayland gives apps no such
//! hook, so there push-to-talk is unavailable and voice activity is used.
//!
//! The hook runs only while needed (in a call, or capturing a key). Its
//! callback never blocks or allocates: it compares the event with the
//! binding, fixed for the hook's lifetime, and stores the result in the
//! gate's atomic.
//!
//! Bindings are input names: monio key names such as "F8", "KeyV" or
//! "Grave", or "Mouse4"/"Mouse5" for side buttons.

use crate::audio::Gate;
use monio::{Event, EventType, Hook};
use std::fmt::Write;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How long the OS gets to install the hook, and a failed one to exit.
const START_TIMEOUT: Duration = Duration::from_secs(2);

/// The hook, if any. Only control code touches it, never the callback.
static STATE: Mutex<State> = Mutex::new(State {
    running: None,
    stray: None,
});

/// Cancels a running [`capture`].
static CAPTURE: Mutex<Option<SyncSender<Option<String>>>> = Mutex::new(None);

struct State {
    running: Option<Running>,
    /// A hook thread that did not start in time and has not exited yet.
    /// monio's hook is process-global, so no new hook starts until it ends.
    stray: Option<JoinHandle<()>>,
}

struct Running {
    hook: Arc<Hook>,
    thread: JoinHandle<()>,
    /// The session attempt that started it; `None` for a capture.
    owner: Option<u64>,
}

/// Starts push-to-talk on `binding` for session `attempt`, replacing any
/// running hook, unless a newer attempt's hook runs (then this start is
/// stale and does nothing). The error says why push-to-talk is
/// unavailable, for the user.
pub fn start(binding: &str, gate: Arc<Gate>, attempt: u64) -> Result<(), String> {
    let binding = binding.to_owned();
    let on_event = move |event: &Event| {
        let mut name = Name::default();
        if let Some(pressed) = input(event, &mut name)
            && name.as_str() == binding
        {
            gate.set_key_held(pressed);
        }
    };
    let mut state = STATE.lock().unwrap();
    if state
        .running
        .as_ref()
        .and_then(|r| r.owner)
        .is_some_and(|owner| owner > attempt)
    {
        return Ok(());
    }
    run(&mut state, on_event, Some(attempt))
}

/// Stops the hook if `attempt` started it; `None` stops any hook.
pub fn stop(attempt: Option<u64>) {
    let mut state = STATE.lock().unwrap();
    let owned = state
        .running
        .as_ref()
        .is_some_and(|r| attempt.is_none_or(|attempt| r.owner == Some(attempt)));
    if owned {
        stop_locked(&mut state);
    }
}

fn stop_locked(state: &mut State) {
    if let Some(Running { hook, thread, .. }) = state.running.take() {
        let _ = hook.stop();
        let _ = thread.join();
    }
}

/// Waits up to `timeout` for the user to press a key or side button and
/// returns its name; Escape or [`cancel_capture`] cancel. Stops any running
/// hook; restart push-to-talk afterwards. Blocks: call it off the main
/// thread.
pub fn capture(timeout: Duration) -> Result<Option<String>, String> {
    let (tx, rx) = mpsc::sync_channel(1);
    *CAPTURE.lock().unwrap() = Some(tx.clone());
    let on_event = move |event: &Event| {
        let mut name = Name::default();
        // Left and right clicks press the UI's own buttons.
        if input(event, &mut name) == Some(true) && !matches!(name.as_str(), "Mouse1" | "Mouse2") {
            let _ = tx.try_send(Some(name.as_str().to_owned()));
        }
    };
    let started = run(&mut STATE.lock().unwrap(), on_event, None);
    let pressed = started.map(|()| rx.recv_timeout(timeout).ok().flatten());
    *CAPTURE.lock().unwrap() = None;
    stop(None);
    Ok(pressed?.filter(|input| input != "Escape"))
}

/// Ends a running [`capture`] with no input, e.g. when the window loses
/// focus.
pub fn cancel_capture() {
    if let Some(tx) = &*CAPTURE.lock().unwrap() {
        let _ = tx.try_send(None);
    }
}

/// Starts a hook calling `on_event`, replacing any running one. The caller
/// holds the state lock throughout, which keeps starts and stops in order.
fn run(
    state: &mut State,
    on_event: impl Fn(&Event) + Send + Sync + 'static,
    owner: Option<u64>,
) -> Result<(), String> {
    stop_locked(state);
    if cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return Err("Global push-to-talk does not work on Wayland yet; \
                    using voice activity instead."
            .into());
    }
    let fail = |e: &str| {
        let hint = if cfg!(target_os = "macos") {
            " Allow Kuulu in System Settings > Privacy & Security > Accessibility."
        } else {
            ""
        };
        format!("Global push-to-talk is unavailable ({e}); using voice activity instead.{hint}")
    };
    if let Some(stray) = state.stray.take() {
        if !stray.is_finished() {
            state.stray = Some(stray);
            return Err(fail("an earlier input hook is still shutting down"));
        }
        let _ = stray.join();
    }
    let hook = Arc::new(Hook::new());
    let (started, wait) = mpsc::sync_channel::<Result<(), String>>(1);
    let thread = {
        let hook = hook.clone();
        let ok = started.clone();
        thread::Builder::new()
            .name("kuulu-ptt".into())
            .spawn(move || {
                let result = hook.run(move |event: &Event| {
                    if event.event_type == EventType::HookEnabled {
                        let _ = ok.try_send(Ok(()));
                    }
                    on_event(event);
                });
                if let Err(e) = result {
                    let _ = started.try_send(Err(e.to_string()));
                }
            })
            .map_err(|e| e.to_string())?
    };
    let result = wait.recv_timeout(START_TIMEOUT);
    if let Ok(Ok(())) = result {
        state.running = Some(Running {
            hook,
            thread,
            owner,
        });
        return Ok(());
    }
    // Not running: make sure the thread ends before another hook starts.
    let _ = hook.stop();
    let deadline = Instant::now() + START_TIMEOUT;
    while !thread.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    match thread.is_finished() {
        true => {
            let _ = thread.join();
        }
        false => state.stray = Some(thread),
    }
    Err(fail(match &result {
        Ok(Err(e)) => e,
        _ => "the input hook did not start",
    }))
}

/// An input name on the stack, so the hook callback does not allocate.
#[derive(Default)]
struct Name {
    bytes: [u8; 32],
    len: usize,
}

impl Name {
    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }
}

impl Write for Name {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let end = self.len + s.len();
        let room = self.bytes.get_mut(self.len..end).ok_or(std::fmt::Error)?;
        room.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// Whether `event` presses (`true`) or releases an input; its name goes
/// into `name`.
fn input(event: &Event, name: &mut Name) -> Option<bool> {
    let pressed = matches!(
        event.event_type,
        EventType::KeyPressed | EventType::MousePressed
    );
    match event.event_type {
        EventType::KeyPressed | EventType::KeyReleased => {
            write!(name, "{:?}", event.keyboard.as_ref()?.key).ok()?
        }
        EventType::MousePressed | EventType::MouseReleased => {
            write!(name, "Mouse{}", event.mouse.as_ref()?.button?.number()).ok()?
        }
        _ => return None,
    }
    Some(pressed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use monio::{Button, Key};

    fn named(event: Event) -> Option<(String, bool)> {
        let mut name = Name::default();
        input(&event, &mut name).map(|pressed| (name.as_str().to_owned(), pressed))
    }

    #[test]
    fn names_keys_and_mouse_buttons() {
        assert_eq!(
            named(Event::key_pressed(Key::F8, 0)),
            Some(("F8".into(), true))
        );
        assert_eq!(
            named(Event::key_released(Key::Grave, 0)),
            Some(("Grave".into(), false))
        );
        assert_eq!(
            named(Event::mouse_pressed(Button::Button4, 0.0, 0.0)),
            Some(("Mouse4".into(), true))
        );
        assert_eq!(
            named(Event::mouse_released(Button::Unknown(8), 0.0, 0.0)),
            Some(("Mouse8".into(), false))
        );
        assert_eq!(named(Event::mouse_moved(1.0, 1.0)), None);
        // Longest key names still fit on the stack.
        assert!(named(Event::key_pressed(Key::Unknown(u32::MAX), 0)).is_some());
    }
}
