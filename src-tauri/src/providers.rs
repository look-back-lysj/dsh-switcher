//! 提供方配置携带：迁移/切换后，确保目标端具备源端会话需要的模型提供方。
//!
//! 病根（2026-10-09 在用户机器上端到端实测）：
//!   迁移过来的会话记住路由 `xiaomi-token-plan-cn/mimo-v2.5-pro`，
//!   但目标官方版 llm-pi-ai 配置里没有这个 provider id（只有 `xiaomi`），
//!   发消息报 `no adapter registered for provider "xiaomi-token-plan-cn"`（NO_ADAPTER）。
//!   而官方版内置目录（@earendil-works/pi-ai，40 个提供方）里有 xiaomi-token-plan-cn 的
//!   api/baseURL/models —— 只需要配置里出现该 provider id（含 apiKeyEnv 引用）即可注册适配器。
//!
//! 设计红线：
//!   - 只搬运"提供方定义"（api/baseURL/models/apiKeyEnv 引用名），绝不搬运凭据值；
//!   - 真实 API Key 由用户在目标版本界面录入一次（密钥环绑定，跨版本不可移植）；
//!   - 合并写入前先备份目标配置文件，原子写；不破坏已有提供方与其他配置。

use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDef {
    pub id: String,
    pub api_key_env: Option<String>,
    /// provider id 行及其子行的原始文本（保留源缩进）
    pub lines: Vec<String>,
    /// provider id 行的缩进（重缩进时用）
    pub base_indent: usize,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProviderMergeReport {
    pub added: Vec<String>,
    pub already_present: Vec<String>,
    /// 需要在目标版本界面补录的 Key（只列名字）
    pub keys_to_enter: Vec<String>,
    pub config_file: Option<String>,
    pub notes: Vec<String>,
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// 找 llm-pi-ai 段的行区间（半开）。兼容两种载体：
/// - settings.yaml：缩进 0 的 `llm-pi-ai:`
/// - cordis 补丁：缩进 0 的 `- id: llm-pi-ai` 数组项
fn llm_pi_ai_region(lines: &[&str]) -> Option<(usize, usize)> {
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim_end();
        let tr = t.trim_start();
        if tr.is_empty() || tr.starts_with('#') { continue; }
        if indent_of(t) != 0 { continue; }
        let is_settings_head = tr == "llm-pi-ai:" || tr.starts_with("llm-pi-ai:");
        let is_patch_head = tr == "- id: llm-pi-ai" || tr.starts_with("- id: llm-pi-ai ");
        if !is_settings_head && !is_patch_head { continue; }
        let mut end = lines.len();
        for (j, l2) in lines.iter().enumerate().skip(i + 1) {
            let t2 = l2.trim_end();
            let tr2 = t2.trim_start();
            if tr2.is_empty() || tr2.starts_with('#') { continue; }
            if indent_of(t2) == 0 {
                if is_patch_head && tr2.starts_with("- ") { end = j; break; }
                if !is_patch_head { end = j; break; }
            }
        }
        return Some((i, end));
    }
    None
}

/// 在区间内找 `providers:` 行。
fn providers_line(lines: &[&str], region: (usize, usize)) -> Option<usize> {
    for i in region.0..region.1 {
        let t = lines[i].trim_end();
        if t.trim_start().starts_with('#') { continue; }
        if t.trim() == "providers:" { return Some(i); }
    }
    None
}

/// 在区间内找 `config:` 行（补丁形态用）。
fn config_line(lines: &[&str], region: (usize, usize)) -> Option<usize> {
    for i in region.0..region.1 {
        let t = lines[i].trim_end();
        if t.trim_start().starts_with('#') { continue; }
        if t.trim() == "config:" { return Some(i); }
    }
    None
}

fn extract_api_key_env(block: &[String]) -> Option<String> {
    for l in block {
        let t = l.trim();
        if let Some(rest) = t.strip_prefix("apiKeyEnv:") {
            let v = rest.trim().trim_matches('"').trim_matches('\'').trim();
            if !v.is_empty() { return Some(v.to_string()); }
        }
    }
    None
}

/// 从一段 YAML 文本提取 llm-pi-ai.providers 下的所有提供方定义。
pub fn extract_providers_from_text(text: &str, source: &str) -> BTreeMap<String, ProviderDef> {
    let mut out = BTreeMap::new();
    let lines: Vec<&str> = text.lines().collect();
    let Some(region) = llm_pi_ai_region(&lines) else { return out };
    let Some(p_idx) = providers_line(&lines, region) else { return out };
    let p_indent = indent_of(lines[p_idx]);
    let mut child_indent: Option<usize> = None;
    let mut cur: Option<(String, usize, Vec<String>)> = None;
    let mut flush = |cur: &mut Option<(String, usize, Vec<String>)>, out: &mut BTreeMap<String, ProviderDef>| {
        if let Some((id, base, v)) = cur.take() {
            let api_key_env = extract_api_key_env(&v);
            out.insert(id.clone(), ProviderDef { id, api_key_env, lines: v, base_indent: base, source: source.to_string() });
        }
    };
    for i in (p_idx + 1)..region.1 {
        let raw = lines[i].trim_end();
        let tr = raw.trim_start();
        let ind = indent_of(raw);
        if tr.is_empty() || tr.starts_with('#') {
            if let Some((_, _, v)) = cur.as_mut() { v.push(raw.to_string()); }
            continue;
        }
        if ind <= p_indent { break; }
        let ci = *child_indent.get_or_insert(ind);
        if ind == ci {
            if let Some(name) = tr.strip_suffix(':') {
                let ok = !name.is_empty()
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
                if ok {
                    flush(&mut cur, &mut out);
                    cur = Some((name.to_string(), ind, vec![raw.to_string()]));
                    continue;
                }
            }
        }
        if let Some((_, _, v)) = cur.as_mut() { v.push(raw.to_string()); }
    }
    flush(&mut cur, &mut out);
    out
}

/// 收集某环境配置里的全部 llm-pi-ai 提供方定义。
/// 来源优先：home/settings.yaml → profiles/*/cordis.patch.yml → profiles/*/cordis.yml。
pub fn collect_provider_defs(home: &Path) -> BTreeMap<String, ProviderDef> {
    let mut out: BTreeMap<String, ProviderDef> = BTreeMap::new();
    let mut files: Vec<PathBuf> = Vec::new();
    let settings = home.join("settings.yaml");
    if settings.is_file() { files.push(settings); }
    if let Ok(rd) = fs::read_dir(home.join("profiles")) {
        let mut patches: Vec<PathBuf> = Vec::new();
        let mut roots: Vec<PathBuf> = Vec::new();
        for e in rd.flatten() {
            let p = e.path();
            let patch = p.join("cordis.patch.yml");
            if patch.is_file() { patches.push(patch); }
            let root = p.join("cordis.yml");
            if root.is_file() { roots.push(root); }
        }
        patches.sort();
        roots.sort();
        files.extend(patches);
        files.extend(roots);
    }
    for f in files {
        if let Ok(text) = fs::read_to_string(&f) {
            let defs = extract_providers_from_text(&text, &f.to_string_lossy());
            for (k, v) in defs {
                out.entry(k).or_insert(v); // 先出现的优先
            }
        }
    }
    out
}

/// 目标环境的配置文件选择：live 风格（settings.yaml 存在）优先；否则用含 llm-pi-ai 的补丁文件。
fn target_config_file(home: &Path) -> Option<PathBuf> {
    let settings = home.join("settings.yaml");
    if settings.is_file() { return Some(settings); }
    if let Ok(rd) = fs::read_dir(home.join("profiles")) {
        let mut candidates: Vec<PathBuf> = Vec::new();
        for e in rd.flatten() {
            let p = e.path().join("cordis.patch.yml");
            if p.is_file() { candidates.push(p); }
        }
        candidates.sort();
        for p in &candidates {
            if let Ok(t) = fs::read_to_string(p) {
                if t.contains("llm-pi-ai") { return Some(p.clone()); }
            }
        }
        if let Some(p) = candidates.into_iter().next() { return Some(p); }
    }
    None
}

fn reindent(def: &ProviderDef, target_base: usize) -> Vec<String> {
    let mut out = Vec::new();
    for l in &def.lines {
        if l.trim().is_empty() { out.push(String::new()); continue; }
        let cur = indent_of(l);
        let adjusted = if target_base as isize >= def.base_indent as isize {
            cur + (target_base - def.base_indent)
        } else {
            cur.saturating_sub(def.base_indent - target_base)
        };
        out.push(format!("{}{}", " ".repeat(adjusted), l.trim_start()));
    }
    out
}

/// 把缺失的提供方定义合并进目标环境配置。已有提供方不动；写前备份、原子写。
pub fn ensure_providers(target_home: &Path, defs: &BTreeMap<String, ProviderDef>) -> Result<ProviderMergeReport, String> {
    let existing = collect_provider_defs(target_home);
    let mut rep = ProviderMergeReport::default();
    rep.already_present = defs.keys().filter(|k| existing.contains_key(*k)).cloned().collect();
    let missing: Vec<&ProviderDef> = defs.values().filter(|d| !existing.contains_key(&d.id)).collect();
    if missing.is_empty() { return Ok(rep); }

    let file = target_config_file(target_home);
    let is_settings_new = file.is_none();
    let (file, mut text) = match &file {
        Some(p) => (p.clone(), fs::read_to_string(p).map_err(|e| format!("读取目标配置失败：{e}"))?),
        None => (target_home.join("settings.yaml"), String::new()),
    };

    let source_lines: Vec<&str> = text.lines().collect();
    let region = llm_pi_ai_region(&source_lines);
    let mut insert_at: usize; // 行号（插到该行之前）
    let mut child_indent: usize;

    let mut lines_out: Vec<String> = text.lines().map(|s| s.to_string()).collect();

    if is_settings_new {
        // 从零创建 settings.yaml
        let mut block: Vec<String> = Vec::new();
        block.push("llm-pi-ai:".to_string());
        block.push("  providers:".to_string());
        for d in &missing {
            block.extend(reindent(d, 4));
        }
        lines_out = block;
        insert_at = lines_out.len();
        child_indent = 4;
    } else if let Some(region) = region {
        if let Some(p_idx) = providers_line(&source_lines, region) {
            let p_indent = indent_of(source_lines[p_idx]);
            // 已有子项缩进用第一个子项；否则 +2
            let mut ci = p_indent + 2;
            for i in (p_idx + 1)..region.1 {
                let raw = source_lines[i].trim_end();
                let tr = raw.trim_start();
                if tr.is_empty() || tr.starts_with('#') { continue; }
                ci = indent_of(raw);
                break;
            }
            insert_at = p_idx + 1;
            child_indent = ci;
            // 真正把缺失的提供方块插到 providers: 行之后
            let mut block: Vec<String> = Vec::new();
            for d in &missing {
                block.extend(reindent(d, child_indent));
            }
            let mut result: Vec<String> = lines_out[..insert_at].to_vec();
            result.extend(block);
            result.extend(lines_out[insert_at..].to_vec());
            lines_out = result;
            return finish_write(&file, target_home, lines_out, missing, &mut rep);
        } else {
            // 有 llm-pi-ai 段但没 providers：插到段尾前
            let pad = config_line(&source_lines, region)
                .map(|c| indent_of(source_lines[c]) + 2)
                .unwrap_or(2);
            insert_at = region.1;
            child_indent = pad + 2;
            let mut block: Vec<String> = Vec::new();
            block.push(format!("{}providers:", " ".repeat(pad)));
            for d in &missing {
                block.extend(reindent(d, child_indent));
            }
            let mut result: Vec<String> = lines_out[..insert_at].to_vec();
            result.extend(block);
            result.extend(lines_out[insert_at..].to_vec());
            lines_out = result;
            // 已插入，直接收尾
            return finish_write(&file, target_home, lines_out, missing, &mut rep);
        }
    } else {
        // 完全没有 llm-pi-ai 段
        let is_patch = file.file_name().map(|n| n.to_string_lossy().contains("cordis")).unwrap_or(false);
        if is_patch {
            let mut block: Vec<String> = Vec::new();
            block.push("- id: llm-pi-ai".to_string());
            block.push("  name: \"@deepseek-ai/dsh-llm-pi-ai\"".to_string());
            block.push("  config:".to_string());
            block.push("    providers:".to_string());
            child_indent = 6;
            for d in &missing {
                block.extend(reindent(d, child_indent));
            }
            insert_at = lines_out.len();
            lines_out.extend(block);
        } else {
            let mut block: Vec<String> = Vec::new();
            block.push("llm-pi-ai:".to_string());
            block.push("  providers:".to_string());
            child_indent = 4;
            for d in &missing {
                block.extend(reindent(d, child_indent));
            }
            insert_at = lines_out.len();
            lines_out.extend(block);
        }
    }
    let _ = (insert_at, child_indent);
    finish_write(&file, target_home, lines_out, missing, &mut rep)
}

fn finish_write(
    file: &Path,
    target_home: &Path,
    lines: Vec<String>,
    missing: Vec<&ProviderDef>,
    rep: &mut ProviderMergeReport,
) -> Result<ProviderMergeReport, String> {
    // 备份（若原文件存在）
    if file.is_file() {
        let bak = file.with_extension("vault-bak");
        let _ = fs::copy(file, &bak);
    }
    let mut body = lines.join("\n");
    if !body.ends_with('\n') { body.push('\n'); }
    let tmp = file.with_extension("vault-tmp");
    if let Some(parent) = file.parent() { let _ = fs::create_dir_all(parent); }
    fs::write(&tmp, &body).map_err(|e| format!("写临时文件失败：{e}"))?;
    fs::rename(&tmp, file).map_err(|e| format!("改名失败：{e}"))?;

    rep.config_file = Some(file.to_string_lossy().to_string());
    let cred_refs = crate::routecheck::collect_credential_refs(target_home);
    for d in missing {
        rep.added.push(d.id.clone());
        if let Some(env) = &d.api_key_env {
            if !cred_refs.iter().any(|r| r == env) && !rep.keys_to_enter.contains(env) {
                rep.keys_to_enter.push(env.clone());
            }
        }
    }
    if !rep.added.is_empty() {
        rep.notes.push(format!("已向目标配置补齐模型提供方：{}", rep.added.join("、")));
    }
    if !rep.keys_to_enter.is_empty() {
        rep.notes.push(format!(
            "这些提供方还需要在目标版本里录入 API Key（密钥不随文件迁移）：{}",
            rep.keys_to_enter.join("、")
        ));
    }
    Ok(rep.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dsh-prov-{}-{}", tag, uuid::Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn extracts_providers_from_settings() {
        let src = "llm-pi-ai:\n  providers:\n    xiaomi-token-plan-cn:\n      apiKeyEnv: XIAOMI_TOKEN_PLAN_CN_API_KEY\n    qiu:\n      apiKeyEnv: QIU_API_KEY\n      api: openai-completions\n      baseURL: https://vulcanapi.com/v1\nagent-default-model:\n  provider: x\n";
        let defs = extract_providers_from_text(src, "test");
        assert_eq!(defs.len(), 2);
        assert_eq!(defs["xiaomi-token-plan-cn"].api_key_env.as_deref(), Some("XIAOMI_TOKEN_PLAN_CN_API_KEY"));
        assert_eq!(defs["qiu"].base_indent, 4);
        assert!(defs["qiu"].lines.iter().any(|l| l.contains("baseURL")));
    }

    #[test]
    fn extracts_from_cordis_patch_entry() {
        let src = "- id: llm-pi-ai\n  name: \"@deepseek-ai/dsh-llm-pi-ai\"\n  config:\n    providers:\n      xiaomi:\n        apiKeyEnv: XIAOMI_API_KEY\n- id: next\n  name: x\n";
        let defs = extract_providers_from_text(src, "patch");
        assert_eq!(defs.len(), 1);
        assert!(defs.contains_key("xiaomi"));
        assert_eq!(defs["xiaomi"].base_indent, 6);
    }

    #[test]
    fn merges_missing_provider_into_settings() {
        let src_home = tmp_dir("src");
        let tgt_home = tmp_dir("tgt");
        fs::write(
            src_home.join("settings.yaml"),
            "llm-pi-ai:\n  providers:\n    xiaomi-token-plan-cn:\n      apiKeyEnv: XIAOMI_TOKEN_PLAN_CN_API_KEY\n",
        ).unwrap();
        fs::write(
            tgt_home.join("settings.yaml"),
            "llm-pi-ai:\n  providers:\n    qiu:\n      apiKeyEnv: QIU_API_KEY\n      api: openai-completions\n",
        ).unwrap();
        let defs = collect_provider_defs(&src_home);
        let rep = ensure_providers(&tgt_home, &defs).unwrap();
        assert_eq!(rep.added, vec!["xiaomi-token-plan-cn".to_string()]);
        assert!(rep.keys_to_enter.contains(&"XIAOMI_TOKEN_PLAN_CN_API_KEY".to_string()));
        // 目标重新解析应同时含 qiu 与 xiaomi-token-plan-cn
        let after = collect_provider_defs(&tgt_home);
        assert!(after.contains_key("qiu"), "已有提供方不能被破坏");
        assert!(after.contains_key("xiaomi-token-plan-cn"), "新提供方应补齐");
        // 备份存在
        assert!(tgt_home.join("settings.vault-bak").is_file() || fs::read_dir(&tgt_home).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains("vault-bak")));
        let _ = fs::remove_dir_all(&src_home);
        let _ = fs::remove_dir_all(&tgt_home);
    }

    #[test]
    fn merges_into_cordis_patch_of_official_style() {
        let src_home = tmp_dir("src2");
        let tgt_home = tmp_dir("tgt2");
        fs::write(
            src_home.join("settings.yaml"),
            "llm-pi-ai:\n  providers:\n    xiaomi-token-plan-cn:\n      apiKeyEnv: XIAOMI_TOKEN_PLAN_CN_API_KEY\n",
        ).unwrap();
        let prof = tgt_home.join("profiles").join("desktop");
        fs::create_dir_all(&prof).unwrap();
        fs::write(
            prof.join("cordis.patch.yml"),
            "- id: ui-theme\n  name: \"@deepseek-ai/dsh-client-ui-theme\"\n  config:\n    preference: light\n- id: llm-pi-ai\n  name: \"@deepseek-ai/dsh-llm-pi-ai\"\n  config:\n    providers:\n      xiaomi:\n        apiKeyEnv: XIAOMI_API_KEY\n- id: agent-default-model\n  name: x\n  config:\n    provider: deepseek-account\n",
        ).unwrap();
        let defs = collect_provider_defs(&src_home);
        let rep = ensure_providers(&tgt_home, &defs).unwrap();
        assert_eq!(rep.added, vec!["xiaomi-token-plan-cn".to_string()]);
        // 重新解析补丁：应含 xiaomi 和 xiaomi-token-plan-cn
        let after_text = fs::read_to_string(prof.join("cordis.patch.yml")).unwrap();
        let after = extract_providers_from_text(&after_text, "after");
        assert!(after.contains_key("xiaomi"));
        assert!(after.contains_key("xiaomi-token-plan-cn"), "补丁里应插入新提供方\n{}", after_text);
        // 后面的 agent-default-model 条目不能被破坏
        assert!(after_text.contains("agent-default-model"));
        let _ = fs::remove_dir_all(&src_home);
        let _ = fs::remove_dir_all(&tgt_home);
    }

    #[test]
    fn noop_when_all_present() {
        let home = tmp_dir("same");
        fs::write(
            home.join("settings.yaml"),
            "llm-pi-ai:\n  providers:\n    a:\n      apiKeyEnv: A_KEY\n",
        ).unwrap();
        let defs = collect_provider_defs(&home);
        let rep = ensure_providers(&home, &defs).unwrap();
        assert!(rep.added.is_empty());
        let _ = fs::remove_dir_all(&home);
    }
}
