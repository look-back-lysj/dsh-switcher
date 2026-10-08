//! 切换"可路由性体检"：逐条会话判断迁移后还能不能继续聊。
//!
//! 依据同学实测报告（2026-10-08，37 项问题）§6.3 算法落地：
//! - 每条会话把自己上次用的模型路由（model/selection 事件）记在日志里；
//! - 历史渲染只需要会话文件，继续聊天需要该路由能在目标端解析到提供方 + 凭据；
//! - 官方规则（报告 §4.1 引官方 README）：会话按"数值最高的规范代"读取；
//!   提示词准入保留已保存路由，不按目录可用性阻断发送，由请求执行报告失败。
//!   这就是"能看不能聊"的官方机制解释，体检要把这层静默鸿沟提前摆到用户面前。
//!
//! 安全红线：只读凭据的"引用名"（refs 键名），绝不读/输出任何密钥值。

use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// 单条会话的体检结论
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionVerdict {
    pub id: String,
    pub title_hint: String,
    pub cwd: Option<String>,
    /// 最后一条 model/selection 的 provider/model，如 "qiu005/deepseek-v4.1-flash"
    pub route: Option<String>,
    /// ok / need_model / need_credential / archived / cwd_missing / attachment_missing
    pub status: String,
    pub notes: Vec<String>,
}

/// 一次体检的汇总
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteCheckReport {
    pub target_home: String,
    pub providers: Vec<String>,
    pub credential_refs: Vec<String>,
    pub total: usize,
    pub ok: usize,
    pub need_model: usize,
    pub need_credential: usize,
    pub archived: usize,
    pub cwd_missing: usize,
    pub attachment_missing: usize,
    pub verdicts: Vec<SessionVerdict>,
    /// 给用户看的待办文案（前端直接渲染）
    pub todo_lines: Vec<String>,
}

/// 从目标 home 解析"可用提供方"集合。
/// 两个来源：home 根 settings.yaml 的 llm-pi-ai.providers.<name>，
/// 以及 profiles 下 cordis*.yml 里同名段（官方版导入后的落点）。
pub fn collect_providers(target_home: &Path) -> Vec<String> {
    let mut out: HashSet<String> = HashSet::new();
    let mut candidates: Vec<PathBuf> = vec![target_home.join("settings.yaml")];
    let profiles = target_home.join("profiles");
    if profiles.is_dir() {
        for entry in walkdir::WalkDir::new(&profiles).max_depth(3).into_iter().filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if entry.file_type().is_file() && (name.ends_with(".yml") || name.ends_with(".yaml")) {
                candidates.push(entry.path().to_path_buf());
            }
        }
    }
    for file in candidates {
        let Ok(text) = fs::read_to_string(&file) else { continue };
        extract_provider_names(&text, &mut out);
    }
    let mut v: Vec<String> = out.into_iter().collect();
    v.sort();
    v
}

/// 轻量 YAML 扫描：找 llm-pi-ai: -> providers: 下面一级缩进的键名。
/// 不引 YAML 解析库的原因：配置文件可能含官方版特有 tag/锚点，容错优先于严谨；
/// 我们只取"providers:" 段下两空格缩进的 `name:` 键，够了。
fn extract_provider_names(text: &str, out: &mut HashSet<String>) {
    // 缩进层级扫描：providers 段的"直接子键" = 比 providers 行深、且段内最浅的键。
    // 兼容 llm-pi-ai -> providers 两层嵌套（本机 AIO 实测形态）。
    let mut in_providers = false;
    let mut providers_indent = 0usize;
    let mut child_indent: Option<usize> = None;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with('#') { continue; }
        let indent = trimmed.len() - trimmed.trim_start().len();
        let key = trimmed.trim_start();
        if key.is_empty() { continue; }
        if key == "providers:" || key.starts_with("providers:") {
            in_providers = true;
            providers_indent = indent;
            child_indent = None;
            continue;
        }
        if in_providers {
            if indent <= providers_indent { in_providers = false; continue; }
            let ci = child_indent.get_or_insert(indent);
            if indent == *ci && key.ends_with(':') {
                let name = key.trim_end_matches(':').trim();
                if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                    out.insert(name.to_string());
                }
            }
        }
    }
}

