use crate::layout::{DeviceInfo, Layout, Placement, Point};
use serde::{Deserialize, Serialize};
pub const VERSION: u32 = 3;
pub const MAX_TEXT_BYTES: usize = 32 * 1024;
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum MouseMode {
    #[default]
    ScreenRelative,
    Relative,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum InputEvent {
    Absolute {
        x: i32,
        y: i32,
    },
    Relative {
        dx: i32,
        dy: i32,
    },
    Button {
        button: u8,
        down: bool,
    },
    Wheel {
        delta: i32,
        horizontal: bool,
    },
    Key {
        vk: u16,
        scan: u16,
        extended: bool,
        down: bool,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Control {
    pub controller: String,
    pub target: String,
    pub epoch: u64,
    pub point: Point,
    pub mode: MouseMode,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Message {
    Hello {
        version: u32,
        #[serde(default = "missing_device")]
        device: DeviceInfo,
    },
    Ping,
    Topology {
        revision: u64,
        layout: Layout,
    },
    Arrange {
        revision: u64,
        positions: Vec<Placement>,
    },
    TakeControl {
        target: String,
        point: Point,
        mode: MouseMode,
    },
    Control(Option<Control>),
    Release {
        epoch: u64,
    },
    Input {
        epoch: u64,
        event: InputEvent,
    },
    Cursor {
        epoch: u64,
        point: Point,
    },
    SendClipboard {
        target: String,
        text: String,
    },
    DeliverClipboard {
        source: String,
        text: String,
    },
    Notice(String),
}

// v0.2 Hello only contains version/name. Parse enough to report a clear
// version error before validating v0.3 screen metadata.
fn missing_device() -> DeviceInfo {
    DeviceInfo {
        id: String::new(),
        name: String::new(),
        bounds: crate::layout::Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        },
        screens: Vec::new(),
    }
}
