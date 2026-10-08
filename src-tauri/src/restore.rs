//! 恢复、覆盖前快照、撤回与事务回滚。
//!
//! 恢复边界：
//! - Config：配置档案，不动 sessions；
//! - Sessions：只恢复对话记录；
//! - Home：配置 + 会话 + 技能 + 记忆等档案。
//!
//! 安全策略：
//! - 默认只补缺；
//! - 更新/强制覆盖前先快照；
//! - 任何文件恢复失败立即停止，并自动回滚本次已写入的文件；
//! - 恢复前要求 DSH 已关闭；
//! - 逐文件推送进度事件，事件按进度阈值节流，避免前端事件洪峰。
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use crate::repo::{atomic_write, is_safe_rel_path, load_manifest, Manifest, ManifestFile, RestoreMode, RestoreScope};
use tauri::{AppHandle, Emitter};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreFilter {
    pub scope: RestoreScope,
    pub source_home_id: Option<String>,
    pub target_home: Option<String>,
    pub ids: Vec<String>,
    pub project: Option<String>,
    pub mode: RestoreMode,
    pub since: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestorePreviewItem {
    pub rel: String,
    pub action: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestorePreview {
    pub target_home: String,
    pub items: Vec<RestorePreviewItem>,
    pub create_count: u32,
    pub skip_count: u32,
    pub overwrite_count: u32,
    pub conflict_count: u32,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreResult {
    pub created: u32,
    pub skipped: u32,
    pub overwritten: u32,
    pub failed: Vec<String>,
    pub snapshot: Option<String>,
    pub target_home: String,
    pub rolled_back: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoResult {
    pub restored: u32,
    pub skipped: u32,
    pub snapshot: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyResult {
    pub total: u32,
    pub ok: u32,
    pub bad: Vec<String>,
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressPayload {
    pub phase: String,
    pub done: u32,
    pub total: u32,
    pub current: String,
}

#[derive(Debug, Clone)]
struct TransactionRecord {
    pub rel: String,
    target: PathBuf,
    old_snapshot: Option<PathBuf>,
    pub was_created: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackLedger {
    pub created_at: String,
    pub target_home: String,
    pub errors: Vec<String>,
    pub pending: Vec<RollbackLedgerItem>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackLedgerItem {
    pub rel: String,
    pub target: String,
    pub old_snapshot: Option<String>,
    pub was_created: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackLedgerEntry {
    pub file: String,
    pub created_at: String,
    pub target_home: String,
    pub pending_count: u32,
    pub ledger: RollbackLedger,
}

pub fn list_rollback_ledgers(repo: &Path) -> Result<Vec<RollbackLedgerEntry>, String> {
    let dir = repo.join("rollback-ledger");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .map_err(|e| format!("无法读取回滚日志目录：{e}"))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|v| v == "json"))
        .collect();
    paths.sort();
    for path in paths {
        let text = fs::read_to_string(&path).map_err(|e| format!("读取回滚日志失败：{e}"))?;
        let ledger: RollbackLedger = serde_json::from_str(&text)
            .map_err(|e| format!("回滚日志格式不正确：{e}"))?;
        entries.push(RollbackLedgerEntry {
            pending_count: ledger.pending.len() as u32,
            file: path.to_string_lossy().to_string(),
            created_at: ledger.created_at.clone(),
            target_home: ledger.target_home.clone(),
            ledger,
        });
    }
    Ok(entries)
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompensateResult {
    pub ledger_file: String,
    pub restored: u32,
    pub skipped: u32,
    pub failed: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompensateItem {
    pub rel: String,
    pub action: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompensatePreview {
    pub target_home: String,
    pub items: Vec<CompensateItem>,
}

fn sha256_path(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Some(format!("{:x}", hasher.finalize()))
}

fn target_within_home(target: &Path, home: &Path) -> bool {
    let Ok(target_canon) = target.canonicalize() else { return false };
    let Ok(home_canon) = home.canonicalize() else { return false };
    target_canon.starts_with(&home_canon)
}

pub fn preview_rollback_compensate(ledger_file: &Path) -> Result<CompensatePreview, String> {
    let text = fs::read_to_string(ledger_file).map_err(|e| format!("读取回滚日志失败：{e}"))?;
    let ledger: RollbackLedger = serde_json::from_str(&text).map_err(|e| format!("回滚日志格式不正确：{e}"))?;
    let target_home = PathBuf::from(&ledger.target_home);
    let mut items = Vec::new();

    for item in &ledger.pending {
        if item.was_created {
            items.push(CompensateItem {
                rel: item.rel.clone(),
                action: "skip-created".into(),
                reason: "本次新建的文件不自动删除，请人工确认后处理".into(),
            });
            continue;
        }
        let Some(snapshot) = &item.old_snapshot else {
            items.push(CompensateItem {
                rel: item.rel.clone(),
                action: "skip-no-snapshot".into(),
                reason: "没有覆盖前快照，无法安全恢复".into(),
            });
            continue;
        };
        let snapshot = PathBuf::from(snapshot);
        if !snapshot.is_file() {
            items.push(CompensateItem {
                rel: item.rel.clone(),
                action: "skip-missing-snapshot".into(),
                reason: "覆盖前快照不存在".into(),
            });
            continue;
        }
        let target = PathBuf::from(&item.target);
        if !target_within_home(&target, &target_home) {
            items.push(CompensateItem {
                rel: item.rel.clone(),
                action: "skip-out-of-home".into(),
                reason: "目标路径超出 Home，拒绝补偿".into(),
            });
            continue;
        }
        items.push(CompensateItem {
            rel: item.rel.clone(),
            action: "restore-snapshot".into(),
            reason: "从覆盖前快照恢复".into(),
        });
    }

    Ok(CompensatePreview {
        target_home: ledger.target_home,
        items,
    })
}

pub fn apply_rollback_compensate(ledger_file: &Path) -> Result<CompensateResult, String> {
    if dsh_running() {
        return Err("检测到 DSH 正在运行。请先完全退出 DSH，再执行补偿。".into());
    }
    let preview = preview_rollback_compensate(ledger_file)?;
    let mut restored = 0u32;
    let mut skipped = 0u32;
    let mut failed = Vec::new();

    let text = fs::read_to_string(ledger_file).map_err(|e| format!("读取回滚日志失败：{e}"))?;
    let ledger: RollbackLedger = serde_json::from_str(&text).map_err(|e| format!("回滚日志格式不正确：{e}"))?;
    let target_home = PathBuf::from(&ledger.target_home);

    for (index, item) in ledger.pending.iter().enumerate() {
        let action = preview.items.get(index).map(|v| v.action.as_str()).unwrap_or("skip");
        if action != "restore-snapshot" {
            skipped += 1;
            continue;
        }
        let snapshot = PathBuf::from(item.old_snapshot.as_deref().unwrap_or_default());
        let target = PathBuf::from(&item.target);
        if !target_within_home(&target, &target_home) {
            skipped += 1;
            continue;
        }
        let Some(snapshot_sha) = sha256_path(&snapshot) else {
            failed.push(format!("无法读取快照 SHA：{}", item.rel));
            continue;
        };
        let Some(target_sha_before) = sha256_path(&target) else {
            failed.push(format!("无法读取目标 SHA：{}", item.rel));
            continue;
        };
        if target_sha_before == snapshot_sha {
            skipped += 1;
            continue;
        }
        let bytes = fs::read(&snapshot).map_err(|e| format!("读取快照失败：{e}"))?;
        atomic_write(&target, &bytes)?;
        let Some(target_sha_after) = sha256_path(&target) else {
            failed.push(format!("补偿后无法读取目标：{}", item.rel));
            continue;
        };
        if target_sha_after != snapshot_sha {
            failed.push(format!("补偿后 SHA 不匹配：{}", item.rel));
            continue;
        }
        restored += 1;
    }

    Ok(CompensateResult {
        ledger_file: ledger_file.to_string_lossy().to_string(),
        restored,
        skipped,
        failed,
    })
}
fn write_rollback_ledger(
    repo: &Path,
    target: &Path,
    errors: &[String],
    pending: &[TransactionRecord],
) -> Option<String> {
    let dir = repo.join("rollback-ledger");
    fs::create_dir_all(&dir).ok()?;
    let ledger = RollbackLedger {
        created_at: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        target_home: target.to_string_lossy().to_string(),
        errors: errors.to_vec(),
        pending: pending
            .iter()
            .map(|record| RollbackLedgerItem {
                rel: record.rel.clone(),
                target: record.target.to_string_lossy().to_string(),
                old_snapshot: record
                    .old_snapshot
                    .as_ref()
                    .map(|v| v.to_string_lossy().to_string()),
                was_created: record.was_created,
            })
            .collect(),
    };
    let text = serde_json::to_string_pretty(&ledger).ok()?;
    let path = dir.join(format!("{}.json", timestamp()));
    atomic_write(&path, text.as_bytes()).ok()?;
    Some(path.to_string_lossy().to_string())
}
fn emit_progress(app: Option<&AppHandle>, done: u32, total: u32, current: &str) {
    let Some(app) = app else { return };
    let payload = ProgressPayload {
        phase: "restore".into(),
        done,
        total,
        current: current.to_string(),
    };
    let _ = app.emit("restore-progress", payload);
}

fn matches_scope(file: &ManifestFile, scope: RestoreScope) -> bool {
    match scope {
        RestoreScope::Config => !file.rel.starts_with("sessions/"),
        RestoreScope::Sessions => file.rel.starts_with("sessions/"),
        RestoreScope::Home => true,
    }
}

fn target_for(manifest: &Manifest, filter: &RestoreFilter) -> Result<PathBuf, String> {
    if let Some(path) = &filter.target_home {
        return Ok(PathBuf::from(path));
    }
    let source = filter
        .source_home_id
        .as_deref()
        .and_then(|id| manifest.homes.iter().find(|h| h.id == id));
    if let Some(home) = source {
        return Ok(PathBuf::from(&home.path));
    }
    Err("请选择要恢复到哪个 DSH Home".into())
}

fn file_matches_filter(file: &ManifestFile, filter: &RestoreFilter) -> bool {
    if !matches_scope(file, filter.scope) {
        return false;
    }
    if let Some(source) = &filter.source_home_id {
        if file.root != *source && file.kind != "skill" {
            return false;
        }
    }
    if !filter.ids.is_empty() {
        let rel = file.rel.to_lowercase();
        return filter.ids.iter().any(|id| rel.contains(&id.to_lowercase()));
    }
    if let Some(project) = &filter.project {
        return file.rel.to_lowercase().contains(&project.to_lowercase());
    }
    true
}

fn restored_rel(file: &ManifestFile) -> Result<String, String> {
    if !is_safe_rel_path(&file.rel) {
        return Err(format!("恢复路径不安全：{}", file.rel));
    }
    Ok(file.rel.clone())
}

pub fn preview_restore(repo: &Path, filter: RestoreFilter) -> Result<RestorePreview, String> {
    let manifest = load_manifest(repo)?;
    let target = target_for(&manifest, &filter)?;
    let mut items = Vec::new();
    let mut create_count = 0u32;
    let mut skip_count = 0u32;
    let mut overwrite_count = 0u32;
    let conflict_count = 0u32;
    let mut warnings = Vec::new();

    let mut credential_skipped = 0u32;
    for file in manifest.files.iter().filter(|f| file_matches_filter(f, &filter)) {
        let rel = restored_rel(file)?;
        let dst = target.join(&rel);
        let exists = dst.exists();
        // 凭据占位符永远跳过，不进任何计数
        if file.credential_placeholder {
            credential_skipped += 1;
            items.push(RestorePreviewItem { rel, action: "credential-skip".to_string() });
            continue;
        }
        let action = if !exists {
            create_count += 1;
            "create"
        } else {
            let same = existing_sha(&dst).map(|v| v == file.sha256).unwrap_or(false);
            if same {
                skip_count += 1;
                "skip"
            } else if filter.mode == RestoreMode::FillMissing {
                skip_count += 1;
                "skip-existing"
            } else {
                overwrite_count += 1;
                if filter.mode == RestoreMode::MergeNewer {
                    "overwrite-newer"
                } else {
                    "overwrite"
                }
            }
        };
        items.push(RestorePreviewItem {
            rel,
            action: action.to_string(),
        });
    }

    if credential_skipped > 0 {
        warnings.push(format!(
            "有 {} 个凭据文件已被跳过（备份时出于安全已脱敏，恢复不会覆盖你现有的登录与 API Key）。恢复后若发不出消息，请在该版本界面重新录入 API Key。",
            credential_skipped
        ));
    }
    if manifest.homes.iter().any(|h| h.sessions.corrupt > 0) {
        warnings.push("仓库中记录了损坏会话；恢复时同样会跳过这些文件。".into());
    }
    if manifest
        .warnings
        .iter()
        .any(|v| v.to_string().contains("token") || v.to_string().contains("CREDENTIAL"))
    {
        warnings.push("备份时已剥离登录 token，恢复后需要重新登录 DSH。".into());
    }

    Ok(RestorePreview {
        target_home: target.to_string_lossy().to_string(),
        items,
        create_count,
        skip_count,
        overwrite_count,
        conflict_count,
        warnings,
    })
}

fn existing_sha(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Some(format!("{:x}", hasher.finalize()))
}

fn timestamp() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    format!("{}", now.as_millis())
}

fn rollback_transactions(records: &[TransactionRecord], repo: &Path) -> (Vec<String>, Vec<TransactionRecord>) {
    let mut pending = Vec::new();
    let mut errors = Vec::new();
    // 反向回滚：后写入的先撤销，减少目录状态变化带来的影响。
    for record in records.iter().rev() {
        if record.was_created {
            if record.target.exists() {
                if let Err(e) = fs::remove_file(&record.target) {
                    errors.push(format!("回滚新建文件失败 {}：{e}", record.rel));
                    pending.push(record.clone());
                }
            }
        } else if let Some(snapshot) = &record.old_snapshot {
            if let Ok(bytes) = fs::read(snapshot) {
                if let Err(e) = atomic_write(&record.target, &bytes) {
                    errors.push(format!("回滚覆盖文件失败 {}：{e}", record.rel));
                }
                    pending.push(record.clone());
            } else {
                errors.push(format!("找不到覆盖前快照：{}", record.rel));
                pending.push(record.clone());
            }
        }
    }
    let _ = repo;
    (errors, pending)
}

pub fn run_restore(
    repo: &Path,
    filter: RestoreFilter,
    app: Option<&AppHandle>,
) -> Result<RestoreResult, String> {
    let manifest = load_manifest(repo)?;
    let target = target_for(&manifest, &filter)?;
    if dsh_running() {
        return Err("检测到 DSH 正在运行。请先完全退出 DSH，再执行恢复。".into());
    }

    let selected: Vec<&ManifestFile> = manifest
        .files
        .iter()
        .filter(|f| file_matches_filter(f, &filter))
        .collect();
    let total = selected.len() as u32;

    let mut snapshot_dir = None;
    if filter.mode != RestoreMode::FillMissing {
        snapshot_dir = Some(repo.join("snapshots").join(timestamp()));
        fs::create_dir_all(snapshot_dir.as_ref().unwrap())
            .map_err(|e| format!("创建快照目录失败：{e}"))?;
    }

    let mut created = 0u32;
    let mut skipped = 0u32;
    let mut overwritten = 0u32;
    let mut failed = Vec::new();
    let mut snapshot_records = Vec::new();
    let mut transactions = Vec::new();
    let mut done = 0u32;
    let mut last_percent = u32::MAX;

    for file in selected {
        done += 1;
        let percent = if total == 0 { 100 } else { done * 100 / total };
        // 至少 1% 或前 5 个文件发一次，避免 2000+ 事件洪峰。
        if done <= 5 || percent != last_percent {
            emit_progress(app, done, total, &file.rel);
            last_percent = percent;
        }

        let src = repo.join(&file.path);
        let rel = restored_rel(file)?;
        let dst = target.join(&rel);
        if !src.is_file() {
            failed.push(format!("仓库缺少文件：{}", file.path));
            break;
        }

        // v4.1 安全红线：凭据占位符（备份时脱敏成的空壳）绝不覆盖目标文件。
        // 同学排查报告 03.3 已证实：空壳覆盖真实凭据会让目标 Key 全丢且更难排查。
        // 凭据由用户在目标版本界面重录（方案 A），不走文件迁移。
        if file.credential_placeholder {
            skipped += 1;
            continue;
        }

        let dst_exists = dst.exists();
        let mut old_snapshot = None;
        if dst_exists {
            let same = existing_sha(&dst).map(|v| v == file.sha256).unwrap_or(false);
            if same || filter.mode == RestoreMode::FillMissing {
                skipped += 1;
                continue;
            }
            if filter.mode == RestoreMode::MergeNewer {
                let target_newer = fs::metadata(&dst)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|v| v.duration_since(UNIX_EPOCH).ok())
                    .map(|v| v.as_millis() as u64)
                    .unwrap_or_default();
                if target_newer >= file.mtime_ms.as_millis() {
                    skipped += 1;
                    continue;
                }
            }

            if let Some(snapshot) = &snapshot_dir {
                let snap_file = snapshot.join(&rel);
                fs::create_dir_all(snap_file.parent().ok_or("快照路径无父目录")?)
                    .map_err(|e| format!("创建快照目录失败：{e}"))?;
                fs::copy(&dst, &snap_file).map_err(|e| format!("创建撤回快照失败：{e}"))?;
                old_snapshot = Some(snap_file.clone());
                snapshot_records.push(SnapshotRecord {
                    rel: rel.clone(),
                    old_sha256: existing_sha(&dst).unwrap_or_default(),
                    new_sha256: file.sha256.clone(),
                });
            }
        }

        fs::create_dir_all(dst.parent().ok_or("目标路径无父目录")?)
            .map_err(|e| format!("创建目标目录失败：{e}"))?;
        let bytes = fs::read(&src).map_err(|e| format!("读取备份失败：{e}"))?;
        atomic_write(&dst, &bytes)?;
        transactions.push(TransactionRecord {
            rel: rel.clone(),
            target: dst.clone(),
            old_snapshot: old_snapshot.clone(),
            was_created: !dst_exists,
        });

        if existing_sha(&dst) != Some(file.sha256.clone()) {
            failed.push(format!("恢复后校验失败：{rel}"));
            break;
        }
        if dst_exists {
            overwritten += 1;
        } else {
            created += 1;
        }
    }

    let rolled_back = if !failed.is_empty() {
        let (rollback_errors, pending_rollback) = rollback_transactions(&transactions, repo);
        failed.extend(rollback_errors);
        let rollback_ledger = write_rollback_ledger(repo, &target, &failed, &pending_rollback);
        if let Some(ledger) = rollback_ledger {
            failed.push(format!("回滚补偿日志：{ledger}"));
        } else {
            failed.push("无法写入回滚补偿日志，请勿继续操作目标目录。".into());
        }
        // 回滚后重新扫描会话状态，避免界面继续显示半恢复统计。
        true
    } else {
        false
    };

    if !failed.is_empty() {
        return Err(format!(
            "恢复失败，已自动回滚本次写入的文件。\n{}",
            failed.join("\n")
        ));
    }

    if let Some(snapshot) = &snapshot_dir {
        let records = SnapshotFile {
            current: snapshot.to_string_lossy().to_string(),
            target_home: target.to_string_lossy().to_string(),
            records: snapshot_records,
        };
        let text = serde_json::to_string_pretty(&records).map_err(|e| e.to_string())?;
        atomic_write(&repo.join("snapshots/last.json"), text.as_bytes())?;
    }

    emit_progress(app, total, total, "完成");

    Ok(RestoreResult {
        created,
        skipped,
        overwritten,
        failed,
        snapshot: snapshot_dir.map(|v| v.to_string_lossy().to_string()),
        target_home: target.to_string_lossy().to_string(),
        rolled_back,
    })
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotRecord {
    pub rel: String,
    old_sha256: String,
    new_sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotFile {
    current: String,
    pub target_home: String,
    records: Vec<SnapshotRecord>,
}

pub fn undo_last(repo: &Path) -> Result<UndoResult, String> {
    let last_path = repo.join("snapshots/last.json");
    let text = fs::read_to_string(&last_path).map_err(|_| "没有可撤回的恢复".to_string())?;
    let last: SnapshotFile = serde_json::from_str(&text).map_err(|e| format!("撤回记录损坏：{e}"))?;
    let snapshot = PathBuf::from(&last.current);
    let target = PathBuf::from(&last.target_home);
    if !target.is_dir() {
        return Err(format!("快照记录的目标目录不存在：{}", target.display()));
    }
    if dsh_running() {
        return Err("检测到 DSH 正在运行。请先完全退出 DSH，再撤回。".to_string());
    }

    let mut restored = 0u32;
    let mut skipped = 0u32;
    for record in &last.records {
        if !is_safe_rel_path(&record.rel) {
            return Err(format!("撤回路径不安全：{}", record.rel));
        }
        let dst = target.join(&record.rel);
        let source = snapshot.join(&record.rel);
        // 只有当前文件仍然是恢复后内容时才撤回；用户后来手动改过的不动。
        if existing_sha(&dst) != Some(record.new_sha256.clone()) {
            skipped += 1;
            continue;
        }
        if !source.is_file() {
            skipped += 1;
            continue;
        }
        let bytes = fs::read(&source).map_err(|e| format!("读取撤回快照失败：{e}"))?;
        atomic_write(&dst, &bytes)?;
        if existing_sha(&dst) != Some(record.old_sha256.clone()) {
            return Err(format!("撤回后校验失败：{}", record.rel));
        }
        restored += 1;
    }

    Ok(UndoResult {
        restored,
        skipped,
        snapshot: last.current,
    })
}

pub fn verify_repo(repo: &Path) -> Result<VerifyResult, String> {
    let manifest = load_manifest(repo)?;
    let mut ok = 0u32;
    let mut bad = Vec::new();
    let mut missing = Vec::new();

    for file in &manifest.files {
        let path = repo.join(&file.path);
        if !path.is_file() {
            missing.push(file.path.clone());
            continue;
        }
        if existing_sha(&path) == Some(file.sha256.clone()) {
            ok += 1;
        } else {
            bad.push(file.path.clone());
        }
    }

    Ok(VerifyResult {
        total: manifest.files.len() as u32,
        ok,
        bad,
        missing,
    })
}

pub fn dsh_running() -> bool {
    // Windows 下先用进程名快查；后续如果引入 sysinfo，再替换为精确镜像名。
    if let Ok(output) = std::process::Command::new("tasklist")
        .arg("/FI")
        .arg("IMAGENAME eq Deepseek Harness.exe")
        .output()
    {
        let text = String::from_utf8_lossy(&output.stdout).to_lowercase();
        if text.contains("deepseek harness.exe") {
            return true;
        }
    }
    false
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::{Manifest, ManifestFile, ManifestHome, MTime};
    use crate::model::{HomeKind, SessionStats};

    fn write_min_repo(repo: &std::path::Path) {
        std::fs::create_dir_all(repo.join("blobs/root-1")).unwrap();
        // 凭据空壳 blob（10 字节，模拟 strip 后的骨架）
        std::fs::write(repo.join("blobs/root-1/.credentials.yaml"), b"# stripped").unwrap();
        let manifest = Manifest {
            version: 2,
            tool: "dsh-vault".into(),
            created_at: "2026-10-08T00:00:00".into(),
            host: "test".into(),
            note: String::new(),
            homes: vec![ManifestHome {
                id: "root-1".into(),
                kind: HomeKind::DshHome,
                label: "测试".into(),
                path: "C:\\fake".into(),
                variant: "unknown".into(),
                dsh_version: None,
                sessions: SessionStats::default(),
            }],
            files: vec![ManifestFile {
                root: "root-1".into(),
                path: "blobs/root-1/.credentials.yaml".into(),
                size: 10,
                mtime_ms: MTime::Millis(0),
                sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
                status: "ok".into(),
                kind: "config".into(),
                rel: ".credentials.yaml".into(),
                credential_placeholder: true,
            }],
            warnings: vec![],
        };
        let text = serde_json::to_string_pretty(&manifest).unwrap();
        std::fs::write(repo.join("manifest.json"), text).unwrap();
    }

    #[test]
    fn credential_placeholder_is_never_restored_over_existing() {
        // 跳过条件：dsh_running() 若真检测到 DSH 会提前返回，本测试环境一般无 DSH 进程。
        if dsh_running() {
            eprintln!("skip: DSH 正在运行");
            return;
        }
        let base = std::env::temp_dir().join(format!("dsh-vault-restore-test-{}", std::process::id()));
        let repo = base.join("repo");
        let target = base.join("target");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&target).unwrap();
        write_min_repo(&repo);
        // 目标已有真实凭据（478 字节、含 Key 引用，绝不允许被空壳覆盖）
        let real_cred = target.join(".credentials.yaml");
        std::fs::write(&real_cred, b"records:\n  - name: DEEPSEEK_API_KEY\n    value: <real>").unwrap();
        let before = std::fs::read(&real_cred).unwrap();

        let filter = RestoreFilter {
            scope: crate::repo::RestoreScope::Home,
            source_home_id: None,
            target_home: Some(target.to_string_lossy().to_string()),
            ids: Vec::new(),
            project: None,
            mode: crate::repo::RestoreMode::Force, // 即使强制覆盖也不许动凭据
            since: None,
        };
        let result = run_restore(&repo, filter, None).expect("恢复应成功（凭据被跳过）");
        assert_eq!(result.created, 0, "不应新建任何文件");
        assert_eq!(result.skipped, 1, "凭据占位符应计入跳过");
        let after = std::fs::read(&real_cred).unwrap();
        assert_eq!(before, after, "目标真实凭据绝不允许被空壳覆盖");
        let _ = std::fs::remove_dir_all(&base);
    }
}
