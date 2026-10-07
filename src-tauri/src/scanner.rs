//! 扫描 DeepSeek Harness Home 并统计会话健康状态。
//!
//! 扫描策略分三层：
//! 1. 显式环境变量 `DSH_HOME`；
//! 2. 用户目录下的 `~/.dsh*`；
//! 3. AppData 中桌面壳的 `dsh-home`。
//! 再用内容特征过滤，避免把缓存目录误认成 Home。

use crate::model::*;
use crate::zstd_check::{verify_session, SessionStatus};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub fn default_repo_dir() -> PathBuf {
    if Path::new("D:\\").exists() {
        PathBuf::from("D:\\DSH-Backups\\repo")
    } else {
        dirs_home().join("DSH-Backups")
    }

}

pub fn dirs_home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 特征打分制识别 DSH Home，零固定路径。
/// 审查修正版：阈值 >=60 确认，40-59 疑似，<40 排除。
/// 实测：.dsh=215, .dsh-v4lite=200, AIO=280, 非 DSH 目录=0-10。
pub fn score_home(path: &Path) -> i32 {
    if !path.is_dir() {
        return 0;
    }
    let mut score = 0i32;

    // ---- 负信号（先扣，避免误检）----
    let name = path.file_name().and_then(|v| v.to_str()).unwrap_or("");
    if path.join("node_modules").is_dir() && path.join("sessions").is_dir() == false && path.join("profiles").is_dir() == false {
        score -= 50;
    }
    if path.join(".git").is_dir() {
        score -= 30;
    }
    if name.eq_ignore_ascii_case("cache") || name.eq_ignore_ascii_case("gpucache") || name.eq_ignore_ascii_case("blob_storage") {
        score -= 50;
    }

    // ---- 强信号 ----
    // sessions/ 下有 .zstd 会话文件
    let sessions = path.join("sessions");
    if sessions.is_dir() {
        if dir_has_zstd(&sessions) {
            score += 40;
        } else {
            score += 10;
        }
    }
    // profiles/ 下有 @deepseek-ai/dsh 依赖
    let profiles = path.join("profiles");
    if profiles.is_dir() {
        if profiles_has_dsh(&profiles) {
            score += 40;
        } else {
            score += 15;
        }
    }
    // 身份文件
    if path.join(".credentials.yaml").is_file() {
        score += 35;
    }
    if settings_has_agent_presets(&path.join("settings.yaml")) {
        score += 35;
    } else if path.join("settings.yaml").is_file() {
        score += 10;
    }
    // .dshw-*.json（用量/体积统计）
    if has_dshw_json(path) {
        score += 30;
    }

    // ---- 中信号 ----
    if path.join(".agent-presets").is_dir() {
        score += 15;
    }
    if path.join("guard").join("state.json").is_file() {
        score += 15;
    }
    if path.join("skills").is_dir() {
        score += 10;
    }
    if path.join("storages").join("workspace.json").is_file() {
        score += 10;
    }
    if path.join("rollbacks").is_dir() {
        score += 10;
    }
    if path.join("memories").is_dir() {
        score += 10; // AIO 特有
    }
    if path.join("team").is_dir() {
        score += 10; // AIO 特有
    }

    score
}

/// 确认为 DSH Home（>= 60）。
pub fn is_confirmed_home(path: &Path) -> bool {
    score_home(path) >= 60
}

fn dir_has_zstd(sessions: &Path) -> bool {
    for entry in walkdir::WalkDir::new(sessions).max_depth(4).follow_links(false).into_iter().filter_map(Result::ok) {
        if entry.file_type().is_file() {
            let name = entry.file_name().to_string_lossy();
            if name.ends_with(".jsonl.zstd") {
                return true;
            }
        }
    }
    false
}

fn profiles_has_dsh(profiles: &Path) -> bool {
    if let Ok(entries) = fs::read_dir(profiles) {
        for profile in entries.flatten() {
            let pkg = profile.path().join("package.json");
            if let Ok(text) = fs::read_to_string(pkg) {
                if text.contains("@deepseek-ai/dsh") {
                    return true;
                }
            }
        }
    }
    false
}

fn settings_has_agent_presets(settings: &Path) -> bool {
    if let Ok(text) = fs::read_to_string(settings) {
        return text.contains("agent-presets") || text.contains("agent_presets") || text.contains("presets");
    }
    false
}

fn has_dshw_json(path: &Path) -> bool {
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(".dshw-") && name.ends_with(".json") {
                return true;
            }
        }
    }
    false
}

