//! WinTab (the Wacom tablet API on Windows) → [`Sample`]s.
//!
//! Windows' own pen input (`WM_POINTER`, which winit forwards as touch force) carries pressure
//! but no tilt, rotation or eraser end. The WinTab driver reports all of them, so when
//! Preferences › Tools › Tablet input mode is WinTab this opens a WinTab context on the app's
//! window and reads its packet queue once per frame ([`Monitor::poll`]).
//!
//! `wintab32.dll` is installed by the tablet driver (not by Windows), so it is loaded at
//! runtime; without it (a mouse-only machine, or a driver without WinTab) the monitor is simply
//! not installed and pens keep painting through `WM_POINTER`.
//!
//! The context is opened without `CXO_SYSTEM`: the driver must not take over the cursor, or
//! winit would see the tablet's own mouse motion next to its pointer events. Packets are
//! retrieved by polling instead of `WT_PACKET` messages (the spec queues them either way), which
//! keeps winit's window procedure untouched. Everything the packets mean is mapped by the pure,
//! tested [`crate::wintab`].

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::time::Instant;

use libloading::Library;

use crate::wintab::{self, Axis, Pump};
use crate::{Error, Sample};

/// Loaded once and kept until the event loop returns; dropping it closes the context and the
/// library.
pub struct Monitor {
    _lib: Library,
    close: unsafe extern "system" fn(*mut c_void) -> i32,
    packets_get: unsafe extern "system" fn(*mut c_void, i32, *mut c_void) -> i32,
    info: unsafe extern "system" fn(u32, u32, *mut c_void) -> u32,
    ctx: *mut c_void,
    /// Packet bytes → samples; the same state machine the tests drive on every platform.
    pump: Pump,
    last_packet: Instant,
    /// Pen samples were reported: winit's touch force for the same pen must then be ignored.
    pen: bool,
    /// A packet has arrived at some point: WinTab really is feeding this pen (see [`Self::poll`]).
    seen_packets: bool,
    /// When the "no packets yet" warning was last printed, so it repeats at most every 2 s.
    waiting_since: Instant,
    callback: Box<dyn Fn(Option<Sample>)>,
}

// The context handle and function pointers belong to the driver and are only used from the main
// thread, like `Monitor` itself; `Send` is not needed and deliberately not implemented.

/// WTI_DEVICES + device, DVC_NPRESSURE.
const WTI_DEVICES: u32 = 100;
const DVC_NPRESSURE: u32 = 15;
/// WTI_DEVICES + device, DVC_NAME (the tablet's name, for the log).
const DVC_NAME: u32 = 1;
/// WTI_CURSORS + cursor, CSR_TYPE.
const WTI_CURSORS: u32 = 200;
const CSR_TYPE: u32 = 20;
/// WTI_DEFCONTEXT: the driver's default *application* context. The system-cursor context
/// (`WTI_DEFSYSCTX`) is the one Windows itself drives; opening our own copy of it is refused or
/// silently never queues packets on several drivers, which is why pens looked dead.
const WTI_DEFCONTEXT: u32 = 3;
/// CXO_SYSTEM: the context drives the system cursor.
const CXO_SYSTEM: u32 = 0x0001;
/// CXO_MESSAGES: post `WT_PACKET` to the window. Not wanted: the queue is polled, which keeps
/// winit's window procedure untouched.
const CXO_MESSAGES: u32 = 0x0004;

/// `LOGCONTEXTW` (`wintab.h`). The name is 40 `WCHAR`s; it is left empty and never read.
#[repr(C)]
#[derive(Clone, Copy)]
struct LogContext {
    name: [u8; 80],
    options: u32,
    status: u32,
    locks: u32,
    msg_base: u32,
    device: u32,
    pkt_rate: u32,
    pkt_data: u32,
    pkt_mode: u32,
    move_mask: u32,
    btn_dn_mask: u32,
    btn_up_mask: u32,
    in_org: [i32; 3],
    in_ext: [i32; 3],
    out_org: [i32; 3],
    out_ext: [i32; 3],
    sens: [i32; 3],
    sys_mode: i32,
    sys_org: [i32; 2],
    sys_ext: [i32; 2],
    sys_sens: [i32; 2],
}

impl Default for LogContext {
    fn default() -> Self {
        Self {
            name: [0; 80],
            options: 0,
            status: 0,
            locks: 0,
            msg_base: 0,
            device: 0,
            pkt_rate: 0,
            pkt_data: 0,
            pkt_mode: 0,
            move_mask: 0,
            btn_dn_mask: 0,
            btn_up_mask: 0,
            in_org: [0; 3],
            in_ext: [0; 3],
            out_org: [0; 3],
            out_ext: [0; 3],
            sens: [0; 3],
            sys_mode: 0,
            sys_org: [0; 2],
            sys_ext: [0; 2],
            sys_sens: [0; 2],
        }
    }
}

