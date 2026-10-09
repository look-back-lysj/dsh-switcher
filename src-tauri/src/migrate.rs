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

/// 复刻官方 realpathNormalize（Node fs.realpath）：解析链接/短名/`..`，
/// 并去掉 Windows verbatim 前缀（\\?\），让写进 workspace.json 的 path
/// 与 DSH 内部 sessionPath 的字符串完全一致——对不上时，官方一次 mutate
/// 就会把这条会话登记当作「不再匹配」清掉。
pub(crate) fn realpath_like(path: &str) -> Option<String> {
    let canonical = std::fs::canonicalize(path).ok()?;
    Some(strip_verbatim_prefix(&canonical.to_string_lossy()))
}

fn strip_verbatim_prefix(value: &str) -> String {
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{}", rest)
    } else if let Some(rest) = value.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        value.to_string()
    }
}

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
    /// v6：本次为让对话能发消息而补齐到目标端的模型提供方 id
    #[serde(default)]
    pub provider_added: Vec<String>,
    /// v6：还需要在目标版本界面补录的 Key 名（只列名，不列值）
    #[serde(default)]
    pub keys_to_enter: Vec<String>,
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

    // v8：把 cwd 规范化为官方 realpathNormalize 的形态（目录存在时）。
    // 官方的 sessionPath 是 realpath 结果，字符串对不上时，官方一次 mutate
    // 就会把这条会话登记当作「不再匹配」清掉。
    let cwd_norm = realpath_like(cwd).unwrap_or_else(|| cwd.replace('/', "\\"));

    // 在 tables.workspaces 里找 path 匹配的工作区（先精确，再按 realpath 规范化比较）
    let ws_table = doc["tables"]["workspaces"].as_object_mut().ok_or("tables.workspaces 不是对象")?;
    let mut target_key: Option<String> = None;
    for (k, v) in ws_table.iter() {
        let Some(entry_path) = v.get("path").and_then(|x| x.as_str()) else { continue };
        let matched = entry_path == cwd_norm
            || realpath_like(entry_path).map(|p| p == cwd_norm).unwrap_or(false);
        if matched {
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
            "path": cwd_norm,
            "title": title,
            "sessionIds": [],
            "createdAt": now,
            "updatedAt": now,
        }));
    }
    // 追加 sessionIds（去重），并刷新 updatedAt
    let entry = ws_table.get_mut(&key).ok_or("工作区条目缺失")?;
    if entry.get("path").is_none() { entry["path"] = serde_json::json!(cwd_norm); }
    // 已有记录的 path 若不是 realpath 形态，顺手修正，避免官方启动后被过滤清除
    if entry.get("path").and_then(|x| x.as_str()) != Some(cwd_norm.as_str()) {
        entry["path"] = serde_json::json!(cwd_norm);
    }
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
        provider_added: Vec::new(),
        keys_to_enter: Vec::new(),
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

    // v6 提供方携带：确保目标端有这些会话需要的模型提供方，否则发消息会报 NO_ADAPTER。
    // 只搬"提供方定义"（api/baseURL/models/apiKeyEnv 引用名），密钥仍由用户录入。
    let src_defs = crate::providers::collect_provider_defs(source_home);
    if !src_defs.is_empty() {
        match crate::providers::ensure_providers(target_home, &src_defs) {
            Ok(rep) => {
                result.provider_added = rep.added.clone();
                result.keys_to_enter = rep.keys_to_enter.clone();
                for n in rep.notes { result.notes.push(n); }
            }
            Err(e) => result.errors.push(format!("提供方配置补齐失败（不影响对话文件）：{e}")),
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

/// 合并两份 workspace.json（目标 + 来源）：工作区按 path 对齐，sessionIds 取并集；
/// 来源独有的工作区整体并入（id 冲突则换新 id）；archivedSessionIds 取并集。
/// 返回（新增工作区数, 新增会话登记数）。目标文件不存在时直接用来源覆盖。
pub(crate) fn merge_workspace_json(target_file: &Path, source_file: &Path) -> Result<(usize, usize), String> {
    if !source_file.is_file() {
        return Ok((0, 0));
    }
    if !target_file.is_file() {
        if let Some(parent) = target_file.parent() { let _ = fs::create_dir_all(parent); }
        fs::copy(source_file, target_file).map_err(|e| format!("复制 workspace.json 失败：{e}"))?;
        return Ok((0, 0));
    }
    let mut tgt: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(target_file).map_err(|e| format!("读目标 workspace.json 失败：{e}"))?,
    ).map_err(|e| format!("目标 workspace.json 不是有效 JSON：{e}"))?;
    let src: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(source_file).map_err(|e| format!("读来源 workspace.json 失败：{e}"))?,
    ).map_err(|e| format!("来源 workspace.json 不是有效 JSON：{e}"))?;

    if tgt.get("unit").is_none() { tgt["unit"] = serde_json::json!({"name":"workspace","version":2}); }
    if tgt.get("global").is_none() { tgt["global"] = serde_json::json!({"initialized":true,"workspaceIds":[],"archivedSessionIds":[]}); }
    if tgt.get("tables").is_none() { tgt["tables"] = serde_json::json!({"workspaces":{}}); }
    if tgt["tables"].get("workspaces").is_none() { tgt["tables"]["workspaces"] = serde_json::json!({}); }

    let src_workspaces = src.get("tables").and_then(|x| x.get("workspaces")).and_then(|x| x.as_object()).cloned().unwrap_or_default();

    let mut added_ws = 0usize;
    let mut added_sessions = 0usize;

    for (_src_id, src_ws) in src_workspaces.iter() {
        let src_path = src_ws.get("path").and_then(|x| x.as_str()).unwrap_or("");
        if src_path.is_empty() { continue; }
        // 找目标端同 path 的工作区（v8：按官方 realpath 形态比较，兼容旧记录）
        let src_path_norm = realpath_like(src_path).unwrap_or_else(|| src_path.replace('/', "\\"));
        let mut found_key: Option<String> = None;
        if let Some(ws_map) = tgt["tables"]["workspaces"].as_object() {
            for (k, v) in ws_map.iter() {
                let Some(entry_path) = v.get("path").and_then(|x| x.as_str()) else { continue };
                let matched = entry_path == src_path_norm
                    || realpath_like(entry_path).map(|p| p == src_path_norm).unwrap_or(false);
                if matched { found_key = Some(k.clone()); break; }
            }
        }
        match found_key {
            Some(k) => {
                // 已有工作区：sessionIds 取并集（目标原有的全部保留）
                let entry = &mut tgt["tables"]["workspaces"][&k];
                if entry.get("sessionIds").is_none() { entry["sessionIds"] = serde_json::json!([]); }
                if entry.get("path").and_then(|x| x.as_str()) != Some(src_path_norm.as_str()) {
                    entry["path"] = serde_json::json!(src_path_norm);
                }
                let src_ids: Vec<String> = src_ws.get("sessionIds").and_then(|x| x.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
                if let Some(arr) = entry["sessionIds"].as_array_mut() {
                    for id in src_ids {
                        if !arr.iter().any(|x| x.as_str() == Some(id.as_str())) { arr.push(serde_json::json!(id)); added_sessions += 1; }
                    }
                }
            }
            None => {
                // 目标端没有这个工作区：并入（沿用来源 id；若与已有 key 冲突则换新 id）
                let mut new_id = src_ws.get("id").and_then(|x| x.as_str()).map(String::from)
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                if tgt["tables"]["workspaces"].get(&new_id).is_some() { new_id = uuid::Uuid::new_v4().to_string(); }
                let count = src_ws.get("sessionIds").and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0);
                let mut entry = src_ws.clone();
                if let Some(obj) = entry.as_object_mut() {
                    obj.remove("id");
                    obj.insert("path".to_string(), serde_json::json!(src_path_norm));
                }
                tgt["tables"]["workspaces"][&new_id] = entry;
                if let Some(ids) = tgt["global"]["workspaceIds"].as_array_mut() {
                    if !ids.iter().any(|x| x.as_str() == Some(new_id.as_str())) { ids.push(serde_json::json!(new_id)); }
                }
                added_ws += 1;
                added_sessions += count;
            }
        }
    }
    // archived 名单取并集
    let src_archived: Vec<String> = src.pointer("/global/archivedSessionIds").and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
    if tgt["global"].get("archivedSessionIds").is_none() { tgt["global"]["archivedSessionIds"] = serde_json::json!([]); }
    if let Some(arr) = tgt["global"]["archivedSessionIds"].as_array_mut() {
        for id in src_archived { if !arr.iter().any(|x| x.as_str() == Some(id.as_str())) { arr.push(serde_json::json!(id)); } }
    }

    // 备份 + 原子写
    let bak = target_file.with_extension("vault-bak");
    let _ = fs::copy(target_file, &bak);
    let text = serde_json::to_string_pretty(&tgt).map_err(|e| format!("序列化失败：{e}"))?;
    let tmp = target_file.with_extension("vault-tmp");
    fs::write(&tmp, text).map_err(|e| format!("写 workspace.json 失败：{e}"))?;
    fs::rename(&tmp, target_file).map_err(|e| format!("改名失败：{e}"))?;
    Ok((added_ws, added_sessions))
}

