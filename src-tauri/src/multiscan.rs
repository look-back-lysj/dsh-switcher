//! 多算法融合扫描：权威指针 + 内核特征 + 会话签名 + 位置启发。
//!
//! 设计目标：任何单一算法失效都有其他算法兜底。
//! - 算法A 权威指针：进程级 + 注册表 HKCU/HKLM 的 DSH_HOME（Tauri 版安装器写注册表，进程读不到）。
//! - 算法B 内核特征：sessions/.zstd、profiles 含 @deepseek-ai/dsh、.credentials.yaml 等（scanner::score_home）。
//! - 算法C 会话签名：发现 session*.jsonl.zstd 且首帧能解压出合法 DSH header（含 cwd 字段）。
//! - 算法D 位置启发：目录名含 dsh|deepseek|harness|eac|aio|v4lite 或在常见位置，仅加分不判决。
//!
//! 融合：总分 = A(直接确认) + B + C + D。>=60 确认，40-59 疑似，<40 排除。

use crate::model::*;
use crate::scanner::{dirs_home, score_home, scan_session_stats, estimate_backup_scope_pub};
use crate::zstd_check::{verify_session, SessionStatus};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// 算法A：读所有权威指针（进程级 + 注册表用户级/机器级 DSH_HOME）。
/// Tauri 桌面版安装器写 HKCU\\Environment\\DSH_HOME，进程变量读不到，必须读注册表。
pub fn authoritative_dsh_homes() -> Vec<PathBuf> {
    let mut out = Vec::new();
    // 进程级（继承自父进程）
    if let Some(v) = std::env::var_os("DSH_HOME") {
        out.push(PathBuf::from(v));
    }
    // 注册表用户级 + 机器级
    #[cfg(windows)]
    {
        out.extend(registry_dsh_homes());
    }
    // 去重
    let mut seen = HashSet::new();
    out.retain(|p| seen.insert(p.to_string_lossy().to_lowercase()));
    out
}

#[cfg(windows)]
fn registry_dsh_homes() -> Vec<PathBuf> {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
        KEY_READ, REG_SZ, REG_EXPAND_SZ,
    };
    let mut out = Vec::new();
    let subkey: Vec<u16> = "Environment".encode_utf16().chain(std::iter::once(0)).collect();
    let machine_subkey: Vec<u16> = "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment"
        .encode_utf16().chain(std::iter::once(0)).collect();
    let value_name: Vec<u16> = "DSH_HOME".encode_utf16().chain(std::iter::once(0)).collect();

    for (root, sub) in [(HKEY_CURRENT_USER, &subkey), (HKEY_LOCAL_MACHINE, &machine_subkey)] {
        unsafe {
            let mut hkey: HKEY = std::mem::zeroed();
            if RegOpenKeyExW(root, sub.as_ptr(), 0, KEY_READ, &mut hkey) != 0 {
                continue;
            }
            let mut buf = [0u16; 1024];
            let mut len = (buf.len() * 2) as u32;
            let mut ty = 0u32;
            let ok = RegQueryValueExW(
                hkey,
                value_name.as_ptr(),
                std::ptr::null_mut(),
                &mut ty,
                buf.as_mut_ptr() as *mut u8,
                &mut len,
            );
            RegCloseKey(hkey);
            if ok == 0 && (ty == REG_SZ || ty == REG_EXPAND_SZ) && len >= 2 {
                let chars = (len as usize) / 2;
                let s = String::from_utf16_lossy(&buf[..chars.saturating_sub(1)]);
                let s = s.trim().to_string();
                if !s.is_empty() {
                    out.push(PathBuf::from(s));
                }
            }
        }
    }
    out
}

/// 算法C：会话签名识别。目录下是否有可解压出合法 DSH header 的会话文件。
/// 能认出"只剩对话没配置"的残缺 home。
fn has_valid_session_signature(home: &Path) -> bool {
    let sessions = home.join("sessions");
    if !sessions.is_dir() {
        return false;
    }
    for entry in walkdir::WalkDir::new(&sessions)
        .max_depth(4)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .take(40)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if !(name.ends_with(".jsonl.zstd") || name.ends_with(".jsonl")) {
            continue;
        }
        // 用 zstd_check 校验结构（Ok/Truncated 都算有签名，Corrupt 不算）
        match verify_session(entry.path()).status {
            SessionStatus::Ok | SessionStatus::Truncated => return true,
            SessionStatus::Corrupt => continue,
        }
    }
    false
}

