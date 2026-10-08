#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod adopt;
mod mcp;
mod model;
mod oplog;
mod multiscan;
mod repo;
mod routecheck;
mod scan_cache;
mod scanner;
mod export;
mod zstd_check;

use repo::{default_repo, load_manifest, run_backup, RestoreMode, RestoreScope};
use restore::{preview_restore as preview_restore_impl, run_restore as run_restore_impl, undo_last as undo_last_impl};
use serde::Serialize;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanResult {
    homes: Vec<crate::model::DiscoveredHome>,
    default_repo: String,
}

#[tauri::command]
fn scan_homes() -> ScanResult {
    let mut homes = scanner::discover_homes();
    if let Some(agents) = scanner::discover_agents_home() {
        homes.push(agents);
    }
    // 记录扫描结果（quick），失败不阻断返回；增量合并防浅扫覆盖深扫
    let _ = scan_cache::merge_save(&homes, "quick");
    oplog::record("scan", "", "快速扫描", &format!("识别到 {} 个环境", homes.len()), "ok", "");
    ScanResult {
        homes,
        default_repo: default_repo(),
    }
}

#[tauri::command]
fn get_catalog(repo: String) -> Result<repo::Manifest, String> {
    load_manifest(&PathBuf::from(repo))
}

#[tauri::command]
async fn backup(app: tauri::AppHandle, repo: String, note: String, only_config: bool) -> Result<repo::BackupResult, String> {
    let repo_for_log = repo.clone();
    let only_cfg = only_config;
    let result = backup_inner(app, repo, note, only_config).await;
    match &result {
        Ok(r) => oplog::record("backup", &repo_for_log, &format!("{} 个环境", r.homes),
            &format!("文件 {}（新增 {} 复用 {}）/ {}", r.files, r.created, r.skipped, crate::adopt::format_bytes_pub(r.bytes)),
            "ok", ""),
        Err(e) => oplog::record("backup", &repo_for_log, "", "", "fail",
            &e.chars().take(200).collect::<String>()),
    }
    let _ = only_cfg;
    result
}

async fn backup_inner(app: tauri::AppHandle, repo: String, note: String, only_config: bool) -> Result<repo::BackupResult, String> {
    tauri::async_runtime::spawn_blocking(move || run_backup(&repo, &note, only_config, Some(&app)))
        .await
        .map_err(|e| format!("备份任务中断：{e}"))?
}

#[tauri::command]
fn verify(repo: String) -> Result<restore::VerifyResult, String> {
    restore::verify_repo(&PathBuf::from(repo))
}

#[tauri::command]
async fn preview_restore(
    repo: String,
    scope: RestoreScope,
    source_home_id: Option<String>,
    target_home: Option<String>,
    ids: Vec<String>,
    project: Option<String>,
    mode: RestoreMode,
) -> Result<restore::RestorePreview, String> {
    tauri::async_runtime::spawn_blocking(move || {
        preview_restore_impl(&PathBuf::from(repo), restore::RestoreFilter {
            scope,
            source_home_id,
            target_home,
            ids,
            mode,
            project,
            since: None,
        })
    })
    .await
    .map_err(|e| format!("预览任务中断：{e}"))?
}

#[tauri::command]
async fn restore(app: tauri::AppHandle,
    repo: String,
    scope: RestoreScope,
    source_home_id: Option<String>,
    target_home: Option<String>,
    ids: Vec<String>,
    project: Option<String>,
    mode: RestoreMode,
) -> Result<restore::RestoreResult, String> {
    let repo_log = repo.clone();
    let tgt_log = target_home.clone().unwrap_or_default();
    let result = tauri::async_runtime::spawn_blocking(move || {
        run_restore_impl(&PathBuf::from(repo), restore::RestoreFilter {
            scope,
            source_home_id,
            target_home,
            ids,
            project,
            mode,
            since: None,
        }, Some(&app))
    })
    .await
    .map_err(|e| format!("恢复任务中断：{e}"))?;
    match &result {
        Ok(r) => oplog::record("restore", &repo_log, &tgt_log,
            &format!("新建 {} 跳过 {} 覆盖 {}", r.created, r.skipped, r.overwritten), "ok", ""),
        Err(e) => oplog::record("restore", &repo_log, &tgt_log, "", "fail",
            &e.chars().take(200).collect::<String>()),
    }
    result
}