#[derive(Debug, Clone, serde::Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RegistryRepairReport {
    pub registered: usize,
    pub already: usize,
    pub cache_written: usize,
    /// v8：header 里没有 cwd、无法判断归属的会话数
    pub no_cwd: usize,
    /// v8：cwd 目录已不存在/无法解析的会话数（官方版不会显示它们）
    pub missing_dir: usize,
    /// v8：已登记但在归档名单里、官方默认隐藏的会话数
    pub archived: usize,
    /// v8：无法登记的原因示例（最多 8 条）
    pub issues: Vec<String>,
    pub notes: Vec<String>,
}

/// 对话登记修复：把磁盘上存在、但没登记进 workspace.json 的会话补登记
/// （并补建 projcache），让它们在侧栏可见。修复型操作，只增不删。
pub fn repair_session_registry(target_home: &Path) -> Result<RegistryRepairReport, String> {
    let all = list_sessions(target_home);
    let ws_file = target_home.join("storages").join("workspace.json");
    let mut registered: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Ok(text) = fs::read_to_string(&ws_file) {
        if let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(map) = doc.pointer("/tables/workspaces").and_then(|x| x.as_object()) {
                for (_k, w) in map {
                    if let Some(ids) = w.get("sessionIds").and_then(|x| x.as_array()) {
                        for i in ids { if let Some(s) = i.as_str() { registered.insert(s.to_string()); } }
                    }
                }
            }
        }
    }
    let mut rep = RegistryRepairReport::default();
    let mut by_cwd: std::collections::BTreeMap<String, Vec<(String, u64, u32)>> = std::collections::BTreeMap::new();
    for e in &all {
        if registered.contains(&e.id) { rep.already += 1; continue; }
        let Some(cwd_raw) = e.cwd.clone() else { rep.no_cwd += 1; continue };
        let Some(cwd) = realpath_like(&cwd_raw) else {
            rep.missing_dir += 1;
            if rep.issues.len() < 8 {
                rep.issues.push(format!("{}（原目录 {} 不存在）", e.id, cwd_raw));
            }
            continue;
        };
        by_cwd.entry(cwd).or_default().push((e.id.clone(), e.created_at_ms, e.generation));
        rep.registered += 1;
    }
    for (cwd, list) in &by_cwd {
        let ids: Vec<String> = list.iter().map(|x| x.0.clone()).collect();
        append_to_workspace_index(target_home, cwd, &ids).map_err(|e| format!("登记失败（{cwd}）：{e}"))?;
        for (id, created, gen) in list {
            if write_minimal_projection_cache(target_home, id, *created, cwd, *gen).is_ok() {
                rep.cache_written += 1;
            }
        }
    }
    if rep.registered > 0 {
        rep.notes.push(format!("已把 {} 条磁盘上有、但侧栏没登记的对话登记回去了（下次打开 DSH 即可看到）。", rep.registered));
    } else if rep.missing_dir == 0 && rep.no_cwd == 0 {
        rep.notes.push("所有对话都已正常登记，无需修复。".to_string());
    }
    if rep.no_cwd > 0 {
        rep.notes.push(format!("有 {} 条对话的 header 里没有记录工作目录，无法判断归属，未做登记。", rep.no_cwd));
    }
    // v8：统计处于归档名单里的会话——官方默认「隐藏已归档」，用户会误以为没迁过来。
    let mut archived_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Ok(text) = fs::read_to_string(&ws_file) {
        if let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(arr) = doc.pointer("/global/archivedSessionIds").and_then(|x| x.as_array()) {
                for v in arr {
                    if let Some(id) = v.as_str() {
                        archived_ids.insert(id.to_string());
                    }
                }
            }
        }
    }
    rep.archived = all.iter().filter(|e| archived_ids.contains(&e.id)).count();
    if rep.archived > 0 {
        rep.notes.push(format!(
            "另有 {} 条对话处于「已归档」状态，DSH 默认隐藏它们；在侧栏把筛选切到「全部对话（显示已归档）」即可看到。",
            rep.archived
        ));
    }
    if rep.missing_dir > 0 {
        rep.notes.push(format!(
            "另有 {} 条对话的原工作目录已不存在，官方版不会显示它们（登记了也会被过滤）。例如：{}。可以先重建原目录，或用「迁移」把这些对话迁到存在的工作区。",
            rep.missing_dir,
            rep.issues.join("；")
        ));
    }
    Ok(rep)
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotRestoreReport {
    pub restored: usize,
    pub skipped: usize,
    pub registered: usize,
    pub snapshot: String,
    /// v8：找回后仍因原目录不存在而无法显示的条数
    #[serde(default)]
    pub missing_dir: usize,
    /// v8：header 没有 cwd、无法判断归属的条数
    #[serde(default)]
    pub no_cwd: usize,
    /// v8：找回后仍处于归档名单、默认隐藏的条数
    #[serde(default)]
    pub archived: usize,
}

