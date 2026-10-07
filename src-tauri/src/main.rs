#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod adopt;
mod mcp;
mod model;
mod multiscan;
mod repo;
mod scanner;
mod export;
mod zstd_check;

use repo::{default_repo, load_manifest, run_backup, RestoreMode, RestoreScope};
use restore::{preview_restore as preview_restore_impl, run_restore as run_restore_impl, undo_last as undo_last_impl};
use serde::Serialize;
use std::path::PathBuf;

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
fn backup(app: tauri::AppHandle, repo: String, note: String, only_config: bool) -> Result<repo::BackupResult, String> {
    run_backup(&repo, &note, only_config, Some(&app))
}

#[tauri::command]
fn verify(repo: String) -> Result<restore::VerifyResult, String> {
    restore::verify_repo(&PathBuf::from(repo))
}

#[tauri::command]
fn preview_restore(
    repo: String,
    scope: RestoreScope,
    source_home_id: Option<String>,
    target_home: Option<String>,
    ids: Vec<String>,
    project: Option<String>,
    mode: RestoreMode,
) -> Result<restore::RestorePreview, String> {
    preview_restore_impl(&PathBuf::from(repo), restore::RestoreFilter {
        scope,
        source_home_id,
        target_home,
        ids,
        mode,
        project,
        since: None,
    })
}

#[tauri::command]
fn restore(app: tauri::AppHandle,
    repo: String,
    scope: RestoreScope,
    source_home_id: Option<String>,
    target_home: Option<String>,
    ids: Vec<String>,
    project: Option<String>,
    mode: RestoreMode,
) -> Result<restore::RestoreResult, String> {
    run_restore_impl(&PathBuf::from(repo), restore::RestoreFilter {
        scope,
        source_home_id,
        target_home,
        ids,
        project,
        mode,
        since: None,
    }, Some(&app))
}

#[tauri::command]
fn export_backup(repo: String, target: String) -> Result<export::ExportResult, String> {
    export::export_repo(&PathBuf::from(repo), &PathBuf::from(target))
}

#[tauri::command]
fn import_backup(archive: String, repo: String) -> Result<export::ImportResult, String> {
    export::import_archive(&PathBuf::from(archive), &PathBuf::from(repo))
}

// ===================== v3：接管 / 切换 / 深度扫描 =====================

#[tauri::command]
fn deep_scan(deep: bool) -> ScanResult {
    // 多算法融合扫描：quick=快速（常见位置深度3），deep=全盘（所有固定盘深度5）
    let mut homes = multiscan::multi_scan(!deep);
    if let Some(agents) = scanner::discover_agents_home() {
        homes.push(agents);
    }
    ScanResult {
        homes,
        default_repo: default_repo(),
    }
}

#[tauri::command]
fn check_dsh_running() -> Vec<String> {
    adopt::detect_dsh_processes()
}

#[tauri::command]
fn adopt_home(app: tauri::AppHandle, repo: String, home_path: String, note: String) -> Result<adopt::AdoptResult, String> {
    adopt::adopt_home_with_progress(&PathBuf::from(repo), &PathBuf::from(home_path), &note, Some(&app))
}

#[tauri::command]
fn unadopt_home(repo: String, home_path: String) -> Result<adopt::UnadoptResult, String> {
    adopt::unadopt_home(&PathBuf::from(repo), &PathBuf::from(home_path))
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
fn repair_links(repo: String, home_path: String) -> Result<adopt::RepairResult, String> {
    adopt::repair_links(&PathBuf::from(repo), &PathBuf::from(home_path))
}

#[tauri::command]
fn switch_links(
    repo: String,
    source_home: String,
    target_home: String,
    include_sessions: bool,
    include_skills: bool,
    include_config: bool,
    include_memories: bool,
) -> Result<adopt::SwitchResult, String> {
    adopt::switch_links(
        &PathBuf::from(repo),
        &PathBuf::from(source_home),
        &PathBuf::from(target_home),
        include_sessions,
        include_skills,
        include_config,
        include_memories,
    )
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
            repair_links
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















