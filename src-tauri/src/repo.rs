//! 备份仓库：manifest v2、原子写入、SHA-256、增量备份、仓库锁。
//!
//! 设计原则：
//! - 明文目录仓库，便于用户用资源管理器检查；
//! - manifest 记录恢复所需的全部元数据；
//! - blob 路径与源文件相对路径一致，用户可读；
//! - 不备份可重建内容，避免仓库膨胀。

use crate::model::*;
use crate::scanner::{collect_home_files, default_repo_dir, dirs_home, discover_agents_home, discover_homes};
use serde::{Deserialize, Serialize};
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(untagged)]
pub enum MTime {
    Millis(u64),
    Float(f64),
}

impl MTime {
    pub fn as_millis(&self) -> u64 {
        match self {
            MTime::Millis(v) => *v,
            MTime::Float(v) => *v as u64,
        }
    }
}
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter};


#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestoreMode {
    FillMissing,
    MergeNewer,
    Force,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestoreScope {
    Config,
    Sessions,
    Home,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub version: u32,
    pub tool: String,
    pub created_at: String,
    pub host: String,
    #[serde(default)]
    pub note: String,
    pub homes: Vec<ManifestHome>,
    pub files: Vec<ManifestFile>,
    #[serde(default)]
    pub warnings: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ManifestHome {
    pub id: String,
    pub kind: HomeKind,
    pub label: String,
    pub path: String,
    #[serde(default = "default_variant")]
    pub variant: String,
    #[serde(default)]
    pub dsh_version: Option<String>,
    pub sessions: SessionStats,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ManifestFile {
    pub root: String,
    pub path: String,
    pub size: u64,
    #[serde(default = "default_mtime")]
    pub mtime_ms: MTime,
    pub sha256: String,
    pub status: String,
    pub kind: String,
    #[serde(default = "default_rel")]
    pub rel: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupResult {
    pub repo: String,
    pub homes: u32,
    pub files: u32,
    pub created: u32,
    pub skipped: u32,
    pub bytes: u64,
    #[serde(default)]
    pub warnings: Vec<serde_json::Value>,
}

#[derive(Debug)]
pub struct RepoLock {
    #[allow(dead_code)]
    file: File,
    path: PathBuf,
}

impl RepoLock {
    pub fn acquire(repo: &Path) -> std::io::Result<Self> {
        fs::create_dir_all(repo)?;
        let path = repo.join(".vault-lock");

        // create_new 可以可靠拒绝“文件已存在”。若锁残留，再验证 PID 是否存活。
        let mut attempts = 0;
        loop {
            attempts += 1;
            match OpenOptions::new().read(true).write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    use std::io::Write;
                    let _ = file.write_all(format!("{}", std::process::id()).as_bytes());
                    let _ = file.sync_all();
                    return Ok(Self { file, path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let text = fs::read_to_string(&path).unwrap_or_default();
                    let pid: u32 = text.trim().parse().unwrap_or_default();
                    if pid == 0 || !process_exists(pid) {
                        if attempts >= 2 {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::Other,
                                "备份仓库锁残留且无法安全接管，请手动删除 .vault-lock",
                            ));
                        }
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("另一个 DSH Vault 进程（PID {pid}）正在操作该仓库"),
                    ));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(windows)]
fn process_exists(pid: u32) -> bool {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) != 0 }
}

#[cfg(not(windows))]
fn process_exists(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        // File 会在 RepoLock 析构时自动关闭，这里无需显式 drop。
    }
}

fn now_iso() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn default_repo() -> String {
    default_repo_dir().to_string_lossy().to_string()
}

pub fn load_manifest(repo: &Path) -> Result<Manifest, String> {
    let path = repo.join("manifest.json");
    let text = fs::read_to_string(path).map_err(|e| format!("无法读取仓库清单：{e}"))?;
    let manifest: Manifest =
        serde_json::from_str(&text).map_err(|e| format!("仓库清单格式不正确：{e}"))?;
    if !REPO_COMPAT_VERSIONS.contains(&manifest.version) {
        return Err(format!(
            "仓库版本不兼容：当前支持 v1/v2，仓库是 v{}",
            manifest.version
        ));
    }
    validate_manifest(&manifest)?;
    Ok(manifest)
}

/// 拒绝路径逃逸。备份清单本质上是外部输入，不能直接信任其中的相对路径。
fn validate_manifest(manifest: &Manifest) -> Result<(), String> {
    for home in &manifest.homes {
        if !is_safe_component(&home.id) {
            return Err(format!("仓库环境 ID 不安全：{}", home.id));
        }
    }

    for file in &manifest.files {
        if !is_safe_repo_path(&file.path) {
            return Err(format!("仓库文件路径不安全：{}", file.path));
        }
        let effective_rel = if file.rel.is_empty() {
            file.path.strip_prefix(&format!("blobs/{}/", file.root)).unwrap_or("").replace("\\\\", "/")
        } else {
            file.rel.clone()
        };
        if !is_safe_rel_path(&effective_rel) {
            return Err(format!("恢复路径不安全：{effective_rel}"));
        }
        if file.sha256.len() != 64 || !file.sha256.chars().all(|v| v.is_ascii_hexdigit()) {
            return Err(format!("SHA-256 格式不正确：{effective_rel}"));
        }
        let Some(home) = manifest.homes.iter().find(|h| h.id == file.root) else {
            return Err(format!("文件引用了不存在的环境：{effective_rel}"));
        };
        if home.kind == HomeKind::AgentsHome && !effective_rel.starts_with("skills/") {
            return Err(format!("共享技能库路径不合法：{effective_rel}"));
        }
    }
    Ok(())
}

fn is_safe_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains(['/', '\\'])
        && !value.contains(':')
}

