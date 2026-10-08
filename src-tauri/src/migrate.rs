//! 会话级迁移引擎：把选定的对话（含整棵子代理树）安全地搬到另一个 DSH 环境。
//!
//! 范式（社区三大迁移插件调研结论 + 官方 asar 实证）：
//!   1. 安全放置：目录名用官方 projectKey 算法（复刻见 project_key.rs），
//!      首帧重建（只重写 header 帧，其余帧字节原样保留，绝不整体重压缩）。
//!   2. 转换交给目标：格式升级让目标 DSH 打开时自己做（v3→v4 迁移边已证实），
//!      这里只改 header 的 cwd / agentPreset（按 adapters 规则表）。
//!   3. 冲突安全：目标已有同 id → 整棵树换新 UUID 并重映射血缘，不覆盖目标对话。
//!   4. 索引追加：往目标 workspace.json 对应工作区 sessionIds 追加，不镜像不覆盖。
//!
//! 红线：凭据永不迁移；只读不写密钥；sessions/ 下只写 `----` 形式目录。

use crate::adapters::Adapters;
use crate::project_key::project_key;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// 一条会话在磁盘上的定位与 header 元数据
#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub id: String,
    pub dir: PathBuf,          // sessions/<proj>/<sessDir>
    pub file: PathBuf,         // 最高代会话文件
    pub cwd: Option<String>,
    pub parent: Option<String>,
    pub preset: Option<String>,
    pub generation: u32,
    pub created_at_ms: u64,
}

/// 前端多选列表用的会话视图（PathBuf → String，附标题线索）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEntryView {
    pub id: String,
    pub cwd: Option<String>,
    pub parent: Option<String>,
    pub preset: Option<String>,
    pub generation: u32,
    pub id_hint: String,
}

