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

/// 缓存文件并发写保护：测试并行跑、GUI 多处同时触发扫描时，防止
/// 「load→merge→save」窗口期被另一个线程的 save 插进来导致结果互相覆盖。
static CACHE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    // 测试隔离：单元测试用进程专属临时目录，绝不写真实 %APPDATA% 缓存（防污染用户界面）。
    // 之前测试直接写真缓存，导致 C:\m5-* 测试环境残留出现在用户扫描结果里。
    if cfg!(test) {
        return std::env::temp_dir().join(format!("dsh-vault-scan-cache-test-{}", std::process::id()));
    }
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("com.dsh.vault")
}

fn now_string() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// 增量合并保存：以旧缓存为基准，新扫到的更新/新增，旧有但本次未扫到的保留并标注。
/// 解决「后台浅扫覆盖深扫正确结果」：浅扫扫不到深路径时，不会把深扫发现的条目删掉。
/// 路径不存在的条目仍会在前端标灰（由 with_existence 负责）。
pub fn merge_save(new_homes: &[DiscoveredHome], mode: &str) -> Result<(), String> {
    let _guard = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut merged: Vec<DiscoveredHome> = new_homes.to_vec();
    if let Some(old) = load() {
        let new_paths: std::collections::HashSet<&str> = new_homes.iter().map(|h| h.path.as_str()).collect();
        for old_home in old.homes {
            if !new_paths.contains(old_home.path.as_str()) {
                let mut kept = old_home.clone();
                // P-15 修正：先复核路径是否还在，避免"目录完好却报可能已删除"的自相矛盾。
                let still_exists = std::path::Path::new(&old_home.path).exists();
                let note = if still_exists {
                    format!("本次{}扫描未覆盖到（路径仍在，深度扫描可确认）", mode)
                } else {
                    format!("上次由{}扫描发现的路径现已不存在，可能已移动或删除", old.last_scan_mode)
                };
                let tag = if still_exists { "未覆盖" } else { "已不存在" };
                if !kept.warnings.iter().any(|w| w.contains("未覆盖") || w.contains("已不存在") || w.contains("未重新发现")) {
                    kept.warnings.push(note);
                }
                let _ = tag;
                merged.push(kept);
            }
        }
    }
    save_locked(&merged, mode)
}

/// 保存扫描结果（覆盖写，原子落盘）。mode 传 "quick" 或 "deep"。
pub fn save(homes: &[DiscoveredHome], mode: &str) -> Result<(), String> {
    let _guard = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    save_locked(homes, mode)
}

/// 已持有 CACHE_LOCK 时的内部实现（merge_save 复用，避免同锁重入死锁）。
fn save_locked(homes: &[DiscoveredHome], mode: &str) -> Result<(), String> {
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
    fs::write(&tmp_path, &json).map_err(|e| format!("写缓存临时文件失败：{e}"))?;
    if fs::rename(&tmp_path, &final_path).is_err() {
        // Windows 上 rename 对已存在目标/杀软占用更挑剔，退化为 copy + remove
        fs::copy(&tmp_path, &final_path).map_err(|e| format!("缓存落盘失败：{e}"))?;
        let _ = fs::remove_file(&tmp_path);
    }
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
    let _guard = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ = fs::remove_file(cache_path().join("scan-cache.json"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HomeKind, SessionStats};

    fn fake_home(path: &str) -> DiscoveredHome {
        DiscoveredHome {
            id: format!("id-{}", path),
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
    fn merge_save_keeps_missing_entries() {
        // 并发安全：用本测试专属路径，不依赖全局 clear()，别的测试并发写不影响断言
        let pid = std::process::id();
        let deep = format!("C:\\m5-deep-{pid}");
        let shallow = format!("C:\\m5-shallow-{pid}");
        // 先存两个环境（模拟深扫发现）
        save(&[fake_home(&deep), fake_home(&shallow)], "deep").unwrap();
        // 浅扫只发现 shallow，deep 不应被删，而是保留并标注
        merge_save(&[fake_home(&shallow)], "quick").unwrap();
        // merge_save 结束后缓存必然已落盘（save_locked 内部完成），直接读
        let loaded = load().expect("merge 后应能读回缓存");
        let deep_only = loaded.homes.iter().find(|h| h.path == deep)
            .expect("浅扫不应删掉深扫发现的条目");
        assert!(deep_only.warnings.iter().any(|w| w.contains("已不存在") || w.contains("未覆盖")), "路径不存在应标注已不存在");
        // 再扫到 deep 时标注应被新结果覆盖（新结果无该 warning）
        merge_save(&[fake_home(&shallow), fake_home(&deep)], "quick").unwrap();
        let loaded2 = load().expect("再次 merge 后应能读回缓存");
        let deep_only2 = loaded2.homes.iter().find(|h| h.path == deep)
            .expect("deep 应仍在");
        assert!(!deep_only2.warnings.iter().any(|w| w.contains("已不存在") || w.contains("未覆盖")), "重新扫到后不应再标注");
    }

    #[test]
    fn save_then_load_roundtrip() {
        let pid = std::process::id();
        let path = format!("C:\\nonexistent-dsh-home-{pid}");
        save(&[fake_home(&path)], "quick").expect("保存应成功");
        let loaded = load().expect("应能读回缓存");
        assert_eq!(loaded.version, SCAN_CACHE_VERSION);
        assert_eq!(loaded.last_scan_mode, "quick");
        let mine = loaded.homes.iter().find(|h| h.path == path)
            .expect("应包含本测试写入的条目");
        assert_eq!(mine.label, "测试");
        // existence 标注：不存在的路径应为 false
        let marked = with_existence(&loaded);
        let mine_marked = marked.iter().find(|(h, _)| h.path == path)
            .expect("existence 标注应包含本测试条目");
        assert!(!mine_marked.1, "不存在的路径应标注为不存在");
    }
}