#[tauri::command]
fn export_backup(repo: String, target: String) -> Result<export::ExportResult, String> {
    let result = export::export_repo(&PathBuf::from(&repo), &PathBuf::from(&target));
    match &result {
        Ok(r) => oplog::record("export", &repo, &target,
            &format!("{} 个条目 / {}", r.entries, crate::adopt::format_bytes_pub(r.bytes)), "ok", ""),
        Err(e) => oplog::record("export", &repo, &target, "", "fail",
            &e.chars().take(200).collect::<String>()),
    }
    result
}

#[tauri::command]
fn import_backup(archive: String, repo: String) -> Result<export::ImportResult, String> {
    export::import_archive(&PathBuf::from(archive), &PathBuf::from(repo))
}

// ===================== v3：接管 / 切换 / 深度扫描 =====================

#[tauri::command]
async fn deep_scan(deep: bool, cancel: tauri::State<'_, CancellationToken>) -> Result<ScanResult, String> {
    // 新扫描前重置取消令牌，供本次扫描使用
    let token = cancel.inner().clone();
    // 多算法融合扫描放进阻塞线程池，避免遍历目录时冻结 IPC/WebView；可被 cancel_operation 中断。
    let result = tauri::async_runtime::spawn_blocking(move || {
        let mut homes = multiscan::multi_scan_cancellable(!deep, Some(&token));
        if let Some(agents) = scanner::discover_agents_home() {
            homes.push(agents);
        }
        ScanResult {
            homes,
            default_repo: default_repo(),
        }
    })
    .await
    .unwrap_or_else(|_| ScanResult { homes: Vec::new(), default_repo: default_repo() });
    // 记录扫描结果：deep 记 "deep"，quick 记 "quick"，失败不阻断返回
    let _ = scan_cache::merge_save(&result.homes, if deep { "deep" } else { "quick" });
    oplog::record("scan", "", if deep { "深度扫描" } else { "快速扫描" },
        &format!("识别到 {} 个环境", result.homes.len()), "ok", "");
    Ok(result)
}

#[tauri::command]
fn cancel_operation(cancel: tauri::State<'_, CancellationToken>) {
    cancel.inner().cancel();
}

#[tauri::command]
fn check_dsh_running() -> Vec<String> {
    adopt::detect_dsh_processes()
}

#[tauri::command]
async fn adopt_home(app: tauri::AppHandle, repo: String, home_path: String, note: String) -> Result<adopt::AdoptResult, String> {
    let repo_log = repo.clone();
    let home_log = home_path.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        adopt::adopt_home_with_progress(&PathBuf::from(repo), &PathBuf::from(home_path), &note, Some(&app))
    })
    .await
    .map_err(|e| format!("接管任务中断：{e}"))?;
    match &result {
        Ok(r) => oplog::record("adopt", &repo_log, &home_log,
            &format!("移动 {} 个目录，建 {} 个链接", r.moved_dirs.len(), r.created_links.len()), "ok", ""),
        Err(e) => oplog::record("adopt", &repo_log, &home_log, "", "fail",
            &e.chars().take(200).collect::<String>()),
    }
    result
}

#[tauri::command]
async fn unadopt_home(app: tauri::AppHandle, repo: String, home_path: String) -> Result<adopt::UnadoptResult, String> {
    let repo_log = repo.clone();
    let home_log = home_path.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        adopt::unadopt_home_with_progress(&PathBuf::from(repo), &PathBuf::from(home_path), Some(&app))
    })
    .await
    .map_err(|e| format!("断开接管任务中断：{e}"))?;
    match &result {
        Ok(r) => oplog::record("unadopt", &repo_log, &home_log,
            &format!("还原 {} 个目录", r.restored_dirs.len()), "ok", ""),
        Err(e) => oplog::record("unadopt", &repo_log, &home_log, "", "fail",
            &e.chars().take(200).collect::<String>()),
    }
    result
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdoptStatus {
    adopted: bool,
    record: Option<adopt::AdoptRecord>,
}

