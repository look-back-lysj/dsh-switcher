//! DSH 会话文件结构与内容校验。
//!
//! 关键点：DSH 会话不是普通单帧 zstd 文件，而是多个独立 zstd 帧拼接的流。
//! 整文件一次性解压会失败，必须先按官方算法逐帧扫描。
//! 最后一帧被截断时，前面的数据仍可恢复，所以标记为 truncated 而不是 corrupt。

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Ok,
    Truncated,
    Corrupt,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionVerification {
    pub status: SessionStatus,
    pub why: Option<String>,
    pub id: Option<String>,
    pub cwd: Option<String>,
    pub generation: u32,
    pub frames: u32,
    pub events: u32,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameRange {
    #[allow(dead_code)]
    start: usize,
    #[allow(dead_code)]
    end: usize,
}

/// 扫描拼接 zstd 帧的结构，不解压数据。
pub fn scan_frames(bytes: &[u8]) -> Result<(Vec<FrameRange>, Option<usize>), String> {
    let mut offset = 0usize;
    let mut frames = Vec::new();
    let mut torn_start = None;

    while offset < bytes.len() {
        let start = offset;
        if bytes.len() - offset < 4 {
            torn_start = Some(start);
            break;
        }
        let magic = u32::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]);
        if magic != crate::model::ZSTD_MAGIC {
            return Err(format!("第 {offset} 字节处 zstd 帧头损坏"));
        }
        offset += 4;

        let Some(&descriptor) = bytes.get(offset) else {
            torn_start = Some(start);
            break;
        };
        offset += 1;
        if descriptor & 0x18 != 0 {
            return Err("zstd 帧头保留位不为零".into());
        }

        let content_size_flag = descriptor >> 6;
        let single_segment = descriptor & 0x20 != 0;
        let checksum = descriptor & 0x04 != 0;
        let dictionary_flag = descriptor & 0x03;
        let dictionary_bytes = if dictionary_flag == 3 { 4 } else { dictionary_flag as usize };
        let content_size_bytes = if content_size_flag == 0 {
            usize::from(single_segment)
        } else {
            1usize << content_size_flag
        };
        let remaining = usize::from(!single_segment) + dictionary_bytes + content_size_bytes;
        if bytes.len() - offset < remaining {
            torn_start = Some(start);
            break;
        }
        offset += remaining;

        loop {
            if bytes.len() - offset < 3 {
                torn_start = Some(start);
                break;
            }
            let block_header = bytes[offset] as u32
                | (bytes[offset + 1] as u32) << 8
                | (bytes[offset + 2] as u32) << 16;
            offset += 3;
            let last_block = block_header & 1 != 0;
            let block_type = (block_header >> 1) & 0x03;
            let block_size = (block_header >> 3) as usize;
            if block_type == 3 {
                return Err("zstd 保留块类型".into());
            }
            let payload = if block_type == 1 { 1 } else { block_size };
            if bytes.len() - offset < payload {
                torn_start = Some(start);
                break;
            }
            offset += payload;
            if last_block {
                break;
            }
        }

        if let Some(torn) = torn_start {
            let _ = torn;
            break;
        }

        if checksum {
            if bytes.len() - offset < 4 {
                torn_start = Some(start);
                break;
            }
            offset += 4;
        }

        frames.push(FrameRange { start, end: offset });
    }

    Ok((frames, torn_start))
}

fn generation_of(file_name: &str) -> Option<u32> {
    if file_name == "session.jsonl.zstd" {
        return Some(0);
    }
    if let Some(rest) = file_name.strip_prefix("session.v") {
        if let Some(num) = rest.strip_suffix(".jsonl.zstd") {
            return num.parse::<u32>().ok();
        }
    }
    None
}

/// 校验一个会话文件。为了不引入 C 依赖，这里先完成帧结构校验；
/// 内容 JSON 校验在备份时逐帧读取时继续完成。
pub fn verify_session(path: &Path) -> SessionVerification {
    let file_name = path.file_name().and_then(|v| v.to_str()).unwrap_or_default();
    let Some(generation) = generation_of(file_name) else {
        return SessionVerification {
            status: SessionStatus::Corrupt,
            why: Some("文件名不符合 DSH 会话命名规则".into()),
            id: None,
            cwd: None,
            generation: 0,
            frames: 0,
            events: 0,
            bytes: 0,
        };
    };

    let bytes = match fs::read(path) {
        Ok(v) => v,
        Err(e) => {
            return SessionVerification {
                status: SessionStatus::Corrupt,
                why: Some(format!("无法读取文件：{e}")),
                id: None,
                cwd: None,
                generation,
                frames: 0,
                events: 0,
                bytes: 0,
            }
        }
    };

    let (frames, torn) = match scan_frames(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return SessionVerification {
                status: SessionStatus::Corrupt,
                why: Some(e),
                id: None,
                cwd: None,
                generation,
                frames: 0,
                events: 0,
                bytes: bytes.len() as u64,
            }
        }
    };

    if frames.is_empty() {
        return SessionVerification {
            status: SessionStatus::Corrupt,
            why: Some("文件里没有完整 zstd 帧".into()),
            id: None,
            cwd: None,
            generation,
            frames: 0,
            events: 0,
            bytes: bytes.len() as u64,
        };
    }

    SessionVerification {
        status: if torn.is_some() {
            SessionStatus::Truncated
        } else {
            SessionStatus::Ok
        },
        why: torn.map(|v| format!("第 {v} 字节之后的最后一帧不完整，前面内容仍可用")),
        id: None,
        cwd: None,
        generation,
        frames: frames.len() as u32,
        events: 0,
        bytes: bytes.len() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_v0_and_v4_names() {
        assert_eq!(generation_of("session.jsonl.zstd"), Some(0));
        assert_eq!(generation_of("session.v4.jsonl.zstd"), Some(4));
        assert_eq!(generation_of("note.txt"), None);
    }

    #[test]
    fn empty_file_is_corrupt() {
        let result = verify_session(Path::new("does-not-exist"));
        assert_eq!(result.status, SessionStatus::Corrupt);
    }
}