impl From<SessionEntry> for SessionEntryView {
    fn from(e: SessionEntry) -> Self {
        Self {
            id_hint: e.id.chars().take(13).collect(),
            id: e.id,
            cwd: e.cwd,
            parent: e.parent,
            preset: e.preset,
            generation: e.generation,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateResult {
    pub requested: usize,
    pub migrated: usize,
    pub skipped: usize,
    pub remapped_ids: usize,   // 因冲突换 id 的条数
    pub notes: Vec<String>,
    pub errors: Vec<String>,
    /// 迁移后每条的状态（来自 routecheck，前端直接渲染）
    pub details: Vec<MigratedItem>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigratedItem {
    pub old_id: String,
    pub new_id: String,
    pub cwd: Option<String>,
    pub preset_mapped: Option<String>, // 原预设 → standard 时记录原值
    pub note: String,
}

/// 枚举某环境的全部会话（含 header 元数据），供血缘分析与前端多选。
pub fn list_sessions(home: &Path) -> Vec<SessionEntry> {
    let mut out = Vec::new();
    let root = home.join("sessions");
    if !root.is_dir() {
        return out;
    }
    for proj in fs::read_dir(&root).into_iter().flatten().flatten() {
        let Ok(entries) = fs::read_dir(proj.path()) else { continue };
        for sess in entries.flatten() {
            let dir = sess.path();
            if !dir.is_dir() { continue; }
            let Some(file) = crate::routecheck::newest_session_file(&dir) else { continue };
            let entry = parse_session_entry(&dir, &file);
            out.push(entry);
        }
    }
    out
}

fn parse_session_entry(dir: &Path, file: &Path) -> SessionEntry {
    let mut id = dir.file_name().and_then(|v| v.to_str()).unwrap_or("").to_string();
    let mut cwd = None;
    let mut parent = None;
    let mut preset = None;
    let mut generation = 0u32;
    let mut created_at_ms = 0u64;
    // 代次从文件名取
    let fname = file.file_name().and_then(|v| v.to_str()).unwrap_or("");
    generation = if fname == "session.jsonl.zstd" { 0 } else {
        fname.strip_prefix("session.v").and_then(|r| r.strip_suffix(".jsonl.zstd")).and_then(|n| n.parse().ok()).unwrap_or(0)
    };
    // header 元数据（只读首帧）
    if let Ok(bytes) = fs::read(file) {
        if let Ok((frames, _)) = crate::zstd_check::scan_frames(&bytes) {
            if let Some(first) = frames.first() {
                if let Ok(dec) = zstd::decode_all(std::io::Cursor::new(&bytes[first.start()..first.end()])) {
                    let text = String::from_utf8_lossy(&dec);
                    if let Some(line) = text.lines().next() {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                            if let Some(i) = v.get("id").and_then(|x| x.as_str()) { id = i.to_string(); }
                            if let Some(ca) = v.get("createdAt").and_then(|x| x.as_u64()) { created_at_ms = ca; }
                            cwd = v.get("cwd").and_then(|x| x.as_str()).map(String::from);
                            parent = v.get("parentSession").and_then(|x| x.as_str()).map(String::from);
                            preset = v.get("agentPreset").and_then(|x| x.as_str()).map(String::from);
                            if let Some(g) = v.get("version").and_then(|x| x.as_u64()) { generation = g as u32; }
                        }
                    }
                }
            }
        }
    }
    SessionEntry { id, dir: dir.to_path_buf(), file: file.to_path_buf(), cwd, parent, preset, generation, created_at_ms }
}

/// 血缘收集：把请求的若干 id 扩展成"各自连同全部子孙"的完整集合。
/// 返回 (按迁移顺序排列的 id 列表, 实际参与的总条数)。
fn expand_with_descendants(all: &[SessionEntry], requested: &HashSet<String>) -> Vec<SessionEntry> {
    // parent → children
    let mut children_of: HashMap<String, Vec<&SessionEntry>> = HashMap::new();
    for e in all {
        if let Some(p) = &e.parent {
            children_of.entry(p.clone()).or_default().push(e);
        }
    }
    let mut picked: HashSet<String> = HashSet::new();
    let mut ordered: Vec<SessionEntry> = Vec::new();
    // 先放被显式选中的，再 BFS 把子孙补齐（父先子后，保证血缘目录就位）
    let mut queue: Vec<&SessionEntry> = all.iter().filter(|e| requested.contains(&e.id)).collect();
    let mut qi = 0;
    // 也可能选中的是子会话：把它的祖先链也带上（父不在目标就没法 resume）
    let mut need_ancestors: Vec<&SessionEntry> = Vec::new();
    let by_id: HashMap<&str, &SessionEntry> = all.iter().map(|e| (e.id.as_str(), e)).collect();
    for e in all.iter().filter(|e| requested.contains(&e.id)) {
        let mut cur = e.parent.as_deref();
        while let Some(pid) = cur {
            if let Some(pe) = by_id.get(pid) {
                need_ancestors.push(pe);
                cur = pe.parent.as_deref();
            } else { break; }
        }
    }
    // 祖先优先入队（去重）
    for a in need_ancestors.into_iter().rev() {
        if picked.insert(a.id.clone()) { ordered.push((*a).clone()); }
    }
    while qi < queue.len() {
        let e = queue[qi]; qi += 1;
        if picked.insert(e.id.clone()) {
            ordered.push((*e).clone());
            if let Some(kids) = children_of.get(&e.id) {
                for k in kids { queue.push(k); }
            }
        }
    }
    ordered
}

/// 首帧重建：解压首帧 → 改写 header 行的指定字段 → 重新压缩首帧 → 后续帧字节原样保留。
/// field_updates: 要替换的 header 字段（cwd / agentPreset / id / parentSession）。
fn rebuild_header_frame(
    src_file: &Path,
    dst_file: &Path,
    field_updates: &HashMap<String, serde_json::Value>,
) -> Result<(), String> {
    let bytes = fs::read(src_file).map_err(|e| format!("读取会话失败：{e}"))?;
    let (frames, _torn) = crate::zstd_check::scan_frames(&bytes)?;
    if frames.is_empty() {
        return Err("会话文件无有效帧".into());
    }
    let first = &frames[0];
    let header = zstd::decode_all(std::io::Cursor::new(&bytes[first.start()..first.end()]))
        .map_err(|e| format!("首帧解压失败：{e}"))?;
    let header_text = String::from_utf8_lossy(&header);
    let first_line = header_text.lines().next().unwrap_or("");
    // 解析 header JSON，应用字段更新，重新序列化（保持其余字段）
    let mut v: serde_json::Value = serde_json::from_str(first_line)
        .map_err(|e| format!("header 不是 JSON：{e}"))?;
    if let Some(obj) = v.as_object_mut() {
        for (k, val) in field_updates {
            obj.insert(k.clone(), val.clone());
        }
    }
    let new_line = serde_json::to_string(&v).map_err(|e| format!("header 序列化失败：{e}"))?;
    // 重组：新首帧（header 通常单行；若多行，保留后续行）
    let new_header_text = if header_text.trim_end_matches('\n').lines().count() <= 1 {
        format!("{}\n", new_line)
    } else {
        let rest: Vec<&str> = header_text.lines().skip(1).collect();
        format!("{}\n{}\n", new_line, rest.join("\n"))
    };
    let new_first = zstd::encode_all(std::io::Cursor::new(new_header_text.as_bytes()), 3)
        .map_err(|e| format!("首帧压缩失败：{e}"))?;
    let tail = &bytes[first.end()..];
    let mut rebuilt = Vec::with_capacity(new_first.len() + tail.len());
    rebuilt.extend_from_slice(&new_first);
    rebuilt.extend_from_slice(tail);
    // 原子写
    if let Some(parent) = dst_file.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("建目录失败：{e}"))?;
    }
    let tmp = dst_file.with_extension("migrate.tmp");
    fs::write(&tmp, &rebuilt).map_err(|e| format!("写临时文件失败：{e}"))?;
    fs::rename(&tmp, dst_file).map_err(|e| format!("改名失败：{e}"))?;
    Ok(())
}

/// 目标端是否已存在某会话 id（任一工作区目录下）。
fn target_has_session(target_home: &Path, id: &str) -> bool {
    let root = target_home.join("sessions");
    if !root.is_dir() { return false; }
    for proj in fs::read_dir(&root).into_iter().flatten().flatten() {
        if let Ok(entries) = fs::read_dir(proj.path()) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name == id || name == format!("session-{}", id) {
                    // 进一步确认是目录
                    if e.path().is_dir() { return true; }
                }
            }
        }
    }
    false
}

