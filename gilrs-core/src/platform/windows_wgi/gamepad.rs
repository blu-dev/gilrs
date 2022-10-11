// Copyright 2016-2018 Mateusz Sieczko and other GilRs Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use super::FfDevice;
use crate::{utils, AxisInfo, Event, EventType, PlatformError, PowerInfo};

#[cfg(feature = "serde-serialize")]
use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, SystemTime};
use std::{thread, u32};
use windows::Foundation::EventHandler;
use windows::Gaming::Input::RawGameController;
use windows::Gaming::Input::{GameControllerSwitchPosition, Gamepad as WgiGamepad};

const SDL_HARDWARE_BUS_USB: u32 = 0x03;
const SDL_HARDWARE_BUS_BLUETOOTH: u32 = 0x05;

use uuid::Uuid;
use windows::core::HSTRING;
use windows::Devices::Power::BatteryReport;
use windows::System::Power::BatteryStatus;

/// This is similar to `gilrs_core::Event` but has a raw_game_controller that still needs to be
/// converted to a gilrs gamepad id.
struct WgiEvent {
    raw_game_controller: RawGameController,
    event: EventType,
    pub time: SystemTime,
}

// Chosen by dice roll ;)
const EVENT_THREAD_SLEEP_TIME: u64 = 10;

impl WgiEvent {
    fn new(raw_game_controller: RawGameController, event: EventType) -> Self {
        let time = utils::time_now();
        WgiEvent {
            raw_game_controller,
            event,
            time,
        }
    }
}

#[derive(Debug)]
pub struct Gilrs {
    gamepads: Vec<Gamepad>,
    rx: Receiver<WgiEvent>,
}

#[derive(Debug, Clone)]
struct GamePadReading {
    axes: Vec<f64>,
    buttons: Vec<bool>,
    switches: Vec<GameControllerSwitchPosition>,
    time: u64,
}

impl GamePadReading {
    fn new(raw_game_controller: &RawGameController) -> windows::core::Result<Self> {
        let axis_count = raw_game_controller.AxisCount()? as usize;
        let button_count = raw_game_controller.ButtonCount()? as usize;
        let switch_count = raw_game_controller.SwitchCount()? as usize;
        let mut new = Self {
            axes: vec![0.0; axis_count],
            buttons: vec![false; button_count],
            switches: vec![GameControllerSwitchPosition::default(); switch_count],
            time: 0,
        };
        new.time = raw_game_controller.GetCurrentReading(
            &mut new.buttons,
            &mut new.switches,
            &mut new.axes,
        )?;
        Ok(new)
    }

    fn update(&mut self, raw_game_controller: &RawGameController) -> windows::core::Result<()> {
        self.time = raw_game_controller.GetCurrentReading(
            &mut self.buttons,
            &mut self.switches,
            &mut self.axes,
        )?;
        Ok(())
    }

    /// Create a list of event types that describe the differences from this reading to the
    /// provided new reading.
    fn events_from_differences(&self, new_reading: &Self) -> Vec<EventType> {
        let mut changed = Vec::new();

        // Axis changes
        for index in 0..new_reading.axes.len() {
            if self.axes.get(index) != new_reading.axes.get(index) {
                let value = (((new_reading.axes[index] - 0.5) * 2.0) * u16::MAX as f64) as i32;
                let event = EventType::AxisValueChanged(
                    value,
                    crate::EvCode(EvCode {
                        kind: EvCodeKind::Axis,
                        index: index as u32,
                    }),
                );
                changed.push(event);
            }
        }
        for index in 0..new_reading.buttons.len() {
            if self.buttons.get(index) != new_reading.buttons.get(index) {
                let event = match new_reading.buttons[index] {
                    true => EventType::ButtonPressed(crate::EvCode(EvCode {
                        kind: EvCodeKind::Button,
                        index: index as u32,
                    })),
                    false => EventType::ButtonReleased(crate::EvCode(EvCode {
                        kind: EvCodeKind::Button,
                        index: index as u32,
                    })),
                };
                changed.push(event);
            }
        }
        // Todo: Decide if this should be treated as a button or axis
        // for index in 0..new_reading.switches.len() {
        //     if self.switches.get(index) != new_reading.switches.get(index) {
        //
        //     }
        // }
        changed
    }
}

