use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
pub const MAX_TEXT_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Message {
    Hello {
        version: u32,
        name: String,
    },
    Ping,
    Enter {
        from_left: bool,
    },
    Leave,
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
