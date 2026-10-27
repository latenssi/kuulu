//! Passive global push-to-talk. An OS input hook (monio) observes keys and
//! mouse buttons without consuming them, so the game still gets the key:
//! low-level hooks on Windows, a listen-only event tap on macOS (needs the
//! Accessibility permission), XRecord on X11. Wayland gives apps no such
//! hook, so there push-to-talk is unavailable and voice activity is used.
//!
//! The hook runs only while needed (in a call, or capturing a key). Its
//! callback never blocks: it compares the event with the binding, fixed for
//! the hook's lifetime, and stores the result in the gate's atomic.
//!
//! Bindings are input names: monio key names such as "F8", "KeyV" or
//! "Grave", or "Mouse4"/"Mouse5" for side buttons.

use crate::audio::Gate;
use monio::{Event, EventType, Hook};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// How long the OS gets to install the hook.
const START_TIMEOUT: Duration = Duration::from_secs(2);

/// The running hook. Only control code touches it, never the callback.
static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

struct Running {
    hook: Arc<Hook>,
    thread: JoinHandle<()>,
}

/// Starts push-to-talk on `binding`, replacing any running hook. The error
/// says why push-to-talk is unavailable, for the user.
pub fn start(binding: &str, gate: Arc<Gate>) -> Result<(), String> {
    let binding = binding.to_owned();
    run(move |event| {
        if let Some((input, pressed)) = input(event)
            && input == binding
        {
            gate.set_key_held(pressed);
        }
    })
}

/// Stops the hook, if running.
pub fn stop() {
    stop_locked(&mut RUNNING.lock().unwrap());
}

fn stop_locked(running: &mut Option<Running>) {
    if let Some(Running { hook, thread }) = running.take() {
        let _ = hook.stop();
        let _ = thread.join();
    }
}

/// Waits up to `timeout` for the user to press a key or side button and
/// returns its name; Escape cancels. Stops any running hook; restart
/// push-to-talk afterwards. Blocks: call it off the main thread.
pub fn capture(timeout: Duration) -> Result<Option<String>, String> {
    let (tx, rx) = mpsc::sync_channel(1);
    run(move |event| {
        // Left and right clicks press the UI's own buttons.
        if let Some((input, true)) = input(event)
            && input != "Mouse1"
            && input != "Mouse2"
        {
            let _ = tx.try_send(input);
        }
    })?;
    let pressed = rx.recv_timeout(timeout).ok();
    stop();
    Ok(pressed.filter(|input| input != "Escape"))
}

/// Starts a hook calling `on_event`, replacing any running one. Holding
/// the lock throughout keeps concurrent starts and stops in order: monio's
/// hook is process-global.
fn run(on_event: impl Fn(&Event) + Send + Sync + 'static) -> Result<(), String> {
    let mut running = RUNNING.lock().unwrap();
    stop_locked(&mut running);
    if cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return Err("Global push-to-talk does not work on Wayland yet; \
                    using voice activity instead."
            .into());
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
    let fail = |e: String| {
        let hint = if cfg!(target_os = "macos") {
            " Allow Kuulu in System Settings > Privacy & Security > Accessibility."
        } else {
            ""
        };
        format!("Global push-to-talk is unavailable ({e}); using voice activity instead.{hint}")
    };
    match wait.recv_timeout(START_TIMEOUT) {
        Ok(Ok(())) => {
            *running = Some(Running { hook, thread });
            Ok(())
        }
        Ok(Err(e)) => Err(fail(e)),
        Err(_) => {
            let _ = hook.stop();
            Err(fail("the input hook did not start".into()))
        }
    }
}

/// The input an event presses (`true`) or releases, by binding name.
fn input(event: &Event) -> Option<(String, bool)> {
    let pressed = matches!(
        event.event_type,
        EventType::KeyPressed | EventType::MousePressed
    );
    match event.event_type {
        EventType::KeyPressed | EventType::KeyReleased => {
            Some((format!("{:?}", event.keyboard.as_ref()?.key), pressed))
        }
        EventType::MousePressed | EventType::MouseReleased => Some((
            format!("Mouse{}", event.mouse.as_ref()?.button?.number()),
            pressed,
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use monio::{Button, Key};

    #[test]
    fn names_keys_and_mouse_buttons() {
        let key = Event::key_pressed(Key::F8, 0);
        assert_eq!(input(&key), Some(("F8".into(), true)));
        let key = Event::key_released(Key::Grave, 0);
        assert_eq!(input(&key), Some(("Grave".into(), false)));
        let side = Event::mouse_pressed(Button::Button4, 0.0, 0.0);
        assert_eq!(input(&side), Some(("Mouse4".into(), true)));
        let side = Event::mouse_released(Button::Unknown(8), 0.0, 0.0);
        assert_eq!(input(&side), Some(("Mouse8".into(), false)));
        assert_eq!(input(&Event::mouse_moved(1.0, 1.0)), None);
    }
}
