use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 2;
pub const MAX_TEXT_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum MouseMode {
    #[default]
    ScreenRelative,
    Relative,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Message {
    Hello {
        version: u32,
        name: String,
    },
    Ping,
    Enter {
        from_left: bool,
        x: u16,
        y: u16,
        mode: MouseMode,
    },
    Leave,
    Return {
        y: u16,
    },
    MoveAbsolute {
        x: i32,
        y: i32,
    },
    Move {
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
    Clipboard(String),
}
