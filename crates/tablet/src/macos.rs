//! AppKit local event monitor for tablet data. The only module in the workspace that allows
//! `unsafe`: three calls into AppKit's Objective-C API that objc2 can't prove sound on its own
//! (each has a `SAFETY:` comment). Everything the monitor reads goes through the pure, tested
//! mapping in [`crate::appkit`].
//!
//! A *local* monitor sees this app's events only, on the main thread, inside `-[NSApplication
//! sendEvent:]` before the event reaches winit's view, so the sample is current when the UI
//! handles the matching pointer event. The monitor passes every event through unchanged.

#![allow(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::rc::Rc;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSApplication, NSEvent, NSEventMask};

use crate::appkit::{InputEvent, InputPhase, RawEvent, State};
use crate::{Error, Sample, Update, deliver};

/// Keeps the monitor installed; dropping it removes the monitor. Main-thread only (not `Send`).
pub struct Monitor {
    token: Retained<AnyObject>,
    _handler: RcBlock<dyn Fn(NonNull<NSEvent>) -> *mut NSEvent>,
    coalescing: Rc<Cell<Option<bool>>>,
}

impl Monitor {
    /// Install the monitor. `callback` gets `Some(sample)` for pen events and `None` when a mouse
    /// moves; it runs on the main thread. Call on the main thread (before or after the event loop
    /// starts).
    pub fn install(callback: impl Fn(Option<Sample>) + 'static) -> Result<Self, Error> {
        Self::install_input(move |update, _| {
            if let Update::Set(sample) = update {
                deliver(&callback, sample);
            }
        })
    }

    /// Read position, time and pen data together, before winit dispatches the same event.
    /// Native pointer events are confined to the main window present during installation.
    pub fn install_input(callback: impl Fn(Update, Option<InputEvent>) + 'static) -> Result<Self, Error> {
        let mtm = MainThreadMarker::new().ok_or(Error::NotMainThread)?;
        let root_window = NSApplication::sharedApplication(mtm).windows().firstObject();
        let root_number = root_window.as_ref().map(|w| w.windowNumber());
        let state = RefCell::new(State::default());
        let contact = Cell::new(None::<InputEvent>);
        let coalescing = Rc::new(Cell::new(None));
        let handler_coalescing = Rc::clone(&coalescing);
        let handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: AppKit calls a local monitor's handler with the event it is about to
            // dispatch, a valid `NSEvent` that it keeps alive for the duration of the call; we
            // only borrow it within the call.
            let e: &NSEvent = unsafe { event.as_ref() };
            let raw = read(e);
            // The handler can't re-enter itself (AppKit runs it synchronously on the main
            // thread), but `try_borrow_mut` keeps a re-entrant call from panicking anyway.
            if let Ok(mut st) = state.try_borrow_mut() {
                let update = st.handle(&raw);
                let mut input = input_event(e, &raw, &st, mtm).filter(|i| Some(i.window_number as isize) == root_number);
                let ended = ((raw.kind == crate::appkit::event_type::TABLET_PROXIMITY || raw.subtype == crate::appkit::subtype::TABLET_PROXIMITY)
                    && !raw.entering)
                    || (raw.kind == 13 && e.subtype().0 == 2)
                    || (handler_coalescing.get().is_some() && root_window.as_ref().is_some_and(|w| !w.isKeyWindow()));
                if ended && let Some(mut last) = contact.take() {
                    last.phase = InputPhase::Up;
                    last.timestamp = e.timestamp();
                    last.modifiers = e.modifierFlags().0 as u64;
                    last.sample = last.sample.map(|s| Sample { pressure: 0.0, ..s });
                    input = Some(last);
                }
                if let Some(i) = input {
                    match i.phase {
                        InputPhase::Down => contact.set(Some(i)),
                        InputPhase::Move if contact.get().is_some() => contact.set(Some(i)),
                        InputPhase::Up => contact.set(None),
                        _ => {}
                    }
                }
                if input.is_some_and(|i| i.phase == InputPhase::Down) && handler_coalescing.get().is_none() {
                    handler_coalescing.set(Some(NSEvent::isMouseCoalescingEnabled()));
                    NSEvent::setMouseCoalescingEnabled(false);
                }
                if input.is_some_and(|i| i.phase == InputPhase::Up) || raw.kind == crate::appkit::event_type::LEFT_MOUSE_UP || ended {
                    restore_coalescing(&handler_coalescing);
                }
                // Do not let an application callback unwind through AppKit's block boundary.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(update, input)));
            }
            // Pass the event on unchanged.
            event.as_ptr()
        });
        // AppKit-defined ApplicationDeactivated ends a held contact when another app takes focus.
        let mask = NSEventMask(crate::appkit::event_mask() | (1 << 13));
        // SAFETY: the handler returns the (non-null, valid) event it was given, as the API
        // requires ("block's return must be a valid pointer or null"). We keep the block alive
        // in `Monitor` as well, although AppKit copies it.
        let token = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &handler) };
        let token = token.ok_or_else(|| Error::Platform("addLocalMonitorForEventsMatchingMask returned nil".into()))?;
        Ok(Self { token, _handler: handler, coalescing })
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        restore_coalescing(&self.coalescing);
        // SAFETY: `token` is exactly the object `addLocalMonitorForEventsMatchingMask:handler:`
        // returned, removed once (here). `Monitor` is not `Send`, so this runs on the main thread.
        unsafe { NSEvent::removeMonitor(&self.token) };
    }
}

