//! 环境接管（adopt）：把 DSH Home 的关键子目录移入仓库，原位置留目录级 symlink。
//!
//! 设计来源：GNU Stow --adopt + winstow 相对路径 symlink，经 REVIEW-FINDINGS.md 修正：
//! - 只接管小目录（sessions/skills/...），不接管 profiles/（827MB 可重建）；
//! - symlink 链接到目录级（DSH 需要在其下新建 project_dir/session_dir）；
//! - 同盘符用相对路径保证便携性，跨盘符必须用绝对路径（Windows 限制）；
//! - 接管前必须检测 DSH 进程，运行中则拒绝并提示用户先关闭。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

// ===================== Junction（免管理员目录链接）=====================
// 依据：Rust std::sys::fs::windows::junction_point（nightly 模板）+ Microsoft Learn：
// - junction 是 IO_REPARSE_TAG_MOUNT_POINT reparse point，经 FSCTL_SET_REPARSE_POINT 写入，
//   全程不需要 SeCreateSymbolicLinkPrivilege，故普通用户免管理员即可创建；
// - junction 仅支持本机目录、目标必须真实存在、目标总是绝对路径（不支持相对/网络路径）。
// 对照：symlink（IO_REPARSE_TAG_SYMLINK）即使加 ALLOW_UNPRIVILEGED_CREATE 也要求先开开发者模式。
#[cfg(windows)]
mod junction {
    use std::ffi::OsStr;
    use std::mem::{offset_of, MaybeUninit};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Foundation::{GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, MAXIMUM_REPARSE_DATA_BUFFER_SIZE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_MOUNT_POINT;

    fn to_wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// 把任意路径转成 NT 风格 \??\C:\... 绝对路径（junction 要求）。
    fn nt_absolute(original: &Path) -> Result<Vec<u16>, String> {
        let abs: PathBuf = if original.is_absolute() {
            original.to_path_buf()
        } else {
            std::env::current_dir().map_err(|e| e.to_string())?.join(original)
        };
        let s = abs.to_string_lossy().replace('/', "\\");
        let nt = if let Some(rest) = s.strip_prefix("\\?\\") {
            format!("\\??\\{}", rest)
        } else if let Some(rest) = s.strip_prefix("\\.\\") {
            format!("\\??\\{}", rest)
        } else if let Some(rest) = s.strip_prefix("\\") {
            format!("\\??\\UNC\\{}", rest)
        } else if s.len() >= 2 && s.as_bytes()[1] == b':' {
            format!("\\??\\{}", s)
        } else {
            return Err(format!("无法转成 NT 绝对路径：{s}"));
        };
        Ok(nt.encode_utf16().collect())
    }

    #[repr(C)]
    struct MountPointBuffer {
        reparse_tag: u32,
        reparse_data_length: u16,
        reserved: u16,
        substitute_name_offset: u16,
        substitute_name_length: u16,
        print_name_offset: u16,
        print_name_length: u16,
        path_buffer: [MaybeUninit<u16>; (MAXIMUM_REPARSE_DATA_BUFFER_SIZE as usize) / 2],
    }

    /// 创建 junction（免管理员）。link 必须不存在；target 必须已存在且为本机目录。
    pub fn create(link: &Path, target: &Path) -> Result<(), String> {
        if !target.is_dir() {
            return Err(format!("目标不是目录：{}", target.display()));
        }
        if link.symlink_metadata().is_ok() {
            return Err(format!("链接位置已存在：{}", link.display()));
        }
        std::fs::create_dir(link).map_err(|e| format!("创建空目录失败：{e}"))?;

        let wide = to_wide(link.as_os_str());
        let handle: HANDLE = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let err = std::io::Error::last_os_error();
            let _ = std::fs::remove_dir(link);
            return Err(format!("打开目录失败：{err}"));
        }
        let owned = unsafe { OwnedHandle::from_raw_handle(handle as _) };
        let raw = owned.as_raw_handle() as HANDLE;

        let abs = nt_absolute(target)?;
        let mut buf = MountPointBuffer {
            reparse_tag: IO_REPARSE_TAG_MOUNT_POINT,
            reparse_data_length: 0,
            reserved: 0,
            substitute_name_offset: 0,
            substitute_name_length: (abs.len() * 2) as u16,
            print_name_offset: ((abs.len() + 1) * 2) as u16,
            print_name_length: 0,
            path_buffer: [MaybeUninit::uninit(); (MAXIMUM_REPARSE_DATA_BUFFER_SIZE as usize) / 2],
        };
        let need = abs.len() + 2;
        if need > buf.path_buffer.len() {
            drop(owned);
            let _ = std::fs::remove_dir(link);
            return Err("目标路径过长".to_string());
        }
        for (i, w) in abs.iter().enumerate() {
            buf.path_buffer[i].write(*w);
        }
        buf.path_buffer[abs.len()].write(0);
        buf.path_buffer[abs.len() + 1].write(0);
        let total_len = offset_of!(MountPointBuffer, path_buffer) + (abs.len() + 2) * 2;
        buf.reparse_data_length = (total_len - offset_of!(MountPointBuffer, substitute_name_offset)) as u16;

        let mut ret = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                raw,
                FSCTL_SET_REPARSE_POINT,
                (&buf as *const MountPointBuffer).cast(),
                total_len as u32,
                std::ptr::null_mut(),
                0,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        drop(owned); // CloseHandle
        if ok == 0 {
            let err = std::io::Error::last_os_error();
            let _ = std::fs::remove_dir(link);
            return Err(format!("写 reparse point 失败：{err}"));
        }
        Ok(())
    }
}


/// 接管的子目录（只接管这些小而重要的目录）。
pub const ADOPT_DIRS: &[&str] = &[
    "sessions",
    "skills",
    ".agent-presets",
    "guard",
    "rollbacks",
    "storages",
    "undo-snapshots",
    "memories", // AIO 特有
    "team",     // AIO 特有
];

/// profiles/ 不整体接管，只备份这些声明文件。
pub const PROFILE_DECL_FILES: &[&str] = &[
    "package.json",
    "cordis.patch.yml",
    "cordis.yml",
    "pnpm-workspace.yaml",
    ".dsh-builtin-plugins.json",
    ".dsh-profile-compatibility.json",
];

/// 仓库内的接管标记文件名。
pub const ADOPT_MARK: &str = ".dshvault";

/// 进度回调：报告 (done, total, current)。
pub type ProgressFn<'a> = Option<&'a dyn Fn(u64, u64, &str)>;

fn emit_progress(cb: ProgressFn, done: u64, total: u64, current: &str) {
    if let Some(f) = cb {
        f(done, total, current);
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdoptResult {
    pub home_id: String,
    pub moved_dirs: Vec<String>,
    pub created_links: Vec<String>,
    pub profile_decl_files: u32,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnadoptResult {
    pub home_id: String,
    pub restored_dirs: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LinkMapping {
    pub rel: String,        // 相对于 home 的子路径，如 "sessions"
    pub target: String,     // symlink 实际指向（相对或绝对）
    pub absolute: bool,
}

/// 接管清单：记录一个 home 被接管后的全部 symlink 映射。
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdoptRecord {
    pub version: u32,
    pub home_id: String,
    pub home_path: String,
    pub note: String,
    pub adopted_at: String,
    pub links: Vec<LinkMapping>,
    /// 接管时的文件清单（rel → sha256），用于变更检测。version >= 2 才有。
    #[serde(default)]
    pub manifest: std::collections::HashMap<String, String>,
}

fn format_bytes(n: u64) -> String {
    format_bytes_impl(n)
}

/// 公开给 main.rs 的日志摘要使用（format_bytes 是私有的）。
pub fn format_bytes_pub(n: u64) -> String {
    format_bytes_impl(n)
}

fn format_bytes_impl(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if n >= GB {
        format!("{:.1} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{} B", n)
    }
}

fn now_string() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

pub(crate) fn home_id_of(path: &Path) -> String {
    let normalized = path.to_string_lossy().to_lowercase();
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("root-{}", &digest[..12])
}

/// 检测 DSH 是否在运行（Windows ToolHelp32 进程快照）。
/// 返回运行中的 DSH 进程名列表（空 = 未运行）。
#[cfg(windows)]
pub(crate) fn detect_dsh_processes() -> Vec<String> {
    // 测试隔离：单元测试不应被开发机正在运行的 DSH 拦住（否则测试结果依赖本机状态）。
    if cfg!(test) {
        return Vec::new();
    }
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::Foundation::CloseHandle;

    let mut found = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return found;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut has = Process32FirstW(snap, &mut entry);
        while has != 0 {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            let name = OsString::from_wide(&entry.szExeFile[..len]).to_string_lossy().to_lowercase();
            // DSH 官方/EAC/AIO 进程名特征：dsh、deepseek、harness
            let is_dsh = (name.contains("dsh") || name.contains("deepseek") || name.contains("harness"))
                && name.ends_with(".exe")
                && !name.contains("dsh-vault") // 排除本工具自身
                && !name.contains("dsh_vault"); // 排除本工具的测试二进制（下划线）
            if is_dsh {
                found.push(name);
            }
            has = Process32NextW(snap, &mut entry);
        }
        CloseHandle(snap);
    }
    found.sort();
    found.dedup();
    found
}

#[cfg(not(windows))]
pub(crate) fn detect_dsh_processes() -> Vec<String> {
    Vec::new()
}

/// 判断 symlink 目标该用相对还是绝对路径。
/// Windows 限制：跨盘符没有相对路径，必须用绝对路径。
fn same_volume(a: &Path, b: &Path) -> bool {
    let pa = a.components().next();
    let pb = b.components().next();
    match (pa, pb) {
        (Some(std::path::Component::Prefix(x)), Some(std::path::Component::Prefix(y))) => {
            x.as_os_str().to_string_lossy().to_lowercase() == y.as_os_str().to_string_lossy().to_lowercase()
        }
        _ => false,
    }
}

/// 计算从 from_dir 到 target 的相对路径（仅同盘符调用）。
fn relative_path(from_dir: &Path, target: &Path) -> Option<PathBuf> {
    let from: Vec<_> = from_dir.components().collect();
    let to: Vec<_> = target.components().collect();
    let mut i = 0;
    while i < from.len() && i < to.len() && from[i] == to[i] {
        i += 1;
    }
    let mut rel = PathBuf::new();
    for _ in i..from.len() {
        rel.push("..");
    }
    for c in &to[i..] {
        rel.push(c.as_os_str());
    }
    Some(rel)
}

/// 创建目录级链接：Windows 上优先 junction（免管理员），失败再退回 symlink（需开发者模式/管理员）。
/// 返回记录的 target 字符串：junction 用绝对路径，symlink 保持原有相对/绝对逻辑。
pub fn create_dir_link(link: &Path, target: &Path) -> Result<String, String> {
    if link.exists() || link.symlink_metadata().is_ok() {
        return Err(format!("链接位置已存在：{}", link.display()));
    }

    #[cfg(windows)]
    {
        // 1) 优先 junction：免管理员、跨盘支持、兼容性更高。
        match junction::create(link, target) {
            Ok(()) => {
                return Ok(target.to_string_lossy().to_string());
            }
            Err(_jerr) => {
                // junction 失败（罕见：非 NTFS / 目标不存在等），落回 symlink。
            }
        }
        // 2) symlink 兜底：保留相对路径便携性。
        let (final_target, _absolute) = if same_volume(link, target) {
            let parent = link.parent().ok_or_else(|| "无法获取链接父目录".to_string())?;
            match relative_path(parent, target) {
                Some(rel) => (rel, false),
                None => (target.to_path_buf(), true),
            }
        } else {
            (target.to_path_buf(), true)
        };
        std::os::windows::fs::symlink_dir(&final_target, link).map_err(|e| {
            format!(
                "创建链接失败：{e}。已优先尝试 junction（免管理员）失败；请开启开发者模式（设置→系统→开发者选项→开发者模式）或以管理员身份运行本工具后重试。"
            )
        })?;
        return Ok(final_target.to_string_lossy().to_string());
    }
    #[cfg(not(windows))]
    {
        let final_target = target.to_path_buf();
        std::os::unix::fs::symlink(&final_target, link).map_err(|e| format!("创建链接失败：{e}"))?;
        Ok(final_target.to_string_lossy().to_string())
    }
}

/// 通过 Tauri 发送 op-progress 事件（无 app 时静默）。
pub fn emit_op_progress(app: Option<&tauri::AppHandle>, done: u64, total: u64, current: &str) {
    use tauri::Emitter;
    if let Some(app) = app {
        let _ = app.emit("op-progress", serde_json::json!({
            "done": done, "total": total, "current": current,
        }));
    }
}

/// 跨盘符安全移动目录：copy 目录树 → 校验文件数 → 删除源。
/// 同盘符直接 rename。
fn move_dir(src: &Path, dst: &Path) -> Result<(), String> {
    move_dir_with_progress(src, dst, None)
}

fn move_dir_with_progress(src: &Path, dst: &Path, app: Option<&tauri::AppHandle>) -> Result<(), String> {
    if dst.exists() {
        return Err(format!("目标已存在：{}", dst.display()));
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建目标父目录失败：{e}"))?;
    }
    // 同盘符优先 rename（原子）
    if same_volume(src, dst) {
        if fs::rename(src, dst).is_ok() {
            return Ok(());
        }
        // rename 失败则退回 copy+delete
    }
    copy_dir_recursive(src, dst)?;
    // 校验：源/目标文件数一致才删源
    let src_count = count_files(src);
    let dst_count = count_files(dst);
    if src_count != dst_count {
        return Err(format!(
            "移动校验失败：源 {src_count} 个文件，目标 {dst_count} 个文件。源目录未删除，请手动检查。"
        ));
    }
    fs::remove_dir_all(src).map_err(|e| format!("删除源目录失败：{e}"))?;
    Ok(())
}

/// 合并复制：把来源内容复制进目标，**绝不删除目标端独有的文件**（同名以来源为准覆盖）。
/// 返回复制的文件数。切默认的"合并模式"用它——根治"切换把目标端新对话抹掉"。
pub(crate) fn copy_dir_merge(src: &Path, dst: &Path) -> Result<u64, String> {
    fs::create_dir_all(dst).map_err(|e| format!("创建目录失败：{e}"))?;
    let mut copied = 0u64;
    for entry in fs::read_dir(src).map_err(|e| format!("读取目录失败：{e}"))? {
        let entry = entry.map_err(|e| format!("读取条目失败：{e}"))?;
        let ty = entry.file_type().map_err(|e| format!("读取类型失败：{e}"))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_symlink() {
            continue; // 链接不搬运（各方自己管理）
        }
        if ty.is_dir() {
            copied += copy_dir_merge(&from, &to)?;
        } else if ty.is_file() {
            fs::copy(&from, &to).map_err(|e| format!("复制 {} 失败：{e}", from.display()))?;
            copied += 1;
        }
    }
    Ok(copied)
}

pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    copy_dir_progress(src, dst, None, &mut 0, 0)
}

fn copy_dir_progress(
    src: &Path,
    dst: &Path,
    cb: ProgressFn,
    done: &mut u64,
    total: u64,
) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("创建目录失败：{e}"))?;
    for entry in fs::read_dir(src).map_err(|e| format!("读取目录失败：{e}"))? {
        let entry = entry.map_err(|e| format!("读取条目失败：{e}"))?;
        let ty = entry.file_type().map_err(|e| format!("读取类型失败：{e}"))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_progress(&from, &to, cb, done, total)?;
        } else if ty.is_file() {
            fs::copy(&from, &to).map_err(|e| format!("复制 {} 失败：{e}", from.display()))?;
            *done += 1;
            emit_progress(cb, *done, total, &from.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
        }
        // symlink 条目跳过（接管前理论上不应有）
    }
    Ok(())
}