fn content_hash(path: &Path) -> String {
    // 兼容 Node 原型：hash(小写、保留反斜杠的绝对路径) 前 12 位。
    // 不能把反斜杠替换成斜杠，否则旧仓库会被当成另一个 root 重复备份。
    let normalized = path.to_string_lossy().to_lowercase();
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("root-{}", &digest[..12.min(digest.len())])
}

fn add_home(out: &mut Vec<DiscoveredHome>, seen: &mut HashSet<String>, path: PathBuf, variant: &str) {
    if !path.is_dir() || !is_confirmed_home(&path) {
        return;
    }
    let normalized = path.to_string_lossy().to_lowercase().replace('\\', "/");
    if !seen.insert(normalized) {
        return;
    }

    let label = path
        .file_name()
        .map(|v| v.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());
    let sessions = scan_session_stats(&path);
    let (backup_file_count, backup_size) = estimate_backup_scope(&path);
    let mut warnings = Vec::new();
    if sessions.corrupt > 0 {
        warnings.push(format!("有 {} 个会话文件损坏，备份时会跳过并记录", sessions.corrupt));
    }
    if sessions.truncated > 0 {
        warnings.push(format!("有 {} 个会话文件末尾截断，仍可部分恢复", sessions.truncated));
    }
    if sessions.double_generation > 0 {
        warnings.push(format!(
            "有 {} 个会话目录同时存在多代际文件，恢复时会原样保留",
            sessions.double_generation
        ));
    }

    out.push(DiscoveredHome {
        id: content_hash(&path),
        kind: HomeKind::DshHome,
        label,
        path: path.to_string_lossy().to_string(),
        variant: variant.to_string(),
        dsh_version: read_dsh_version(&path),
        sessions,
        backup_file_count,
        backup_size,
        warnings,
    });
}

fn read_dsh_version(home: &Path) -> Option<String> {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        for product in ["com.deepseek.dsh.desktop.aio", "com.deepseek.dsh.desktop.lite"] {
            let path = PathBuf::from(&appdata).join(product).join("bundle-verified.json");
            if let Ok(text) = fs::read_to_string(path) {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(version) = value.get("version").and_then(|v| v.as_str()) {
                        return Some(version.to_string());
                    }
                }
            }
        }
    }

    if let Ok(profiles) = fs::read_dir(home.join("profiles")) {
        for profile in profiles.flatten() {
            let package = profile.path().join("package.json");
            if let Ok(text) = fs::read_to_string(package) {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(dep) = value.get("dependencies").and_then(|v| v.get("@deepseek-ai/dsh")).and_then(|v| v.as_str()) {
                        return Some(dep.trim_start_matches(['^', '~']).to_string());
                    }
                }
            }
        }
    }
    None
}

/// 递归统计 sessions 下所有 DSH 会话文件，并把结构校验结果合并进统计。
pub fn scan_session_stats(home: &Path) -> SessionStats {
    let mut stats = SessionStats::default();
    let mut generation_by_dir: HashMap<PathBuf, u32> = HashMap::new();
    let root = home.join("sessions");
    if !root.is_dir() {
        return stats;
    }

    for entry in walkdir::WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let is_v0 = name == "session.jsonl.zstd";
        let is_v4 = name.starts_with("session.v")
            && name.ends_with(".jsonl.zstd")
            && name != "session.jsonl.zstd";
        if !is_v0 && !is_v4 {
            continue;
        }

        stats.total += 1;
        if is_v0 {
            stats.v0 += 1;
        } else {
            stats.v4 += 1;
        }
        let parent = entry.path().parent().map(Path::to_path_buf);
        if let Some(parent) = parent {
            *generation_by_dir.entry(parent).or_insert(0) += 1;
        }

        match verify_session(entry.path()).status {
            SessionStatus::Ok => stats.ok += 1,
            SessionStatus::Truncated => stats.truncated += 1,
            SessionStatus::Corrupt => stats.corrupt += 1,
        }
    }

    stats.double_generation = generation_by_dir.values().filter(|v| **v > 1).count() as u32;
    stats
}

pub fn estimate_backup_scope_pub(home: &Path) -> (u64, u64) {
    estimate_backup_scope(home)
}

fn estimate_backup_scope(home: &Path) -> (u64, u64) {
    let files = collect_home_files(home, false);
    let mut size = 0u64;
    for file in &files {
        if let Ok(meta) = fs::metadata(&file.src) {
            size += meta.len();
        }
    }
    (files.len() as u64, size)
}

#[derive(Debug, Clone)]
pub struct CollectedFile {
    pub src: PathBuf,
    pub rel: String,
    pub kind: String,
    pub size: u64,
    pub modified_ms: u64,
    pub credential: bool,
}