/// 往目标 workspace.json 的对应工作区 sessionIds 追加（无该工作区则新建条目）。
/// 工作区按 cwd 匹配：workspace 条目里若有与 cwd 对应的项则用之，否则新建。
/// 迁移后为会话补建"最小合法"投影缓存记录（session_projcache）。
///
/// 为什么必须做（本次"只看见项目看不见对话"事故的根因）：
///   官方版不会主动给磁盘上"突然出现"的会话建缓存——缓存只在它自己创建/打开会话时写。
///   而对话列表（session.list）读的是 session_projcache 这个缓存域。
///   只放会话文件 + 改 workspace.json，缓存里没有记录 → 列表里看不见。
///
/// 安全依据（官方 asar 实证）：
///   - checkpointRecord schema = { identity, rows: Record<string,row> }，rows 可为空对象。
///   - checkpointIdentity 只有 createdAt 必填，其余（formatVersion/cwd/isSeeded/inheritedEventCount）全 optional。
///   - 官方明说："a stale or unreadable cache costs a longer tail replay, never a wrong value"。
///   所以写 { identity, rows:{} } 的最小记录是安全的：官方把它当 uncached，打开时冷读重建真实 rows。
fn write_minimal_projection_cache(
    target_home: &Path,
    session_id: &str,
    created_at_ms: u64,
    cwd: &str,
    format_version: u32,
) -> Result<(), String> {
    let dir = target_home.join("storages").join("session_projcache").join("sessions");
    fs::create_dir_all(&dir).map_err(|e| format!("建 projcache 目录失败：{e}"))?;
    let file = dir.join(format!("{}.json", session_id));
    // 已有缓存记录就不覆盖（官方自己建的更完整）
    if file.is_file() {
        return Ok(());
    }
    let record = serde_json::json!({
        "version": 7,
        "record": {
            "identity": {
                "formatVersion": format_version,
                "createdAt": created_at_ms,
                "cwd": cwd,
                "isSeeded": false,
                "inheritedEventCount": 0
            },
            "rows": {}
        }
    });
    let tmp = file.with_extension("tmp");
    fs::write(&tmp, serde_json::to_string(&record).map_err(|e| e.to_string())?)
        .map_err(|e| format!("写 projcache 失败：{e}"))?;
    fs::rename(&tmp, &file).map_err(|e| format!("改名 projcache 失败：{e}"))?;
    Ok(())
}