fn is_safe_repo_path(value: &str) -> bool {
    if !value.starts_with("blobs/") {
        return false;
    }
    value.split('/').all(is_safe_component)
}

pub fn is_safe_rel_path(value: &str) -> bool {
    !value.is_empty() && !value.starts_with('/') && value.split('/').all(is_safe_component)
}

#[allow(dead_code)]
fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("无法打开文件：{e}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|e| format!("读取失败：{e}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    Ok(format!("{digest:x}"))
}
fn hash_and_copy(src: &Path, dst: &Path) -> Result<(u64, String), String> {
    let mut input = File::open(src).map_err(|e| format!("无法打开源文件 {}: {e}", src.display()))?;
    fs::create_dir_all(dst.parent().ok_or("目标路径缺少父目录")?).map_err(|e| format!("创建备份目录失败：{e}"))?;
    let tmp = dst.with_extension("vault-tmp");
    let mut output = File::create(&tmp).map_err(|e| format!("无法创建临时文件 {}: {e}", tmp.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut size = 0u64;
    loop {
        let read = input.read(&mut buffer).map_err(|e| format!("读取源文件失败：{e}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        output.write_all(&buffer[..read]).map_err(|e| format!("写入备份失败：{e}"))?;
        size += read as u64;
    }
    output.sync_all().map_err(|e| format!("落盘失败：{e}"))?;
    drop(output);
    if dst.exists() {
        fs::remove_file(dst).map_err(|e| format!("替换旧备份失败：{e}"))?;
    }
    fs::rename(&tmp, dst).map_err(|e| format!("备份落盘失败：{e}"))?;
    Ok((size, format!("{:x}", hasher.finalize())))
}

fn strip_credential_records(text: &str) -> String {
    let mut output = Vec::new();
    let mut in_records = false;
    for line in text.lines() {
        if line.starts_with("records:") || line.starts_with("records :") {
            in_records = true;
            continue;
        }
        if in_records {
            if !line.starts_with(' ') && !line.starts_with('\t') && !line.is_empty() {
                in_records = false;
                output.push(line.to_string());
            }
            continue;
        }
        output.push(line.to_string());
    }
    output.join("\n")
}

fn file_deps_need_strip(text: &str) -> bool {
    let home_text = dirs_home().to_string_lossy().to_string();
    text.contains("file:") && text.contains(&home_text)
}

fn strip_file_deps(text: &str, home: &str) -> String {
    // 旧仓库实测：profile package.json 里的 file: 绝对路径跨机恢复会导致安装失败。
    // 这里统一改为 file:./，让 pnpm 在新 Home 内按相对路径解析。
    let normalized_home = home.replace(char::from(92), "/");
    let normalized_home_backslash = home.replace("/", "\\");
    let slash_dep = format!("file:{normalized_home}");
    let backslash_dep = format!("file:{normalized_home_backslash}");
    text.replace(&slash_dep, "file:./").replace(&backslash_dep, "file:./")
}

fn previous_index(manifest: Option<&Manifest>) -> HashMap<String, ManifestFile> {
    let mut map = HashMap::new();
    if let Some(manifest) = manifest {
        for file in &manifest.files {
            map.insert(format!("{}|{}", file.root, file.path), file.clone());
        }
    }
    map
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupProgress {
    pub phase: String,
    pub done: u32,
    pub total: u32,
    pub current: String,
}

fn emit_backup_progress(app: Option<&AppHandle>, done: u32, total: u32, current: &str) {
    let Some(app) = app else { return };
    let _ = app.emit("backup-progress", BackupProgress {
        phase: "backup".into(),
        done,
        total,
        current: current.to_string(),
    });
}
pub fn run_backup(repo_text: &str, note: &str, only_config: bool, app: Option<&AppHandle>) -> Result<BackupResult, String> {
    let repo = PathBuf::from(repo_text);
    let _lock = RepoLock::acquire(&repo).map_err(|e| format!("无法锁定备份仓库：{e}"))?;
    let previous = load_manifest(&repo).ok();
    let previous_by_path = previous_index(previous.as_ref());
    // 模块三：本快照的 blob 写到快照专属目录，物理隔离防覆盖；同 hash 用硬链接去重。
    let snap_id = now_iso().replace(['-', ':'], "").replace('T', "-").get(..15).unwrap_or("unknown").to_string();
    let snap_files_prefix = format!("snapshots/{snap_id}/files");
    let mut homes_meta = Vec::new();
    let mut files_meta = Vec::new();
    let mut created = 0u32;
    let mut skipped = 0u32;
    let mut bytes = 0u64;
    let mut warnings = Vec::new();
    let all_files: Vec<_> = discover_homes()
        .into_iter()
        .map(|discovered| {
            let files = collect_home_files(&PathBuf::from(&discovered.path), only_config);
            (discovered, files)
        })
        .collect();
    let agents_files: Vec<_> = if only_config {
        Vec::new()
    } else {
        discover_agents_home()
            .map(|agents| {
                let mut collected = Vec::new();
                crate::scanner::walk_agents_collect(&PathBuf::from(&agents.path), &mut collected);
                collected
            })
            .unwrap_or_default()
    };
    let total_files: u32 = all_files.iter().map(|(_, files)| files.len() as u32).sum::<u32>()
        + agents_files.len() as u32;
    let mut done_files = 0u32;
    let mut last_percent = u32::MAX;

    for (discovered, files) in all_files {
        for file in files {
            done_files += 1;
            let percent = if total_files == 0 { 100 } else { done_files * 100 / total_files };
            if done_files <= 5 || percent != last_percent {
                emit_backup_progress(app, done_files, total_files, &file.rel);
                last_percent = percent;
            }
            let rel = file.rel.clone();
            // 新结构：blob 存到本快照专属目录
            let blob_rel = format!("{}/{}/{}", snap_files_prefix, discovered.id, rel);
            let blob_abs = repo.join(&blob_rel);
            // 旧结构路径（用于增量去重判断）
            let old_blob_rel = format!("blobs/{}/{}", discovered.id, rel);
            let old_blob_abs = repo.join(&old_blob_rel);
            let key = format!("{}|{}", discovered.id, old_blob_rel);
            let prev = previous_by_path.get(&key);
            let unchanged = prev
                .as_ref()
                .is_some_and(|p| p.size == file.size && p.mtime_ms.as_millis() == file.modified_ms);
            if unchanged {
                let prev = prev.cloned().unwrap();
                // 复用旧 blob：优先硬链接到本快照目录（省空间且快照独立）
                let source_abs = if old_blob_abs.is_file() { old_blob_abs.clone() } else { repo.join(&prev.path) };
                if let Some(parent) = blob_abs.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                if source_abs.is_file() && !blob_abs.is_file() {
                    // 硬链接失败则退回复制
                    if fs::hard_link(&source_abs, &blob_abs).is_err() {
                        let _ = fs::copy(&source_abs, &blob_abs);
                    }
                }
                files_meta.push(ManifestFile {
                    root: discovered.id.clone(),
                    path: blob_rel.clone(),
                    size: prev.size,
                    mtime_ms: prev.mtime_ms,
                    sha256: prev.sha256,
                    status: prev.status,
                    kind: file.kind.clone(),
                    rel,
                });
                skipped += 1;
                bytes += prev.size;
                continue;
            }

            if file.credential {
                let text = fs::read_to_string(&file.src).map_err(|e| format!("读取凭据失败：{e}"))?;
                let transformed = strip_credential_records(&text);
                let bytes_data = transformed.into_bytes();
                let transformed_size = bytes_data.len() as u64;
                fs::create_dir_all(blob_abs.parent().unwrap()).map_err(|e| format!("创建目录失败：{e}"))?;
                atomic_write(&blob_abs, &bytes_data)?;
                let sha = sha256_bytes(&bytes_data);
                files_meta.push(ManifestFile {
                    root: discovered.id.clone(),
                    path: blob_rel.clone(),
                    size: transformed_size,
                    mtime_ms: MTime::Millis(file.modified_ms),
                    sha256: sha,
                    status: "ok".into(),
                    kind: file.kind.clone(),
                    rel,
                });
                created += 1;
                bytes += transformed_size;
                warnings.push(format!("已剥离登录 token，只保留 API Key 引用：{}", file.rel));
                continue;
            }

            if file.kind == "profile" && file.src.extension().is_some_and(|v| v == "json") {
                let text = fs::read_to_string(&file.src).map_err(|e| format!("读取 profile 声明失败：{e}"))?;
                if file_deps_need_strip(&text) {
                    let transformed = strip_file_deps(&text, &discovered.path);
                    let bytes_data = transformed.into_bytes();
                    fs::create_dir_all(blob_abs.parent().unwrap()).map_err(|e| format!("创建目录失败：{e}"))?;
                    atomic_write(&blob_abs, &bytes_data)?;
                    let sha = sha256_bytes(&bytes_data);
                    files_meta.push(ManifestFile {
                        root: discovered.id.clone(),
                        path: blob_rel.clone(),
                        size: bytes_data.len() as u64,
                        mtime_ms: MTime::Millis(file.modified_ms),
                        sha256: sha,
                        status: "ok".into(),
                        kind: file.kind.clone(),
                        rel,
                    });
                    created += 1;
                    bytes += bytes_data.len() as u64;
                    warnings.push(format!("已把本机绝对路径依赖改为相对路径：{}", file.rel));
                    continue;
                }
            }

            let (size, sha) = hash_and_copy(&file.src, &blob_abs)?;
            files_meta.push(ManifestFile {
                root: discovered.id.clone(),
                path: blob_rel.clone(),
                size,
                mtime_ms: MTime::Millis(file.modified_ms),
                sha256: sha,
                status: "ok".into(),
                kind: file.kind.clone(),
                rel,
            });
            created += 1;
            bytes += size;
        }

        homes_meta.push(ManifestHome {
            id: discovered.id,
            kind: HomeKind::DshHome,
            label: discovered.label,
            path: discovered.path,
            variant: discovered.variant,
            dsh_version: discovered.dsh_version,
            sessions: discovered.sessions,
        });
    }

    if !only_config {
        if let Some(agents) = discover_agents_home() {
            for file in agents_files {
                let blob_rel = format!("{}/{}/{}", snap_files_prefix, agents.id, file.rel);
                let blob_abs = repo.join(&blob_rel);
                let old_blob_rel = format!("blobs/{}/{}", agents.id, file.rel);
                let old_blob_abs = repo.join(&old_blob_rel);
                let key = format!("{}|{}", agents.id, old_blob_rel);
                let prev = previous_by_path.get(&key);
                let unchanged = prev
                    .as_ref()
                    .is_some_and(|p| p.size == file.size && p.mtime_ms.as_millis() == file.modified_ms);
                if unchanged {
                    let prev = prev.cloned().unwrap();
                    if let Some(parent) = blob_abs.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let source_abs = if old_blob_abs.is_file() { old_blob_abs.clone() } else { repo.join(&prev.path) };
                    if source_abs.is_file() && !blob_abs.is_file() {
                        if fs::hard_link(&source_abs, &blob_abs).is_err() {
                            let _ = fs::copy(&source_abs, &blob_abs);
                        }
                    }
                    files_meta.push(ManifestFile {
                        root: agents.id.clone(),
                        path: blob_rel.clone(),
                        size: prev.size,
                        mtime_ms: prev.mtime_ms,
                        sha256: prev.sha256,
                        status: prev.status,
                        kind: "skill".into(),
                        rel: file.rel,
                    });
                    skipped += 1;
                    bytes += prev.size;
                    continue;
                }
                let (size, sha) = hash_and_copy(&file.src, &blob_abs)?;
                files_meta.push(ManifestFile {
                    root: agents.id.clone(),
                    path: blob_rel.clone(),
                    size,
                    mtime_ms: MTime::Millis(file.modified_ms),
                    sha256: sha,
                    status: "ok".into(),
                    kind: "skill".into(),
                    rel: file.rel,
                });
                created += 1;
                bytes += size;
            }
            homes_meta.push(ManifestHome {
                id: agents.id,
                kind: HomeKind::AgentsHome,
                label: agents.label,
                path: agents.path,
                variant: agents.variant,
                dsh_version: None,
                sessions: SessionStats::default(),
            });
        }
    }

    let note = if note.trim().is_empty() {
        format!("自动备份 {}", now_iso().replace('T', " ").get(..16).unwrap_or(""))
    } else {
        note.trim().to_string()
    };
    let manifest = Manifest {
        version: REPO_VERSION,
        tool: format!("dsh-vault/{VAULT_VERSION}"),
        created_at: now_iso(),
        host: hostname(),
        note,
        homes: homes_meta,
        files: files_meta,
        warnings: warnings.into_iter().map(|v| serde_json::Value::String(v)).collect(),
    };
    let text = serde_json::to_string_pretty(&manifest).map_err(|e| format!("生成清单失败：{e}"))?;
    atomic_write(&repo.join("manifest.json"), text.as_bytes())?;
    fs::write(repo.join("README.txt"), "DSH Vault 备份仓库。包含对话、技能与配置，请勿上传云盘。").map_err(|e| format!("写入仓库说明失败：{e}"))?;

    // 保存历史快照副本：snapshots/<timestamp>/manifest.json（blob 已写入同目录 files/ 下）
    let snap_dir = repo.join("snapshots").join(&snap_id);
    if fs::create_dir_all(&snap_dir).is_ok() {
        let _ = fs::write(snap_dir.join("manifest.json"), &text);
    }

    Ok(BackupResult {
        repo: repo.to_string_lossy().to_string(),
        homes: manifest.homes.len() as u32,
        files: manifest.files.len() as u32,
        created,
        skipped,
        bytes,
        warnings: manifest.warnings,
    })
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub fn atomic_write(dst: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建目录失败：{e}"))?;
    }
    let tmp = dst.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut file = File::create(&tmp).map_err(|e| format!("创建临时文件失败：{e}"))?;
        file.write_all(data).map_err(|e| format!("写入临时文件失败：{e}"))?;
        file.sync_all().map_err(|e| format!("临时文件落盘失败：{e}"))?;
    }
    if dst.exists() {
        fs::remove_file(dst).map_err(|e| format!("删除旧文件失败：{e}"))?;
    }
    fs::rename(&tmp, dst).map_err(|e| format!("替换文件失败：{e}"))?;
    Ok(())
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".into())
}













fn default_variant() -> String { "unknown".into() }




fn default_mtime() -> MTime { MTime::Millis(0) }


fn default_rel() -> String { String::new() }
















#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotInfo {
    pub id: String,
    pub note: String,
    pub created_at: String,
    pub host: String,
    pub homes: Vec<ManifestHome>,
    pub file_count: u32,
    pub total_size: u64,
    pub is_current: bool,
}

/// 列出仓库中的所有快照（当前只有最新备份 + undo 快照目录）。
/// v2 快照制仓库会读取 snapshots/ 目录；v1 平铺仓库视为单个"最新备份"。
pub fn list_snapshots(repo: &Path) -> Result<Vec<SnapshotInfo>, String> {
    let mut snapshots = Vec::new();

    // 检查 snapshots/ 目录（v2 快照制）
    let snap_dir = repo.join("snapshots");
    if snap_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&snap_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() { continue; }
                let manifest_path = path.join("manifest.json");
                if !manifest_path.is_file() { continue; }
                if let Ok(text) = fs::read_to_string(&manifest_path) {
                    if let Ok(m) = serde_json::from_str::<Manifest>(&text) {
                        let total_size: u64 = m.files.iter().map(|f| f.size).sum();
                        snapshots.push(SnapshotInfo {
                            id: entry.file_name().to_string_lossy().to_string(),
                            note: m.note.clone(),
                            created_at: m.created_at.clone(),
                            host: m.host.clone(),
                            homes: m.homes.clone(),
                            file_count: m.files.len() as u32,
                            total_size,
                            is_current: false,
                        });
                    }
                }
            }
        }
    }

    // 当前 manifest（最新备份）
    if let Ok(m) = load_manifest(repo) {
        let total_size: u64 = m.files.iter().map(|f| f.size).sum();
        snapshots.push(SnapshotInfo {
            id: "current".to_string(),
            note: m.note.clone(),
            created_at: m.created_at.clone(),
            host: m.host.clone(),
            homes: m.homes.clone(),
            file_count: m.files.len() as u32,
            total_size,
            is_current: true,
        });
    }

    // 按时间倒序
    snapshots.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(snapshots)
}
