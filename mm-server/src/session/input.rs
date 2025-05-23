// Copyright 2024 Colin Marc <hi@colinmarc.com>
//
// SPDX-License-Identifier: BUSL-1.1

use std::{
    ffi::{OsStr, OsString},
    path::Path,
    sync::Arc,
};

use fuser as fuse;
use parking_lot::Mutex;
use southpaw::{
    sys::{EV_ABS, EV_KEY},
    AbsAxis, AbsInfo, InputEvent, KeyCode,
};
use tracing::{debug, error};

use crate::container::Container;

mod udevfs;
use udevfs::*;

use super::compositor::ButtonState;

/// A simulated gamepad layout.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum GamepadLayout {
    #[default]
    GenericDualStick,
}

/// Manages input devices (mostly gamepads) n a container using a variety of
/// well-intentioned but horrible hacks.
pub struct InputDeviceManager {
    southpaw: southpaw::DeviceTree,
    state: Arc<Mutex<InputManagerState>>,
}

struct DeviceState {
    id: u64,
    counter: u16,
    devname: OsString,   // inputX
    eventname: OsString, // eventX
}

#[derive(Default)]
struct InputManagerState {
    counter: u16,
    devices: Vec<DeviceState>,
}

impl InputManagerState {
    fn device_by_id(&self, id: u64) -> Option<&DeviceState> {
        self.devices.iter().find(|dev| dev.id == id)
    }

    fn device_by_devname(&self, name: impl AsRef<OsStr>) -> Option<&DeviceState> {
        self.devices.iter().find(|dev| dev.devname == name.as_ref())
    }

    fn device_by_eventname(&self, name: impl AsRef<OsStr>) -> Option<&DeviceState> {
        self.devices
            .iter()
            .find(|dev| dev.eventname == name.as_ref())
    }
}

/// A handle for a plugged gamepad.
pub struct GamepadHandle {
    device: southpaw::Device,
    ev_buffer: Vec<southpaw::InputEvent>,
    pub permanent: bool,
}
impl GamepadHandle {
    pub(crate) fn axis(&mut self, axis_code: u32, value: f64) {
        let value = value.clamp(-1.0, 1.0) * 128.0 + 128.0;
        self.ev_buffer.push(InputEvent::new(
            EV_ABS,
            axis_code as u16,
            value.floor() as i32,
        ));
    }

    pub(crate) fn trigger(&mut self, trigger_code: u32, value: f64) {
        let value = value.clamp(0.0, 1.0) * 256.0;
        self.ev_buffer.push(InputEvent::new(
            EV_ABS,
            trigger_code as u16,
            value.floor() as i32,
        ))
    }

    pub(crate) fn input(&mut self, button_code: u32, state: ButtonState) {
        let value = match state {
            super::compositor::ButtonState::Pressed => 1,
            super::compositor::ButtonState::Released => 0,
        };

        // The DualSense sends D-pad buttons as ABS_HAT0{X,Y}.
        let key_code = southpaw::KeyCode::try_from(button_code as u16);
        if let Some((axis, direction)) = match key_code {
            Ok(KeyCode::BtnDpadUp) => Some((AbsAxis::HAT0Y, -1)),
            Ok(KeyCode::BtnDpadDown) => Some((AbsAxis::HAT0Y, 1)),
            Ok(KeyCode::BtnDpadLeft) => Some((AbsAxis::HAT0X, -1)),
            Ok(KeyCode::BtnDpadRight) => Some((AbsAxis::HAT0X, 1)),
            _ => None,
        } {
            // Simulate a press and release, each in a frame.
            self.ev_buffer
                .push(InputEvent::new(EV_ABS, axis, value * direction));
            return;
        }

        self.ev_buffer
            .push(InputEvent::new(EV_KEY, button_code as u16, value));
    }

    pub(crate) fn frame(&mut self) {
        if let Err(err) = self.device.publish_packet(&self.ev_buffer) {
            error!(?err, "failed to publish event packet to device");
        }

        self.ev_buffer.clear();
    }
}

impl InputDeviceManager {
    pub fn new(container: &mut Container) -> anyhow::Result<Self> {
        let state = Arc::new(Mutex::new(InputManagerState::default()));

        let udevfs_path = container.intern_run_path().join(".udevfs");
        let southpaw_path = container.intern_run_path().join(".southpaw");

        let udevfs = UdevFs::new(state.clone());
        let udevfs_path_clone = udevfs_path.clone();

        let southpaw = southpaw::DeviceTree::new();
        let southpaw_clone = southpaw.clone();
        let southpaw_path_clone = southpaw_path.clone();

        container.setup_hook(move |c| {
            let mode = 0o755 | rustix::fs::FileType::Directory.as_raw_mode();

            let device_fd = c.fuse_mount(udevfs_path_clone, "udevfs", mode)?;
            let mut session = fuse::Session::from_fd(udevfs, device_fd, fuse::SessionACL::Owner);
            std::thread::spawn(move || session.run());

            let device_fd = c.fuse_mount(southpaw_path_clone, "southpaw", mode)?;
            southpaw_clone.wrap_fd(device_fd);

            // Headless servers won't have /sys/devices/virtual/input, and we
            // can't mkdir the mount point, because it's sysfs.
            if !Path::new("/sys/devices/virtual/input").exists() {
                c.fs_mount(
                    "/sys/devices/virtual",
                    "tmpfs",
                    rustix::mount::MountAttrFlags::empty(),
                    [(c"mode", c"0777")],
                )?;
            }

            Ok(())
        });

        container.internal_bind_mount(
            udevfs_path.join("sys/devices/virtual/input"),
            "/sys/devices/virtual/input",
        );
        container.internal_bind_mount(udevfs_path.join("sys/class/input"), "/sys/class/input");
        container.internal_bind_mount(udevfs_path.join("run/udev"), "/run/udev");
        container.internal_bind_mount(southpaw_path, "/dev/input");

        // Shadow /sys/class/hidraw.
        if Path::new("/sys/class/hidraw").exists() {
            container
                .internal_bind_mount(udevfs_path.join("sys/class/hidraw"), "/sys/class/hidraw");
        }

        // Without this, udev refuses to accept our FUSE filesystem.
        container.set_env("SYSTEMD_DEVICE_VERIFY_SYSFS", "false");

        Ok(Self { state, southpaw })
    }