pub fn collect_home_files(home: &Path, only_config: bool) -> Vec<CollectedFile> {
    let mut out = Vec::new();

    for name in SINGLE_FILES {
        push_file(&mut out, &home.join(name), name, "config");
    }

    if !only_config {
        for dir in RECURSE_DIRS {
            let root = home.join(dir);
            if !root.is_dir() {
                continue;
            }
            walk_collect(&root, dir, &mut out, "file");
        }

        for name in STORAGES_FILES {
            push_file(&mut out, &home.join("storages").join(name), &format!("storages/{name}"), "storage");
        }
    }

    if let Ok(profiles) = fs::read_dir(home.join("profiles")) {
        for profile in profiles.flatten() {
            if !profile.path().is_dir() {
                continue;
            }
            let profile_name = profile.file_name().to_string_lossy().to_string();
            for name in PROFILE_FILES {
                let rel = format!("profiles/{profile_name}/{name}");
                push_file(&mut out, &profile.path().join(name), &rel, "profile");
            }
        }
    }

    out
}

fn push_file(out: &mut Vec<CollectedFile>, src: &Path, rel: &str, kind: &str) {
    let Ok(meta) = fs::symlink_metadata(src) else { return };
    if !meta.is_file() {
        return;
    }
    let modified_ms = meta
        .modified()
        .ok()
        .and_then(|v| v.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|v| v.as_millis() as u64)
        .unwrap_or_default();
    let credential = src.file_name().and_then(|v| v.to_str()).is_some_and(|v| {
        v == ".credentials.yaml" || v == ".env"
    });
    out.push(CollectedFile {
        src: src.to_path_buf(),
        rel: rel.replace('\\', "/"),
        kind: kind.to_string(),
        size: meta.len(),
        modified_ms,
        credential,
    });
}

fn walk_collect(root: &Path, base_rel: &str, out: &mut Vec<CollectedFile>, kind: &str) {
    let Ok(entries) = fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let rel = format!("{base_rel}/{name}");
        let Ok(meta) = fs::symlink_metadata(&path) else { continue };
        let is_link = meta.file_type().is_symlink();
        // 关键：symlink 目录要跟随进入（接管后的 sessions/skills 都是链接，备份必须读真实内容）；
        // symlink 文件仍跳过（避免重复/越界）。用 fs::metadata 解析链接后的真实类型。
        let real_is_dir = if is_link {
            fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false)
        } else {
            meta.is_dir()
        };
        let real_is_file = if is_link {
            fs::metadata(&path).map(|m| m.is_file()).unwrap_or(false)
        } else {
            meta.is_file()
        };
        if real_is_dir {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk_collect(&path, &rel, out, kind);
        } else if real_is_file && !is_link && !is_skipped_file(&name) {
            let modified_ms = meta
                .modified()
                .ok()
                .and_then(|v| v.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|v| v.as_millis() as u64)
                .unwrap_or_default();
            let credential = name == ".credentials.yaml" || name == ".env";
            out.push(CollectedFile {
                src: path,
                rel,
                kind: kind.to_string(),
                size: meta.len(),
                modified_ms,
                credential,
            });
        }
    }
}

fn is_skipped_file(name: &str) -> bool {
    name == ".anonymous-user-id"
        || name == "session.lock"
        || name == "lock"
        || name == ".DS_Store"
        || name == "Thumbs.db"
        || name.starts_with("workspace.json.")
        || name.starts_with("session_projcache")
}

pub fn discover_homes() -> Vec<DiscoveredHome> {
    let mut homes = Vec::new();
    let mut seen = HashSet::new();
    let home = dirs_home();

    if let Some(env_home) = std::env::var_os("DSH_HOME") {
        add_home(&mut homes, &mut seen, PathBuf::from(env_home), "DSH_HOME");
    }
    add_home(&mut homes, &mut seen, home.join(".dsh"), "默认");
    if let Ok(entries) = fs::read_dir(&home) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(".dsh") && entry.path().is_dir() {
                add_home(&mut homes, &mut seen, entry.path(), &name);
            }
        }
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        if let Ok(entries) = fs::read_dir(PathBuf::from(appdata)) {
            for entry in entries.flatten() {
                let candidate = entry.path().join("dsh-home");
                if candidate.is_dir() {
                    let variant = entry.file_name().to_string_lossy().to_string();
                    add_home(&mut homes, &mut seen, candidate, &variant);
                }
            }
        }
    }

    homes
}

