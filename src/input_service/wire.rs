use crate::{
    layout::Point,
    protocol::{InputEvent, MouseMode},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

// Version 2 requires a worker whose UIAccess token was verified before launch.
// Keep the well-known pipe name stable so an old installation gets a clear
// version-mismatch error instead of appearing to be absent.
pub(super) const VERSION: u32 = 2;
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

impl Response {
    pub fn validate(&self, sequence: u64) -> Result<()> {
        ensure!(
            self.version == VERSION,
            "锁屏输入服务版本不同（主程序 IPC {}，服务 IPC {}），请重新安装配套主程序和输入服务并重启服务",
            VERSION,
            self.version
        );
        ensure!(self.sequence == sequence, "锁屏输入服务响应序号不匹配");
        Ok(())
    }
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
    fn old_worker_is_rejected_with_upgrade_guidance_before_any_input() {
        let legacy: Response = decode(br#"{"version":1,"sequence":1,"result":"Ok"}"#).unwrap();
        let error = legacy.validate(1).unwrap_err().to_string();
        assert!(error.contains("重新安装"));
        assert!(error.contains("服务 IPC 1"));
        let current = Response {
            version: VERSION,
            sequence: 1,
            result: Reply::Ok,
        };
        current.validate(1).unwrap();
        assert!(current.validate(2).is_err());
    }

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
