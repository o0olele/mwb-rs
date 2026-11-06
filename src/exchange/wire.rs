use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use snow::TransportState;

pub const CHUNK: usize = 32 * 1024;
pub const TEXT_LIMIT: u64 = 16 * 1024 * 1024;
pub const IMAGE_LIMIT: u64 = 128 * 1024 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClipboardVersion {
    pub clock: u64,
    pub origin: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Kind {
    File { name: String },
    Text,
    Image { width: u32, height: u32 },
    FileList,
    ClearFiles,
}
impl Kind {
    pub fn validate(&self, size: u64) -> Result<()> {
        match self {
            Self::FileList => {
                ensure!(size <= 128 * 1024, "文件列表过大");
                Ok(())
            }
            Self::ClearFiles => {
                ensure!(size == 0, "文件剪贴板清理消息无效");
                Ok(())
            }
            Self::File { name } => super::files::validate_name(name),
            Self::Text => {
                ensure!(size <= TEXT_LIMIT, "剪贴板文本超过 16 MiB");
                Ok(())
            }
            Self::Image { width, height } => {
                ensure!(
                    *width > 0
                        && *height > 0
                        && size <= IMAGE_LIMIT
                        && u64::from(*width)
                            .checked_mul(u64::from(*height))
                            .and_then(|n| n.checked_mul(4))
                            == Some(size),
                    "剪贴板图像尺寸无效或超过 128 MiB"
                );
                Ok(())
            }
        }
    }
    pub fn is_file(&self) -> bool {
        matches!(self, Self::File { .. })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Event {
    Pull {
        id: String,
        clipboard: String,
        index: u32,
    },
    Join {
        ticket: String,
    },
    Ping,
    Clock {
        clock: u64,
    },
    Offer {
        id: String,
        kind: Kind,
        size: u64,
        version: Option<ClipboardVersion>,
    },
    Accept {
        id: String,
    },
    Chunk {
        id: String,
        offset: u64,
    },
    Finish {
        id: String,
        digest: String,
    },
    Done {
        id: String,
    },
    Cancel {
        id: String,
        reason: String,
    },
}
impl Event {
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Pull { id, .. } => Some(id),
            Self::Offer { id, .. }
            | Self::Accept { id }
            | Self::Chunk { id, .. }
            | Self::Finish { id, .. }
            | Self::Done { id }
            | Self::Cancel { id, .. } => Some(id),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Header {
    pub source: String,
    pub target: String,
    pub event: Event,
}
#[derive(Clone, Debug)]
pub struct Packet {
    pub header: Header,
    pub data: Vec<u8>,
}
impl Packet {
    pub fn new(source: &str, target: &str, event: Event) -> Self {
        Self {
            header: Header {
                source: source.into(),
                target: target.into(),
                event,
            },
            data: Vec::new(),
        }
    }
}

pub fn encode(noise: &mut TransportState, packet: &Packet) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(&packet.header)?;
    ensure!(
        json.len() <= 4096 && packet.data.len() <= CHUNK,
        "数据帧过大"
    );
    let mut plain = Vec::with_capacity(2 + json.len() + packet.data.len());
    plain.extend_from_slice(&(json.len() as u16).to_be_bytes());
    plain.extend(json);
    plain.extend_from_slice(&packet.data);
    let mut cipher = vec![0; plain.len() + 16];
    let n = noise.write_message(&plain, &mut cipher)?;
    let mut frame = Vec::with_capacity(n + 2);
    frame.extend_from_slice(&(n as u16).to_be_bytes());
    frame.extend_from_slice(&cipher[..n]);
    Ok(frame)
}
pub fn decode(noise: &mut TransportState, bytes: &mut Vec<u8>) -> Result<Option<Packet>> {
    if bytes.len() < 2 {
        return Ok(None);
    }
    let size = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
    ensure!((18..=CHUNK + 4096 + 18).contains(&size), "数据帧长度无效");
    if bytes.len() < size + 2 {
        return Ok(None);
    }
    let mut plain = vec![0; size];
    let n = noise.read_message(&bytes[2..2 + size], &mut plain)?;
    ensure!(n >= 2, "数据帧缺少头部");
    let json = u16::from_be_bytes([plain[0], plain[1]]) as usize;
    ensure!(json <= 4096 && json + 2 <= n, "数据头长度无效");
    let header: Header = serde_json::from_slice(&plain[2..2 + json])?;
    ensure!(
        header.source.len() <= 32 && header.target.len() <= 32,
        "数据路由无效"
    );
    let data = plain[2 + json..n].to_vec();
    ensure!(
        data.len() <= CHUNK && (data.is_empty() || matches!(header.event, Event::Chunk { .. })),
        "数据块无效"
    );
    if let Some(id) = header.event.id() {
        ensure!(valid_id(id), "传输标识无效");
    }
    bytes.drain(..size + 2);
    Ok(Some(Packet { header, data }))
}
pub fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}
pub fn new_id() -> String {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).expect("Windows secure random source unavailable");
    hex::encode(bytes)
}
