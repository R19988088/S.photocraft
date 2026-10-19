# macOS brush input

The canvas processes the current frame's painting input before preparing its display texture.
Previously the texture was prepared first, so even an immediate brush Down became visible only
on a later frame.

AppKit contact events retain their own content-view position, timestamp, pressure, tilt,
rotation and modifiers. The primary canvas consumes this stream once and suppresses duplicate
winit stroke events. Widgets still determine whether the canvas owns the press. Other platforms
and the control protocol retain their existing input paths. Mouse coalescing is disabled during
native contact and its prior setting is restored when contact ends or the monitor is removed.

The pressure pipeline follows C3's `SaiStrokeInputStabilizer.swift` and
`TabletPressureSettings.swift` from `minimal_20261019_092816`: 16-bit normalization, maximum
pressure 0.76, dead zone 0, elasticity 2.1, coordinate window 2 and pressure window 5. Preferences
exposes these values under Tools / Tablet Pressure. The filter is enabled by default on macOS
and applies to pen contacts only. The first activation emits immediately at pressure 0.001;
history windows use samples already received. Coordinate smoothing emits the previous cubic
segment, and release flushes the final segment using the last contact rather than the lift's
zero pressure. The existing PhotoCraft pressure curve and brush dynamics consume the result.

Event times remain paired with samples; time-dependent brushes also keep a monotonic clock
when held repeats overlap delayed coordinate segments. Temporary view tools and dialogs finish
an active contact before taking ownership of the pointer.

PicSSS's independent GPU-command/present-fence workers were examined as a reference. The
confirmed extra-frame delay here came from frame ordering; it does not establish a Rust runtime
stall. Hardware tablet latency and subjective feel require a physical tablet session.