fn count_files(root: &Path) -> u64 {
    let mut n = 0;
    for entry in walkdir::WalkDir::new(root).follow_links(false).into_iter().filter_map(Result::ok) {
        if entry.file_type().is_file() {
            n += 1;
        }
    }
    n
}

pub(crate) fn repo_files_dir(repo: &Path, home_id: &str) -> PathBuf {
    repo.join("adopted").join(home_id).join("files")
}

fn adopt_record_path(repo: &Path, home_id: &str) -> PathBuf {
    repo.join("adopted").join(home_id).join("record.json")
}

/// 读取某 home 的接管记录（若已接管）。
pub fn read_adopt_record(repo: &Path, home_path: &Path) -> Option<AdoptRecord> {
    let id = home_id_of(home_path);
    let p = adopt_record_path(repo, &id);
    let text = fs::read_to_string(p).ok()?;
    serde_json::from_str(&text).ok()
}

/// 判断 home 是否已被接管（原位置是否已是 symlink）。
pub fn is_adopted(home_path: &Path) -> bool {
    let sessions = home_path.join("sessions");
    sessions
        .symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// 接管一个 DSH Home。
/// 前置：DSH 未运行（调用方已检测）、home 未被接管。
pub fn adopt_home(repo: &Path, home_path: &Path, note: &str) -> Result<AdoptResult, String> {
    adopt_home_with_progress(repo, home_path, note, None)
}

/// 带进度回调的接管（app 用于 emit op-progress 事件）。
pub fn adopt_home_with_progress(
    repo: &Path,
    home_path: &Path,
    note: &str,
    app: Option<&tauri::AppHandle>,
) -> Result<AdoptResult, String> {
    // 1. 运行检测（双保险）
    let running = detect_dsh_processes();
    if !running.is_empty() {
        return Err(format!(
            "检测到 DSH 正在运行（{}），请先完全关闭所有 DSH 窗口后再接管。",
            running.join(", ")
        ));
    }
    // 1.5 链接能力预检：junction 与 symlink 都不可用时，提前返回清晰指引（不动任何文件）。
    let cap = check_link_capability(home_path);
    if !cap.junction_ok && !cap.symlink_ok {
        return Err(cap.advice);
    }
    // 1.6 磁盘空间检查：跨盘移动需要 home 与仓库双方都有足够空间（预估 ×2 保险）。
    if let (Some(free), need) = (disk_free_bytes(repo), estimate_adopt_bytes(home_path).saturating_mul(2)) {
        if need > 0 && free < need {
            return Err(format!(
                "磁盘空间不足：接管预计需要 {}（含保险余量），但仓库所在盘仅剩 {}。请清理磁盘或更换仓库位置后重试。",
                format_bytes(need),
                format_bytes(free)
            ));
        }
    }
    // 2. 半完成状态自愈：历史接管中断会把目录搬进仓库但没建链接，先还原再继续。
    if let Some(partial) = detect_partial_adoption(repo, home_path) {
        let restored = repair_partial_adoption(repo, home_path)
            .map_err(|e| format!("检测到上次接管未完成，自动修复失败：{e}"))?;
        if !restored.is_empty() {
            eprintln!("[dsh-vault] 自动修复半完成接管：已还原 {:?}", restored);
        }
    }
    // 3. 重复接管检测
    if is_adopted(home_path) {
        return Err("该环境已被接管（sessions 已是链接），无需重复接管。".to_string());
    }
    if !crate::scanner::is_confirmed_home(home_path) {
        return Err("该目录未通过 DSH Home 特征校验，无法接管。若确认是 DSH 环境，可能是会话/配置文件缺失。".to_string());
    }

    let home_id = home_id_of(home_path);
    let files_root = repo_files_dir(repo, &home_id);
    fs::create_dir_all(&files_root).map_err(|e| format!("创建仓库目录失败：{e}"))?;

    let mut moved_dirs = Vec::new();
    let mut created_links = Vec::new();
    let mut links: Vec<LinkMapping> = Vec::new();
    let mut warnings = Vec::new();

    // 3. 逐个移动 ADOPT_DIRS 并创建链接；任一失败则自动回滚到接管前状态。
    for dir in ADOPT_DIRS {
        let src = home_path.join(dir);
        if !src.is_dir() {
            continue;
        }
        // 防御：若已是链接则跳过
        if src.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            warnings.push(format!("{dir} 已是链接，跳过"));
            continue;
        }
        let dst = files_root.join(dir);
        // 进度：接管移动大目录时报告
        let total = count_files(&src);
        emit_op_progress(app, 0, total, &format!("正在移动 {dir}"));
        // 移动失败 → 回滚已处理目录后返回错误
        if let Err(e) = move_dir_with_progress(&src, &dst, app) {
            rollback_partial(&files_root, home_path, &links, &mut warnings);
            return Err(format!("移动 {dir} 失败：{e}。已自动还原到接管前状态。"));
        }
        emit_op_progress(app, total, total, &format!("{dir} 完成"));
        moved_dirs.push(dir.to_string());

        // 建链接失败 → 先把刚移动的 dir 搬回，再回滚前面的，最后返回错误
        match create_dir_link(&src, &dst) {
            Ok(target_str) => {
                created_links.push(dir.to_string());
                let absolute = !same_volume(&src, &dst);
                links.push(LinkMapping {
                    rel: dir.to_string(),
                    target: target_str,
                    absolute,
                });
            }
            Err(e) => {
                // 把刚移动的这一个搬回原位
                if move_dir(&dst, &src).is_err() {
                    warnings.push(format!("还原 {dir} 失败，请手动把仓库里的 {dir} 移回原位"));
                }
                rollback_partial(&files_root, home_path, &links, &mut warnings);
                return Err(format!("为 {dir} 创建链接失败：{e}。已自动还原到接管前状态。"));
            }
        }
    }

    // 4. profiles/ 声明文件备份（不移动 profiles 本体）
    let mut profile_decl = 0u32;
    let profiles = home_path.join("profiles");
    if profiles.is_dir() {
        if let Ok(entries) = fs::read_dir(&profiles) {
            for profile in entries.flatten() {
                if !profile.path().is_dir() {
                    continue;
                }
                let pname = profile.file_name().to_string_lossy().to_string();
                for f in PROFILE_DECL_FILES {
                    let src_f = profile.path().join(f);
                    if src_f.is_file() {
                        let dst_f = repo.join("adopted").join(&home_id).join("profiles").join(&pname).join(f);
                        if let Some(parent) = dst_f.parent() {
                            let _ = fs::create_dir_all(parent);
                        }
                        if fs::copy(&src_f, &dst_f).is_ok() {
                            profile_decl += 1;
                        }
                    }
                }
            }
        }
    }

    // 5. 写接管标记与记录
    // 4.5 生成接管时的文件清单（SHA-256），供后续变更检测
    let mut manifest = std::collections::HashMap::new();
    for link in &links {
        let store = files_root.join(&link.rel);
        if !store.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&store).follow_links(false).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry.path().strip_prefix(&store).unwrap().to_string_lossy().replace('\\', "/");
            let key = format!("{}/{}", link.rel, rel);
            if let Ok(bytes) = fs::read(entry.path()) {
                let mut h = Sha256::new();
                h.update(&bytes);
                manifest.insert(key, format!("{:x}", h.finalize()));
            }
        }
    }

    let record = AdoptRecord {
        version: 2,
        manifest,
        home_id: home_id.clone(),
        home_path: home_path.to_string_lossy().to_string(),
        note: note.to_string(),
        adopted_at: now_string(),
        links,
    };
    let record_path = adopt_record_path(repo, &home_id);
    if let Some(parent) = record_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let json = serde_json::to_string_pretty(&record).map_err(|e| format!("序列化接管记录失败：{e}"))?;
    fs::write(&record_path, json).map_err(|e| format!("写入接管记录失败：{e}"))?;
    // 标记文件（防止误删 + 供扫描识别）
    let _ = fs::write(repo.join("adopted").join(&home_id).join(ADOPT_MARK), &record.adopted_at);

    // 模块六：接管后自动塞 AI 小纸条（失败不阻断接管）
    let _ = write_ai_note(repo, home_path);

    Ok(AdoptResult {
        home_id,
        moved_dirs,
        created_links,
        profile_decl_files: profile_decl,
        warnings,
    })
}

/// 断开接管：删除 symlink，把仓库里的目录移回原位置。
pub fn unadopt_home(repo: &Path, home_path: &Path) -> Result<UnadoptResult, String> {
    unadopt_home_with_progress(repo, home_path, None)
}

pub fn unadopt_home_with_progress(
    repo: &Path,
    home_path: &Path,
    app: Option<&tauri::AppHandle>,
) -> Result<UnadoptResult, String> {
    let running = detect_dsh_processes();
    if !running.is_empty() {
        return Err(format!(
            "检测到 DSH 正在运行（{}），请先完全关闭后再断开接管。",
            running.join(", ")
        ));
    }
    let home_id = home_id_of(home_path);
    let record = read_adopt_record(repo, home_path)
        .ok_or_else(|| "未找到接管记录，无法断开接管。".to_string())?;
    let files_root = repo_files_dir(repo, &home_id);

    let mut restored = Vec::new();
    let mut warnings = Vec::new();

    for link in &record.links {
        let link_path = home_path.join(&link.rel);
        let store_path = files_root.join(&link.rel);
        if !store_path.is_dir() {
            warnings.push(format!("仓库中缺少 {}，跳过还原", link.rel));
            continue;
        }
        // 删除 symlink（不删目标）
        if link_path.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            fs::remove_dir(&link_path).map_err(|e| format!("删除链接 {} 失败：{e}", link.rel))?;
        } else if link_path.exists() {
            warnings.push(format!("{} 位置已有真实目录，跳过还原以免覆盖", link.rel));
            continue;
        }
        let total = count_files(&store_path);
        emit_op_progress(app, 0, total, &format!("正在还原 {}", link.rel));
        move_dir_with_progress(&store_path, &link_path, app)
            .map_err(|e| format!("还原 {} 失败：{e}", link.rel))?;
        emit_op_progress(app, total, total, &format!("{} 已还原", link.rel));
        restored.push(link.rel.clone());
    }

    // 清理记录与标记
    let _ = fs::remove_file(adopt_record_path(repo, &home_id));
    let _ = fs::remove_file(repo.join("adopted").join(&home_id).join(ADOPT_MARK));
    // 断开接管理顺空的 adopted/<id> 目录（files 已还原为空，避免仓库残留空壳）
    let adopted_dir = repo.join("adopted").join(&home_id);
    if adopted_dir.is_dir() {
        let has_files = walkdir::WalkDir::new(&adopted_dir)
            .into_iter()
            .filter_map(Result::ok)
            .any(|e| e.file_type().is_file());
        if !has_files {
            let _ = fs::remove_dir_all(&adopted_dir);
        }
    }

    // 模块六：断开接管时移除小纸条
    let _ = remove_ai_note(home_path);

    Ok(UnadoptResult {
        home_id,
        restored_dirs: restored,
        warnings,
    })
}


#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeInfo {
    pub rel: String,
    pub kind: String, // "modified" | "added" | "deleted"
}