pub fn discover_agents_home() -> Option<DiscoveredHome> {
    let root = std::env::var_os("DSH_AGENTS_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs_home().join(".agents"));
    if !root.join("skills").is_dir() {
        return None;
    }
    let mut files = Vec::new();
    walk_collect(&root.join("skills"), "skills", &mut files, "skill");
    let mut size = 0u64;
    for file in &files {
        size += file.size;
    }
    Some(DiscoveredHome {
        id: content_hash(&root),
        kind: HomeKind::AgentsHome,
        label: ".agents".into(),
        path: root.to_string_lossy().to_string(),
        variant: "共享技能库".into(),
        dsh_version: None,
        sessions: SessionStats::default(),
        backup_file_count: files.len() as u64,
        backup_size: size,
        warnings: Vec::new(),
    })
}
/// 收享共享技能库文件。
pub fn walk_agents_collect(root: &std::path::Path, out: &mut Vec<CollectedFile>) {
    walk_collect(&root.join("skills"), "skills", out, "skill");
}




// ===================== v3：深度智能扫描 =====================

/// 深度扫描常见根目录，寻找 DSH Home。
/// 审查修正：不依赖固定路径，纯靠 score_home 特征打分；junction/symlink 用 realpath 去重。
/// deep=true 时扫更多根目录（用户手动触发"深度扫描"）。
pub fn deep_scan_homes(deep: bool) -> Vec<DiscoveredHome> {
    let mut homes = Vec::new();
    let mut seen_real: HashSet<String> = HashSet::new();

    // 候选根目录（只作起点，识别仍靠打分）
    let mut roots: Vec<PathBuf> = Vec::new();
    let home = dirs_home();
    roots.push(home.clone());
    if let Some(appdata) = std::env::var_os("APPDATA") {
        roots.push(PathBuf::from(appdata));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(local));
    }
    if deep {
        for drive in ["D:\\", "E:\\", "F:\\"] {
            let d = PathBuf::from(drive);
            if d.is_dir() {
                roots.push(d);
            }
        }
    }

    let max_depth = if deep { 5 } else { 3 };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(if deep { 30 } else { 5 });

    for root in roots {
        if !root.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&root)
            .max_depth(max_depth)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !should_prune(e))
            .filter_map(Result::ok)
        {
            if std::time::Instant::now() > deadline {
                break;
            }
            if !entry.file_type().is_dir() {
                continue;
            }
            let path = entry.path();
            if !is_confirmed_home(path) {
                continue;
            }
            // junction/symlink 去重：realpath 规范化
            let real = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            let key = real.to_string_lossy().to_lowercase().replace('\\', "/");
            if !seen_real.insert(key) {
                continue;
            }
            let mut tmp_seen = HashSet::new();
            add_home(&mut homes, &mut tmp_seen, path.to_path_buf(), "深度扫描");
        }
    }

    // 深度扫描结果与快速扫描合并去重（按规范化路径）
    homes
}

/// 遍历剪枝：跳过明显不可能的目录，控制扫描时间。
fn should_prune(entry: &walkdir::DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    let lower = name.to_lowercase();
    // 不进入大型无关目录
    const PRUNE: &[&str] = &[
        "node_modules", ".git", "windows", "$recycle.bin", "system volume information",
        "program files", "program files (x86)", "programdata\\microsoft",
        "gpucache", "blob_storage", "code cache", "service worker",
    ];
    for p in PRUNE {
        if lower == *p {
            return false;
        }
    }
    // 已确认的 DSH Home 不再深入其子目录（profiles/node_modules 等）
    true
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_real_homes() {
        let home = dirs_home();
        let dsh = home.join(".dsh");
        if dsh.is_dir() {
            let s = score_home(&dsh);
            assert!(s >= 60, ".dsh 应确认为 DSH Home，得分 {}", s);
        }
    }

    #[test]
    fn score_negative_non_dsh() {
        // 桌面目录不应被识别
        let desktop = dirs_home().join("Desktop");
        if desktop.is_dir() {
            assert!(score_home(&desktop) < 40, "Desktop 不应识别为 DSH Home");
        }
    }

    #[test]
    fn score_empty_dir_is_not_home() {
        let tmp = std::env::temp_dir().join("dsh-vault-test-empty");
        let _ = fs::create_dir_all(&tmp);
        assert!(score_home(&tmp) < 40);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn probe_scores_print() {
        let home = dirs_home();
        let mut cands = vec![home.join(".dsh"), home.join(".dsh-v4lite")];
        if let Some(appdata) = std::env::var_os("APPDATA") {
            cands.push(PathBuf::from(&appdata).join("com.deepseek.dsh.desktop.aio").join("dsh-home"));
        }
        for c in cands {
            eprintln!("SCORE {} = {}", c.display(), score_home(&c));
        }
    }
}