/// `AXIS` (`wintab.h`).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawAxis {
    min: i32,
    max: i32,
    units: u32,
    resolution: u32,
}

/// Most packets read per `WTPacketsGet` call; the loop drains until the queue is empty.
const PACKETS_PER_CALL: i32 = 128;
/// The packet queue requested: a few frames of a 1000 Hz pen, so a slow frame loses nothing.
const QUEUE_SIZE: i32 = 1024;

/// A WinTab context counts as the pen's source only while packets are this recent. A context
/// that opens but never queues (a driver that won't share it) or stops queueing must leave
/// pressure to winit's `WM_POINTER` path, or both sources end up switched off.
const HOLD_SECS: f64 = 2.0;

impl Monitor {
    /// Open a WinTab context on `hwnd`. `callback` gets `Some(sample)` for each pen packet and
    /// `None` once the pen leaves range. Call on the thread that owns the window.
    pub fn open(hwnd: isize, callback: impl Fn(Option<Sample>) + 'static) -> Result<Self, Error> {
        // SAFETY: loading a system-installed DLL and reading its exported C functions is the
        // only way to reach WinTab; the signatures below match wintab.h (all Win32 stdcall).
        let lib = unsafe { Library::new("wintab32.dll") }.map_err(|e| Error::Unsupported(format!("wintab32.dll is not installed ({e})")))?;
        // SAFETY: symbols keep the library alive (`_lib`); the function types are wintab.h's.
        let (info, open, close, packets_get, queue_size_set, enable, overlap) = unsafe {
            let info: unsafe extern "system" fn(u32, u32, *mut c_void) -> u32 = match lib.get(b"WTInfoW\0") {
                Ok(f) => *f,
                Err(_) => *lib.get(b"WTInfoA\0").map_err(|e| Error::Platform(format!("WTInfo: {e}")))?,
            };
            let open: unsafe extern "system" fn(*mut c_void, *mut LogContext, i32) -> *mut c_void = match lib.get(b"WTOpenW\0") {
                Ok(f) => *f,
                Err(_) => *lib.get(b"WTOpenA\0").map_err(|e| Error::Platform(format!("WTOpen: {e}")))?,
            };
            let close: unsafe extern "system" fn(*mut c_void) -> i32 = *lib.get(b"WTClose\0").map_err(|e| Error::Platform(format!("WTClose: {e}")))?;
            let packets_get: unsafe extern "system" fn(*mut c_void, i32, *mut c_void) -> i32 =
                *lib.get(b"WTPacketsGet\0").map_err(|e| Error::Platform(format!("WTPacketsGet: {e}")))?;
            let queue_size_set: unsafe extern "system" fn(*mut c_void, i32) -> i32 =
                *lib.get(b"WTQueueSizeSet\0").map_err(|e| Error::Platform(format!("WTQueueSizeSet: {e}")))?;
            // Optional: a context opens disabled on some drivers and needs WTEnable(1).
            let enable: Option<unsafe extern "system" fn(*mut c_void, i32) -> i32> = lib.get(b"WTEnable\0").ok().map(|f| *f);
            let overlap: Option<unsafe extern "system" fn(*mut c_void, i32) -> i32> = lib.get(b"WTOverlap\0").ok().map(|f| *f);
            (info, open, close, packets_get, queue_size_set, enable, overlap)
        };
        let mut lc = LogContext::default();
        // SAFETY: `lc` is a `LOGCONTEXTW` this crate lays out; the driver fills it.
        let size = unsafe { info(WTI_DEFCONTEXT, 0, (&raw mut lc).cast()) };
        if size == 0 {
            return Err(Error::Unsupported("no WinTab default context (no tablet driver)".into()));
        }
        // Pressure is read through the context's device; the mask is what `RawPacket` parses.
        let mut axis = RawAxis::default();
        // SAFETY: `axis` is a `DVC_NPRESSURE` `AXIS`.
        let axis_size = unsafe { info(WTI_DEVICES + lc.device, DVC_NPRESSURE, (&raw mut axis).cast()) };
        let pressure = if axis_size == 0 { Axis { min: 0, max: 1023 } } else { Axis { min: axis.min, max: axis.max } };
        // Input extents, so the log can tell a wrong device from a wrong axis.
        let (in_ext, out_ext) = (lc.in_ext, lc.out_ext);
        // Keep CXO_SYSTEM (the pen still moves the pointer, so winit keeps delivering the
        // positions these samples belong to) and CXO_MESSAGES off (the queue is polled).
        // CXO_SYSTEM must be asked for explicitly: a default context does not set it.
        lc.options |= CXO_SYSTEM;
        lc.options &= !CXO_MESSAGES;
        lc.pkt_data = wintab::packet_mask();
        lc.move_mask = lc.pkt_data;
        lc.pkt_mode = 0;
        let hwnd = hwnd as *mut c_void;
        // SAFETY: `hwnd` is this app's live window and `lc` a filled `LOGCONTEXTW`.
        let ctx = unsafe { open(hwnd, &raw mut lc, 0) };
        if ctx.is_null() {
            return Err(Error::Platform("WinTab refused to open a context".into()));
        }
        // Open explicitly enabled and on top of any other context (Photoshop, a driver panel).
        // Drivers differ about the default state of both; a disabled or obscured context never
        // queues a packet.
        // SAFETY: `ctx` is the context just opened.
        unsafe {
            if let Some(enable) = enable {
                enable(ctx, 1);
            }
            if let Some(overlap) = overlap {
                overlap(ctx, 1);
            }
            let _ = queue_size_set(ctx, QUEUE_SIZE);
        }
        // The device name is the one thing that tells "no tablet" from "wrong tablet" in a report.
        let mut name = [0u16; 256];
        // SAFETY: `name` is a 256-wide buffer; WTInfo wants `DVC_NAME` as a NUL-terminated string.
        let name_len = unsafe { info(WTI_DEVICES + lc.device, DVC_NAME, name.as_mut_ptr().cast()) };
        let name_len = (usize::try_from(name_len).unwrap_or(0) / 2).min(name.len());
        let device = String::from_utf16_lossy(&name[..name_len]).trim_end_matches('\0').to_string();
        log::info!(
            "tablet: WinTab context open on device {} ({:?}); pressure {}..{} in-ext {}x{} out-ext {}x{}",
            lc.device,
            device,
            pressure.min,
            pressure.max,
            in_ext[0],
            in_ext[1],
            out_ext[0],
            out_ext[1]
        );
        Ok(Self {
            _lib: lib,
            close,
            packets_get,
            info,
            ctx,
            pump: Pump::with_pressure(pressure),
            last_packet: Instant::now(),
            pen: false,
            seen_packets: false,
            waiting_since: Instant::now(),
            callback: Box::new(callback),
        })
    }