/// 检测 DSH 对已接管文件的修改（对比接管时的 SHA-256 清单）。
/// 读取通过 symlink 的当前文件，与接管清单对比。
pub fn check_home_changes(repo: &Path, home_path: &Path) -> Result<Vec<ChangeInfo>, String> {
    let record = read_adopt_record(repo, home_path)
        .ok_or_else(|| "该环境未被接管，无法检测变更。".to_string())?;
    let mut changes = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for link in &record.links {
        let live = home_path.join(&link.rel); // 通过 symlink 读当前
        if !live.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&live).follow_links(false).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry.path().strip_prefix(&live).unwrap().to_string_lossy().replace('\\', "/");
            let key = format!("{}/{}", link.rel, rel);
            seen.insert(key.clone());
            let cur_hash = fs::read(entry.path()).ok().map(|bytes| {
                let mut h = Sha256::new();
                h.update(&bytes);
                format!("{:x}", h.finalize())
            });
            match record.manifest.get(&key) {
                Some(old) => {
                    if Some(old) != cur_hash.as_ref() {
                        changes.push(ChangeInfo { rel: key, kind: "modified".into() });
                    }
                }
                None => changes.push(ChangeInfo { rel: key, kind: "added".into() }),
            }
        }
    }
    // 清单里有、当前没有 → deleted
    for key in record.manifest.keys() {
        if !seen.contains(key) {
            changes.push(ChangeInfo { rel: key.clone(), kind: "deleted".into() });
        }
    }
    Ok(changes)
}


// ===================== 切换（switch）=====================

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchResult {
    pub switched_links: u32,
    pub backup_snapshot: String,
    pub details: Vec<String>,
    pub warnings: Vec<String>,
    /// v4.1：凭据不会随切换迁移，前端据此弹重录提示。
    #[serde(default)]
    pub credential_note: bool,
    /// v4.2 模块B：本次是否把源端 settings.yaml 带到了目标 home 根。
    /// 这是"能看不能聊"的根治：模型提供方配置随切换走（API Key 仍不带）。
    #[serde(default)]
    pub settings_copied: bool,
    /// v4.3：本次改写了多少条对话的非法预设（anchored-standard → standard）。
    #[serde(default)]
    pub preset_fixed: u32,
    /// v6：本次为让对话能发消息而补齐到目标端的模型提供方 id
    #[serde(default)]
    pub provider_added: Vec<String>,
    /// v6：还需要在目标版本界面补录的 Key 名（只列名，不列值）
    #[serde(default)]
    pub keys_to_enter: Vec<String>,
    /// v7：本次是否为合并模式（false=合并，true=替换）
    #[serde(default)]
    pub replace_mode: bool,
    /// v7：本次补登记的对话条数
    #[serde(default)]
    pub registry_repaired: usize,
    /// v8：切换后仍因「原工作目录不存在」而不会被官方显示的对话条数
    #[serde(default)]
    pub sessions_missing_dir: usize,
    /// v8：切换后处于归档名单、默认隐藏的对话条数
    #[serde(default)]
    pub sessions_archived: usize,
}

/// v5 模块三：为保险快照生成 manifest（rel → sha256 + size），回滚校验用。
fn build_snapshot_manifest(snap_root: &Path) -> serde_json::Value {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(snap_root).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() { continue; }
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "manifest.json" { continue; }
        let rel = entry.path().strip_prefix(snap_root).unwrap_or(entry.path()).to_string_lossy().replace('\\', "/");
        let (mut sha, mut size) = (String::new(), 0u64);
        if let Ok(bytes) = fs::read(entry.path()) {
            let mut h = Sha256::new();
            h.update(&bytes);
            sha = format!("{:x}", h.finalize());
            size = bytes.len() as u64;
        }
        files.push(serde_json::json!({ "rel": rel, "sha256": sha, "size": size }));
    }
    serde_json::json!({ "version": 1, "fileCount": files.len(), "files": files })
}

/// v5 模块三：回滚前校验保险快照完整性（manifest 里的每个文件都在且哈希一致）。
/// 返回 (总文件数, 缺失/损坏数, 问题描述)。
pub fn verify_snapshot_integrity(snap_root: &Path) -> (usize, usize, Vec<String>) {
    let manifest_file = snap_root.join("manifest.json");
    let Ok(text) = fs::read_to_string(&manifest_file) else {
        return (0, 0, vec!["快照无 manifest（旧版快照），跳过校验".to_string()]);
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return (0, 0, vec!["manifest 损坏，跳过校验".to_string()]);
    };
    let mut total = 0usize;
    let mut bad = 0usize;
    let mut issues = Vec::new();
    if let Some(arr) = v.get("files").and_then(|f| f.as_array()) {
        for f in arr {
            total += 1;
            let rel = f.get("rel").and_then(|x| x.as_str()).unwrap_or("");
            let expected = f.get("sha256").and_then(|x| x.as_str()).unwrap_or("");
            let path = snap_root.join(rel.replace('/', "\\"));
            match fs::read(&path) {
                Ok(bytes) => {
                    let mut h = Sha256::new();
                    h.update(&bytes);
                    if format!("{:x}", h.finalize()) != expected {
                        bad += 1;
                        issues.push(format!("{} 哈希不符", rel));
                    }
                }
                Err(_) => {
                    bad += 1;
                    issues.push(format!("{} 缺失", rel));
                }
            }
        }
    }
    (total, bad, issues)
}

/// v4.2 模块F：一键回滚到"切换前"。
/// 把最近一次保险快照的内容写回目标端自己的存档（链接不动），凭据文件硬跳过。

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchSnapshotInfo {
    pub name: String,
    pub time: String,
    pub from_label: String,
    pub file_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackResult {
    pub restored_dirs: u32,
    pub restored_settings: bool,
    pub snapshot: String,
    pub warnings: Vec<String>,
}

/// 列出某目标环境的切换保险快照（新→旧）。
pub fn list_switch_snapshots(repo: &Path, target_home: &Path) -> Vec<SwitchSnapshotInfo> {
    let tgt_id = home_id_of(target_home);
    let index_file = repo.join("switch-backups").join(&tgt_id).join("index.json");
    let mut out = Vec::new();
    if let Ok(text) = fs::read_to_string(&index_file) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(arr) = v.get("snapshots").and_then(|a| a.as_array()) {
                for s in arr.iter().rev() {
                    out.push(SwitchSnapshotInfo {
                        name: s.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        time: s.get("time").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        from_label: s.get("fromLabel").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        file_count: s.get("fileCount").and_then(|x| x.as_u64()).unwrap_or(0),
                    });
                }
            }
        }
    }
    out.retain(|s| !s.name.is_empty());
    out
}

/// 回滚：把指定保险快照写回目标端。
/// 安全：先给当前目标端做一份"回滚前自保"快照；凭据文件绝不写。
pub fn rollback_switch(repo: &Path, target_home: &Path, snapshot_name: &str) -> Result<RollbackResult, String> {
    let running = detect_dsh_processes();
    if !running.is_empty() {
        return Err(format!("检测到 DSH 正在运行（{}），请先关闭再回滚。", running.join(", ")));
    }
    if read_adopt_record(repo, target_home).is_none() {
        return Err("目标环境未接管，无法回滚。".to_string());
    }
    let tgt_id = home_id_of(target_home);
    let tgt_files = repo_files_dir(repo, &tgt_id);
    let snap_root = repo.join("switch-backups").join(&tgt_id).join(snapshot_name);
    if !snap_root.is_dir() {
        return Err(format!("保险快照不存在：{snapshot_name}"));
    }
    // v5 模块三：回滚前校验快照完整性，损坏则拒绝（防止回滚出半残环境）。
    let (total, bad, issues) = verify_snapshot_integrity(&snap_root);
    if bad > 0 {
        return Err(format!(
            "保险快照已损坏（{} 个文件中 {} 个有问题），为安全起见已取消回滚。{}",
            total, bad, issues.first().map(|s| format!(" 例：{}", s)).unwrap_or_default()
        ));
    }

    // 1. 回滚前自保：把目标端当前内容存一份，防回滚错方向。
    let self_snap = repo.join("switch-backups").join(&tgt_id)
        .join(format!("{}-回滚前自保", chrono::Local::now().format("%Y%m%d-%H%M%S")));
    for dir in ["sessions", "skills", ".agent-presets", "guard", "storages", "rollbacks", "undo-snapshots", "memories", "team"] {
        let store = tgt_files.join(dir);
        if store.is_dir() {
            copy_dir_recursive(&store, &self_snap.join(dir)).map_err(|e| format!("回滚前自保失败：{e}"))?;
        }
    }

    // 2. 把快照里的 9 类目录写回目标端存档（镜像：先清再写）
    let mut restored = 0u32;
    let mut warnings = Vec::new();
    for dir in ["sessions", "skills", ".agent-presets", "guard", "storages", "rollbacks", "undo-snapshots", "memories", "team"] {
        let src = snap_root.join(dir);
        if !src.is_dir() { continue; }
        let dst = tgt_files.join(dir);
        if dst.is_dir() {
            fs::remove_dir_all(&dst).map_err(|e| format!("清理目标 {dir} 失败：{e}"))?;
        }
        copy_dir_recursive(&src, &dst).map_err(|e| format!("回滚 {dir} 失败：{e}"))?;
        restored += 1;
    }

    // 3. settings.yaml：快照里存的是 home-settings.yaml，写回 home 根
    let mut restored_settings = false;
    let snap_settings = snap_root.join("home-settings.yaml");
    if snap_settings.is_file() {
        let dst = target_home.join("settings.yaml");
        fs::copy(&snap_settings, &dst).map_err(|e| format!("回滚 settings.yaml 失败：{e}"))?;
        restored_settings = true;
    }
    // 凭据文件永远不在保险快照里，也绝不写——明示
    warnings.push("凭据（API Key / 登录态）不在回滚范围，保持你当前的登录不受影响。".to_string());
    if restored == 0 && !restored_settings {
        return Err("保险快照里没有可回滚的内容。".to_string());
    }
    Ok(RollbackResult { restored_dirs: restored, restored_settings, snapshot: snapshot_name.to_string(), warnings })
}

/// v4.2 模块D：动态跟随的变化指纹。
/// 只扫 sessions / skills 两个动态目录，取"文件数 + 最新 mtime（毫秒）"，
/// 不哈希、不解压，毫秒级完成，供启动时快速比对"有没有新东西"。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WatchFingerprint {
    pub sessions_files: u64,
    pub sessions_latest_ms: u64,
    pub skills_files: u64,
    pub skills_latest_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchChange {
    pub home: String,
    pub label: String,
    pub changed: bool,
    pub new_sessions: i64,
    pub new_skills: i64,
    pub fingerprint: WatchFingerprint,
}

fn dir_fingerprint(root: &Path) -> (u64, u64) {
    if !root.is_dir() {
        return (0, 0);
    }
    let mut count = 0u64;
    let mut latest = 0u64;
    for entry in walkdir::WalkDir::new(root).follow_links(true).into_iter().filter_map(Result::ok).take(5000) {
        if !entry.file_type().is_file() { continue; }
        count += 1;
        if let Ok(meta) = entry.metadata() {
            if let Ok(m) = meta.modified() {
                if let Ok(d) = m.duration_since(std::time::UNIX_EPOCH) {
                    latest = latest.max(d.as_millis() as u64);
                }
            }
        }
    }
    (count, latest)
}

pub fn watch_fingerprint(home: &Path) -> WatchFingerprint {
    let (sf, sm) = dir_fingerprint(&home.join("sessions"));
    let (kf, km) = dir_fingerprint(&home.join("skills"));
    WatchFingerprint { sessions_files: sf, sessions_latest_ms: sm, skills_files: kf, skills_latest_ms: km }
}

/// 对比当前指纹与仓库里记录的指纹（存在 adopted 记录旁）。
/// 返回每个已接管环境的变化情况。
pub fn watch_check(repo: &Path) -> Vec<WatchChange> {
    let mut out = Vec::new();
    let adopted_root = repo.join("adopted");
    if !adopted_root.is_dir() { return out; }
    for entry in fs::read_dir(&adopted_root).into_iter().flatten().flatten() {
        let record_file = entry.path().join("record.json");
        let Ok(text) = fs::read_to_string(&record_file) else { continue };
        let Ok(record) = serde_json::from_str::<AdoptRecord>(&text) else { continue };
        let home = PathBuf::from(&record.home_path);
        if !home.is_dir() { continue; }
        let label = home.file_name().and_then(|v| v.to_str()).unwrap_or("环境").to_string();
        let current = watch_fingerprint(&home);
        let saved = read_watch_fingerprint(repo, &record.home_path);
        let changed = saved.as_ref().map(|s| *s != current).unwrap_or(true);
        let (ns, nk) = saved.as_ref().map(|s| {
            (current.sessions_files as i64 - s.sessions_files as i64,
             current.skills_files as i64 - s.skills_files as i64)
        }).unwrap_or((0, 0));
        out.push(WatchChange {
            home: record.home_path.clone(),
            label,
            changed,
            new_sessions: ns,
            new_skills: nk,
            fingerprint: current,
        });
    }
    out
}

/// 收录完成后，把当前指纹写回（下次比对基准）。
pub fn watch_mark_synced(repo: &Path, home: &Path) -> Result<(), String> {
    let fp = watch_fingerprint(home);
    let id = home_id_of(home);
    let file = repo.join("adopted").join(&id).join("watch.json");
    let text = serde_json::to_string(&fp).map_err(|e| format!("指纹序列化失败：{e}"))?;
    fs::write(&file, text).map_err(|e| format!("指纹写入失败：{e}"))?;
    Ok(())
}

fn read_watch_fingerprint(repo: &Path, home: &str) -> Option<WatchFingerprint> {
    let id = home_id_of(Path::new(home));
    let file = repo.join("adopted").join(&id).join("watch.json");
    let text = fs::read_to_string(&file).ok()?;
    serde_json::from_str(&text).ok()
}

/// v4.2 模块C：切换前预检（只读，不写任何东西）。
/// 给前端在弹确认框前拿到"风险清单"，让用户知情后再点确认。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchPreflight {
    /// 源端 sessions 最近 10 秒内是否有写入（源 DSH 可能开着）
    pub source_writing: bool,
    /// 目标端有、源端没有的会话条数（切换后会被移入保险快照）
    pub target_only_sessions: usize,
    /// 源端是否有 settings.yaml 可带
    pub source_has_settings: bool,
    /// 逐条提示文案（前端直接渲染）
    pub notices: Vec<String>,
}

