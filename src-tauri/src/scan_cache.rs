//! 扫描结果缓存：把最近一次多算法扫描发现的 DSH Home 落盘，启动秒读。
//!
//! 依据 UPGRADE-PLAN-v4 模块 D：用户反馈「扫出来就该记住，不要每次重扫」。
//! 设计要点：
//! - 缓存写在 %APPDATA%/com.dsh.vault/scan-cache.json（与 Tauri 标识一致，易找）；
//! - 每次快速/深度扫描成功后自动覆盖写；接管/断开/切换后由前端触发重扫刷新；
//! - 读取时校验版本与路径存在性：路径已消失的条目保留但由前端标灰，不静默丢；
//! - 写盘用「临时文件 + rename」保证崩溃时不会出现半截 JSON。

use crate::model::DiscoveredHome;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

pub const SCAN_CACHE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanCache {
    pub version: u32,
    pub last_scan_at: String,
    /// "quick" | "deep"
    pub last_scan_mode: String,
    pub homes: Vec<DiscoveredHome>,
}

/// 缓存文件位置：%APPDATA%/com.dsh.vault/scan-cache.json
fn cache_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("com.dsh.vault")
}

fn now_string() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// 保存扫描结果（覆盖写，原子落盘）。mode 传 "quick" 或 "deep"。
pub fn save(homes: &[DiscoveredHome], mode: &str) -> Result<(), String> {
    let dir = cache_path();
    fs::create_dir_all(&dir).map_err(|e| format!("创建缓存目录失败：{e}"))?;
    let cache = ScanCache {
        version: SCAN_CACHE_VERSION,
        last_scan_at: now_string(),
        last_scan_mode: mode.to_string(),
        homes: homes.to_vec(),
    };
    let json = serde_json::to_string_pretty(&cache).map_err(|e| format!("序列化缓存失败：{e}"))?;
    let final_path = dir.join("scan-cache.json");
    let tmp_path = dir.join("scan-cache.json.tmp");
    fs::write(&tmp_path, json).map_err(|e| format!("写缓存临时文件失败：{e}"))?;
    fs::rename(&tmp_path, &final_path).map_err(|e| format!("缓存落盘失败：{e}"))?;
    Ok(())
}

/// 读取缓存。版本不符或文件损坏返回 None（前端退化为现场扫描）。
pub fn load() -> Option<ScanCache> {
    let path = cache_path().join("scan-cache.json");
    let text = fs::read_to_string(path).ok()?;
    let cache: ScanCache = serde_json::from_str(&text).ok()?;
    if cache.version != SCAN_CACHE_VERSION {
        return None;
    }
    Some(cache)
}

/// 标注缓存中哪些 home 的路径当前仍存在（前端据此标灰已消失的环境）。
pub fn with_existence(cache: &ScanCache) -> Vec<(DiscoveredHome, bool)> {
    cache
        .homes
        .iter()
        .map(|h| {
            let exists = std::path::Path::new(&h.path).is_dir();
            (h.clone(), exists)
        })
        .collect()
}

/// 清除缓存（用户点「重新扫描」前 / 缓存损坏自愈时调用）。
pub fn clear() {
    let _ = fs::remove_file(cache_path().join("scan-cache.json"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HomeKind, SessionStats};

    fn fake_home(path: &str) -> DiscoveredHome {
        DiscoveredHome {
            id: "root-1".into(),
            kind: HomeKind::DshHome,
            label: "测试".into(),
            path: path.into(),
            variant: "特征识别".into(),
            dsh_version: None,
            sessions: SessionStats::default(),
            backup_file_count: 0,
            backup_size: 0,
            warnings: vec![],
        }
    }

    #[test]
    fn save_then_load_roundtrip() {
        let homes = vec![fake_home("C:\\nonexistent-dsh-home-xyz")];
        save(&homes, "quick").expect("保存应成功");
        let loaded = load().expect("应能读回缓存");
        assert_eq!(loaded.version, SCAN_CACHE_VERSION);
        assert_eq!(loaded.last_scan_mode, "quick");
        assert_eq!(loaded.homes.len(), 1);
        assert_eq!(loaded.homes[0].path, "C:\\nonexistent-dsh-home-xyz");
        // existence 标注：不存在的路径应为 false
        let marked = with_existence(&loaded);
        assert_eq!(marked.len(), 1);
        assert!(!marked[0].1, "不存在的路径应标注为不存在");
        clear();
        assert!(load().is_none(), "清除后应读不到缓存");
    }
}