impl Gilrs {
    pub(crate) fn new() -> Result<Self, PlatformError> {
        let gamepads: Vec<_> = RawGameController::RawGameControllers()
            .map_err(|e| PlatformError::Other(Box::new(e)))?
            .into_iter()
            .enumerate()
            .map(|(i, controller)| Gamepad::new(i as u32, controller))
            .collect();

        let (tx, rx) = mpsc::channel();
        Self::spawn_thread(tx);
        Ok(Gilrs { gamepads, rx })
    }

    fn spawn_thread(tx: Sender<WgiEvent>) {
        let added_tx = tx.clone();
        let added_handler: EventHandler<RawGameController> =
            EventHandler::new(move |_, g: &Option<RawGameController>| {
                if let Some(g) = g {
                    added_tx
                        .send(WgiEvent::new(g.clone(), EventType::Connected))
                        .expect("should be able to send to main thread");
                }
                Ok(())
            });
        RawGameController::RawGameControllerAdded(&added_handler).unwrap();

        let removed_tx = tx.clone();
        let removed_handler: EventHandler<RawGameController> =
            EventHandler::new(move |_, g: &Option<RawGameController>| {
                if let Some(g) = g {
                    removed_tx
                        .send(WgiEvent::new(g.clone(), EventType::Disconnected))
                        .expect("should be able to send to main thread");
                }
                Ok(())
            });
        RawGameController::RawGameControllerRemoved(&removed_handler).unwrap();

        thread::spawn(move || {
            // To avoid allocating every update, store old and new readings for every controller
            // and swap their memory
            let mut readings: Vec<(GamePadReading, GamePadReading)> = Vec::new();
            loop {
                let controllers: Vec<RawGameController> = RawGameController::RawGameControllers()
                    .into_iter()
                    .flatten()
                    .collect();
                for (index, controller) in controllers.iter().enumerate() {
                    if readings.get(index).is_none() {
                        let reading = GamePadReading::new(controller).unwrap();
                        readings.push((reading.clone(), reading));
                    }
                    let (old_reading, new_reading) = &mut readings[index];
                    std::mem::swap(old_reading, new_reading);
                    new_reading.update(controller).unwrap();
                    {
                        // skip if this is the same reading as the last one.
                        if old_reading.time == new_reading.time {
                            continue;
                        }

                        for event_type in old_reading.events_from_differences(new_reading) {
                            tx.send(WgiEvent::new(controller.clone(), event_type))
                                .unwrap();
                        }
                    };
                }
                thread::sleep(Duration::from_millis(EVENT_THREAD_SLEEP_TIME));
            }
        });
    }

    pub(crate) fn next_event(&mut self) -> Option<Event> {
        self.rx.try_recv().ok().map(|wgi_event: WgiEvent| {
            // Find the index of the gamepad in our vec or insert it
            let id = self
                .gamepads
                .iter()
                .position(
                    |gamepad| match wgi_event.raw_game_controller.NonRoamableId() {
                        Ok(id) => id == gamepad.non_roamable_id,
                        _ => false,
                    },
                )
                .unwrap_or_else(|| {
                    self.gamepads.push(Gamepad::new(
                        self.gamepads.len() as u32,
                        wgi_event.raw_game_controller,
                    ));
                    self.gamepads.len() - 1
                });

            match wgi_event.event {
                EventType::Connected => self.gamepads[id].is_connected = true,
                EventType::Disconnected => self.gamepads[id].is_connected = false,
                _ => (),
            }
            Event {
                id,
                event: wgi_event.event,
                time: wgi_event.time,
            }
        })
    }

    pub fn gamepad(&self, id: usize) -> Option<&Gamepad> {
        self.gamepads.get(id)
    }

    pub fn last_gamepad_hint(&self) -> usize {
        self.gamepads.len()
    }
}