/// 从某次切换保险快照里把"目标端当前缺失的对话"找回来（只增不删），并自动登记。
pub fn restore_sessions_from_snapshot(
    repo: &Path,
    target_home: &Path,
    snapshot_name: &str,
) -> Result<SnapshotRestoreReport, String> {
    let running = crate::adopt::detect_dsh_processes();
    if !running.is_empty() {
        return Err(format!("检测到 DSH 正在运行（{}），请先完全关闭再找回对话。", running.join(", ")));
    }
    let tgt_id = crate::adopt::home_id_of(target_home);
    let snap_root = repo.join("switch-backups").join(&tgt_id).join(snapshot_name).join("sessions");
    if !snap_root.is_dir() {
        return Err(format!("该快照里没有 sessions 目录：{snapshot_name}"));
    }
    let current: std::collections::HashSet<String> =
        list_sessions(target_home).into_iter().map(|e| e.id).collect();
    let mut report = SnapshotRestoreReport {
        restored: 0,
        skipped: 0,
        registered: 0,
        snapshot: snapshot_name.to_string(),
        missing_dir: 0,
        no_cwd: 0,
        archived: 0,
    };

    for proj in fs::read_dir(&snap_root).into_iter().flatten().flatten() {
        let Ok(sessions) = fs::read_dir(proj.path()) else { continue };
        for s in sessions.flatten() {
            let dir = s.path();
            if !dir.is_dir() { continue; }
            let Some(file) = crate::routecheck::newest_session_file(&dir) else { continue };
            let entry = parse_session_entry(&dir, &file);
            if current.contains(&entry.id) { report.skipped += 1; continue; }
            let Some(cwd) = entry.cwd.clone() else { report.skipped += 1; continue };
            let dst = target_home.join("sessions").join(project_key(&cwd)).join(&entry.id);
            if dst.exists() { report.skipped += 1; continue; }
            crate::adopt::copy_dir_recursive(&dir, &dst).map_err(|e| format!("找回 {} 失败：{e}", entry.id))?;
            report.restored += 1;
        }
    }
    if report.restored > 0 {
        let repair = repair_session_registry(target_home)?;
        report.registered = repair.registered;
        report.missing_dir = repair.missing_dir;
        report.no_cwd = repair.no_cwd;
        report.archived = repair.archived;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realpath_like_matches_existing_dir_and_rejects_missing() {
        // v8：写进 workspace.json 的 path 必须与官方 realpathNormalize 的形态一致，
        // 否则官方 mutate 时会把登记当作不匹配清掉。
        let base = std::env::temp_dir().join(format!("dsh-vault-realpath-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let got = realpath_like(&base.to_string_lossy()).expect("存在的目录应可解析");
        assert!(!got.starts_with(r"\\?\"), "必须去掉 Windows verbatim 前缀");
        assert!(std::path::Path::new(&got).is_dir());
        assert!(realpath_like(&base.join("not-exist").to_string_lossy()).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn repair_reports_sessions_with_missing_cwd() {
        // v8：cwd 目录不存在的会话不登记（官方本来就不会显示），但必须在报告里说出来。
        let base = std::env::temp_dir().join(format!("dsh-vault-repairmiss-{}", uuid::Uuid::new_v4()));
        let home = base.join("home");
        make_session(&home, "E:\\gone", "session-gone-1", None, "standard");
        let rep = repair_session_registry(&home).unwrap();
        assert_eq!(rep.registered, 0, "目录不存在的会话不应登记");
        assert_eq!(rep.missing_dir, 1, "应报告 1 条缺目录会话");
        assert!(rep.notes.iter().any(|n| n.contains("不存在")), "报告里应有人话解释");
        let _ = std::fs::remove_dir_all(&base);
    }

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