fn restore_coalescing(state: &Cell<Option<bool>>) {
    if let Some(previous) = state.take() {
        NSEvent::setMouseCoalescingEnabled(previous);
    }
}

fn input_event(e: &NSEvent, raw: &RawEvent, state: &State, mtm: MainThreadMarker) -> Option<InputEvent> {
    use crate::appkit::event_type::*;
    let phase = match raw.kind {
        LEFT_MOUSE_DOWN => InputPhase::Down,
        LEFT_MOUSE_DRAGGED | MOUSE_MOVED => InputPhase::Move,
        LEFT_MOUSE_UP => InputPhase::Up,
        _ => return None,
    };
    let number = e.windowNumber();
    if number <= 0 {
        return None;
    }
    let view = e.window(mtm)?.contentView()?;
    let point = view.convertPoint_fromView(e.locationInWindow(), None);
    let y = if view.isFlipped() { point.y } else { view.bounds().size.height - point.y };
    let sample = (raw.subtype == crate::appkit::subtype::TABLET_POINT).then(|| state.sample(raw));
    Some(InputEvent { phase, position: [point.x, y], timestamp: e.timestamp(), sample, window_number: number as i64, modifiers: e.modifierFlags().0 as u64 })
}

/// Read the tablet fields of an event. Every getter is safe for every event type: AppKit
/// returns 0 / zero points for fields an event doesn't carry (`subtype` and the proximity fields
/// are only read for the event types that define them, see [`crate::appkit`]).
fn read(e: &NSEvent) -> RawEvent {
    let kind = e.r#type().0;
    let tabletish = matches!(kind, crate::appkit::event_type::TABLET_POINT | crate::appkit::event_type::TABLET_PROXIMITY);
    let mouse = !tabletish && is_mouse(kind);
    // `subtype` raises an exception for event types without one; ask only mouse events.
    let subtype = if mouse { e.subtype().0 } else { 0 };
    let proximity = kind == crate::appkit::event_type::TABLET_PROXIMITY || (mouse && subtype == crate::appkit::subtype::TABLET_PROXIMITY);
    let point = kind == crate::appkit::event_type::TABLET_POINT || (mouse && subtype == crate::appkit::subtype::TABLET_POINT);
    let (pressure, tilt, rotation) = if point {
        let t = e.tilt();
        (e.pressure(), (t.x, t.y), e.rotation())
    } else {
        (0.0, (0.0, 0.0), 0.0)
    };
    let (device, entering) = if proximity { (e.pointingDeviceType().0, e.isEnteringProximity()) } else { (0, false) };
    RawEvent { kind, subtype, pressure, tilt, rotation, device, entering }
}