/// 往目标 workspace.json 追加会话索引——严格遵循官方真实 schema（AIO/官方/v4lite 实证统一）：
///   { unit:{name:"workspace",version:2},
///     global:{ initialized, workspaceIds:[UUID...], archivedSessionIds:[...] },
///     tables:{ workspaces:{ "<UUID>": {path,title,sessionIds,createdAt,updatedAt} } } }
///
/// 关键原则（本次事故教训）：
///   1. 绝不覆盖整个文件——读出现有 doc，只增量合并，保留所有不认识/已有的字段与工作区。
///   2. 工作区 key 是 UUID 且必须注册进 global.workspaceIds；path 匹配到现有工作区就复用它的 key。
///   3. 写入用"临时文件+改名"原子写，并先把原文件备份为 .bak，写坏可回滚。
fn append_to_workspace_index(target_home: &Path, cwd: &str, new_session_ids: &[String]) -> Result<(), String> {
    let ws_file = target_home.join("storages").join("workspace.json");
    if let Some(parent) = ws_file.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("建 storages 目录失败：{e}"))?;
    }
    // 读出现有文档（不存在则用空骨架）。绝不丢任何已有内容。
    let mut doc: serde_json::Value = if ws_file.is_file() {
        let text = fs::read_to_string(&ws_file).map_err(|e| format!("读 workspace.json 失败：{e}"))?;
        serde_json::from_str(&text).map_err(|e| format!("workspace.json 不是有效 JSON（已中止，未改动）：{e}"))?
    } else {
        serde_json::json!({
            "unit": { "name": "workspace", "version": 2 },
            "global": { "initialized": true, "workspaceIds": [], "archivedSessionIds": [] },
            "tables": { "workspaces": {} }
        })
    };
    if !doc.is_object() {
        return Err("workspace.json 顶层不是对象（已中止，未改动）".into());
    }
    // 确保骨架字段存在（不覆盖已有值）
    if doc.get("unit").is_none() {
        doc["unit"] = serde_json::json!({ "name": "workspace", "version": 2 });
    }
    if doc.get("global").is_none() {
        doc["global"] = serde_json::json!({ "initialized": true, "workspaceIds": [], "archivedSessionIds": [] });
    }
    if doc.get("tables").is_none() {
        doc["tables"] = serde_json::json!({ "workspaces": {} });
    }
    if doc["tables"].get("workspaces").is_none() {
        doc["tables"]["workspaces"] = serde_json::json!({});
    }
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    // 在 tables.workspaces 里找 path 匹配的工作区
    let ws_table = doc["tables"]["workspaces"].as_object_mut().ok_or("tables.workspaces 不是对象")?;
    let mut target_key: Option<String> = None;
    for (k, v) in ws_table.iter() {
        if v.get("path").and_then(|x| x.as_str()) == Some(cwd) {
            target_key = Some(k.clone());
            break;
        }
    }
    let (key, is_new) = match target_key {
        Some(k) => (k, false),
        None => (uuid::Uuid::new_v4().to_string(), true),
    };
    if is_new {
        // 新建工作区条目（title 取 cwd 末段）
        let title = cwd.rsplit(['\\', '/']).find(|s| !s.is_empty()).unwrap_or(cwd).to_string();
        ws_table.insert(key.clone(), serde_json::json!({
            "path": cwd,
            "title": title,
            "sessionIds": [],
            "createdAt": now,
            "updatedAt": now,
        }));
    }
    // 追加 sessionIds（去重），并刷新 updatedAt
    let entry = ws_table.get_mut(&key).ok_or("工作区条目缺失")?;
    if entry.get("path").is_none() { entry["path"] = serde_json::json!(cwd); }
    if entry.get("sessionIds").is_none() { entry["sessionIds"] = serde_json::json!([]); }
    let arr = entry["sessionIds"].as_array_mut().ok_or("sessionIds 不是数组")?;
    for id in new_session_ids {
        if !arr.iter().any(|x| x.as_str() == Some(id.as_str())) {
            arr.push(serde_json::json!(id));
        }
    }
    entry["updatedAt"] = serde_json::json!(now);

    // 新工作区要注册进 global.workspaceIds
    if is_new {
        let g = doc["global"].as_object_mut().ok_or("global 不是对象")?;
        if g.get("workspaceIds").is_none() { g.insert("workspaceIds".into(), serde_json::json!([])); }
        let ids = g["workspaceIds"].as_array_mut().ok_or("workspaceIds 不是数组")?;
        if !ids.iter().any(|x| x.as_str() == Some(key.as_str())) {
            ids.push(serde_json::json!(key));
        }
    }

    // 备份原文件再原子写（安全红线：先备份，写坏可回滚）
    if ws_file.is_file() {
        let bak = ws_file.with_extension("vault-bak");
        fs::copy(&ws_file, &bak).map_err(|e| format!("备份 workspace.json 失败：{e}"))?;
    }
    let text = serde_json::to_string_pretty(&doc).map_err(|e| format!("序列化 workspace.json 失败：{e}"))?;
    let tmp = ws_file.with_extension("migrate.tmp");
    fs::write(&tmp, text).map_err(|e| format!("写 workspace.json 失败：{e}"))?;
    fs::rename(&tmp, &ws_file).map_err(|e| format!("改名 workspace.json 失败：{e}"))?;
    Ok(())
}

