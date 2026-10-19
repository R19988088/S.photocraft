//! The AppKit monitor end to end, on the main thread (`harness = false`): install it, dispatch
//! real tablet `NSEvent`s through `-[NSApplication sendEvent:]` (where AppKit runs local
//! monitors), and check the samples the callback gets; dropping the monitor removes it.

#[cfg(target_os = "macos")]
fn main() {
    use std::cell::RefCell;
    use std::rc::Rc;

    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventMask, NSEventModifierFlags, NSEventType, NSView, NSViewController, NSWindow,
    };
    use objc2_core_graphics::{CGEvent, CGEventField, CGEventMouseSubtype, CGEventType, CGMouseButton};
    use objc2_foundation::{NSDate, NSPoint, NSSize, NSString};
    use photocraft_tablet::Sample;
    use photocraft_tablet::appkit::InputPhase;
    use photocraft_tablet::macos::Monitor;

    // Answer cargo-nextest's `--list` (and nothing for `--ignored`), as tests/mac_menu_appkit.rs
    // in apps/photocraft does; `monitor_macos --exact` falls through to the test.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--list") {
        if !args.iter().any(|a| a == "--ignored") {
            println!("monitor_macos: test");
        }
        return;
    }

    let mtm = MainThreadMarker::new().expect("harness = false runs on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    assert!(app.setActivationPolicy(NSApplicationActivationPolicy::Regular));
    app.finishLaunching();
    let event = |kind: CGEventType, pressure: f64, tilt: (f64, f64), subtype: CGEventMouseSubtype| {
        let cg = CGEvent::new_mouse_event(None, kind, NSPoint::new(5.0, 5.0), CGMouseButton::Left).expect("CGEvent");
        let set = |f, v| CGEvent::set_double_value_field(Some(&cg), f, v);
        CGEvent::set_integer_value_field(Some(&cg), CGEventField::MouseEventSubtype, i64::from(subtype.0));
        set(CGEventField::MouseEventPressure, pressure);
        set(CGEventField::TabletEventPointPressure, pressure);
        set(CGEventField::TabletEventTiltX, tilt.0);
        set(CGEventField::TabletEventTiltY, tilt.1);
        NSEvent::eventWithCGEvent(&cg).expect("NSEvent")
    };

    let seen: Rc<RefCell<Vec<Option<Sample>>>> = Rc::default();
    let monitor = {
        let seen = seen.clone();
        Monitor::install(move |s| seen.borrow_mut().push(s)).expect("install on the main thread")
    };
    app.sendEvent(&event(CGEventType::LeftMouseDragged, 0.5, (0.5, 0.0), CGEventMouseSubtype::TabletPoint));
    app.sendEvent(&event(CGEventType::LeftMouseDragged, 1.0, (0.0, 0.0), CGEventMouseSubtype::Default));
    {
        let seen = seen.borrow();
        assert_eq!(seen.len(), 2, "{seen:?}");
        let s = seen[0].expect("pen sample");
        assert!((s.pressure - 0.5).abs() < 0.01 && (s.tilt_x - 30.0).abs() < 0.1, "{s:?}");
        assert_eq!(seen[1], None, "a mouse event resets to mouse");
    }
    drop(monitor);
    app.sendEvent(&event(CGEventType::LeftMouseDragged, 0.5, (0.0, 0.0), CGEventMouseSubtype::TabletPoint));
    assert_eq!(seen.borrow().len(), 2, "the monitor is gone after drop");

    // Events without a window still update the compatibility pressure feed, but must never
    // create canvas input or change application-wide mouse coalescing.
    let coalescing = NSEvent::isMouseCoalescingEnabled();
    let inputs = Rc::new(RefCell::new(Vec::new()));
    let monitor = {
        let inputs = inputs.clone();
        Monitor::install_input(move |update, input| inputs.borrow_mut().push((update, input))).expect("native input monitor")
    };
    app.sendEvent(&event(CGEventType::LeftMouseDown, 0.5, (0.5, 0.0), CGEventMouseSubtype::TabletPoint));
    {
        let inputs = inputs.borrow();
        assert_eq!(inputs.len(), 1);
        assert!(matches!(inputs[0].0, photocraft_tablet::Update::Set(Some(_))));
        assert!(inputs[0].1.is_none(), "windowless events cannot reach the canvas");
    }
    assert_eq!(NSEvent::isMouseCoalescingEnabled(), coalescing);
    drop(monitor);
    assert_eq!(NSEvent::isMouseCoalescingEnabled(), coalescing);

    // Create the root window through the safe controller convenience API, just as the app
    // installs its monitor after creating its native window.
    let controller = NSViewController::new(mtm);
    controller.setView(&NSView::new(mtm));
    let window = NSWindow::windowWithContentViewController(&controller);
    window.setContentSize(NSSize::new(320.0, 240.0));
    window.makeKeyAndOrderFront(None);
    app.activate();
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let mode = NSString::from_str("kCFRunLoopDefaultMode");
    let activation_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !window.isKeyWindow() && std::time::Instant::now() < activation_deadline {
        if let Some(event) =
            app.nextEventMatchingMask_untilDate_inMode_dequeue(NSEventMask::Any, Some(&NSDate::dateWithTimeIntervalSinceNow(0.02)), &mode, true)
        {
            app.sendEvent(&event);
        }
        window.makeKeyWindow();
    }
    assert!(window.isKeyWindow());
    let number = window.windowNumber();
    let native = Rc::new(RefCell::new(Vec::new()));
    let monitor = {
        let native = native.clone();
        Monitor::install_input(move |_, input| {
            if let Some(input) = input {
                native.borrow_mut().push(input);
            }
        })
        .expect("window input monitor")
    };
    let tablet_event = |kind, point, timestamp, pressure| {
        let base = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            kind,
            point,
            NSEventModifierFlags::Shift,
            timestamp,
            number,
            None,
            1,
            1,
            pressure,
        )
        .expect("window mouse event");
        let cg = base.CGEvent().expect("window event's Quartz representation");
        CGEvent::set_integer_value_field(Some(&cg), CGEventField::MouseEventSubtype, i64::from(CGEventMouseSubtype::TabletPoint.0));
        CGEvent::set_integer_value_field(Some(&cg), CGEventField::MouseEventWindowUnderMousePointer, number as i64);
        CGEvent::set_integer_value_field(Some(&cg), CGEventField::MouseEventWindowUnderMousePointerThatCanHandleThisEvent, number as i64);
        CGEvent::set_double_value_field(Some(&cg), CGEventField::MouseEventPressure, pressure as f64);
        CGEvent::set_double_value_field(Some(&cg), CGEventField::TabletEventPointPressure, pressure as f64);
        CGEvent::set_double_value_field(Some(&cg), CGEventField::TabletEventTiltX, 0.5);
        CGEvent::set_double_value_field(Some(&cg), CGEventField::TabletEventRotation, 45.0);
        let event = NSEvent::eventWithCGEvent(&cg).expect("window tablet event");
        assert_eq!(event.windowNumber(), number);
        event
    };
    app.sendEvent(&tablet_event(NSEventType::LeftMouseDown, NSPoint::new(20.0, 30.0), 100.0, 0.25));
    assert!(!NSEvent::isMouseCoalescingEnabled(), "contact disables AppKit movement coalescing");
    app.sendEvent(&tablet_event(NSEventType::LeftMouseDragged, NSPoint::new(80.0, 60.0), 100.01, 0.75));
    app.sendEvent(&tablet_event(NSEventType::LeftMouseUp, NSPoint::new(80.0, 60.0), 100.02, 0.0));
    assert_eq!(NSEvent::isMouseCoalescingEnabled(), coalescing, "release restores the previous setting");
    {
        let native = native.borrow();
        assert_eq!(native.len(), 3, "one native sample for each AppKit event");
        assert_eq!(native.iter().map(|e| e.phase).collect::<Vec<_>>(), [InputPhase::Down, InputPhase::Move, InputPhase::Up]);
        for (sample, position, timestamp, pressure) in
            [(&native[0], [20.0, 210.0], 100.0, 0.25), (&native[1], [80.0, 180.0], 100.01, 0.75), (&native[2], [80.0, 180.0], 100.02, 0.0)]
        {
            assert!((sample.position[0] - position[0]).abs() < 0.01 && (sample.position[1] - position[1]).abs() < 0.01, "{sample:?}");
            assert!((sample.timestamp - timestamp).abs() < 0.0001, "{sample:?}");
            let pen = sample.sample.expect("tablet pose is attached to the same event");
            assert!((pen.pressure - pressure).abs() < 0.01 && (pen.tilt_x - 30.0).abs() < 0.1 && (pen.rotation - 45.0).abs() < 0.1, "{sample:?}");
            assert_ne!(sample.modifiers & (1 << 17), 0);
        }
    }
    drop(monitor);
    window.orderOut(None);
    println!("monitor_macos: ok");
}

#[cfg(not(target_os = "macos"))]
fn main() {}