#[derive(Debug)]
pub struct Gamepad {
    id: u32,
    name: String,
    uuid: Uuid,
    is_connected: bool,
    /// This is the generic controller handle without any mappings
    /// https://learn.microsoft.com/en-us/uwp/api/windows.gaming.input.rawgamecontroller
    raw_game_controller: RawGameController,
    /// An ID for this device that will survive disconnects and restarts.
    /// [NonRoamableIds](https://learn.microsoft.com/en-us/uwp/api/windows.gaming.input.rawgamecontroller.nonroamableid)
    ///
    /// Changes if plugged into a different port and is not the same between different applications
    /// or PCs.
    non_roamable_id: HSTRING,
    /// If the controller has a [Gamepad](https://learn.microsoft.com/en-us/uwp/api/windows.gaming.input.gamepad?view=winrt-22621)
    /// mapping, this is used to access the mapped values.
    wgi_gamepad: Option<WgiGamepad>,
    axes: Vec<EvCode>,
    buttons: Vec<EvCode>,
}

impl Gamepad {
    fn new(id: u32, raw_game_controller: RawGameController) -> Gamepad {
        let is_connected = true;

        let non_roamable_id = raw_game_controller.NonRoamableId().unwrap();

        // See if we can cast this to a windows definition of a gamepad
        let wgi_gamepad = WgiGamepad::FromGameController(&raw_game_controller).ok();
        let name = match raw_game_controller.DisplayName() {
            Ok(hstring) => hstring.to_string_lossy(),
            Err(_) => "unknown".to_string(),
        };

        // If it's wireless, use the Bluetooth bustype to match SDL
        // https://github.com/libsdl-org/SDL/blob/294ccba0a23b37fffef62189423444f93732e565/src/joystick/windows/SDL_windows_gaming_input.c#L335-L338
        let bustype = match raw_game_controller.IsWireless() {
            Ok(true) => SDL_HARDWARE_BUS_BLUETOOTH,
            _ => SDL_HARDWARE_BUS_USB,
        }
        .to_be();

        let vendor_id = raw_game_controller.HardwareVendorId().unwrap_or(0).to_be();
        let product_id = raw_game_controller.HardwareProductId().unwrap_or(0).to_be();
        let version = 0;
        let uuid = Uuid::from_fields(
            bustype,
            vendor_id,
            0,
            &[
                (product_id >> 8) as u8,
                product_id as u8,
                0,
                0,
                (version >> 8) as u8,
                version as u8,
                0,
                0,
            ],
        );

        let mut gamepad = Gamepad {
            id,
            name,
            uuid,
            is_connected,
            raw_game_controller,
            non_roamable_id,
            wgi_gamepad,
            axes: Vec::new(),
            buttons: Vec::new(),
        };

        gamepad.collect_axes_and_buttons();

        gamepad
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn uuid(&self) -> Uuid {
        self.uuid
    }

    pub fn is_connected(&self) -> bool {
        self.is_connected
    }

    pub fn power_info(&self) -> PowerInfo {
        self.power_info_err().unwrap_or(PowerInfo::Unknown)
    }

    /// Using this function so we can easily map errors to unknown
    fn power_info_err(&self) -> windows::core::Result<PowerInfo> {
        if !self.raw_game_controller.IsWireless()? {
            return Ok(PowerInfo::Wired);
        }
        let report: BatteryReport = self.raw_game_controller.TryGetBatteryReport()?;
        let status: BatteryStatus = report.Status()?;

        let power_info = match status {
            BatteryStatus::Discharging | BatteryStatus::Charging => {
                let full = report.FullChargeCapacityInMilliwattHours()?.GetInt32()? as f32;
                let remaining = report.RemainingCapacityInMilliwattHours()?.GetInt32()? as f32;
                let percent: u8 = ((remaining / full) * 100.0) as u8;
                match status {
                    _ if percent == 100 => PowerInfo::Charged,
                    BatteryStatus::Discharging => PowerInfo::Discharging(percent),
                    BatteryStatus::Charging => PowerInfo::Charging(percent),
                    _ => unreachable!(),
                }
            }
            BatteryStatus::NotPresent => PowerInfo::Wired,
            BatteryStatus::Idle => PowerInfo::Charged,
            BatteryStatus(_) => PowerInfo::Unknown,
        };
        Ok(power_info)
    }

    pub fn is_ff_supported(&self) -> bool {
        self.wgi_gamepad.is_some()
            && self
                .raw_game_controller
                .ForceFeedbackMotors()
                .ok()
                .map(|motors| motors.First())
                .is_some()
    }

    pub fn ff_device(&self) -> Option<FfDevice> {
        Some(FfDevice::new(self.id, self.wgi_gamepad.clone()))
    }

    pub fn buttons(&self) -> &[EvCode] {
        &self.buttons
    }

    pub fn axes(&self) -> &[EvCode] {
        &self.axes
    }

    pub(crate) fn axis_info(&self, _nec: EvCode) -> Option<&AxisInfo> {
        Some(&AxisInfo {
            min: i16::MIN as i32,
            max: i16::MAX as i32,
            deadzone: None,
        })
    }

    fn collect_axes_and_buttons(&mut self) {
        self.buttons = (0..(self.raw_game_controller.ButtonCount().unwrap() as u32))
            .map(|index| EvCode {
                kind: EvCodeKind::Button,
                index,
            })
            .collect();
        self.axes = (0..(self.raw_game_controller.AxisCount().unwrap() as u32))
            .map(|index| EvCode {
                kind: EvCodeKind::Axis,
                index,
            })
            .collect();
    }
}

#[cfg_attr(feature = "serde-serialize", derive(Serialize, Deserialize))]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum EvCodeKind {
    Button = 0,
    Axis,
    Switch,
}

