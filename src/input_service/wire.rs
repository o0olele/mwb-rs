use crate::{
    layout::Point,
    protocol::{InputEvent, MouseMode},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub(super) const VERSION: u32 = 1;
pub(super) const MAX_MESSAGE: usize = 4096;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    pub version: u32,
    pub sequence: u64,
    pub command: Command,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) enum Command {
    Hello,
    Enter { point: Point, mode: MouseMode },
    Input { event: InputEvent, keyboard: bool },
    Position,
    Release,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Response {
    pub version: u32,
    pub sequence: u64,
    pub result: Reply,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) enum Reply {
    Ok,
    Position(Point),
    TakenOver,
    Error(String),
}

pub(super) fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_MESSAGE, "输入服务消息过长");
    Ok(bytes)
}

pub(super) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_MESSAGE,
        "输入服务消息长度无效"
    );
    Ok(serde_json::from_slice(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversized_unknown_and_nonfinite_requests() {
        assert!(decode::<Request>(&vec![b' '; MAX_MESSAGE + 1]).is_err());
        assert!(
            decode::<Request>(br#"{"version":1,"sequence":1,"command":"Hello","extra":1}"#)
                .is_err()
        );
        assert!(decode::<Request>(br#"{"version":1,"sequence":1,"command":{"Enter":{"point":{"x":null,"y":0},"mode":"Relative"}}}"#).is_err());
        assert!(decode::<Request>(br#"{"version":1,"sequence":1,"command":"Execute"}"#).is_err());
    }
}