    /// Drain the packet queue, reporting each packet's sample (and `None` once the pen is gone).
    /// Returns whether WinTab is actually the pen's source: only once a packet has arrived. Until
    /// then the UI keeps reading winit's `WM_POINTER` force, so a driver whose context opens but
    /// never queues packets leaves pressure working through Ink instead of switching both off.
    /// Call once per frame; cheap with no pen in use.
    pub fn poll(&mut self) -> bool {
        let mut buf = [0u8; wintab::PACKET_BYTES * PACKETS_PER_CALL as usize];
        let started = Instant::now();
        let mut got_packets = false;
        loop {
            // SAFETY: `ctx` is open and `buf` holds PACKETS_PER_CALL packets of the mask above.
            let n = unsafe { (self.packets_get)(self.ctx, PACKETS_PER_CALL, buf.as_mut_ptr().cast()) };
            if n <= 0 {
                break;
            }
            let n = usize::try_from(n).unwrap_or(0).min(PACKETS_PER_CALL as usize);
            got_packets |= n > 0;
            // The samples of these bytes; the cursor-type lookup reaches the driver, so it runs
            // only for cursors the pump hasn't seen.
            let info = self.info;
            let samples = self.pump.drain(&buf[..n * wintab::PACKET_BYTES], started.elapsed().as_secs_f64() * 1000.0, |cursor| {
                let mut value = 0u32;
                // SAFETY: `value` is a `CSR_TYPE` `UINT`; `info` is this context's WTInfo.
                let read = unsafe { info(WTI_CURSORS + cursor, CSR_TYPE, (&raw mut value).cast()) };
                (read != 0).then_some(value)
            });
            for sample in samples {
                self.pen = true;
                (self.callback)(Some(sample));
            }
            if n < PACKETS_PER_CALL as usize {
                break;
            }
        }
        if got_packets {
            self.last_packet = started;
            if !self.seen_packets {
                self.seen_packets = true;
                log::info!("tablet: WinTab packets are arriving");
            }
        } else if self.pen && self.pump.went_away(started.duration_since(self.last_packet).as_secs_f64() * 1000.0) {
            // Out of range: a mouse painting now must not inherit the pen's last pressure.
            self.pen = false;
            (self.callback)(None);
        } else if !self.seen_packets && self.waiting_since.elapsed().as_secs() >= 2 {
            self.waiting_since = started;
            log::warn!("tablet: WinTab context open but no packets yet; pressure stays on the Ink path until one arrives");
        }
        // The reader owns the pen only while it is demonstrably feeding it. A stale or silent
        // context hands pressure back to Ink instead of leaving both sources disabled.
        self.seen_packets && started.saturating_duration_since(self.last_packet).as_secs_f64() < HOLD_SECS
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        // SAFETY: the context is still open (nothing else closed it).
        let _ = unsafe { (self.close)(self.ctx) };
    }
}