impl Display for EvCodeKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            EvCodeKind::Button => "Button",
            EvCodeKind::Axis => "Axis",
            EvCodeKind::Switch => "Switch",
        }
        .fmt(f)
    }
}

#[cfg_attr(feature = "serde-serialize", derive(Serialize, Deserialize))]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct EvCode {
    kind: EvCodeKind,
    index: u32,
}

impl EvCode {
    pub fn into_u32(self) -> u32 {
        (self.kind as u32) << 16 | self.index
    }
}

impl Display for EvCode {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        write!(f, "{}({})", self.kind, self.index)
    }
}

pub mod native_ev_codes {
    use super::{EvCode, EvCodeKind};

    pub const AXIS_LSTICKY: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 0,
    };
    pub const AXIS_LSTICKX: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 1,
    };
    pub const AXIS_RSTICKY: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 2,
    };
    pub const AXIS_RSTICKX: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 3,
    };
    pub const AXIS_LT2: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 4,
    };
    pub const AXIS_RT2: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 5,
    };
    pub const AXIS_DPADX: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 6,
    };
    pub const AXIS_DPADY: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 7,
    };
    pub const AXIS_RT: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 8,
    };
    pub const AXIS_LT: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 9,
    };
    pub const AXIS_LEFTZ: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 10,
    };
    pub const AXIS_RIGHTZ: EvCode = EvCode {
        kind: EvCodeKind::Axis,
        index: 11,
    };

    pub const BTN_SOUTH: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 0,
    };
    pub const BTN_EAST: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 1,
    };
    pub const BTN_WEST: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 2,
    };
    pub const BTN_NORTH: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 3,
    };
    pub const BTN_LT: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 4,
    };
    pub const BTN_RT: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 5,
    };
    pub const BTN_SELECT: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 6,
    };
    pub const BTN_START: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 7,
    };
    pub const BTN_LTHUMB: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 8,
    };
    pub const BTN_RTHUMB: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 9,
    };

    pub const BTN_DPAD_UP: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 10,
    };
    pub const BTN_DPAD_RIGHT: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 11,
    };
    pub const BTN_DPAD_DOWN: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 12,
    };
    pub const BTN_DPAD_LEFT: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 13,
    };

    pub const BTN_MODE: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 14,
    };
    pub const BTN_C: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 15,
    };
    pub const BTN_Z: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 16,
    };

    pub const BTN_LT2: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 17,
    };
    pub const BTN_RT2: EvCode = EvCode {
        kind: EvCodeKind::Button,
        index: 18,
    };
}