/// 算法D：位置启发（弱信号，仅加分不判决）。
fn location_hint_score(home: &Path) -> i32 {
    let mut score = 0i32;
    let name = home.file_name().and_then(|v| v.to_str()).unwrap_or("").to_lowercase();
    for kw in ["dsh", "deepseek", "harness", "eac", "aio", "v4lite", "lite"] {
        if name.contains(kw) {
            score += 6;
            break;
        }
    }
    let path_lower = home.to_string_lossy().to_lowercase();
    if path_lower.contains("appdata") || path_lower.contains("\\.dsh") {
        score += 3;
    }
    score
}


/// 快速预筛（不读文件内容、不遍历子树）：只看直接子目录名和浅层文件。
/// 通过预筛的候选才进入昂贵的深度评分（zstd 遍历等）。
fn is_excluded_path(home: &Path) -> bool {
    let p = home.to_string_lossy().to_lowercase().replace('/', "\\");
    let comps: Vec<&str> = p.split('\\').filter(|s| !s.is_empty()).collect();
    // 排除自身备份仓库与测试/临时目录
    const EXCLUDE_DIRS: &[&str] = &[
        "blobs", "adopted", "switch-backups", "snapshots", // DSH Vault 仓库内部
        "temp", "tmp", "$recycle.bin", "recycler",        // 临时/回收站
    ];
    for c in &comps {
        let cl = c.to_lowercase();
        if EXCLUDE_DIRS.contains(&cl.as_str()) {
            return true;
        }
        // vault-e2e / *-test-repo / import-test 等测试目录
        if cl.contains("vault-e2e") || cl.ends_with("-test-repo") || cl.starts_with("import-test") || cl.starts_with("lock-test") || cl.starts_with("progress-test") {
            return true;
        }
    }
    // AppData\\Local\\Temp 路径整体排除
    if p.contains("appdata\\local\\temp") {
        return true;
    }
    false
}

fn cheap_gate(home: &Path) -> bool {
    if is_excluded_path(home) {
        return false;
    }
    // 必须有以下任一强结构特征才继续
    let strong_dirs = ["sessions", "profiles", "skills", ".agent-presets", "memories"];
    let mut has_strong = false;
    for d in strong_dirs {
        if home.join(d).is_dir() {
            has_strong = true;
            break;
        }
    }
    if !has_strong {
        // 或浅层有 DSH 身份文件
        let identity = [".credentials.yaml", "settings.yaml", ".dshw-usage.json", ".dshw-size.json"];
        let mut has_id = false;
        for f in identity {
            if home.join(f).is_file() {
                has_id = true;
                break;
            }
        }
        if !has_id {
            return false;
        }
    }
    true
}

/// 综合评分（B + C + D）。A 是独立的直接确认通道。
fn fused_score(home: &Path) -> (i32, Vec<String>) {
    let mut reasons = Vec::new();
    let b = score_home(home);
    if b > 0 {
        reasons.push(format!("内核特征+{b}"));
    }
    let mut total = b;
    if has_valid_session_signature(home) {
        total += 50;
        reasons.push("会话签名+50".to_string());
    }
    let d = location_hint_score(home);
    if d > 0 {
        total += d;
        reasons.push(format!("位置+{d}"));
    }
    (total, reasons)
}

/// 是否疑似（40-59）或确认（>=60）。
pub fn classify_home(home: &Path) -> (i32, bool, Vec<String>) {
    let (score, reasons) = fused_score(home);
    (score, score >= 60, reasons)
}

/// 枚举所有固定盘符（跳过光驱/可移动）。
#[cfg(windows)]
fn fixed_drives() -> Vec<PathBuf> {
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    const DRIVE_FIXED: u32 = 3;
    let mut out = Vec::new();
    for letter in b'A'..=b'Z' {
        let root = format!("{}:\\\\", letter as char);
        let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            if GetDriveTypeW(wide.as_ptr()) == DRIVE_FIXED {
                out.push(PathBuf::from(root));
            }
        }
    }
    out
}

#[cfg(not(windows))]
fn fixed_drives() -> Vec<PathBuf> {
    vec![PathBuf::from("/")]
}

/// 遍历剪枝（扫描时不进入这些目录）。
fn should_skip_dir(name: &str) -> bool {
    let lower = name.to_lowercase();
    const SKIP: &[&str] = &[
        "node_modules", ".git", "windows", "$recycle.bin", "system volume information",
        "program files", "program files (x86)", "gpucache", "blob_storage", "code cache",
        "service worker", "cache", "ebwebview", "local storage", "session storage",
        "network", "partitions", "logs", "crashpad", "dawngraphitecache", "dawnwebgpucache",
    ];
    SKIP.contains(&lower.as_str())
}