pub fn switch_preflight(repo: &Path, source_home: &Path, target_home: &Path) -> SwitchPreflight {
    let mut notices = Vec::new();

    // 1. 源端是否正在写入：看 sessions 里最近 10 秒的 mtime
    let mut source_writing = false;
    let src_sessions = source_home.join("sessions");
    if src_sessions.is_dir() {
        let now = std::time::SystemTime::now();
        for entry in walkdir::WalkDir::new(&src_sessions).max_depth(3).into_iter().filter_map(Result::ok).take(400) {
            if !entry.file_type().is_file() { continue; }
            if let Ok(meta) = entry.metadata() {
                if let Ok(mtime) = meta.modified() {
                    if now.duration_since(mtime).map(|d| d.as_secs() < 10).unwrap_or(false) {
                        source_writing = true;
                        break;
                    }
                }
            }
        }
    }
    if source_writing {
        notices.push("源环境的对话目录 10 秒内有写入，源 DSH 可能还开着。建议先关闭源端再切，避免切到写了一半的对话。".to_string());
    }

    // 2. 目标端独有会话（集合差，而不是数量差——两边条数相同但内容不同时数量差会漏报）
    let collect_ids = |root: &Path| -> std::collections::HashSet<String> {
        let mut ids = std::collections::HashSet::new();
        let sessions = root.join("sessions");
        if !sessions.is_dir() { return ids; }
        for proj in fs::read_dir(&sessions).into_iter().flatten().flatten() {
            if let Ok(entries) = fs::read_dir(proj.path()) {
                for entry in entries.flatten() {
                    if entry.path().is_dir() {
                        ids.insert(entry.file_name().to_string_lossy().to_string());
                    }
                }
            }
        }
        ids
    };
    let src_id = home_id_of(source_home);
    let tgt_id = home_id_of(target_home);
    let src_store = repo_files_dir(repo, &src_id);
    let tgt_store = repo_files_dir(repo, &tgt_id);
    let src_ids = collect_ids(&src_store);
    let tgt_ids = collect_ids(&tgt_store);
    let target_only = tgt_ids.difference(&src_ids).count();
    if target_only > 0 {
        notices.push(format!(
            "目标环境有 {target_only} 条来源环境没有的独有对话：替换模式下会先存进保险快照（可回滚），合并模式下保持不动。"
        ));
    }

    // 3. settings.yaml
    let source_has_settings = source_home.join("settings.yaml").is_file();
    if source_has_settings {
        notices.push("会把源端的模型提供方配置（settings.yaml）一并带过去，API Key 不带。".to_string());
    } else {
        notices.push("源端没有 settings.yaml（可能用的是 profile 配置）。切换后若历史对话报缺提供方，请在目标版本设置里检查。".to_string());
    }

    SwitchPreflight { source_writing, target_only_sessions: target_only, source_has_settings, notices }
}

/// 把 source_home 的内容切到 target_home：target 的 symlink 改指向 source 的仓库目录。
/// 类型：sessions/skills/config/memories。切换前给 target 做保险快照（记录清单 + 备份小文件）。
pub fn switch_links(
    repo: &Path,
    source_home: &Path,
    target_home: &Path,
    include_sessions: bool,
    include_skills: bool,
    include_config: bool,
    include_memories: bool,
    include_presets: bool,
    replace_mode: bool,
) -> Result<SwitchResult, String> {
    // 1. 校验
    if source_home == target_home {
        return Err("来源与目标不能是同一个环境。".to_string());
    }
    let running = detect_dsh_processes();
    if !running.is_empty() {
        return Err(format!("检测到 DSH 正在运行（{}），请先完全关闭后再切换。", running.join(", ")));
    }
    if read_adopt_record(repo, source_home).is_none() {
        return Err("来源环境未接管，请先在「环境」页接管。".to_string());
    }
    if read_adopt_record(repo, target_home).is_none() {
        return Err("目标环境未接管，请先在「环境」页接管。".to_string());
    }

    // 2. 确定要切换的子目录
    let mut dirs: Vec<&str> = Vec::new();
    if include_sessions { dirs.push("sessions"); }
    if include_skills { dirs.push("skills"); }
    if include_config { dirs.extend(["guard", "storages", "rollbacks", "undo-snapshots"]); }
    // v4.1：.agent-presets 是实验性 preset（如 anchored-standard 走 gateway 路由），
    // 跨版本复制会让官方版报「Unknown agent preset」。默认不跨版本带，用户明确勾选才带。
    if include_presets { dirs.push(".agent-presets"); }
    if include_memories { dirs.extend(["memories", "team"]); }
    if dirs.is_empty() {
        return Err("未选择任何要切换的内容类型。".to_string());
    }

    let src_id = home_id_of(source_home);
    let tgt_id = home_id_of(target_home);
    let src_files = repo_files_dir(repo, &src_id);
    let tgt_files = repo_files_dir(repo, &tgt_id);
    fs::create_dir_all(&tgt_files).map_err(|e| format!("创建目标存档目录失败：{e}"))?;

    // 来源标签（用于保险快照命名）
    let src_label = source_home.file_name().and_then(|v| v.to_str()).unwrap_or("source").to_string();

    // 3. 给目标环境做保险快照：把目标自己的原件完整复制到命名快照目录
    let snap_name = format!(
        "{}-切自{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S"),
        src_label
    );
    let snap_root = repo.join("switch-backups").join(&tgt_id).join(&snap_name);
    let mut details = Vec::new();
    let mut warnings = Vec::new();

    for dir in &dirs {
        let tgt_store = tgt_files.join(dir);
        if tgt_store.is_dir() {
            let dst = snap_root.join(dir);
            copy_dir_recursive(&tgt_store, &dst)
                .map_err(|e| format!("备份目标的 {dir} 失败：{e}"))?;
        }
    }
    // v4.2 模块B：目标 home 根的 settings.yaml 若存在，一并收进保险快照（回滚用）。
    let tgt_settings = target_home.join("settings.yaml");
    if tgt_settings.is_file() {
        let dst = snap_root.join("home-settings.yaml");
        fs::copy(&tgt_settings, &dst).map_err(|e| format!("备份目标端 settings.yaml 失败：{e}"))?;
    }

    // v8 修复一：合并模式下 copy_dir_merge 会用来源的同名文件覆盖目标，
    // 其中 storages/workspace.json 一旦被覆盖，「目标端登记全部保留」就形同虚设。
    // 复制前先留一份目标原件，复制后写回再与来源合并。
    // v8 修复二：对话的「可见性」由 workspace.json 决定——只勾「对话记录」时
    // 也必须同步这张登记表，否则文件搬过去了、侧栏依然看不到。
    let sync_workspace_index = include_sessions || dirs.contains(&"storages");
    let tgt_ws_saved: Option<Vec<u8>> = if !replace_mode && sync_workspace_index {
        fs::read(tgt_files.join("storages").join("workspace.json")).ok()
    } else {
        None
    };

    // 4. 复制式切换：把来源内容复制进目标自己的存档（链接不动，仍指向自己）
    let mut switched = 0u32;
    for dir in &dirs {
        let src_store = src_files.join(dir);
        if !src_store.is_dir() {
            continue; // 来源没有该类型内容
        }
        let tgt_store = tgt_files.join(dir);
        if replace_mode {
            // 替换模式（需用户显式勾选）：清空目标旧内容（已备份到保险快照），再复制来源内容
            if tgt_store.is_dir() {
                fs::remove_dir_all(&tgt_store).map_err(|e| format!("清空目标 {dir} 失败：{e}"))?;
            }
            copy_dir_recursive(&src_store, &tgt_store)
                .map_err(|e| format!("把来源的 {dir} 复制给目标失败：{e}"))?;
            details.push(format!("{dir} 已替换"));
        } else {
            // 合并模式（默认）：来源内容复制进来，目标端独有内容一律保留
            let src_count = count_files(&src_store);
            copy_dir_merge(&src_store, &tgt_store)
                .map_err(|e| format!("把来源的 {dir} 合并给目标失败：{e}"))?;
            let total_after = count_files(&tgt_store);
            let kept = total_after.saturating_sub(src_count);
            details.push(format!("{dir} 已合并（保留目标端独有约 {kept} 个文件）"));
        }
        switched += 1;
    }
    if switched == 0 {
        return Err("没有可切换的内容：来源环境在所选类型下都没有内容。".to_string());
    }

    // v7 合并模式：workspace.json 结构化合并（目标端已有的工作区与对话登记全部保留）
    // v8：先补回上面被 copy_dir_merge 覆盖掉的目标原件，再做合并，否则合并的是「来源自己」；
    //      且只要涉及对话就执行（不再要求同时勾了「配置」）。
    if !replace_mode && sync_workspace_index {
        let src_ws = src_files.join("storages").join("workspace.json");
        let tgt_ws = tgt_files.join("storages").join("workspace.json");
        if let Some(bytes) = &tgt_ws_saved {
            if let Some(parent) = tgt_ws.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Err(e) = fs::write(&tgt_ws, bytes) {
                warnings.push(format!("合并前恢复目标端 workspace.json 原件失败：{e}"));
            }
        }
        if src_ws.is_file() {
            match crate::migrate::merge_workspace_json(&tgt_ws, &src_ws) {
                Ok((ws_added, sess_added)) => details.push(format!(
                    "workspace.json 已合并（新增工作区 {ws_added} 个、新增对话登记 {sess_added} 条，目标端原登记全部保留）"
                )),
                Err(e) => warnings.push(format!("workspace.json 合并失败（保持目标原样，未改动）：{e}")),
            }
        }
    }

    // v7 对话登记修复：把磁盘上但没登记的对话补登记（只增不删，两种模式都跑）
    let mut registry_repaired = 0usize;
    let mut sessions_missing_dir = 0usize;
    let mut sessions_archived = 0usize;
    if include_sessions {
        if let Ok(rep) = crate::migrate::repair_session_registry(target_home) {
            registry_repaired = rep.registered;
            sessions_missing_dir = rep.missing_dir;
            sessions_archived = rep.archived;
            if rep.registered > 0 {
                details.push(format!("对话登记修复：补登记 {} 条磁盘上有但侧栏没登记的对话", rep.registered));
            }
            for note in &rep.notes {
                if note.contains("不存在") || note.contains("没有记录工作目录") || note.contains("归档") {
                    warnings.push(note.clone());
                }
            }
        }
    }

    // 5. 写保险快照索引
    let index_path = repo.join("switch-backups").join(&tgt_id).join("index.json");
    let mut index: serde_json::Value = fs::read_to_string(&index_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({"snapshots": []}));
    let file_count = count_files(&snap_root);
    // v5 模块三：给保险快照生成 manifest（每文件 sha256），回滚时据此校验完整性。
    let snap_manifest = build_snapshot_manifest(&snap_root);
    if let Ok(text) = serde_json::to_string(&snap_manifest) {
        let _ = fs::write(snap_root.join("manifest.json"), text);
    }
    index["snapshots"].as_array_mut().map(|arr| {
        arr.push(serde_json::json!({
            "name": snap_name,
            "time": now_string(),
            "from": source_home.to_string_lossy(),
            "fromLabel": src_label,
            "dirs": dirs,
            "fileCount": file_count,
        }));
    });
    if let Ok(text) = serde_json::to_string_pretty(&index) {
        let _ = fs::write(&index_path, text);
    }

    // 切换涉及 config/preset 时，凭据不会迁移（密钥环保护，文件搬过去也解密不了）。
    // 前端据此提示用户「需在目标版本重新录入 API Key」。
    let credential_note = include_config || include_presets;
    if credential_note {
        warnings.push("凭据（API Key）不会随切换迁移。若目标环境发不出消息，请在该版本设置里重新录入 API Key。".to_string());
    }

    // v4.2 模块B：把源端 settings.yaml 复制到目标 home 根（不含任何密钥，只有提供方目录）。
    // 依据本机实证：官方版启动时会把 legacy settings.yaml 自动导入并改名 .imported。
    // 这是"能看不能聊"的根治：会话记住的模型路由（qiu005 等）在目标端才能解析到提供方。
    let mut settings_copied = false;
    let src_settings = source_home.join("settings.yaml");
    if src_settings.is_file() {
        let dst = target_home.join("settings.yaml");
        // 目标已有 settings.yaml：已在上方保险快照收录为 home-settings.yaml，这里直接覆盖。
        match fs::copy(&src_settings, &dst) {
            Ok(_) => {
                settings_copied = true;
                details.push("settings.yaml 已随切换带过去（官方版重启后会自动导入）".to_string());
                warnings.push("模型提供方配置已带过去，但 API Key 不在文件里（密钥环绑定）。若历史对话用的是自定义提供方，请在目标版本设置里重新粘贴对应 Key。".to_string());
            }
            Err(e) => {
                warnings.push(format!("settings.yaml 复制失败（{e}）。目标端可能缺少模型提供方配置，部分历史对话会「能看不能聊」。"));
            }
        }
    }

    // v4.1 模块④：skills 路径差异提示。
    // 同学排查报告 03.4 已证实：官方版实际用 ~/.agents\skills（共享库），
    // 而被接管/切换的是 <home>\skills，两套路径不一致会导致「切了 skills 但在官方版里看不到」。
    if include_skills {
        let agents_root = std::env::var_os("DSH_AGENTS_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("USERPROFILE")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".agents")
            });
        let agents_skills = agents_root.join("skills");
        let tgt_skills = target_home.join("skills");
        if agents_skills.is_dir() && tgt_skills.is_dir() {
            warnings.push(format!(
                "提示：本机存在共享技能库 {}。部分版本（如官方版）的技能从这里读取，而不是从本环境的 skills 目录。若切换后技能未生效，可在「环境」页把共享技能库也接管。",
                agents_skills.display()
            ));
        }
    }

    // v4.3 预设兼容改写：切了对话记录后，把目标端不认识的 agentPreset 改写成官方内置 standard。
    // 根治"Unknown agent preset: anchored-standard"——用户截图实证这是 resume 失败的直接原因。
    let mut preset_fixed_count = 0u32;
    if include_sessions {
        let adapters = crate::adapters::Adapters::load(repo);
        let report = crate::preset_fix::fix_unknown_presets(target_home, &adapters);
        preset_fixed_count = report.rewritten;
        if report.rewritten > 0 {
            details.push(report.notes.join(" "));
        }
        if report.failed > 0 {
            warnings.push(format!("{} 条对话读取异常未做预设兼容（不影响其它对话）。", report.failed));
        }
    }

    // v6 提供方携带：目标端可能没有源端会话需要的模型提供方（如 xiaomi-token-plan-cn），
    // 缺失时发消息会报 NO_ADAPTER。这里把源端提供方定义补齐到目标配置（不搬密钥）。
    let mut provider_added: Vec<String> = Vec::new();
    let mut keys_to_enter: Vec<String> = Vec::new();
    if include_sessions || include_config {
        let defs = crate::providers::collect_provider_defs(source_home);
        if !defs.is_empty() {
            match crate::providers::ensure_providers(target_home, &defs) {
                Ok(rep) => {
                    provider_added = rep.added.clone();
                    keys_to_enter = rep.keys_to_enter.clone();
                    for n in rep.notes { warnings.push(n); }
                }
                Err(e) => warnings.push(format!("提供方配置补齐失败（不影响对话文件）：{e}")),
            }
        }
    }

    Ok(SwitchResult {
        switched_links: switched,
        backup_snapshot: snap_name,
        details,
        warnings,
        credential_note,
        settings_copied,
        preset_fixed: preset_fixed_count,
        provider_added,
        keys_to_enter,
        replace_mode,
        registry_repaired,
        sessions_missing_dir,
        sessions_archived,
    })
}