fn is_mouse(kind: usize) -> bool {
    use crate::appkit::event_type::*;
    matches!(
        kind,
        LEFT_MOUSE_DOWN
            | LEFT_MOUSE_UP
            | RIGHT_MOUSE_DOWN
            | RIGHT_MOUSE_UP
            | MOUSE_MOVED
            | LEFT_MOUSE_DRAGGED
            | RIGHT_MOUSE_DRAGGED
            | OTHER_MOUSE_DOWN
            | OTHER_MOUSE_UP
            | OTHER_MOUSE_DRAGGED
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_graphics::{CGEvent, CGEventField, CGEventMouseSubtype, CGEventType, CGMouseButton};
    use objc2_foundation::NSPoint;

    #[test]
    fn install_off_the_main_thread_is_an_error() {
        // `cargo test` runs tests on worker threads, never on the main thread.
        let r = std::thread::spawn(|| Monitor::install(|_| {}).err()).join().unwrap();
        assert_eq!(r, Some(Error::NotMainThread));
    }

    /// A real `NSEvent` built from a Quartz event, as the tablet driver posts them.
    fn event(kind: CGEventType, set: impl Fn(&CGEvent)) -> objc2::rc::Retained<NSEvent> {
        // Quartz builds mouse events only; other types start blank and get their type set.
        let cg = CGEvent::new_mouse_event(None, kind, NSPoint::new(10.0, 10.0), CGMouseButton::Left)
            .or_else(|| {
                let e = CGEvent::new(None)?;
                CGEvent::set_type(Some(&e), kind);
                Some(e)
            })
            .expect("CGEvent");
        set(&cg);
        NSEvent::eventWithCGEvent(&cg).expect("NSEvent")
    }

    fn int(e: &CGEvent, f: CGEventField, v: i64) {
        CGEvent::set_integer_value_field(Some(e), f, v);
    }

    fn dbl(e: &CGEvent, f: CGEventField, v: f64) {
        CGEvent::set_double_value_field(Some(e), f, v);
    }

    /// Tablet mouse events read back through the same path as live ones.
    #[test]
    fn tablet_events_read_through_the_monitor_path() {
        let mut st = State::default();
        let drag = event(CGEventType::LeftMouseDragged, |e| {
            int(e, CGEventField::MouseEventSubtype, i64::from(CGEventMouseSubtype::TabletPoint.0));
            dbl(e, CGEventField::MouseEventPressure, 0.25);
            dbl(e, CGEventField::TabletEventPointPressure, 0.25);
            dbl(e, CGEventField::TabletEventTiltX, 0.5);
            dbl(e, CGEventField::TabletEventTiltY, -0.5);
            dbl(e, CGEventField::TabletEventRotation, 45.0);
        });
        let raw = read(&drag);
        assert_eq!((raw.kind, raw.subtype), (6, 1));
        let Update::Set(Some(s)) = st.handle(&raw) else { panic!("{raw:?}") };
        assert!((s.pressure - 0.25).abs() < 0.01, "{s:?}"); // Quartz keeps 8 bits of mouse pressure.
        assert!((s.tilt_x - 30.0).abs() < 0.1 && (s.tilt_y - 30.0).abs() < 0.1, "{s:?}");
        assert!((s.rotation - 45.0).abs() < 0.1, "{s:?}");
        assert!(!s.eraser);

        // Eraser end entering proximity, then a press with it.
        let prox = event(CGEventType::TabletProximity, |e| {
            int(e, CGEventField::TabletProximityEventPointerType, crate::appkit::device::ERASER as i64);
            int(e, CGEventField::TabletProximityEventEnterProximity, 1);
        });
        let raw = read(&prox);
        assert_eq!((raw.kind, raw.device, raw.entering), (24, 3, true));
        assert!(matches!(st.handle(&raw), Update::Set(Some(Sample { eraser: true, .. }))));

        // A plain mouse event (subtype 0) is a mouse, whatever its pressure.
        let mouse = event(CGEventType::LeftMouseDown, |e| dbl(e, CGEventField::MouseEventPressure, 0.7));
        let raw = read(&mouse);
        assert_eq!((raw.kind, raw.subtype), (1, 0));
        assert_eq!(st.handle(&raw), Update::Set(None));
    }
}
