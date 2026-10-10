//! Input injection through uinput virtual devices.

use std::collections::HashSet;

use anyhow::{Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode, RelativeAxisCode, UinputAbsSetup};

/// Absolute axis resolution. Coordinates are mapped onto the monitor's
/// rectangle within the compositor's layout (see `set_target`).
const ABS_MAX: i32 = 65535;

const BTN_MIN: u16 = 0x110; // BTN_LEFT
const BTN_MAX: u16 = 0x117; // BTN_TASK

/// Where on the compositor's global layout the streamed monitor lies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Target {
    /// Bounding box of all monitors (logical pixels).
    pub layout_x: i32,
    pub layout_y: i32,
    pub layout_w: u32,
    pub layout_h: u32,
    /// The streamed monitor (logical pixels).
    pub mon_x: i32,
    pub mon_y: i32,
    pub mon_w: u32,
    pub mon_h: u32,
}

pub struct Injector {
    kbd: VirtualDevice,
    ptr: VirtualDevice,
    rel: VirtualDevice,
    keys_down: HashSet<u16>,
    buttons_down: HashSet<u16>,
    target: Option<Target>,
    last_motion_seq: Option<u32>,
    rel_rem: (f32, f32),
    wheel_rem: (i32, i32),
}

fn is_valid_key(code: u16) -> bool {
    // KEY_ESC..KEY_MICMUTE plus the extended range; excludes button codes.
    (1..=0x2ff).contains(&code) && !(0x100..0x160).contains(&code)
}

fn is_valid_button(code: u16) -> bool {
    (BTN_MIN..=BTN_MAX).contains(&code)
}

impl Injector {
    pub fn new() -> Result<Self> {
        let id = InputId::new(BusType::BUS_VIRTUAL, 0x524c, 0x0001, 1);
        let mut keys = AttributeSet::<KeyCode>::new();
        for c in 1..=0x2ffu16 {
            if is_valid_key(c) {
                keys.insert(KeyCode::new(c));
            }
        }
        let kbd = VirtualDevice::builder()
            .context("opening /dev/uinput (is your user allowed to write it? see packaging/70-remotelinux.rules)")?
            .name("RemoteLinux Keyboard")
            .input_id(id)
            .with_keys(&keys)?
            .build()
            .context("creating virtual keyboard")?;

        let mut buttons = AttributeSet::<KeyCode>::new();
        for c in BTN_MIN..=BTN_MAX {
            buttons.insert(KeyCode::new(c));
        }
        let mut wheel = AttributeSet::<RelativeAxisCode>::new();
        for a in [RelativeAxisCode::REL_WHEEL, RelativeAxisCode::REL_HWHEEL, RelativeAxisCode::REL_WHEEL_HI_RES, RelativeAxisCode::REL_HWHEEL_HI_RES] {
            wheel.insert(a);
        }
        let abs = AbsInfo::new(0, 0, ABS_MAX, 0, 0, 0);
        let ptr = VirtualDevice::builder()?
            .name("RemoteLinux Pointer")
            .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x524c, 0x0002, 1))
            .with_keys(&buttons)?
            .with_relative_axes(&wheel)?
            .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_X, abs))?
            .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_Y, abs))?
            .build()
            .context("creating virtual pointer")?;

        // A separate relative mouse for pointer-lock style motion. libinput
        // does not accept REL_X/REL_Y on a device that also has ABS_X/ABS_Y.
        let mut rel_axes = AttributeSet::<RelativeAxisCode>::new();
        rel_axes.insert(RelativeAxisCode::REL_X);
        rel_axes.insert(RelativeAxisCode::REL_Y);
        let mut rel_buttons = AttributeSet::<KeyCode>::new();
        rel_buttons.insert(KeyCode::BTN_LEFT);
        let rel = VirtualDevice::builder()?
            .name("RemoteLinux Relative Mouse")
            .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x524c, 0x0003, 1))
            .with_keys(&rel_buttons)?
            .with_relative_axes(&rel_axes)?
            .build()
            .context("creating virtual relative mouse")?;

        Ok(Self {
            kbd,
            ptr,
            rel,
            keys_down: HashSet::new(),
            buttons_down: HashSet::new(),
            target: None,
            last_motion_seq: None,
            rel_rem: (0.0, 0.0),
            wheel_rem: (0, 0),
        })
    }

    pub fn set_target(&mut self, t: Target) {
        self.target = Some(t);
    }

    /// Reset per-connection state (sequence numbers).
    pub fn new_session(&mut self) {
        self.last_motion_seq = None;
        self.release_all();
    }

    pub fn key(&mut self, code: u16, pressed: bool) -> Result<()> {
        if !is_valid_key(code) {
            return Ok(());
        }
        // Drop repeated presses and releases of keys that aren't down.
        let changed = if pressed { self.keys_down.insert(code) } else { self.keys_down.remove(&code) };
        if changed {
            self.kbd.emit(&[InputEvent::new(EventType::KEY.0, code, pressed as i32)])?;
        }
        Ok(())
    }

    pub fn button(&mut self, code: u16, pressed: bool) -> Result<()> {
        if !is_valid_button(code) {
            return Ok(());
        }
        let changed = if pressed { self.buttons_down.insert(code) } else { self.buttons_down.remove(&code) };
        if changed {
            self.ptr.emit(&[InputEvent::new(EventType::KEY.0, code, pressed as i32)])?;
        }
        Ok(())
    }

    /// Scroll in 1/120 notch units; positive y = content moves up (scroll down).
    pub fn scroll(&mut self, v120_x: i32, v120_y: i32) -> Result<()> {
        let mut ev = Vec::with_capacity(4);
        // evdev: positive REL_WHEEL = scroll up, positive REL_HWHEEL = right.
        let (wy, wx) = (-v120_y, v120_x);
        if wy != 0 {
            ev.push(InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_WHEEL_HI_RES.0, wy));
            self.wheel_rem.1 += wy;
            let notches = self.wheel_rem.1 / 120;
            if notches != 0 {
                self.wheel_rem.1 -= notches * 120;
                ev.push(InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_WHEEL.0, notches));
            }
        }
        if wx != 0 {
            ev.push(InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_HWHEEL_HI_RES.0, wx));
            self.wheel_rem.0 += wx;
            let notches = self.wheel_rem.0 / 120;
            if notches != 0 {
                self.wheel_rem.0 -= notches * 120;
                ev.push(InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_HWHEEL.0, notches));
            }
        }
        if !ev.is_empty() {
            self.ptr.emit(&ev)?;
        }
        Ok(())
    }

    /// Returns false if the motion is stale (older sequence than one already applied).
    fn accept_seq(&mut self, seq: u32) -> bool {
        if let Some(last) = self.last_motion_seq {
            if (seq.wrapping_sub(last) as i32) <= 0 {
                return false;
            }
        }
        self.last_motion_seq = Some(seq);
        true
    }

    /// Absolute motion, normalised 0..1 within the streamed monitor.
    pub fn motion_abs(&mut self, seq: u32, x: f32, y: f32) -> Result<()> {
        if !self.accept_seq(seq) {
            return Ok(());
        }
        let Some(t) = self.target else { return Ok(()) };
        let (ax, ay) = map_abs(&t, x, y);
        self.ptr.emit(&[
            InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, ax),
            InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, ay),
        ])?;
        Ok(())
    }

    pub fn motion_rel(&mut self, seq: u32, dx: f32, dy: f32) -> Result<()> {
        if !self.accept_seq(seq) {
            return Ok(());
        }
        let fx = dx + self.rel_rem.0;
        let fy = dy + self.rel_rem.1;
        let (ix, iy) = (fx.trunc() as i32, fy.trunc() as i32);
        self.rel_rem = (fx - ix as f32, fy - iy as f32);
        if ix != 0 || iy != 0 {
            self.rel.emit(&[
                InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_X.0, ix),
                InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_Y.0, iy),
            ])?;
        }
        Ok(())
    }

    /// Release every key and button currently held. Called on disconnect,
    /// focus loss, and shutdown so nothing stays stuck.
    pub fn release_all(&mut self) {
        let keys: Vec<u16> = self.keys_down.drain().collect();
        if !keys.is_empty() {
            let ev: Vec<_> = keys.iter().map(|&k| InputEvent::new(EventType::KEY.0, k, 0)).collect();
            if let Err(e) = self.kbd.emit(&ev) {
                tracing::warn!("releasing keys: {e}");
            }
        }
        let buttons: Vec<u16> = self.buttons_down.drain().collect();
        if !buttons.is_empty() {
            let ev: Vec<_> = buttons.iter().map(|&b| InputEvent::new(EventType::KEY.0, b, 0)).collect();
            if let Err(e) = self.ptr.emit(&ev) {
                tracing::warn!("releasing buttons: {e}");
            }
        }
        self.wheel_rem = (0, 0);
        self.rel_rem = (0.0, 0.0);
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
    }
}