// ===================== 错位修复（repair）=====================

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepairAction {
    pub rel: String,
    pub action: String,       // "repoint" | "remove" | "ok" | "skip"
    pub from: String,         // 当前指向（root id 或描述）
    pub to: String,           // 修复后指向
    pub reason: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepairPreview {
    pub home_id: String,
    pub home_path: String,
    pub actions: Vec<RepairAction>,
    pub broken_count: u32,
}

/// 从 symlink 的 target 解析出它指向的 root id（形如 .../adopted/<root>/files/<dir>）。
fn root_of_link_target(target: &Path) -> Option<String> {
    let comps: Vec<String> = target.components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect();
    for (i, c) in comps.iter().enumerate() {
        if c == "adopted" && i + 1 < comps.len() {
            return Some(comps[i + 1].clone());
        }
    }
    None
}

/// 分析一个 home 的错位情况（只读，不修改）。
pub fn preview_repair(repo: &Path, home_path: &Path) -> Result<RepairPreview, String> {
    let home_id = home_id_of(home_path);
    let files_root = repo_files_dir(repo, &home_id);
    let mut actions = Vec::new();
    let mut broken = 0u32;

    for dir in ADOPT_DIRS {
        let link_path = home_path.join(dir);
        let Ok(meta) = link_path.symlink_metadata() else { continue };
        if !meta.file_type().is_symlink() {
            continue; // 真实目录：未接管该目录，不动
        }
        let target = fs::read_link(&link_path).map_err(|e| format!("读取链接 {dir} 失败：{e}"))?;
        // 相对路径转绝对
        let abs_target = if target.is_absolute() {
            target.clone()
        } else {
            link_path.parent().unwrap_or(home_path).join(&target)
        };
        let pointed_root = root_of_link_target(&abs_target).unwrap_or_default();
        if pointed_root == home_id {
            actions.push(RepairAction {
                rel: dir.to_string(),
                action: "ok".into(),
                from: pointed_root,
                to: home_id.clone(),
                reason: "指向自己，正常".into(),
            });
            continue;
        }
        // 错位
        broken += 1;
        let own_store = files_root.join(dir);
        if own_store.is_dir() {
            actions.push(RepairAction {
                rel: dir.to_string(),
                action: "repoint".into(),
                from: pointed_root.clone(),
                to: home_id.clone(),
                reason: "指回自己的存档（自己的内容在仓库中完好）".into(),
            });
        } else {
            // 自己的仓库里没有：看保险快照能不能补
            let mut found_in_backup = false;
            let backups = repo.join("switch-backups").join(&home_id);
            if backups.is_dir() {
                if let Ok(mut snaps) = fs::read_dir(&backups).map(|rd| rd.flatten().collect::<Vec<_>>()) {
                    snaps.sort_by_key(|e| e.file_name());
                    if let Some(last) = snaps.last() {
                        if last.path().join(dir).is_dir() {
                            found_in_backup = true;
                        }
                    }
                }
                let _ = &mut found_in_backup;
            }
            if found_in_backup {
                actions.push(RepairAction {
                    rel: dir.to_string(),
                    action: "repoint".into(),
                    from: pointed_root.clone(),
                    to: home_id.clone(),
                    reason: "先从保险快照找回自己的内容，再指回".into(),
                });
            } else {
                actions.push(RepairAction {
                    rel: dir.to_string(),
                    action: "remove".into(),
                    from: pointed_root.clone(),
                    to: String::new(),
                    reason: "这个目录本来不属于此环境（接管时不存在、保险快照也没有），删除错误链接；原内容在别人的存档里不受影响".into(),
                });
            }
        }
    }

    Ok(RepairPreview {
        home_id,
        home_path: home_path.to_string_lossy().to_string(),
        actions,
        broken_count: broken,
    })
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepairResult {
    pub home_id: String,
    pub repointed: Vec<String>,
    pub removed: Vec<String>,
    pub restored_from_backup: Vec<String>,
    pub warnings: Vec<String>,
}

/// 执行错位修复：让每个链接各回各家。
pub fn repair_links(repo: &Path, home_path: &Path) -> Result<RepairResult, String> {
    let running = detect_dsh_processes();
    if !running.is_empty() {
        return Err(format!(
            "检测到 DSH 正在运行（{}），请先完全关闭所有 DSH 窗口后再修复。",
            running.join(", ")
        ));
    }
    let preview = preview_repair(repo, home_path)?;
    let home_id = preview.home_id.clone();
    let files_root = repo_files_dir(repo, &home_id);
    fs::create_dir_all(&files_root).map_err(|e| format!("创建存档目录失败：{e}"))?;

    let mut repointed = Vec::new();
    let mut removed = Vec::new();
    let mut restored = Vec::new();
    let mut warnings = Vec::new();

    for action in &preview.actions {
        let link_path = home_path.join(&action.rel);
        match action.action.as_str() {
            "repoint" => {
                let own_store = files_root.join(&action.rel);
                // 自己的存档不存在时，先从最新保险快照恢复
                if !own_store.is_dir() {
                    let backups = repo.join("switch-backups").join(&home_id);
                    let mut latest: Option<PathBuf> = None;
                    if let Ok(mut snaps) = fs::read_dir(&backups).map(|rd| rd.flatten().collect::<Vec<_>>()) {
                        snaps.sort_by_key(|e| e.file_name());
                        for s in snaps.into_iter().rev() {
                            if s.path().join(&action.rel).is_dir() {
                                latest = Some(s.path().join(&action.rel));
                                break;
                            }
                        }
                    }
                    let Some(src_snap) = latest else {
                        warnings.push(format!("{} 找不到自己的存档，跳过", action.rel));
                        continue;
                    };
                    copy_dir_recursive(&src_snap, &own_store)
                        .map_err(|e| format!("从保险快照恢复 {} 失败：{e}", action.rel))?;
                    restored.push(action.rel.clone());
                }
                // 删除错位的 symlink，指回自己
                if link_path.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false) {
                    fs::remove_dir(&link_path).map_err(|e| format!("删除错位链接 {} 失败：{e}", action.rel))?;
                }
                create_dir_link(&link_path, &own_store)
                    .map_err(|e| format!("为 {} 创建归位链接失败：{e}", action.rel))?;
                repointed.push(action.rel.clone());
            }
            "remove" => {
                if link_path.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false) {
                    fs::remove_dir(&link_path).map_err(|e| format!("删除多余链接 {} 失败：{e}", action.rel))?;
                    removed.push(action.rel.clone());
                }
            }
            _ => {}
        }
    }

    Ok(RepairResult {
        home_id,
        repointed,
        removed,
        restored_from_backup: restored,
        warnings,
    })
}


// ===================== AI 小纸条（接管时自动塞）=====================

const NOTE_SKILL_DIR: &str = "dsh-vault-manager";
const NOTE_BEGIN: &str = "<!-- DSH-VAULT-NOTE-BEGIN -->";
const NOTE_END: &str = "<!-- DSH-VAULT-NOTE-END -->";

fn note_skill_md(home_label: &str, repo: &Path) -> String {
    format!(
        r#"---
name: dsh-vault-manager
description: 本环境的对话、技能与配置由 DSH Vault 统一备份与管理。当用户询问环境迁移、对话找回、备份恢复、或当前文件为何是链接时阅读本说明。
---

# 本环境由 DSH Vault 统一管理

你好，我是这个 DeepSeek Harness 环境的"管家纸条"。

## 你需要知道的事
- 「{home_label}」的对话记录（sessions/）、技能（skills/）、预设（.agent-presets/）等，**正由 DSH Vault 这个工具统一保管**。
- 你在本环境目录里看到的这些文件夹，其实是**链接（symlink）**，真正的文件存放在 DSH Vault 的仓库里：`{repo}`
- **这是正常现象，请放心照常读写**——读写到链接，就等于读写到仓库里的真身，DSH 和 DSH Vault 都会同步看到。

## 你可以做什么
- 正常使用本环境即可，不需要任何额外操作。
- 如果用户想**找回历史对话**、**把对话搬到另一个 DSH 版本**、或**备份/还原整个环境**，请引导用户打开「DSH Vault」程序，或调用它的 MCP 工具（scan_homes / create_backup / switch_env）。
- 如果某个链接失效（指向的目录不存在），说明仓库文件可能被移动了，请提醒用户打开 DSH Vault 检查，不要自行删除链接。

## 不要做的事
- 不要删除 sessions/、skills/ 等目录链接（那会切断与仓库的连接）。
- 不要把这些链接目录当成普通文件夹整体移动或压缩（会只打包到链接本身而非真实内容）。
"#,
        home_label = home_label,
        repo = repo.to_string_lossy()
    )
}

/// 接管后：往 skills/ 塞小纸条技能，并往 AGENTS.md 追加一行。
pub fn write_ai_note(repo: &Path, home_path: &Path) -> Result<(), String> {
    let home_label = home_path
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("这个环境")
        .to_string();
    // 1) skills/dsh-vault-manager/SKILL.md（skills 已被接管为链接，写到链接里即写到仓库）
    let skills = home_path.join("skills");
    if skills.is_dir() {
        let dir = skills.join(NOTE_SKILL_DIR);
        fs::create_dir_all(&dir).map_err(|e| format!("创建小纸条目录失败：{e}"))?;
        fs::write(dir.join("SKILL.md"), note_skill_md(&home_label, repo))
            .map_err(|e| format!("写入小纸条失败：{e}"))?;
    }
    // 2) AGENTS.md 追加一行（幂等：已有则跳过）
    let agents_md = home_path.join("AGENTS.md");
    let pointer = format!(
        "{NOTE_BEGIN}
- 本环境的对话/技能/配置由 **DSH Vault** 统一管理（本地为链接，实体在仓库 `{repo}`）。详见 `skills/{NOTE_SKILL_DIR}/SKILL.md`。
{NOTE_END}",
        repo = repo.to_string_lossy(),
        NOTE_SKILL_DIR = NOTE_SKILL_DIR,
        NOTE_BEGIN = NOTE_BEGIN,
        NOTE_END = NOTE_END
    );
    let existing = fs::read_to_string(&agents_md).unwrap_or_default();
    if !existing.contains(NOTE_BEGIN) {
        let mut content = existing;
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(&pointer);
        content.push('\n');
        fs::write(&agents_md, content).map_err(|e| format!("写入 AGENTS.md 失败：{e}"))?;
    }
    Ok(())
}