    pub fn plug_gamepad(
        &mut self,
        id: u64,
        _layout: GamepadLayout,
        permanent: bool,
    ) -> anyhow::Result<GamepadHandle> {
        debug!(id, ?_layout, "gamepad plugged");

        let mut guard = self.state.lock();

        guard.counter += 1;
        let counter = guard.counter;
        let devname = OsStr::new(&format!("input{counter}")).to_owned();
        let eventname = OsStr::new(&format!("event{counter}")).to_owned();

        let xy_absinfo = AbsInfo {
            value: 128,
            minimum: 0,
            maximum: 255,
            ..Default::default()
        };

        let trigger_absinfo = AbsInfo {
            value: 0,
            minimum: 0,
            maximum: 255,
            ..Default::default()
        };

        let dpad_absinfo = AbsInfo {
            value: 0,
            minimum: -1,
            maximum: 1,
            ..Default::default()
        };

        let device = southpaw::Device::builder()
            .name("Magic Mirror Emulated Controller")
            .id(southpaw::BusType::Usb, 1234, 4567, 111)
            .supported_key_codes([
                KeyCode::BtnSouth,
                KeyCode::BtnNorth,
                KeyCode::BtnEast,
                KeyCode::BtnWest,
                KeyCode::BtnTl,
                KeyCode::BtnTr,
                KeyCode::BtnTl2,
                KeyCode::BtnTr2,
                KeyCode::BtnSelect,
                KeyCode::BtnStart,
                KeyCode::BtnMode,
                KeyCode::BtnThumbl,
                KeyCode::BtnThumbr,
            ])
            .supported_absolute_axis(AbsAxis::X, xy_absinfo)
            .supported_absolute_axis(AbsAxis::Y, xy_absinfo)
            .supported_absolute_axis(AbsAxis::RX, xy_absinfo)
            .supported_absolute_axis(AbsAxis::RY, xy_absinfo)
            .supported_absolute_axis(AbsAxis::Z, trigger_absinfo)
            .supported_absolute_axis(AbsAxis::RZ, trigger_absinfo)
            .supported_absolute_axis(AbsAxis::HAT0X, dpad_absinfo)
            .supported_absolute_axis(AbsAxis::HAT0Y, dpad_absinfo)
            .add_to_tree(&mut self.southpaw, &eventname)?;

        guard.devices.push(DeviceState {
            id,
            counter,
            devname,
            eventname,
        });

        Ok(GamepadHandle {
            device,
            ev_buffer: Vec::new(),
            permanent,
        })
    }
}

#[cfg(test)]
mod test {
    use std::{fs::File, io::Read as _};

    use rustix::pipe::{pipe_with, PipeFlags};

    use super::{GamepadLayout, InputDeviceManager};
    use crate::{config::HomeIsolationMode, container::Container};

    fn run_in_container_with_gamepads<T>(cmd: impl AsRef<[T]>) -> anyhow::Result<String>
    where
        T: AsRef<str>,
    {
        let command = cmd
            .as_ref()
            .iter()
            .map(|s| s.as_ref().to_owned().into())
            .collect();

        let mut container = Container::new(command, HomeIsolationMode::Tmpfs)?;
        let (pipe_rx, pipe_tx) = pipe_with(PipeFlags::CLOEXEC)?;
        container.set_stdout(pipe_tx)?;

        container.set_env("SYSTEMD_LOG_LEVEL", "debug");
        let mut input_manager = InputDeviceManager::new(&mut container)?;

        let mut child = container.spawn()?;

        let _ = input_manager.plug_gamepad(1234, GamepadLayout::GenericDualStick, false)?;
        let _ = input_manager.plug_gamepad(5678, GamepadLayout::GenericDualStick, false)?;
        let _ = child.wait();

        let mut buf = String::new();
        File::from(pipe_rx).read_to_string(&mut buf)?;

        Ok(buf)
    }

    #[test_log::test]
    fn list_devices_subsystem() -> anyhow::Result<()> {
        let output = run_in_container_with_gamepads([
            "udevadm",
            "--debug",
            "trigger",
            "--dry-run",
            "--verbose",
            "--subsystem-match",
            "input",
        ])?;

        let mut expected = String::new();
        for path in [
            "/sys/devices/virtual/input/input1",
            "/sys/devices/virtual/input/input1/event1",
            "/sys/devices/virtual/input/input2",
            "/sys/devices/virtual/input/input2/event2",
        ] {
            expected.push_str(path);
            expected.push('\n');
        }

        pretty_assertions::assert_eq!(output, expected);
        Ok(())
    }
}