/// Map a normalised position within the monitor to the absolute axis range,
/// which the compositor spreads across the whole layout bounding box.
pub fn map_abs(t: &Target, x: f32, y: f32) -> (i32, i32) {
    let x = x.clamp(0.0, 1.0);
    let y = y.clamp(0.0, 1.0);
    // Pixel within the layout; stay inside the monitor (last pixel inclusive).
    let px = (t.mon_x - t.layout_x) as f64 + (x as f64) * (t.mon_w.saturating_sub(1)) as f64 + 0.5;
    let py = (t.mon_y - t.layout_y) as f64 + (y as f64) * (t.mon_h.saturating_sub(1)) as f64 + 0.5;
    let ax = (px / t.layout_w.max(1) as f64 * ABS_MAX as f64).round() as i32;
    let ay = (py / t.layout_h.max(1) as f64 * ABS_MAX as f64).round() as i32;
    (ax.clamp(0, ABS_MAX), ay.clamp(0, ABS_MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_into_monitor_of_layout() {
        // 2x2 grid of 1920x1080, streaming the bottom-right monitor.
        let t = Target { layout_x: 0, layout_y: -1080, layout_w: 3840, layout_h: 2160, mon_x: 1920, mon_y: 0, mon_w: 1920, mon_h: 1080 };
        let (ax, ay) = map_abs(&t, 0.0, 0.0);
        assert!((ax - 32776).abs() < 20, "{ax}");
        assert!((ay - 32776).abs() < 20, "{ay}");
        let (ax, ay) = map_abs(&t, 1.0, 1.0);
        assert!(ax > 65500 && ay > 65500);
    }
}