/// 全盘多算法融合扫描。
/// quick=true：只扫常见根（用户目录、AppData、各盘根直下），深度 3，快。
/// quick=false：全盘所有固定盘，深度 5，慢但全。
pub fn multi_scan(quick: bool) -> Vec<DiscoveredHome> {
    let mut homes: Vec<DiscoveredHome> = Vec::new();
    let mut seen_real: HashSet<String> = HashSet::new();
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(if quick { 8 } else { 30 });

    let mut candidates: Vec<PathBuf> = Vec::new();

    // 算法A：权威指针优先（直接确认，无需遍历）。只算一次，避免循环里反复读注册表。
    let authoritative = authoritative_dsh_homes();
    let authoritative_real: Vec<PathBuf> = authoritative
        .iter()
        .filter_map(|a| fs::canonicalize(a).ok())
        .collect();
    for p in &authoritative {
        if p.is_dir() {
            candidates.push(p.clone());
        }
    }

    // 常见根目录
    let home = dirs_home();
    let mut roots = vec![home.clone()];
    if let Some(a) = std::env::var_os("APPDATA") {
        roots.push(PathBuf::from(a));
    }
    if let Some(a) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(a));
    }
    if !quick {
        for d in fixed_drives() {
            if !roots.iter().any(|r| d.starts_with(r) || r.starts_with(&d)) {
                roots.push(d);
            }
        }
    }

    let max_depth = if quick { 2 } else { 5 };

    // 遍历候选根
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&root)
            .max_depth(max_depth)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                if !e.file_type().is_dir() {
                    return true;
                }
                !should_skip_dir(&e.file_name().to_string_lossy())
            })
            .filter_map(Result::ok)
        {
            if std::time::Instant::now() > deadline {
                break;
            }
            if !entry.file_type().is_dir() {
                continue;
            }
            candidates.push(entry.path().to_path_buf());
        }
    }

    // 融合评分
    for cand in candidates {
        // junction/symlink 去重
        let real = fs::canonicalize(&cand).unwrap_or_else(|_| cand.clone());
        let key = real.to_string_lossy().to_lowercase().replace('\\', "/");
        if seen_real.contains(&key) {
            continue;
        }
        // 用户主目录本身不作为环境候选（它会因子目录含 sessions 被误判为环境）
        let home_real = fs::canonicalize(&dirs_home()).unwrap_or_else(|_| dirs_home());
        if real == home_real {
            continue;
        }
        // 快速预筛：不读内容不看子树，过滤掉 99% 无关目录
        if !cheap_gate(&cand) {
            continue;
        }
        let (_score, confirmed, reasons) = classify_home(&cand);
        let is_authoritative = authoritative_real.iter().any(|c| *c == real);
        if !confirmed && !is_authoritative {
            continue;
        }
        seen_real.insert(key);

        let label = cand
            .file_name()
            .map(|v| v.to_string_lossy().to_string())
            .unwrap_or_else(|| cand.to_string_lossy().to_string());
        let sessions = scan_session_stats(&cand);
        let (cnt, size) = estimate_backup_scope_pub(&cand);
        let mut warnings = Vec::new();
        if sessions.corrupt > 0 {
            warnings.push(format!("有 {} 个会话文件损坏", sessions.corrupt));
        }
        let variant = if is_authoritative {
            format!("DSH_HOME 指针 · {}", reasons.join(" "))
        } else {
            format!("特征识别 · {}", reasons.join(" "))
        };
        homes.push(DiscoveredHome {
            id: format!("root-{:x}", md5_like(&cand)),
            kind: HomeKind::DshHome,
            label,
            path: cand.to_string_lossy().to_string(),
            variant,
            dsh_version: None,
            sessions,
            backup_file_count: cnt,
            backup_size: size,
            warnings,
        });
    }

    homes
}

/// 与 scanner::content_hash 一致的路径哈希（避免重复引入 sha2 依赖差异）。
fn md5_like(path: &Path) -> u64 {
    use sha2::{Digest, Sha256};
    let normalized = path.to_string_lossy().to_lowercase();
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    let digest = hasher.finalize();
    u64::from_be_bytes(digest[..8].try_into().unwrap_or([0; 8])) >> 20
}