#[tauri::command]
fn get_adopt_status(repo: String, home_path: String) -> AdoptStatus {
    let p = PathBuf::from(&home_path);
    AdoptStatus {
        adopted: adopt::is_adopted(&p),
        record: adopt::read_adopt_record(&PathBuf::from(repo), &p),
    }
}

#[tauri::command]
fn scan_home_changes(repo: String, home_path: String) -> Result<Vec<adopt::ChangeInfo>, String> {
    adopt::check_home_changes(&PathBuf::from(repo), &PathBuf::from(home_path))
}

#[tauri::command]
fn preview_repair(repo: String, home_path: String) -> Result<adopt::RepairPreview, String> {
    adopt::preview_repair(&PathBuf::from(repo), &PathBuf::from(home_path))
}

#[tauri::command]
async fn repair_links(repo: String, home_path: String) -> Result<adopt::RepairResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        adopt::repair_links(&PathBuf::from(repo), &PathBuf::from(home_path))
    })
    .await
    .map_err(|e| format!("修复任务中断：{e}"))?
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CachedHome {
    home: crate::model::DiscoveredHome,
    exists: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanCacheResponse {
    found: bool,
    last_scan_at: String,
    last_scan_mode: String,
    homes: Vec<CachedHome>,
}

#[tauri::command]
fn load_scan_cache() -> ScanCacheResponse {
    match scan_cache::load() {
        Some(cache) => {
            let marked = scan_cache::with_existence(&cache);
            ScanCacheResponse {
                found: true,
                last_scan_at: cache.last_scan_at,
                last_scan_mode: cache.last_scan_mode,
                homes: marked
                    .into_iter()
                    .map(|(home, exists)| CachedHome { home, exists })
                    .collect(),
            }
        }
        None => ScanCacheResponse {
            found: false,
            last_scan_at: String::new(),
            last_scan_mode: String::new(),
            homes: Vec::new(),
        },
    }
}

#[tauri::command]
fn clear_scan_cache() {
    scan_cache::clear();
}

// ===================== 操作日志 =====================

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OpLogResponse {
    entries: Vec<oplog::OpEntry>,
    size_bytes: u64,
}

#[tauri::command]
fn list_op_logs() -> OpLogResponse {
    OpLogResponse { entries: oplog::read_all(), size_bytes: oplog::size_bytes() }
}

#[tauri::command]
fn export_op_logs(target: String) -> Result<String, String> {
    oplog::export_markdown(&PathBuf::from(target))
}

#[tauri::command]
fn clear_op_logs() {
    oplog::clear();
}

#[tauri::command]
fn check_link_capability(home_path: String) -> adopt::LinkCapability {
    adopt::check_link_capability(&PathBuf::from(home_path))
}

#[tauri::command]
fn detect_partial(repo: String, home_path: String) -> Option<adopt::PartialAdoption> {
    adopt::detect_partial_adoption(&PathBuf::from(repo), &PathBuf::from(home_path))
}

#[tauri::command]
fn repair_partial(repo: String, home_path: String) -> Result<Vec<String>, String> {
    adopt::repair_partial_adoption(&PathBuf::from(repo), &PathBuf::from(home_path))
}

#[tauri::command]
async fn switch_links(
    repo: String,
    source_home: String,
    target_home: String,
    include_sessions: bool,
    include_skills: bool,
    include_config: bool,
    include_memories: bool,
    include_presets: bool,
) -> Result<adopt::SwitchResult, String> {
    let repo_log = repo.clone();
    let route = format!("{} → {}", source_home, target_home);
    let result = tauri::async_runtime::spawn_blocking(move || {
        adopt::switch_links(
            &PathBuf::from(repo),
            &PathBuf::from(source_home),
            &PathBuf::from(target_home),
            include_sessions,
            include_skills,
            include_config,
            include_memories,
            include_presets,
        )
    })
    .await
    .map_err(|e| format!("切换任务中断：{e}"))?;
    match &result {
        Ok(r) => oplog::record("switch", &repo_log, &route,
            &format!("切换 {} 类内容，保险快照 {}", r.switched_links, r.backup_snapshot), "ok", ""),
        Err(e) => oplog::record("switch", &repo_log, &route, "", "fail",
            &e.chars().take(200).collect::<String>()),
    }
    result
}
#[tauri::command]
fn switch_preflight(repo: String, source_home: String, target_home: String) -> Result<adopt::SwitchPreflight, String> {
    Ok(adopt::switch_preflight(
        &PathBuf::from(repo),
        &PathBuf::from(source_home),
        &PathBuf::from(target_home),
    ))
}

#[tauri::command]
fn check_routability(target_home: String) -> Result<routecheck::RouteCheckReport, String> {
    Ok(routecheck::check_routability(&PathBuf::from(target_home)))
}

#[tauri::command]
fn list_rollback_ledgers(repo: String) -> Result<Vec<restore::RollbackLedgerEntry>, String> {
    restore::list_rollback_ledgers(&PathBuf::from(repo))
}
#[tauri::command]
fn preview_rollback_compensate(ledger_file: String) -> Result<restore::CompensatePreview, String> {
    restore::preview_rollback_compensate(&PathBuf::from(ledger_file))
}

#[tauri::command]
fn apply_rollback_compensate(ledger_file: String) -> Result<restore::CompensateResult, String> {
    restore::apply_rollback_compensate(&PathBuf::from(ledger_file))
}
#[tauri::command]
fn list_snapshots(repo: String) -> Result<Vec<repo::SnapshotInfo>, String> {
    repo::list_snapshots(&PathBuf::from(repo))
}

#[tauri::command]
fn undo_last(repo: String) -> Result<restore::UndoResult, String> {
    undo_last_impl(&PathBuf::from(repo))
}

#[allow(dead_code)]
fn print_scan_json() {
    let mut homes = scanner::discover_homes();
    if let Some(agents) = scanner::discover_agents_home() {
        homes.push(agents);
    }
    // CLI 与 GUI 一致：扫描结果写缓存（增量合并）
    let _ = scan_cache::merge_save(&homes, "quick");
    println!("{}", serde_json::to_string_pretty(&ScanResult { homes, default_repo: default_repo() }).expect("序列化扫描结果失败"));
}

fn main() {
    // CLI 诊断模式：给自动化测试和排障用，不启动窗口。
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--mcp") => {
            mcp::run_mcp_server();
            return;
        }
        Some("--scan-json") => {
            print_scan_json();
            return;
        }
        Some("--export") => {
            let repo = args.get(1).cloned().unwrap_or_else(default_repo);
            let target = args.get(2).cloned().unwrap_or_else(|| "dsh-vault-export.zip".to_string());
            match export::export_repo(&PathBuf::from(repo), &PathBuf::from(target)) {
                Ok(result) => println!("{}", serde_json::to_string_pretty(&result).expect("序列化导出结果失败")),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("--import") => {
            let archive = args.get(1).cloned().unwrap_or_default();
            let repo = args.get(2).cloned().unwrap_or_else(default_repo);
            match export::import_archive(&PathBuf::from(archive), &PathBuf::from(repo)) {
                Ok(result) => println!("{}", serde_json::to_string_pretty(&result).expect("序列化导入结果失败")),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
            return;
        }        Some("--verify") => {
            let repo = args.get(1).cloned().unwrap_or_else(default_repo);
            match restore::verify_repo(&PathBuf::from(repo)) {
                Ok(result) => println!("{}", serde_json::to_string_pretty(&result).expect("序列化校验结果失败")),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("--restore") => {
            let repo = args.get(1).cloned().unwrap_or_else(default_repo);
            let target = args.get(2).cloned().unwrap_or_else(|| "C:\\DSH-Restore-Test".to_string());
            let filter = restore::RestoreFilter {
                scope: RestoreScope::Home,
                source_home_id: None,
                target_home: Some(target),
                ids: Vec::new(),
                project: None,
                mode: RestoreMode::FillMissing,
                since: None,
            };
            match run_restore_impl(&PathBuf::from(repo), filter, None) {
                Ok(result) => println!("{}", serde_json::to_string_pretty(&result).expect("序列化恢复结果失败")),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("--backup") => {
            let repo = args.get(1).cloned().unwrap_or_else(default_repo);
            let only_config = args.iter().any(|v| v == "--only-config");
            match run_backup(&repo, "", only_config, None) {
                Ok(result) => println!("{}", serde_json::to_string_pretty(&result).expect("序列化备份结果失败")),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("--multiscan") => {
            let quick = !args.iter().any(|v| v == "--deep");
            let t = std::time::Instant::now();
            let homes = multiscan::multi_scan(quick);
            let _ = scan_cache::merge_save(&homes, if quick { "quick" } else { "deep" });
            let out = serde_json::json!({
                "elapsed_ms": t.elapsed().as_millis(),
                "count": homes.len(),
                "homes": homes.iter().map(|h| serde_json::json!({
                    "label": h.label, "path": h.path, "variant": h.variant,
                    "sessions": h.sessions.total,
                })).collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&out).expect("序列化失败"));
            return;
        }
        Some("--repair-preview") => {
            let repo = args.get(1).cloned().unwrap_or_else(default_repo);
            let home = args.get(2).cloned().unwrap_or_default();
            match adopt::preview_repair(&PathBuf::from(repo), &PathBuf::from(home)) {
                Ok(r) => println!("{}", serde_json::to_string_pretty(&r).expect("序列化失败")),
                Err(e) => { eprintln!("{e}"); std::process::exit(1); }
            }
            return;
        }
        Some("--repair") => {
            let repo = args.get(1).cloned().unwrap_or_else(default_repo);
            let home = args.get(2).cloned().unwrap_or_default();
            match adopt::repair_links(&PathBuf::from(repo), &PathBuf::from(home)) {
                Ok(r) => println!("{}", serde_json::to_string_pretty(&r).expect("序列化失败")),
                Err(e) => { eprintln!("{e}"); std::process::exit(1); }
            }
            return;
        }
        _ => {}
    }

    let context = tauri::generate_context!();
    let cdp_port = std::env::var("DSH_VAULT_CDP_PORT").ok().filter(|v| !v.trim().is_empty());
    tauri::Builder::default()
        .manage(CancellationToken::new())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            scan_homes,
            get_catalog,
            backup,
            verify,
            preview_restore,
            restore,
            list_snapshots,
            undo_last,
            list_rollback_ledgers,
            check_routability,
            switch_preflight,
            preview_rollback_compensate,
            apply_rollback_compensate,
            export_backup,
            import_backup,
            deep_scan,
            check_dsh_running,
            adopt_home,
            unadopt_home,
            get_adopt_status,
            scan_home_changes,
            switch_links,
            preview_repair,
            repair_links,
            detect_partial,
            repair_partial,
            load_scan_cache,
            clear_scan_cache,
            check_link_capability,
            cancel_operation,
            list_op_logs,
            export_op_logs,
            clear_op_logs
        ])
        .setup(move |app| {
            let mut builder = tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::default())
                .title("DSH Vault — DeepSeek Harness 备份与迁移")
                .inner_size(1180.0, 760.0)
                .min_inner_size(960.0, 640.0)
                .center();
            if let Some(port) = cdp_port {
                builder = builder.additional_browser_args(&format!(
                    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --remote-debugging-port={port} --remote-allow-origins=*"
                ));
                eprintln!("[dsh-vault] CDP 测试端口：http://127.0.0.1:{port}");
            }
            builder.build()?;
            Ok(())
        })
        .run(context)
        .expect("DSH Vault 启动失败");
}
mod restore;