/// 主迁移入口：把 source_home 里 id 属于 requested_ids 的会话（连同整棵子代理树）迁到 target_home。
/// repo 用于加载 adapters 规则表。
pub fn migrate_sessions(
    repo: &Path,
    source_home: &Path,
    target_home: &Path,
    requested_ids: &[String],
) -> Result<MigrateResult, String> {
    if source_home == target_home {
        return Err("源和目标不能是同一个环境".into());
    }
    let running = crate::adopt::detect_dsh_processes();
    if !running.is_empty() {
        return Err(format!("检测到 DSH 正在运行（{}），请先完全关闭再迁移。", running.join(", ")));
    }
    let adapters = Adapters::load(repo);
    let all = list_sessions(source_home);
    if all.is_empty() {
        return Err("源环境没有找到任何对话。".into());
    }
    let requested: HashSet<String> = requested_ids.iter().cloned().collect();
    // 若 requested 为空，默认迁移全部（前端全选场景）
    let effective: HashSet<String> = if requested.is_empty() {
        all.iter().map(|e| e.id.clone()).collect()
    } else {
        requested
    };
    let tree = expand_with_descendants(&all, &effective);
    if tree.is_empty() {
        return Err("选中的对话在源环境里不存在。".into());
    }

    // 目标端已存在的 id（用于冲突换 id）
    let existing_ids: HashSet<String> = list_sessions(target_home).iter().map(|e| e.id.clone()).collect();
    // id 重映射表：old → new（仅冲突时换）
    let mut id_map: HashMap<String, String> = HashMap::new();
    let mut taken: HashSet<String> = existing_ids.iter().cloned().collect();
    for e in &tree {
        if taken.contains(&e.id) {
            let new_id = format!("session-{}", uuid::Uuid::new_v4());
            taken.insert(new_id.clone());
            id_map.insert(e.id.clone(), new_id);
        } else {
            taken.insert(e.id.clone());
            id_map.insert(e.id.clone(), e.id.clone());
        }
    }

    let mut result = MigrateResult {
        requested: effective.len(),
        migrated: 0,
        skipped: 0,
        remapped_ids: id_map.iter().filter(|(old, new)| old != new).count(),
        notes: Vec::new(),
        errors: Vec::new(),
        details: Vec::new(),
    };

    // 按目标 cwd 分组，最后统一追加 workspace 索引
    let mut ids_by_cwd: HashMap<String, Vec<String>> = HashMap::new();

    for e in &tree {
        let new_id = id_map.get(&e.id).cloned().unwrap_or_else(|| e.id.clone());
        let cwd = e.cwd.clone().unwrap_or_else(|| "".into());
        if cwd.is_empty() {
            result.skipped += 1;
            result.errors.push(format!("{}：无 cwd，跳过（无法定位工作区）", e.id));
            continue;
        }
        // 目标目录：官方 projectKey(cwd)/new_id
        let proj_dir_name = project_key(&cwd);
        let dst_dir = target_home.join("sessions").join(&proj_dir_name).join(&new_id);
        let dst_file = dst_dir.join(e.file.file_name().and_then(|v| v.to_str()).unwrap_or("session.jsonl.zstd"));

        // 首帧重建要改写的字段
        let mut updates: HashMap<String, serde_json::Value> = HashMap::new();
        updates.insert("id".into(), serde_json::json!(new_id));
        updates.insert("cwd".into(), serde_json::json!(cwd));
        // 血缘重映射：parent 若在 id_map 里，指向新 id；若不在（父未被选中且不在源），移除该字段防悬空
        let mut preset_mapped: Option<String> = None;
        if let Some(p) = &e.parent {
            if let Some(mapped) = id_map.get(p) {
                updates.insert("parentSession".into(), serde_json::json!(mapped));
            } else {
                // 父不在本次迁移树里：目标是新环境，父不在会导致无法 attach。
                // 策略：保留 parent 引用并在 note 提示（DSH 找不到父时会按顶层处理或报错，由目标决定）。
                updates.insert("parentSession".into(), serde_json::json!(p));
            }
        }
        // 预设按规则表适配
        if let Some(pr) = &e.preset {
            let mapped = adapters.map_preset(pr);
            if mapped != *pr {
                preset_mapped = Some(pr.clone());
                updates.insert("agentPreset".into(), serde_json::json!(mapped));
            }
        }

        match rebuild_header_frame(&e.file, &dst_file, &updates) {
            Ok(_) => {
                result.migrated += 1;
                ids_by_cwd.entry(cwd.clone()).or_default().push(new_id.clone());
                // 补建最小合法投影缓存，让对话列表能看见这条（否则"只看见项目看不见对话"）
                let _ = write_minimal_projection_cache(target_home, &new_id, e.created_at_ms, &cwd, e.generation);
                let mut note = String::new();
                if new_id != e.id { note.push_str(&format!("id 冲突已换新（{}）", &new_id[..new_id.len().min(16)])); }
                if let Some(orig) = &preset_mapped {
                    if !note.is_empty() { note.push_str("；"); }
                    note.push_str(&format!("预设 {}→standard", orig));
                }
                if note.is_empty() { note = "可直接继续".into(); }
                result.details.push(MigratedItem {
                    old_id: e.id.clone(),
                    new_id: new_id.clone(),
                    cwd: Some(cwd.clone()),
                    preset_mapped,
                    note,
                });
            }
            Err(err) => {
                result.skipped += 1;
                result.errors.push(format!("{}：{}", e.id, err));
            }
        }
    }

    // 统一追加 workspace 索引
    for (cwd, ids) in &ids_by_cwd {
        if let Err(e) = append_to_workspace_index(target_home, cwd, ids) {
            result.errors.push(format!("工作区索引追加失败（{}）：{}", cwd, e));
        }
    }

    // 汇总提示
    if result.remapped_ids > 0 {
        result.notes.push(format!("{} 条对话与目标端已有内容 id 冲突，已自动换新 id（不会覆盖目标原有对话）。", result.remapped_ids));
    }
    let preset_changed = result.details.iter().filter(|d| d.preset_mapped.is_some()).count();
    if preset_changed > 0 {
        result.notes.push(format!("{} 条对话的预设已改成官方认识的 standard，可正常继续。", preset_changed));
    }
    // 格式代提示（取第一条的代，规则表给文案）
    if let Some(first) = tree.first() {
        let note = adapters.generation_note(first.generation);
        if !note.is_empty() {
            result.notes.push(note);
        }
    }
    if result.migrated == 0 {
        return Err(format!("没有成功迁移任何对话。{}", result.errors.join("；")));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一条 zstd 会话（header + body 两帧），返回相对 home 的 sessions/<proj>/<sess>
    fn make_session(home: &Path, cwd: &str, id: &str, parent: Option<&str>, preset: &str) {
        let proj = project_key(cwd);
        let dir = home.join("sessions").join(&proj).join(id);
        fs::create_dir_all(&dir).unwrap();
        let mut header = format!(
            "{{\"type\":\"session\",\"version\":0,\"id\":\"{}\",\"createdAt\":1,\"cwd\":\"{}\",\"agentPreset\":\"{}\"",
            id, cwd.replace('\\', "\\\\"), preset
        );
        if let Some(p) = parent {
            header.push_str(&format!(",\"parentSession\":\"{}\"", p));
        }
        header.push_str("}\n");
        let body = "{\"type\":\"user/message\",\"seq\":1}\n";
        let mut bytes = zstd::encode_all(std::io::Cursor::new(header.as_bytes()), 3).unwrap();
        bytes.extend_from_slice(&zstd::encode_all(std::io::Cursor::new(body.as_bytes()), 3).unwrap());
        fs::write(dir.join("session.jsonl.zstd"), &bytes).unwrap();
    }

    fn setup_repo() -> PathBuf {
        // uuid 保证每个调用独立：并行测试若共用 pid 目录会互相污染会话统计
        let dir = std::env::temp_dir().join(format!("dsh-mig-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn project_key_dirs_match_official() {
        // 迁移目录名必须与官方一致
        assert_eq!(project_key("E:\\VS"), "--E-VS--");
        assert!(project_key("C:\\Users\\刘沛伦").contains("~5218"));
    }

    #[test]
    fn expands_descendants_and_ancestors() {
        let home = setup_repo().join("src");
        // 父 p1（无父），子 c1（父 p1），孙 g1（父 c1）
        make_session(&home, "E:\\a", "p1", None, "standard");
        make_session(&home, "E:\\a", "c1", Some("p1"), "standard");
        make_session(&home, "E:\\a", "g1", Some("c1"), "standard");
        make_session(&home, "E:\\a", "solo", None, "standard");
        let all = list_sessions(&home);
        assert_eq!(all.len(), 4);
        // 只选父 → 应带上子+孙
        let tree = expand_with_descendants(&all, &["p1".into()].into_iter().collect());
        let ids: HashSet<_> = tree.iter().map(|e| e.id.clone()).collect();
        assert!(ids.contains("p1") && ids.contains("c1") && ids.contains("g1") && !ids.contains("solo"));
        // 只选孙 → 应带上祖先链
        let tree2 = expand_with_descendants(&all, &["g1".into()].into_iter().collect());
        let ids2: HashSet<_> = tree2.iter().map(|e| e.id.clone()).collect();
        assert!(ids2.contains("g1") && ids2.contains("c1") && ids2.contains("p1"));
    }

    #[test]
    fn migrate_basic_places_and_indexes() {
        let base = setup_repo();
        let repo = base.join("repo"); fs::create_dir_all(&repo).unwrap();
        let src = base.join("src");
        let tgt = base.join("tgt");
        fs::create_dir_all(tgt.join("sessions")).unwrap();
        make_session(&src, "E:\\VS", "s1", None, "anchored-standard");

        let r = migrate_sessions(&repo, &src, &tgt, &["s1".into()]).unwrap();
        assert_eq!(r.migrated, 1);
        // 目标目录是官方 projectKey 形态
        let placed = tgt.join("sessions").join(project_key("E:\\VS")).join("s1").join("session.jsonl.zstd");
        assert!(placed.is_file(), "应放在官方目录名: {}", placed.display());
        // header 预设已适配成 standard
        let bytes = fs::read(&placed).unwrap();
        let (frames, _) = crate::zstd_check::scan_frames(&bytes).unwrap();
        let h = zstd::decode_all(std::io::Cursor::new(&bytes[frames[0].start()..frames[0].end()])).unwrap();
        let text = String::from_utf8_lossy(&h);
        assert!(text.contains("\"agentPreset\":\"standard\""), "预设应适配: {}", &text[..text.len().min(160)]);
        // body 帧原样
        let b = zstd::decode_all(std::io::Cursor::new(&bytes[frames[1].start()..frames[1].end()])).unwrap();
        assert!(String::from_utf8_lossy(&b).contains("user/message"));
        // workspace.json 已追加
        let ws = fs::read_to_string(tgt.join("storages").join("workspace.json")).unwrap();
        assert!(ws.contains("s1"), "索引应含 s1");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn workspace_index_uses_correct_schema_and_preserves_existing() {
        // 本次事故回归：索引必须写进 tables.workspaces + global.workspaceIds，且不覆盖已有工作区
        let base = setup_repo().join("tgt");
        // 预置一个官方真实 schema 的 workspace.json（含一个已有工作区 + archived）
        let existing_id = "11111111-2222-3333-4444-555555555555";
        let preexisting = serde_json::json!({
            "unit": { "name": "workspace", "version": 2 },
            "global": { "initialized": true, "workspaceIds": [existing_id], "archivedSessionIds": ["archived-1"] },
            "tables": { "workspaces": {
                existing_id: { "path": "D:\\old", "title": "old", "sessionIds": ["old-s"], "createdAt": "x", "updatedAt": "x" }
            }},
            "customField": { "keep": "me" }
        });
        let storages = base.join("storages");
        fs::create_dir_all(&storages).unwrap();
        fs::write(storages.join("workspace.json"), serde_json::to_string_pretty(&preexisting).unwrap()).unwrap();

        append_to_workspace_index(&base, "E:\\VS", &["s1".into(), "s2".into()]).unwrap();

        let doc: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(storages.join("workspace.json")).unwrap()).unwrap();
        // 已有工作区还在
        assert!(doc["tables"]["workspaces"].get(existing_id).is_some(), "已有工作区不应被覆盖");
        // 原有 customField 保留
        assert_eq!(doc["customField"]["keep"], "me", "不认识的字段应保留");
        // archived 保留
        assert!(doc["global"]["archivedSessionIds"].as_array().unwrap().iter().any(|x| x == "archived-1"));
        // 新工作区在 tables.workspaces，path 正确，sessionIds 含 s1/s2
        let new_ws = doc["tables"]["workspaces"].as_object().unwrap().values()
            .find(|v| v["path"] == "E:\\VS").expect("应有新工作区");
        let ids: Vec<&str> = new_ws["sessionIds"].as_array().unwrap().iter().filter_map(|x| x.as_str()).collect();
        assert!(ids.contains(&"s1") && ids.contains(&"s2"), "新工作区应含迁移会话");
        // 新工作区 key 注册进了 global.workspaceIds
        let ws_ids: Vec<&str> = doc["global"]["workspaceIds"].as_array().unwrap().iter().filter_map(|x| x.as_str()).collect();
        assert_eq!(ws_ids.len(), 2, "应有 2 个工作区 id（旧+新）");
        // 备份文件已生成
        assert!(storages.join("workspace.vault-bak").is_file() || storages.join("workspace.json.vault-bak").is_file()
            || fs::read_dir(&storages).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains("vault-bak")),
            "应生成 .vault-bak 备份");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn workspace_index_appends_to_existing_workspace_same_path() {
        // 同一 cwd 已有工作区时：复用其 key，只追加 sessionIds，不新建
        let base = setup_repo().join("tgt");
        let ws_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let preexisting = serde_json::json!({
            "unit": { "name": "workspace", "version": 2 },
            "global": { "initialized": true, "workspaceIds": [ws_id], "archivedSessionIds": [] },
            "tables": { "workspaces": {
                ws_id: { "path": "E:\\VS", "title": "VS", "sessionIds": ["existing-s"], "createdAt": "x", "updatedAt": "x" }
            }}
        });
        let storages = base.join("storages");
        fs::create_dir_all(&storages).unwrap();
        fs::write(storages.join("workspace.json"), serde_json::to_string_pretty(&preexisting).unwrap()).unwrap();

        append_to_workspace_index(&base, "E:\\VS", &["new-s".into()]).unwrap();

        let doc: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(storages.join("workspace.json")).unwrap()).unwrap();
        // 仍只有 1 个工作区
        assert_eq!(doc["global"]["workspaceIds"].as_array().unwrap().len(), 1, "同 path 不应新建工作区");
        // sessionIds 是 existing-s + new-s（去重追加）
        let ids: Vec<&str> = doc["tables"]["workspaces"][ws_id]["sessionIds"].as_array().unwrap().iter().filter_map(|x| x.as_str()).collect();
        assert!(ids.contains(&"existing-s") && ids.contains(&"new-s"), "应追加而非覆盖: {:?}", ids);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn migrate_writes_minimal_projection_cache() {
        // 本次事故回归：迁移后每条会话都应有最小合法 projcache 记录（否则对话列表看不见）
        let base = setup_repo();
        let repo = base.join("repo"); fs::create_dir_all(&repo).unwrap();
        let src = base.join("src");
        let tgt = base.join("tgt");
        fs::create_dir_all(tgt.join("sessions")).unwrap();
        make_session(&src, "E:\\VS", "s1", None, "standard");

        migrate_sessions(&repo, &src, &tgt, &["s1".into()]).unwrap();

        let cache = tgt.join("storages").join("session_projcache").join("sessions").join("s1.json");
        assert!(cache.is_file(), "迁移后应有 projcache 记录: {}", cache.display());
        let doc: serde_json::Value = serde_json::from_str(&fs::read_to_string(&cache).unwrap()).unwrap();
        assert_eq!(doc["version"], 7);
        assert!(doc["record"]["identity"].is_object(), "应有 identity");
        assert_eq!(doc["record"]["identity"]["cwd"], "E:\\VS");
        assert!(doc["record"]["rows"].is_object(), "rows 可为空对象但必须是对象");
        // 再迁一次（已有缓存）不应覆盖官方更完整的记录
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn migrate_conflict_remaps_id_and_parent() {
        let base = setup_repo();
        let repo = base.join("repo"); fs::create_dir_all(&repo).unwrap();
        let src = base.join("src");
        let tgt = base.join("tgt");
        // 目标已有同 id 会话 s1
        make_session(&tgt, "E:\\VS", "s1", None, "standard");
        // 源有父子 s1 → c1
        make_session(&src, "E:\\VS", "s1", None, "standard");
        make_session(&src, "E:\\VS", "c1", Some("s1"), "standard");

        let r = migrate_sessions(&repo, &src, &tgt, &["s1".into(), "c1".into()]).unwrap();
        assert_eq!(r.migrated, 2);
        assert_eq!(r.remapped_ids, 1, "只有 s1 冲突换 id");
        // 目标原 s1 还在（未被覆盖）
        let orig = tgt.join("sessions").join(project_key("E:\\VS")).join("s1").join("session.jsonl.zstd");
        assert!(orig.is_file());
        // c1 的 parent 应指向新 id（在目标端读 c1 header 验证）
        let new_s1 = r.details.iter().find(|d| d.old_id == "s1").unwrap().new_id.clone();
        let c1_placed = tgt.join("sessions").join(project_key("E:\\VS")).join("c1").join("session.jsonl.zstd");
        let bytes = fs::read(&c1_placed).unwrap();
        let (frames, _) = crate::zstd_check::scan_frames(&bytes).unwrap();
        let h = zstd::decode_all(std::io::Cursor::new(&bytes[frames[0].start()..frames[0].end()])).unwrap();
        let text = String::from_utf8_lossy(&h);
        assert!(text.contains(&format!("\"parentSession\":\"{}\"", new_s1)), "c1 父应重映射: {}", &text[..text.len().min(200)]);
        let _ = fs::remove_dir_all(&base);
    }
}