/// 断开接管时：移除小纸条。
pub fn remove_ai_note(home_path: &Path) -> Result<(), String> {
    let skills = home_path.join("skills");
    let dir = skills.join(NOTE_SKILL_DIR);
    if dir.is_dir() {
        let _ = fs::remove_dir_all(&dir);
    }
    // 移除 AGENTS.md 里的小纸条块
    let agents_md = home_path.join("AGENTS.md");
    if let Ok(existing) = fs::read_to_string(&agents_md) {
        if existing.contains(NOTE_BEGIN) {
            let mut out = String::new();
            let mut in_block = false;
            for line in existing.lines() {
                if line.contains(NOTE_BEGIN) {
                    in_block = true;
                    continue;
                }
                if line.contains(NOTE_END) {
                    in_block = false;
                    continue;
                }
                if !in_block {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            let _ = fs::write(&agents_md, out);
        }
    }
    Ok(())
}

/// 回滚部分接管：删除已建链接、把已移动目录搬回原位。
fn rollback_partial(files_root: &Path, home_path: &Path, links: &[LinkMapping], warnings: &mut Vec<String>) {
    for link in links {
        let link_path = home_path.join(&link.rel);
        let store_path = files_root.join(&link.rel);
        // 删链接（不删目标）
        if link_path.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            let _ = fs::remove_dir(&link_path);
        }
        // 搬回
        if store_path.is_dir() && !link_path.exists() {
            if move_dir(&store_path, &link_path).is_err() {
                warnings.push(format!("回滚 {} 失败，请手动检查", link.rel));
            }
        }
    }
}

/// 查询指定路径所在盘的可用字节数。
#[cfg(windows)]
pub fn disk_free_bytes(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut free: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free as *mut u64,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok != 0 { Some(free) } else { None }
}

#[cfg(not(windows))]
pub fn disk_free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// 预估待接管目录的总字节数（sessions/skills 等小目录，不含 profiles）。
pub fn estimate_adopt_bytes(home_path: &Path) -> u64 {
    let mut total = 0u64;
    for dir in ADOPT_DIRS {
        let d = home_path.join(dir);
        if !d.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&d).follow_links(false).into_iter().filter_map(Result::ok) {
            if entry.file_type().is_file() {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// 链接能力预检：能否在当前权限下创建目录链接（junction 免管理员）。
/// 在 home 旁试建一个临时 junction，成功即可放心接管；失败则说明需开开发者模式/管理员。
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkCapability {
    /// junction 是否可用（免管理员）
    pub junction_ok: bool,
    /// symlink 是否可用（需开发者模式/管理员）
    pub symlink_ok: bool,
    /// 给用户看的结论与建议
    pub advice: String,
}

pub fn check_link_capability(home_path: &Path) -> LinkCapability {
    let parent = home_path.parent().unwrap_or(home_path);
    let probe_target = parent.join(".dsh-vault-cap-probe-target");
    let probe_link = parent.join(".dsh-vault-cap-probe-link");

    // 清理历史残留
    let _ = fs::remove_dir(&probe_link);
    let _ = fs::remove_dir_all(&probe_target);
    let _ = fs::create_dir_all(&probe_target);

    // 1) junction 能力（免管理员）
    let junction_ok = junction::create(&probe_link, &probe_target).is_ok();
    let _ = fs::remove_dir(&probe_link);

    // 2) symlink 能力（仅 Windows 上有意义；非 Windows 默认可用）
    #[cfg(windows)]
    let symlink_ok = {
        std::os::windows::fs::symlink_dir(&probe_target, &probe_link).is_ok()
    };
    #[cfg(not(windows))]
    let symlink_ok = true;
    let _ = fs::remove_dir(&probe_link);
    let _ = fs::remove_dir_all(&probe_target);

    let advice = if junction_ok {
        "当前权限可直接接管（使用免管理员的目录联接）。".to_string()
    } else if symlink_ok {
        "junction 不可用，但可用符号链接接管。".to_string()
    } else {
        "当前权限无法创建目录链接。请开启 Windows 开发者模式（设置→系统→开发者选项→开发者模式），或右键以管理员身份运行本工具后重试。".to_string()
    };

    LinkCapability { junction_ok, symlink_ok, advice }
}

/// 半完成状态描述：某目录被搬进了仓库，但原位置没有链接（历史接管中断残留）。
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartialAdoption {
    pub home_id: String,
    /// 形如 sessions/skills：在仓库有内容、但原位置缺失且不是链接。
    pub stranded_dirs: Vec<String>,
}

/// 检测半完成状态（不依赖 record.json，直接看文件系统）。
pub fn detect_partial_adoption(repo: &Path, home_path: &Path) -> Option<PartialAdoption> {
    let home_id = home_id_of(home_path);
    let files_root = repo_files_dir(repo, &home_id);
    if !files_root.is_dir() {
        return None;
    }
    let mut stranded = Vec::new();
    for dir in ADOPT_DIRS {
        let store = files_root.join(dir);
        let origin = home_path.join(dir);
        let is_link = origin.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false);
        // 仓库有该目录内容，但原位置既不是链接也不存在 → 搁浅
        if store.is_dir() && !is_link && !origin.exists() {
            stranded.push(dir.to_string());
        }
    }
    if stranded.is_empty() {
        None
    } else {
        Some(PartialAdoption { home_id, stranded_dirs: stranded })
    }
}

/// 自愈半完成状态：把仓库里搁浅的目录搬回原位，恢复到「未接管」干净状态。
pub fn repair_partial_adoption(repo: &Path, home_path: &Path) -> Result<Vec<String>, String> {
    let partial = detect_partial_adoption(repo, home_path)
        .ok_or_else(|| "未检测到半完成状态，无需修复。".to_string())?;
    let home_id = home_id_of(home_path);
    let files_root = repo_files_dir(repo, &home_id);
    let mut restored = Vec::new();
    for dir in &partial.stranded_dirs {
        let store = files_root.join(dir);
        let origin = home_path.join(dir);
        if origin.exists() {
            // 原位置已有真实目录，避免覆盖，跳过
            continue;
        }
        move_dir(&store, &origin).map_err(|e| format!("还原 {dir} 失败：{e}"))?;
        restored.push(dir.clone());
    }
    Ok(restored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_same_volume() {
        let from = Path::new(r"C:\Users\me\.dsh");
        let to = Path::new(r"C:\Users\me\repo\files\sessions");
        let rel = relative_path(from, to).unwrap();
        assert_eq!(rel, PathBuf::from(r"..\repo\files\sessions"));
    }

    #[test]
    fn same_volume_check() {
        assert!(same_volume(Path::new(r"C:\a"), Path::new(r"C:\b")));
        assert!(!same_volume(Path::new(r"C:\a"), Path::new(r"D:\b")));
    }

    #[cfg(windows)]
    #[test]
    fn junction_create_and_readthrough() {
        // junction 在普通权限下即可创建，且能读穿到 target。
        let base = std::env::temp_dir().join(format!("dsh-junction-test-{}", std::process::id()));
        let target = base.join("target");
        let link = base.join("link");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("probe.txt"), b"junction-ok").unwrap();

        junction::create(&link, &target).expect("junction 创建应成功（免管理员）");

        // 被识别为 reparse point / symlink 语义
        let meta = link.symlink_metadata().unwrap();
        assert!(meta.file_type().is_symlink(), "junction 应被 symlink_metadata 识别");
        // 读穿
        let got = fs::read(link.join("probe.txt")).unwrap();
        assert_eq!(got, b"junction-ok");
        // 删除 junction 不影响 target
        fs::remove_dir(&link).unwrap();
        assert!(target.join("probe.txt").is_file(), "删除 junction 后 target 应完好");

        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(windows)]
    #[test]
    fn junction_target_is_absolute_nt_path() {
        // junction 的 target 必须解析为 NT 绝对路径；相对 target 也能正确处理。
        let base = std::env::temp_dir().join(format!("dsh-junction-rel-{}", std::process::id()));
        let target = base.join("real");
        fs::create_dir_all(&target).unwrap();
        let link = base.join("lnk");
        junction::create(&link, &target).expect("junction 应成功");
        assert!(link.is_dir(), "通过 junction 应能看到目录");
        fs::remove_dir(&link).unwrap();
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn e2e_detect_and_repair_partial_adoption() {
        // 模拟历史接管中断：sessions 被搬进仓库，但原位置没有链接。
        let base = std::env::temp_dir().join(format!("dsh-partial-{}", std::process::id()));
        let home = base.join("home");
        let repo = base.join("repo");
        let home_id = home_id_of(&home);
        let store = repo.join("adopted").join(&home_id).join("files").join("sessions").join("proj");
        fs::create_dir_all(&store).unwrap();
        fs::write(store.join("s.jsonl.zstd"), b"data").unwrap();
        // home 有 DSH 特征但 sessions 缺失（搁浅）
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join(".credentials.yaml"), b"t").unwrap();
        fs::write(home.join("settings.yaml"), b"agent-presets: {}").unwrap();

        // 应检测到半完成
        let partial = detect_partial_adoption(&repo, &home).expect("应检测到搁浅");
        assert!(partial.stranded_dirs.contains(&"sessions".to_string()));

        // 修复：sessions 搬回原位
        let restored = repair_partial_adoption(&repo, &home).expect("修复应成功");
        assert!(restored.contains(&"sessions".to_string()));
        assert!(home.join("sessions").join("proj").join("s.jsonl.zstd").is_file());
        // 修复后不再检测到搁浅
        assert!(detect_partial_adoption(&repo, &home).is_none());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn link_capability_check_reports_status() {
        // 预检应返回结论；本机普通权限下 junction 应可用（免管理员）。
        let base = std::env::temp_dir().join(format!("dsh-cap-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        let home = base.join("home");
        fs::create_dir_all(&home).unwrap();
        let cap = check_link_capability(&home);
        // 至少 junction 或 symlink 其一可用；普通权限下 junction 应可用
        assert!(cap.junction_ok || cap.symlink_ok, "本机应至少有一种链接方式可用");
        assert!(!cap.advice.is_empty(), "应给出文字建议");
        // 预检不应在 home 旁留下探针目录
        let leftover: Vec<_> = fs::read_dir(&base).unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("cap-probe"))
            .collect();
        assert!(leftover.is_empty(), "预检后不应残留探针目录");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn e2e_adopt_and_unadopt() {
        // 造一个模拟 DSH Home
        let base = std::env::temp_dir().join(format!("dsh-vault-e2e-{}", std::process::id()));
        let home = base.join("home");
        let repo = base.join("repo");
        let sessions = home.join("sessions").join("proj").join("sess1");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(sessions.join("session.jsonl.zstd"), b"fake-zstd-data").unwrap();
        fs::create_dir_all(home.join("skills").join("myskill")).unwrap();
        fs::write(home.join("skills").join("myskill").join("SKILL.md"), b"# skill").unwrap();
        fs::write(home.join("settings.yaml"), b"agent-presets: {}").unwrap();
        fs::write(home.join(".credentials.yaml"), b"token: x").unwrap();
        fs::create_dir_all(&repo).unwrap();

        // adopt
        let result = adopt_home(&repo, &home, "e2e测试").expect("adopt 应成功");
        assert!(result.moved_dirs.contains(&"sessions".to_string()));
        assert!(result.moved_dirs.contains(&"skills".to_string()));

        // 验证：原位置 sessions 是 symlink
        let sess_link = home.join("sessions");
        let meta = sess_link.symlink_metadata().unwrap();
        assert!(meta.file_type().is_symlink(), "sessions 应成为 symlink");

        // 验证：通过 symlink 能读到仓库里的文件
        let content = fs::read(sess_link.join("proj").join("sess1").join("session.jsonl.zstd")).unwrap();
        assert_eq!(content, b"fake-zstd-data");

        // 验证：仓库里有接管记录
        let record = read_adopt_record(&repo, &home).expect("应有接管记录");
        assert!(record.links.iter().any(|l| l.rel == "sessions"));

        // unadopt
        let un = unadopt_home(&repo, &home).expect("unadopt 应成功");
        assert!(un.restored_dirs.contains(&"sessions".to_string()));
        // 还原后 sessions 是真实目录
        let meta2 = home.join("sessions").symlink_metadata().unwrap();
        assert!(!meta2.file_type().is_symlink(), "还原后 sessions 应是真实目录");
        assert_eq!(fs::read(home.join("sessions").join("proj").join("sess1").join("session.jsonl.zstd")).unwrap(), b"fake-zstd-data");

        let _ = fs::remove_dir_all(&base);
    }


    #[test]
    fn e2e_change_detection() {
        let base = std::env::temp_dir().join(format!("dsh-vault-chg-{}", std::process::id()));
        let home = base.join("home");
        let repo = base.join("repo");
        let sess = home.join("sessions").join("proj").join("s1");
        fs::create_dir_all(&sess).unwrap();
        fs::write(sess.join("session.jsonl.zstd"), b"original").unwrap();
        fs::write(home.join("settings.yaml"), b"agent-presets: {}").unwrap();
        fs::create_dir_all(&repo).unwrap();

        adopt_home(&repo, &home, "chg").unwrap();
        // 初始无变更
        let c0 = check_home_changes(&repo, &home).unwrap();
        assert!(c0.is_empty(), "接管后应立即无变更，实际 {:?}", c0.iter().map(|c|&c.rel).collect::<Vec<_>>());

        // 修改一个文件（通过 symlink）
        fs::write(home.join("sessions").join("proj").join("s1").join("session.jsonl.zstd"), b"modified").unwrap();
        // 新增一个文件
        fs::write(home.join("sessions").join("proj").join("new.jsonl.zstd"), b"new").unwrap();

        let c1 = check_home_changes(&repo, &home).unwrap();
        let kinds: std::collections::HashMap<_,_> = c1.iter().map(|c| (c.kind.as_str(), c.rel.clone())).collect();
        assert!(kinds.values().any(|r| r.contains("session.jsonl.zstd") ), "应检测到修改: {:?}", c1);
        assert!(c1.iter().any(|c| c.kind == "added"), "应检测到新增: {:?}", c1.iter().map(|x|(&x.kind,&x.rel)).collect::<Vec<_>>());

        let _ = fs::remove_dir_all(&base);
    }


    #[test]
    fn e2e_switch_links() {
        let base = std::env::temp_dir().join(format!("dsh-vault-sw-{}", std::process::id()));
        let repo = base.join("repo");
        // 来源环境 A：有自己的对话
        let home_a = base.join("homeA");
        fs::create_dir_all(home_a.join("sessions").join("projA").join("s1")).unwrap();
        fs::write(home_a.join("sessions").join("projA").join("s1").join("session.jsonl.zstd"), b"A-data").unwrap();
        fs::write(home_a.join("settings.yaml"), b"agent-presets: {}").unwrap();
        // 目标环境 B：有自己的对话
        let home_b = base.join("homeB");
        fs::create_dir_all(home_b.join("sessions").join("projB").join("s2")).unwrap();
        fs::write(home_b.join("sessions").join("projB").join("s2").join("session.jsonl.zstd"), b"B-data").unwrap();
        fs::write(home_b.join("settings.yaml"), b"agent-presets: {}").unwrap();
        fs::create_dir_all(&repo).unwrap();

        // 两个都接管
        adopt_home(&repo, &home_a, "A").unwrap();
        adopt_home(&repo, &home_b, "B").unwrap();

        // 切换：把 A 的 sessions 切到 B
        let result = switch_links(&repo, &home_a, &home_b, true, false, false, false, false, false).expect("切换应成功");
        assert_eq!(result.switched_links, 1);

        // 验证：B 的 sessions 现在指向 A 的内容（能读到 A-data）
        let content = fs::read(home_b.join("sessions").join("projA").join("s1").join("session.jsonl.zstd")).unwrap();
        assert_eq!(content, b"A-data", "切换后 B 应能读到 A 的对话");

        // 验证：保险快照里有 B 原来的内容
        let snap = repo.join("switch-backups").join(home_id_of(&home_b)).join(&result.backup_snapshot);
        let b_old = fs::read(snap.join("sessions").join("projB").join("s2").join("session.jsonl.zstd")).unwrap();
        assert_eq!(b_old, b"B-data", "保险快照应保存 B 原对话");

        let _ = fs::remove_dir_all(&base);
    }


    #[test]
    fn e2e_repair_links() {
        let base = std::env::temp_dir().join(format!("dsh-vault-repair-{}", std::process::id()));
        let repo = base.join("repo");
        // 环境 A、B 各有内容并接管
        let home_a = base.join("homeA");
        fs::create_dir_all(home_a.join("sessions").join("pA").join("s1")).unwrap();
        fs::write(home_a.join("sessions").join("pA").join("s1").join("session.jsonl.zstd"), b"A").unwrap();
        fs::write(home_a.join("settings.yaml"), b"agent-presets: {}").unwrap();
        let home_b = base.join("homeB");
        fs::create_dir_all(home_b.join("sessions").join("pB").join("s2")).unwrap();
        fs::write(home_b.join("sessions").join("pB").join("s2").join("session.jsonl.zstd"), b"B").unwrap();
        fs::write(home_b.join("settings.yaml"), b"agent-presets: {}").unwrap();
        fs::create_dir_all(&repo).unwrap();
        adopt_home(&repo, &home_a, "A").unwrap();
        adopt_home(&repo, &home_b, "B").unwrap();

        // 旧链接式切换把 B 的 sessions 指向 A（模拟错位）
        let a_files = repo_files_dir(&repo, &home_id_of(&home_a));
        fs::remove_dir(home_b.join("sessions")).unwrap(); // 删 B 的 symlink
        create_dir_link(&home_b.join("sessions"), &a_files.join("sessions")).unwrap(); // 错指到 A

        // 预览应检测到 sessions 错位
        let preview = preview_repair(&repo, &home_b).unwrap();
        assert_eq!(preview.broken_count, 1, "应检测到 1 个错位: {:?}", preview.actions.iter().map(|a|(&a.rel,&a.action)).collect::<Vec<_>>());
        let sess_action = preview.actions.iter().find(|a| a.rel == "sessions").unwrap();
        assert_eq!(sess_action.action, "repoint");

        // 执行修复
        let result = repair_links(&repo, &home_b).unwrap();
        assert!(result.repointed.contains(&"sessions".to_string()));
        // 修复后 B 读到自己的 B 内容
        let content = fs::read(home_b.join("sessions").join("pB").join("s2").join("session.jsonl.zstd")).unwrap();
        assert_eq!(content, b"B", "修复后 B 应读回自己的对话");
        // A 仍读自己的 A 内容（未被破坏）
        let acontent = fs::read(home_a.join("sessions").join("pA").join("s1").join("session.jsonl.zstd")).unwrap();
        assert_eq!(acontent, b"A");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn e2e_switch_copy_based() {
        // 复制式切换：目标原件进保险快照，目标读到来源内容，链接仍指向自己
        let base = std::env::temp_dir().join(format!("dsh-vault-swcopy-{}", std::process::id()));
        let repo = base.join("repo");
        let home_a = base.join("homeA");
        fs::create_dir_all(home_a.join("sessions").join("pA").join("s1")).unwrap();
        fs::write(home_a.join("sessions").join("pA").join("s1").join("session.jsonl.zstd"), b"A").unwrap();
        fs::write(home_a.join("settings.yaml"), b"agent-presets: {}").unwrap();
        let home_b = base.join("homeB");
        fs::create_dir_all(home_b.join("sessions").join("pB").join("s2")).unwrap();
        fs::write(home_b.join("sessions").join("pB").join("s2").join("session.jsonl.zstd"), b"B").unwrap();
        fs::write(home_b.join("settings.yaml"), b"agent-presets: {}").unwrap();
        fs::create_dir_all(&repo).unwrap();
        adopt_home(&repo, &home_a, "A").unwrap();
        adopt_home(&repo, &home_b, "B").unwrap();

        let result = switch_links(&repo, &home_a, &home_b, true, false, false, false, false, false).unwrap();
        assert_eq!(result.switched_links, 1);
        // 切换后 B 读到 A 的内容
        let content = fs::read(home_b.join("sessions").join("pA").join("s1").join("session.jsonl.zstd")).unwrap();
        assert_eq!(content, b"A");
        // B 的链接仍指向 B 自己（不混链）
        let preview = preview_repair(&repo, &home_b).unwrap();
        assert_eq!(preview.broken_count, 0, "复制式切换后不应有错位: {:?}", preview.actions.iter().map(|a|(&a.rel,&a.action)).collect::<Vec<_>>());
        // B 的原件在保险快照
        let snap = repo.join("switch-backups").join(home_id_of(&home_b)).join(&result.backup_snapshot);
        let b_old = fs::read(snap.join("sessions").join("pB").join("s2").join("session.jsonl.zstd")).unwrap();
        assert_eq!(b_old, b"B");
        // 保险快照名带"切自"
        assert!(result.backup_snapshot.contains("切自"), "快照名应含'切自': {}", result.backup_snapshot);
        // v4.2 模块B：settings.yaml 从源 A 带到了目标 B 的 home 根
        assert!(result.settings_copied, "源端有 settings.yaml，应被带过去");
        assert!(home_b.join("settings.yaml").is_file(), "目标 B 根应有 settings.yaml");
        // 目标 B 原 settings.yaml 应已进保险快照为 home-settings.yaml
        assert!(snap.join("home-settings.yaml").is_file(), "保险快照应含目标原 settings.yaml");

        let _ = fs::remove_dir_all(&base);
    }


    #[test]
    fn e2e_switch_preflight() {
        // 预检：目标比源多会话时应给出 target_only 提示；源端有 settings 应提示会带
        let base = std::env::temp_dir().join(format!("dsh-vault-preflight-{}", std::process::id()));
        let repo = base.join("repo");
        let home_a = base.join("homeA");
        fs::create_dir_all(home_a.join("sessions").join("pA").join("s1")).unwrap();
        fs::write(home_a.join("sessions").join("pA").join("s1").join("session.jsonl.zstd"), b"A").unwrap();
        fs::write(home_a.join("settings.yaml"), b"agent-presets: {}\nllm-pi-ai: {}").unwrap();
        let home_b = base.join("homeB");
        // B 有两条会话（比 A 多一条）
        fs::create_dir_all(home_b.join("sessions").join("pB").join("s2")).unwrap();
        fs::create_dir_all(home_b.join("sessions").join("pB").join("s3")).unwrap();
        fs::write(home_b.join("sessions").join("pB").join("s2").join("session.jsonl.zstd"), b"B").unwrap();
        fs::write(home_b.join("sessions").join("pB").join("s3").join("session.jsonl.zstd"), b"B").unwrap();
        fs::write(home_b.join("settings.yaml"), b"agent-presets: {}").unwrap();
        fs::create_dir_all(&repo).unwrap();
        adopt_home(&repo, &home_a, "A").unwrap();
        adopt_home(&repo, &home_b, "B").unwrap();

        let pf = switch_preflight(&repo, &home_a, &home_b);
        assert!(pf.source_has_settings, "源端有 settings.yaml");
        // v8：独有会话按 id 集合差算——B 的 s2、s3 都不在 A 里，共 2 条（旧的数量差只算 1，会漏报）
        assert_eq!(pf.target_only_sessions, 2, "目标应有 2 条独有会话");
        assert!(pf.notices.iter().any(|n| n.contains("settings.yaml")), "应提示带配置");
        assert!(pf.notices.iter().any(|n| n.contains("替换")), "应提示镜像语义");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn e2e_watch_change_detection() {
        // 模块D：接管后新增对话/技能 → watch_check 应报 changed；收录写指纹后 → changed 消失
        let base = std::env::temp_dir().join(format!("dsh-vault-watch-{}", std::process::id()));
        let repo = base.join("repo");
        let home = base.join("home");
        fs::create_dir_all(home.join("sessions").join("p").join("s1")).unwrap();
        fs::write(home.join("sessions").join("p").join("s1").join("session.jsonl.zstd"), b"x").unwrap();
        fs::write(home.join("settings.yaml"), b"agent-presets: {}").unwrap();
        fs::create_dir_all(&repo).unwrap();
        adopt_home(&repo, &home, "w").unwrap();

        // 初始：无指纹基准 → changed=true（首次）
        let before = watch_check(&repo);
        let rec = before.iter().find(|c| c.home == home.to_string_lossy()).expect("应有该 home");
        assert!(rec.changed, "首次无基准，应视为有变化");

        // 收录：写入指纹基准
        watch_mark_synced(&repo, &home).unwrap();
        let after_sync = watch_check(&repo);
        let rec2 = after_sync.iter().find(|c| c.home == home.to_string_lossy()).unwrap();
        assert!(!rec2.changed, "收录后指纹一致，不应再报变化");

        // 新增一条对话 → 又应报变化，且 new_sessions >= 1
        fs::create_dir_all(home.join("sessions").join("p").join("s2")).unwrap();
        fs::write(home.join("sessions").join("p").join("s2").join("session.jsonl.zstd"), b"y").unwrap();
        let after_new = watch_check(&repo);
        let rec3 = after_new.iter().find(|c| c.home == home.to_string_lossy()).unwrap();
        assert!(rec3.changed, "新增对话后应报变化");
        assert!(rec3.new_sessions >= 1, "应识别出新增对话数: {}", rec3.new_sessions);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn e2e_rollback_switch() {
        // 模块F：切换后回滚 → 目标端恢复到自己原来的内容
        let base = std::env::temp_dir().join(format!("dsh-vault-rollback-{}", std::process::id()));
        let repo = base.join("repo");
        let home_a = base.join("homeA");
        fs::create_dir_all(home_a.join("sessions").join("pA").join("s1")).unwrap();
        fs::write(home_a.join("sessions").join("pA").join("s1").join("session.jsonl.zstd"), b"A-data").unwrap();
        fs::write(home_a.join("settings.yaml"), b"agent-presets: {}").unwrap();
        let home_b = base.join("homeB");
        fs::create_dir_all(home_b.join("sessions").join("pB").join("s2")).unwrap();
        fs::write(home_b.join("sessions").join("pB").join("s2").join("session.jsonl.zstd"), b"B-data").unwrap();
        fs::write(home_b.join("settings.yaml"), b"agent-presets: {}").unwrap();
        fs::create_dir_all(&repo).unwrap();
        adopt_home(&repo, &home_a, "A").unwrap();
        adopt_home(&repo, &home_b, "B").unwrap();

        // 切换 A → B
        let sw = switch_links(&repo, &home_a, &home_b, true, false, false, false, false, false).unwrap();
        // 确认 B 现在是 A 的内容
        assert_eq!(fs::read(home_b.join("sessions").join("pA").join("s1").join("session.jsonl.zstd")).unwrap(), b"A-data");

        // 列出保险快照
        let snaps = list_switch_snapshots(&repo, &home_b);
        assert!(!snaps.is_empty(), "应有保险快照");

        // 回滚
        let rb = rollback_switch(&repo, &home_b, &sw.backup_snapshot).unwrap();
        assert!(rb.restored_dirs >= 1);
        // B 恢复成自己的内容
        assert_eq!(fs::read(home_b.join("sessions").join("pB").join("s2").join("session.jsonl.zstd")).unwrap(), b"B-data");
        // A 的内容不在了（被回滚覆盖）
        assert!(!home_b.join("sessions").join("pA").exists(), "回滚后不应再有 A 的会话");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn e2e_switch_fixes_unknown_preset() {
        // v4.3：源端会话带 anchored-standard，切换后目标端应被改写成 standard
        let base = std::env::temp_dir().join(format!("dsh-vault-pfe2e-{}", std::process::id()));
        let repo = base.join("repo");
        let home_a = base.join("homeA");
        let sess_dir = home_a.join("sessions").join("pA").join("s1");
        fs::create_dir_all(&sess_dir).unwrap();
        // 构造一条带 anchored-standard 的真实 zstd 会话
        let header = "{\"type\":\"session\",\"version\":0,\"id\":\"s1\",\"createdAt\":1,\"cwd\":\"E:\\\\x\",\"agentPreset\":\"anchored-standard\"}\n";
        let body = "{\"type\":\"user/message\",\"seq\":1}\n";
        let mut bytes = zstd::encode_all(std::io::Cursor::new(header.as_bytes()), 3).unwrap();
        bytes.extend_from_slice(&zstd::encode_all(std::io::Cursor::new(body.as_bytes()), 3).unwrap());
        fs::write(sess_dir.join("session.jsonl.zstd"), &bytes).unwrap();
        fs::write(home_a.join("settings.yaml"), b"agent-presets: {}").unwrap();
        let home_b = base.join("homeB");
        fs::create_dir_all(home_b.join("sessions").join("pB").join("s2")).unwrap();
        fs::write(home_b.join("sessions").join("pB").join("s2").join("session.jsonl.zstd"), b"B").unwrap();
        fs::write(home_b.join("settings.yaml"), b"agent-presets: {}").unwrap();
        fs::create_dir_all(&repo).unwrap();
        adopt_home(&repo, &home_a, "A").unwrap();
        adopt_home(&repo, &home_b, "B").unwrap();

        let result = switch_links(&repo, &home_a, &home_b, true, false, false, false, false, false).unwrap();
        assert_eq!(result.preset_fixed, 1, "应改写 1 条非法预设");

        // 目标端会话 header 现在是 standard
        let written = fs::read(home_b.join("sessions").join("pA").join("s1").join("session.jsonl.zstd")).unwrap();
        let (frames, _) = crate::zstd_check::scan_frames(&written).unwrap();
        let header_dec = zstd::decode_all(std::io::Cursor::new(&written[frames[0].start()..frames[0].end()])).unwrap();
        let text = String::from_utf8_lossy(&header_dec);
        assert!(text.contains("\"agentPreset\":\"standard\""), "应改写成 standard: {}", &text[..120.min(text.len())]);
        assert!(!text.contains("anchored-standard"));
        // 对话内容帧原样保留
        let body_dec = zstd::decode_all(std::io::Cursor::new(&written[frames[1].start()..frames[1].end()])).unwrap();
        assert!(String::from_utf8_lossy(&body_dec).contains("user/message"));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn e2e_snapshot_manifest_and_verify() {
        // 模块三：切换快照含 manifest，完整性校验能发现损坏
        let base = std::env::temp_dir().join(format!("dsh-vault-snapm-{}", uuid::Uuid::new_v4()));
        let repo = base.join("repo");
        let home_a = base.join("homeA");
        fs::create_dir_all(home_a.join("sessions").join("pA").join("s1")).unwrap();
        fs::write(home_a.join("sessions").join("pA").join("s1").join("session.jsonl.zstd"), b"A").unwrap();
        fs::write(home_a.join("settings.yaml"), b"agent-presets: {}").unwrap();
        let home_b = base.join("homeB");
        fs::create_dir_all(home_b.join("sessions").join("pB").join("s2")).unwrap();
        fs::write(home_b.join("sessions").join("pB").join("s2").join("session.jsonl.zstd"), b"B").unwrap();
        fs::write(home_b.join("settings.yaml"), b"agent-presets: {}").unwrap();
        fs::create_dir_all(&repo).unwrap();
        adopt_home(&repo, &home_a, "A").unwrap();
        adopt_home(&repo, &home_b, "B").unwrap();

        let result = switch_links(&repo, &home_a, &home_b, true, false, false, false, false, false).unwrap();
        let snap = repo.join("switch-backups").join(home_id_of(&home_b)).join(&result.backup_snapshot);
        // manifest 存在
        assert!(snap.join("manifest.json").is_file(), "快照应含 manifest");
        // 完整性校验通过
        let (total, bad, _) = verify_snapshot_integrity(&snap);
        assert!(total > 0, "manifest 应有文件");
        assert_eq!(bad, 0, "完好快照不应有损坏");
        // 人为破坏一个文件 → 校验发现 → 回滚拒绝
        let victim = snap.join("sessions").join("pB").join("s2").join("session.jsonl.zstd");
        fs::write(&victim, b"CORRUPTED").unwrap();
        let (_, bad2, _) = verify_snapshot_integrity(&snap);
        assert!(bad2 > 0, "损坏应被发现");
        let rb = rollback_switch(&repo, &home_b, &result.backup_snapshot);
        assert!(rb.is_err(), "损坏快照应拒绝回滚");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn e2e_switch_merge_keeps_target_only_sessions() {
        // v7 根治：合并模式（默认）绝不能抹掉目标端独有的对话
        let base = std::env::temp_dir().join(format!("dsh-vault-merge-{}", uuid::Uuid::new_v4()));
        let repo = base.join("repo");
        let home_a = base.join("homeA");
        fs::create_dir_all(home_a.join("sessions").join("pA").join("s1")).unwrap();
        fs::write(home_a.join("sessions").join("pA").join("s1").join("session.jsonl.zstd"), b"A-data").unwrap();
        fs::write(home_a.join("settings.yaml"), b"agent-presets: {}").unwrap();
        let home_b = base.join("homeB");
        fs::create_dir_all(home_b.join("sessions").join("pB").join("s2")).unwrap();
        fs::write(home_b.join("sessions").join("pB").join("s2").join("session.jsonl.zstd"), b"B-only-data").unwrap();
        fs::write(home_b.join("settings.yaml"), b"agent-presets: {}").unwrap();
        // v8：两边各有一份 workspace.json 登记表，验证「登记真合并」
        let storages_a = home_a.join("storages");
        fs::create_dir_all(&storages_a).unwrap();
        fs::write(storages_a.join("workspace.json"),
            r#"{"unit":{"name":"workspace","version":2},"global":{"initialized":true,"workspaceIds":["ws-a"],"archivedSessionIds":[]},"tables":{"workspaces":{"ws-a":{"path":"A:\\proj","title":"A","sessionIds":["s1"],"createdAt":"2026-01-01T00:00:00.000Z","updatedAt":"2026-01-01T00:00:00.000Z"}}}}"#).unwrap();
        let storages_b = home_b.join("storages");
        fs::create_dir_all(&storages_b).unwrap();
        fs::write(storages_b.join("workspace.json"),
            r#"{"unit":{"name":"workspace","version":2},"global":{"initialized":true,"workspaceIds":["ws-b"],"archivedSessionIds":[]},"tables":{"workspaces":{"ws-b":{"path":"B:\\proj","title":"B","sessionIds":["s2"],"createdAt":"2026-01-01T00:00:00.000Z","updatedAt":"2026-01-01T00:00:00.000Z"}}}}"#).unwrap();
        fs::create_dir_all(&repo).unwrap();
        adopt_home(&repo, &home_a, "A").unwrap();
        adopt_home(&repo, &home_b, "B").unwrap();

        // 合并模式切换（最后一个参数 false）
        let result = switch_links(&repo, &home_a, &home_b, true, false, false, false, false, false).unwrap();
        assert!(!result.replace_mode, "默认应为合并模式");
        // 来源的会话进来了
        assert!(home_b.join("sessions").join("pA").join("s1").join("session.jsonl.zstd").is_file(), "来源会话应复制过来");
        // 目标端独有的会话必须还在（这是本次修复的核心）
        assert!(home_b.join("sessions").join("pB").join("s2").join("session.jsonl.zstd").is_file(),
            "合并模式下目标端独有的对话绝不能被删除");
        // v8：登记表也必须真合并——目标端原登记（s2）保留，来源端登记（s1）并入。
        // 旧实现会先用来源的 workspace.json 覆盖目标，导致这里的 s2 登记消失。
        let ws_text = fs::read_to_string(home_b.join("storages").join("workspace.json")).unwrap();
        let ws: serde_json::Value = serde_json::from_str(&ws_text).unwrap();
        let tables = ws["tables"]["workspaces"].as_object().unwrap();
        let all_ids: Vec<String> = tables
            .values()
            .flat_map(|w| w["sessionIds"].as_array().cloned().unwrap_or_default())
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        assert!(all_ids.iter().any(|v| v == "s2"), "目标端原登记 s2 必须保留");
        assert!(all_ids.iter().any(|v| v == "s1"), "来源端登记 s1 应并入");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn e2e_repair_registry_registers_orphan_sessions() {
        // v7：磁盘上有、workspace.json 没登记的对话 → 修复登记后应被登记
        let base = std::env::temp_dir().join(format!("dsh-vault-repair-{}", uuid::Uuid::new_v4()));
        let home = base.join("home");
        // v8：官方只认「cwd 真实存在」的会话（realpath + isDirectory），
        // 测试必须用真实目录，否则构造的是官方根本不会显示的场景。
        let real_cwd = base.join("proj");
        fs::create_dir_all(&real_cwd).unwrap();
        let cwd = real_cwd.to_string_lossy().to_string();
        let dir = home.join("sessions").join(crate::project_key::project_key(&cwd)).join("session-orphan-1");
        fs::create_dir_all(&dir).unwrap();
        let header = format!(
            "{{\"type\":\"session\",\"version\":0,\"id\":\"session-orphan-1\",\"createdAt\":123,\"cwd\":{},\"agentPreset\":\"standard\"}}\n",
            serde_json::to_string(&cwd).unwrap()
        );
        let mut bytes = zstd::encode_all(std::io::Cursor::new(header.as_bytes()), 3).unwrap();
        bytes.extend_from_slice(&zstd::encode_all(std::io::Cursor::new(b"{\"type\":\"user/message\"}\n"), 3).unwrap());
        fs::write(dir.join("session.jsonl.zstd"), &bytes).unwrap();
        // workspace.json 存在但没登记这条（模拟切换后的孤儿）
        let storages = home.join("storages");
        fs::create_dir_all(&storages).unwrap();
        fs::write(storages.join("workspace.json"), r#"{"unit":{"name":"workspace","version":2},"global":{"initialized":true,"workspaceIds":[],"archivedSessionIds":[]},"tables":{"workspaces":{}}}"#).unwrap();

        let rep = crate::migrate::repair_session_registry(&home).unwrap();
        assert_eq!(rep.registered, 1, "应补登记 1 条");
        let doc: serde_json::Value = serde_json::from_str(&fs::read_to_string(storages.join("workspace.json")).unwrap()).unwrap();
        let mut found = false;
        for (_k, w) in doc["tables"]["workspaces"].as_object().unwrap() {
            if w["sessionIds"].as_array().unwrap().iter().any(|x| x == "session-orphan-1") { found = true; }
        }
        assert!(found, "孤儿对话应被登记进 workspace.json");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn e2e_restore_sessions_from_snapshot() {
        // v7：从切换保险快照里把丢失的对话找回来 + 自动登记
        let base = std::env::temp_dir().join(format!("dsh-vault-snapres-{}", uuid::Uuid::new_v4()));
        let repo = base.join("repo");
        let home = base.join("home");
        fs::create_dir_all(home.join("sessions").join("p").join("keep")).unwrap();
        fs::write(home.join("sessions").join("p").join("keep").join("session.jsonl.zstd"), b"keep").unwrap();
        fs::write(home.join("settings.yaml"), b"agent-presets: {}").unwrap();
        fs::create_dir_all(&repo).unwrap();
        adopt_home(&repo, &home, "X").unwrap();
        let home_id = home_id_of(&home);
        // 造一个切换快照，里面有会话
        // v8：cwd 用真实存在的目录，否则官方不会显示该会话，登记也留不住
        let real_lost = base.join("lost");
        fs::create_dir_all(&real_lost).unwrap();
        let cwd = real_lost.to_string_lossy().to_string();
        let snap_sess = repo.join("switch-backups").join(&home_id).join("snap-1").join("sessions").join(crate::project_key::project_key(&cwd)).join("session-lost-1");
        fs::create_dir_all(&snap_sess).unwrap();
        let header = format!(
            "{{\"type\":\"session\",\"version\":0,\"id\":\"session-lost-1\",\"createdAt\":1,\"cwd\":{}}}\n",
            serde_json::to_string(&cwd).unwrap()
        );
        let mut bytes = zstd::encode_all(std::io::Cursor::new(header.as_bytes()), 3).unwrap();
        bytes.extend_from_slice(&zstd::encode_all(std::io::Cursor::new(b"{\"type\":\"user/message\"}\n"), 3).unwrap());
        fs::write(snap_sess.join("session.jsonl.zstd"), &bytes).unwrap();

        let rep = crate::migrate::restore_sessions_from_snapshot(&repo, &home, "snap-1").unwrap();
        assert_eq!(rep.restored, 1, "应找回 1 条");
        assert!(home.join("sessions").join(crate::project_key::project_key(&cwd)).join("session-lost-1").join("session.jsonl.zstd").is_file(), "会话文件应放回目标");
        // 并且被登记
        let doc: serde_json::Value = serde_json::from_str(&fs::read_to_string(home.join("storages").join("workspace.json")).unwrap()).unwrap_or(serde_json::json!({}));
        let mut found = false;
        if let Some(map) = doc.pointer("/tables/workspaces").and_then(|x| x.as_object()) {
            for (_k, w) in map {
                if w["sessionIds"].as_array().map(|a| a.iter().any(|x| x == "session-lost-1")).unwrap_or(false) { found = true; }
            }
        }
        assert!(found, "找回的对话应被登记进 workspace.json");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn e2e_ai_note() {
        let base = std::env::temp_dir().join(format!("dsh-vault-note-{}", std::process::id()));
        let home = base.join("home");
        let repo = base.join("repo");
        fs::create_dir_all(home.join("sessions").join("p").join("s1")).unwrap();
        fs::write(home.join("sessions").join("p").join("s1").join("session.jsonl.zstd"), b"x").unwrap();
        fs::create_dir_all(home.join("skills")).unwrap();
        fs::write(home.join("settings.yaml"), b"agent-presets: {}").unwrap();
        fs::create_dir_all(&repo).unwrap();

        adopt_home(&repo, &home, "note").unwrap();
        // 小纸条技能应已写入（在 skills 链接里）
        let note = home.join("skills").join("dsh-vault-manager").join("SKILL.md");
        assert!(note.is_file(), "小纸条技能应存在");
        let content = fs::read_to_string(&note).unwrap();
        assert!(content.contains("DSH Vault"), "小纸条应提到 DSH Vault");
        // AGENTS.md 应有指针块
        let agents = fs::read_to_string(home.join("AGENTS.md")).unwrap();
        assert!(agents.contains("DSH-VAULT-NOTE-BEGIN"), "AGENTS.md 应有小纸条块");

        // 断开后小纸条应移除
        unadopt_home(&repo, &home).unwrap();
        let agents2 = fs::read_to_string(home.join("AGENTS.md")).unwrap_or_default();
        assert!(!agents2.contains("DSH-VAULT-NOTE-BEGIN"), "断开后 AGENTS.md 小纸条应移除");

        let _ = fs::remove_dir_all(&base);
    }

}
