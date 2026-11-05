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
    callback: Box<dyn Fn(Option<Sample>)>,
}

// The context handle and function pointers belong to the driver and are only used from the main
// thread, like `Monitor` itself; `Send` is not needed and deliberately not implemented.

/// WTI_DEVICES + device, DVC_NPRESSURE.
const WTI_DEVICES: u32 = 100;
const DVC_NPRESSURE: u32 = 15;
/// WTI_CURSORS + cursor, CSR_TYPE.
const WTI_CURSORS: u32 = 200;
const CSR_TYPE: u32 = 20;
/// WTI_DEFSYSCTX: the driver's default *system cursor* context. It keeps the pen moving the
/// pointer as usual; the packets still reach us by polling.
const WTI_DEFSYSCTX: u32 = 4;
/// CXO_SYSTEM: the context is the system cursor's. A default system context already asks for it.
const CXO_SYSTEM: u32 = 0x0001;

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

impl Monitor {
    /// Open a WinTab context on `hwnd`. `callback` gets `Some(sample)` for each pen packet and
    /// `None` once the pen leaves range. Call on the thread that owns the window.
    pub fn open(hwnd: isize, callback: impl Fn(Option<Sample>) + 'static) -> Result<Self, Error> {
        // SAFETY: loading a system-installed DLL and reading its exported C functions is the
        // only way to reach WinTab; the signatures below match wintab.h (all Win32 stdcall).
        let lib = unsafe { Library::new("wintab32.dll") }.map_err(|e| Error::Unsupported(format!("wintab32.dll is not installed ({e})")))?;
        // SAFETY: symbols keep the library alive (`_lib`); the function types are wintab.h's.
        let (info, open, close, packets_get, queue_size_set) = unsafe {
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
            (info, open, close, packets_get, queue_size_set)
        };
        let mut lc = LogContext::default();
        // SAFETY: `lc` is a `LOGCONTEXTW` this crate lays out; the driver fills it.
        let size = unsafe { info(WTI_DEFSYSCTX, 0, (&raw mut lc).cast()) };
        if size == 0 {
            return Err(Error::Unsupported("no WinTab default context (no tablet driver)".into()));
        }
        // Pressure is read through the context's device; the mask is what `RawPacket` parses.
        let mut axis = RawAxis::default();
        // SAFETY: `axis` is a `DVC_NPRESSURE` `AXIS`.
        let axis_size = unsafe { info(WTI_DEVICES + lc.device, DVC_NPRESSURE, (&raw mut axis).cast()) };
        let pressure = if axis_size == 0 { Axis { min: 0, max: 1023 } } else { Axis { min: axis.min, max: axis.max } };
        // Keep CXO_SYSTEM: the pen must still move the pointer, so winit keeps delivering the
        // pointer events whose positions these samples belong to. No CXO_MESSAGES: the queue is
        // polled instead, which leaves winit's window procedure alone.
        lc.options |= CXO_SYSTEM;
        lc.options &= !0x0004;
        lc.pkt_data = wintab::packet_mask();
        lc.move_mask = lc.pkt_data;
        lc.pkt_mode = 0;
        let hwnd = hwnd as *mut c_void;
        // SAFETY: `hwnd` is this app's live window and `lc` a filled `LOGCONTEXTW`.
        let ctx = unsafe { open(hwnd, &raw mut lc, 1) };
        if ctx.is_null() {
            return Err(Error::Platform("WinTab refused to open a context".into()));
        }
        // SAFETY: `ctx` is the context just opened.
        let _ = unsafe { queue_size_set(ctx, QUEUE_SIZE) };
        Ok(Self {
            _lib: lib,
            close,
            packets_get,
            info,
            ctx,
            pump: Pump::with_pressure(pressure),
            last_packet: Instant::now(),
            pen: false,
            callback: Box::new(callback),
        })
    }

    /// Drain the packet queue, reporting each packet's sample (and `None` once the pen is gone).
    /// Call once per frame; cheap with no pen in use.
    pub fn poll(&mut self) {
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
        } else if self.pen && self.pump.went_away(started.duration_since(self.last_packet).as_secs_f64() * 1000.0) {
            // Out of range: a mouse painting now must not inherit the pen's last pressure.
            self.pen = false;
            (self.callback)(None);
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        // SAFETY: the context is still open (nothing else closed it).
        let _ = unsafe { (self.close)(self.ctx) };
    }
}
