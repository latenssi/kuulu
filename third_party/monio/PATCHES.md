# Local patches to monio 0.1.1

Vendored from crates.io `monio` 0.1.1 (Apache-2.0, see LICENSE;
https://github.com/HuakunShen/monio) and used through `[patch.crates-io]`
in the workspace Cargo.toml. It is the published crate (minus its `.github`
and `.journal` directories) plus the diff below; LICENSE is the Apache-2.0
text, which the crate package does not ship. Drop the vendored copy once an
upstream release contains the fix.

## 1. macOS: `stop_hook` stops the hook thread's run loop

`stop_hook` stopped `CFRunLoop::main()`, but the hook runs `CFRunLoop::run()`
on whichever thread called `run_hook` (with `Hook::run_async`, a spawned
thread). That loop kept running until the next input event, when the
callback noticed the cleared stop flag. `Hook::stop` joins the hook thread,
so it blocked until the user pressed a key or moved the mouse. Stopping the
main run loop could also disturb the app's own main loop.

The fix records the hook thread's run loop when the tap is installed (listen
and grab mode), stops that one, and clears it when the loop returns.

Not verified on a Mac: CI only builds it on macOS.

```diff
--- a/src/platform/macos/listen.rs
+++ b/src/platform/macos/listen.rs
@@ -11,7 +11,7 @@
     MASK_CTRL, MASK_META, MASK_SHIFT,
 };
 use core::ptr::NonNull;
-use objc2_core_foundation::{CFMachPort, CFRunLoop, kCFRunLoopCommonModes};
+use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, kCFRunLoopCommonModes};
 use objc2_core_graphics::{
     CGEvent, CGEventField, CGEventFlags, CGEventTapCallBack, CGEventTapLocation, CGEventTapOptions,
     CGEventTapPlacement, CGEventTapProxy, CGEventType, kCGEventMaskForAllEvents,
@@ -45,6 +45,14 @@
 /// Stored event tap for timeout recovery
 static EVENT_TAP: Mutex<Option<TapPointer>> = Mutex::new(None);
 
+/// The hook thread's run loop, which `stop_hook` stops. It is not the main
+/// run loop when the hook runs on its own thread.
+struct RunLoopPtr(CFRetained<CFRunLoop>);
+// Safety: CFRunLoopStop may be called from any thread.
+unsafe impl Send for RunLoopPtr {}
+
+static HOOK_RUN_LOOP: Mutex<Option<RunLoopPtr>> = Mutex::new(None);
+
 /// Flag indicating whether we're in grab mode
 static GRAB_MODE: AtomicBool = AtomicBool::new(false);
 
@@ -410,6 +418,9 @@
             .ok_or_else(|| Error::HookStartFailed("Failed to get current run loop".into()))?;
 
         current_loop.add_source(Some(&source), kCFRunLoopCommonModes);
+        if let Ok(mut hook_loop) = HOOK_RUN_LOOP.lock() {
+            *hook_loop = Some(RunLoopPtr(current_loop.clone()));
+        }
 
         // Enable the tap
         CGEvent::tap_enable(&tap, true);
@@ -425,6 +436,9 @@
 
         // Run the loop
         CFRunLoop::run();
+        if let Ok(mut hook_loop) = HOOK_RUN_LOOP.lock() {
+            *hook_loop = None;
+        }
 
         // Send hook disabled event
         {
@@ -524,6 +538,9 @@
             .ok_or_else(|| Error::HookStartFailed("Failed to get current run loop".into()))?;
 
         current_loop.add_source(Some(&source), kCFRunLoopCommonModes);
+        if let Ok(mut hook_loop) = HOOK_RUN_LOOP.lock() {
+            *hook_loop = Some(RunLoopPtr(current_loop.clone()));
+        }
 
         // Enable the tap
         CGEvent::tap_enable(&tap, true);
@@ -539,6 +556,9 @@
 
         // Run the loop
         CFRunLoop::run();
+        if let Ok(mut hook_loop) = HOOK_RUN_LOOP.lock() {
+            *hook_loop = None;
+        }
 
         // Send hook disabled event
         {
@@ -576,7 +596,9 @@
 
 /// Stop the event hook.
 pub fn stop_hook() -> Result<()> {
-    if let Some(run_loop) = CFRunLoop::main() {
+    if let Ok(hook_loop) = HOOK_RUN_LOOP.lock()
+        && let Some(RunLoopPtr(run_loop)) = &*hook_loop
+    {
         run_loop.stop();
     }
     Ok(())
```

## Draft upstream issue / PR

**Title:** macOS: `Hook::stop` blocks until the next input event (`stop_hook` stops the main run loop)

**Body:**

On macOS, `platform::macos::listen::stop_hook` calls `CFRunLoop::main().stop()`.
The event tap's run loop is the one of the thread that called `run_hook` /
`run_grab_hook` (`CFRunLoop::current()` there), e.g. the thread spawned by
`Hook::run_async`. Stopping the main run loop does not end that loop: the
hook thread exits only when the next event reaches the callback and it sees
the cleared stop flag. `Hook::stop` joins the thread, so it blocks until the
user next types or moves the mouse. In a GUI app, the app's main run loop is
stopped instead.

To reproduce: on macOS, start `Hook::run_async`, leave keyboard and mouse
idle, call `hook.stop()`: it returns only after an input event.

Fix: remember the hook thread's run loop when the tap is added and stop that
one in `stop_hook`; clear it when `CFRunLoop::run()` returns. The change is
confined to `src/platform/macos/listen.rs` (diff above).