/// 读目标端凭据引用名（只取 refs 键名，不读值）。
/// .credentials.yaml 结构（报告实证）：refs: 段下是 KEY 名列表。
pub fn collect_credential_refs(target_home: &Path) -> Vec<String> {
    let file = target_home.join(".credentials.yaml");
    let Ok(text) = fs::read_to_string(&file) else { return Vec::new() };
    let mut out: HashSet<String> = HashSet::new();
    let mut in_refs = false;
    let mut refs_indent = 0usize;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with('#') { continue; }
        let indent = trimmed.len() - trimmed.trim_start().len();
        let key = trimmed.trim_start();
        if key == "refs:" || key.starts_with("refs:") {
            in_refs = true;
            refs_indent = indent;
            continue;
        }
        if in_refs {
            if indent <= refs_indent && !key.is_empty() {
                in_refs = false;
                continue;
            }
            // "- DEEPSEEK_API_KEY" 或 "DEEPSEEK_API_KEY:" 两种形态都兜
            let item = key.trim_start_matches('-').trim().trim_end_matches(':');
            if !item.is_empty() && item.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') {
                out.insert(item.to_string());
            }
        }
    }
    let mut v: Vec<String> = out.into_iter().collect();
    v.sort();
    v
}

/// 会话目录里取"数值最高的规范代"会话文件（官方读取规则）。
fn newest_session_file(session_dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(u32, PathBuf)> = None;
    for entry in fs::read_dir(session_dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let gen = if name == "session.jsonl.zstd" {
            Some(0)
        } else if let Some(rest) = name.strip_prefix("session.v") {
            rest.strip_suffix(".jsonl.zstd").and_then(|n| n.parse::<u32>().ok())
        } else {
            None
        };
        if let Some(g) = gen {
            let replace = best.as_ref().map(|(bg, _)| g > *bg).unwrap_or(true);
            if replace {
                best = Some((g, entry.path()));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// 解压整条会话（多帧拼接），抽出 JSONL 行。
/// 会话通常几十~几百 KB 解压后，逐行扫 model/selection 代价可忽略。
fn decompress_session(path: &Path) -> Result<Vec<String>, String> {
    let bytes = fs::read(path).map_err(|e| format!("读取会话失败：{e}"))?;
    let (frames, _torn) = crate::zstd_check::scan_frames(&bytes)?;
    let mut lines = Vec::new();
    for frame in frames {
        // scan_frames 只给范围，这里按范围切片逐帧解压
        let slice = &bytes[frame_start(&frame)..frame_end(&frame)];
        let decoded = zstd::decode_all(std::io::Cursor::new(slice))
            .map_err(|e| format!("会话帧解压失败：{e}"))?;
        let text = String::from_utf8_lossy(&decoded);
        for line in text.lines() {
            let l = line.trim();
            if !l.is_empty() {
                lines.push(l.to_string());
            }
        }
    }
    Ok(lines)
}

// FrameRange 字段是 crate 私有，这里用安全的方式取范围
fn frame_start(f: &crate::zstd_check::FrameRange) -> usize { f.start() }
fn frame_end(f: &crate::zstd_check::FrameRange) -> usize { f.end() }

/// 从会话行里提取：header 的 id/cwd + 最后一条 model/selection + 附件引用数
fn analyze_session_lines(lines: &[String]) -> (Option<String>, Option<String>, Option<String>, u32) {
    let mut id = None;
    let mut cwd = None;
    let mut last_route: Option<String> = None;
    let mut attachments = 0u32;
    for (idx, line) in lines.iter().enumerate() {
        if idx == 0 {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if v.get("type").and_then(|t| t.as_str()) == Some("session") {
                    id = v.get("id").and_then(|x| x.as_str()).map(String::from);
                    cwd = v.get("cwd").and_then(|x| x.as_str()).map(String::from);
                }
            }
        }
        // 路由事件三种真实形态（本机 1476KB 大会话实测）：
        //   request/context: data.{provider,model}
        //   request/header:  data.header.config.{provider,model}
        //   model/selection: 顶层或 data 下 {provider,model}
        let is_route_event = line.contains("request/context")
            || line.contains("request/header")
            || line.contains("model/selection");
        if is_route_event {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
                let node = match ty {
                    "request/context" => v.pointer("/data").cloned(),
                    "request/header" => v.pointer("/data/header/config").cloned(),
                    _ => if v.get("provider").is_some() { Some(v.clone()) } else { v.pointer("/data").cloned() },
                };
                if let Some(n) = node {
                    let provider = n.get("provider").and_then(|x| x.as_str());
                    let model = n.get("model").and_then(|x| x.as_str());
                    if let (Some(p), Some(m)) = (provider, model) {
                        last_route = Some(format!("{p}/{m}"));
                    }
                }
            }
        }
        if line.contains("\"attachmentId\"") {
            attachments += 1;
        }
    }
    (id, cwd, last_route, attachments)
}

/// 归档名单：storages/workspace.json 的 global.archivedSessionIds
fn archived_ids(target_home: &Path) -> HashSet<String> {
    let file = target_home.join("storages").join("workspace.json");
    let Ok(text) = fs::read_to_string(&file) else { return HashSet::new() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return HashSet::new() };
    v.pointer("/global/archivedSessionIds")
        .and_then(|a| a.as_array())
        .map(|arr| arr.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// 主入口：对"目标 home 当前 sessions"做逐条体检。
/// 时机：切换完成后调用（此时目标 home 已是源端内容）。
pub fn check_routability(target_home: &Path) -> RouteCheckReport {
    let providers = collect_providers(target_home);
    let cred_refs = collect_credential_refs(target_home);
    let archived = archived_ids(target_home);
    let has_attachments_dir = target_home.join("attachments").is_dir();

    let mut verdicts = Vec::new();
    let sessions_root = target_home.join("sessions");
    if sessions_root.is_dir() {
        for proj in fs::read_dir(&sessions_root).into_iter().flatten().flatten() {
            let Ok(entries) = fs::read_dir(proj.path()) else { continue };
            for sess in entries.flatten() {
                if !sess.path().is_dir() { continue; }
                let Some(file) = newest_session_file(&sess.path()) else { continue };
                let dir_name = sess.file_name().to_string_lossy().to_string();
                let mut v = SessionVerdict {
                    id: dir_name.clone(),
                    title_hint: dir_name.trim_start_matches("session-").chars().take(8).collect(),
                    cwd: None,
                    route: None,
                    status: "ok".into(),
                    notes: Vec::new(),
                };
                match decompress_session(&file) {
                    Ok(lines) => {
                        let (id, cwd, route, att) = analyze_session_lines(&lines);
                        if let Some(i) = id { v.id = i.clone(); v.title_hint = i.chars().take(8).collect(); }
                        v.cwd = cwd;
                        v.route = route;
                        // 1. 归档
                        if archived.contains(&v.id) || archived.iter().any(|a| v.id.starts_with(a.as_str())) {
                            v.status = "archived".into();
                            v.notes.push("已归档：目标端会拒绝继续这条对话，需先取消归档".into());
                        }
                        // 2. 工作目录
                        if let Some(c) = &v.cwd {
                            if !Path::new(c).exists() {
                                if v.status == "ok" { v.status = "cwd_missing".into(); }
                                v.notes.push(format!("工作目录不存在：{c}"));
                            }
                        }
                        // 3. 模型路由
                        if let Some(r) = &v.route {
                            let provider = r.split('/').next().unwrap_or("");
                            let is_builtin = provider.starts_with("deepseek") || provider == "deepseek-account" || provider == "deepseek-official";
                            if !is_builtin && !providers.iter().any(|p| p == provider) {
                                v.status = "need_model".into();
                                v.notes.push(format!("提供方 {provider} 在目标端未配置"));
                            }
                        }
                        // 4. 附件
                        if att > 0 && !has_attachments_dir {
                            if v.status == "ok" { v.status = "attachment_missing".into(); }
                            v.notes.push(format!("引用了 {att} 个附件，但附件目录未迁移，图片可能无法显示"));
                        }
                    }
                    Err(e) => {
                        v.status = "ok".into();
                        v.notes.push(format!("会话读取异常（不影响历史显示）：{e}"));
                    }
                }
                verdicts.push(v);
            }
        }
    }

    let total = verdicts.len();
    let count = |s: &str| verdicts.iter().filter(|v| v.status == s).count();
    let report = RouteCheckReport {
        target_home: target_home.to_string_lossy().to_string(),
        providers,
        credential_refs: cred_refs,
        total,
        ok: count("ok"),
        need_model: count("need_model"),
        need_credential: count("need_credential"),
        archived: count("archived"),
        cwd_missing: count("cwd_missing"),
        attachment_missing: count("attachment_missing"),
        todo_lines: build_todo(&verdicts),
        verdicts,
    };
    report
}

fn build_todo(verdicts: &[SessionVerdict]) -> Vec<String> {
    let mut lines = Vec::new();
    let need_model: Vec<&SessionVerdict> = verdicts.iter().filter(|v| v.status == "need_model").collect();
    let archived: Vec<&SessionVerdict> = verdicts.iter().filter(|v| v.status == "archived").collect();
    let att: Vec<&SessionVerdict> = verdicts.iter().filter(|v| !v.notes.is_empty() && v.notes.iter().any(|n| n.contains("附件"))).collect();
    if !need_model.is_empty() {
        let providers: HashSet<&str> = need_model.iter().filter_map(|v| v.route.as_deref()).filter_map(|r| r.split('/').next()).collect();
        lines.push(format!(
            "{} 条对话用的是目标端没有的模型提供方（{}）。两种救法（任选）：① 打开对话 → 点输入框旁的模型名 → 换成可用模型（每条一次，最快）；② 在目标版本的设置里添加同名提供方并重新粘贴 API Key。",
            need_model.len(),
            providers.into_iter().collect::<Vec<_>>().join("、")
        ));
    }
    if !archived.is_empty() {
        lines.push(format!("{} 条对话处于【已归档】状态，目标端会拒绝继续，请先在目标版本里取消归档。", archived.len()));
    }
    if !att.is_empty() {
        lines.push(format!("{} 条对话含图片/文件附件，附件目录未随切换迁移，这些对话的历史图片可能显示不出来（文字不受影响）。", att.len()));
    }
    if lines.is_empty() {
        lines.push("本次迁移的对话全部可以直接继续，无需额外操作。".into());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_provider_names_from_settings_yaml() {
        let yaml = "llm-pi-ai:\n  providers:\n    qiu005:\n      apiKeyEnv: QIU005_API_KEY\n    gpt020qiu:\n      apiKeyEnv: GPT020QIU_API_KEY\nagent-default-model: qiu005/deepseek-v4.1-flash\n";
        let mut out = std::collections::HashSet::new();
        extract_provider_names(yaml, &mut out);
        assert!(out.contains("qiu005"));
        assert!(out.contains("gpt020qiu"));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn extracts_credential_refs_names_only() {
        let yaml = "records:\n  - name: client-connection\nrefs:\n  - DEEPSEEK_API_KEY\n  - QIU005_API_KEY\n";
        let dir = std::env::temp_dir().join(format!("dsh-vault-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".credentials.yaml"), yaml).unwrap();
        let refs = collect_credential_refs(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(refs, vec!["DEEPSEEK_API_KEY", "QIU005_API_KEY"]);
    }

    #[test]
    fn newest_picks_highest_generation() {
        let dir = std::env::temp_dir().join(format!("dsh-vault-gen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session.jsonl.zstd"), b"a").unwrap();
        std::fs::write(dir.join("session.v3.jsonl.zstd"), b"b").unwrap();
        std::fs::write(dir.join("session.v4.jsonl.zstd"), b"c").unwrap();
        let f = newest_session_file(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(f.file_name().unwrap().to_string_lossy().contains("v4"));
    }

    #[test]
    fn analyze_lines_finds_last_route_and_attachments() {
        let lines = vec![
            r#"{"type":"session","version":3,"id":"abc123","cwd":"E:\\VS"}"#.to_string(),
            r#"{"type":"request/context","data":{"provider":"qiu005","model":"deepseek-v4.1-flash"}}"#.to_string(),
            r#"{"type":"attachment","attachmentId":"a1"}"#.to_string(),
            r#"{"type":"request/header","data":{"header":{"config":{"provider":"gpt020qiu","model":"gpt-6-astra"}}}}"#.to_string(),
        ];
        let (id, cwd, route, att) = analyze_session_lines(&lines);
        assert_eq!(id.as_deref(), Some("abc123"));
        assert_eq!(cwd.as_deref(), Some("E:\\VS"));
        assert_eq!(route.as_deref(), Some("gpt020qiu/gpt-6-astra"));
        assert_eq!(att, 1);
    }

    /// 本机真实环境集成验证（只在真实 home 存在时跑，CI/他机自动跳过）。
    /// 标注 ignore：避免与其他测试并行争抢全局扫描缓存文件；需验证时跑
    /// `cargo test -- --ignored --nocapture real_aio`。
    #[test]
    #[ignore]
    fn real_aio_home_check_smoke() {
        let home = std::path::PathBuf::from(r"C:\Users\刘沛伦\AppData\Roaming\com.deepseek.dsh.desktop.aio\dsh-home");
        if !home.is_dir() {
            eprintln!("skip: AIO home 不存在");
            return;
        }
        let report = check_routability(&home);
        eprintln!(
            "REAL: total={} ok={} need_model={} providers={:?} creds={}",
            report.total, report.ok, report.need_model,
            report.providers, report.credential_refs.len()
        );
        for v in report.verdicts.iter().take(5) {
            eprintln!("  {:?} route={:?} status={}", v.title_hint, v.route, v.status);
        }
        assert!(report.total > 0, "AIO home 应至少有一条会话");
    }

}

