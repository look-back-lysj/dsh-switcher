//! 本地 MCP server（标准 MCP over stdio）。
//!
//! 让别的 AI / 工作台通过标准 MCP 协议调用 DSH Vault 的扫描/备份/切换能力。
//! 协议：JSON-RPC 2.0 over stdio，实现 initialize / tools/list / tools/call。
//! 写操作（备份/切换）默认 dry-run 预览，需 confirm:true 才真执行，防 AI 误操作。

use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::PathBuf;

fn tool_defs() -> Value {
    json!([
        {
            "name": "scan_homes",
            "description": "扫描本机所有 DeepSeek Harness 环境（对话、技能、配置），返回环境列表及其会话统计。只读，无副作用。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "deep": {"type": "boolean", "description": "是否全盘深度扫描（较慢但更全），默认 false 快速扫描"}
                }
            }
        },
        {
            "name": "get_status",
            "description": "查看各环境的接管状态与备份仓库位置。只读，无副作用。",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "create_backup",
            "description": "创建一次完整备份（对话、技能、配置存成安全快照）。默认 dry-run 预览；需 confirm:true 才真正执行。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "note": {"type": "string", "description": "备份备注"},
                    "confirm": {"type": "boolean", "description": "必须显式传 true 才会真正执行备份"}
                }
            }
        },
        {
            "name": "switch_env",
            "description": "把一个环境的内容（对话/技能/配置/记忆）切换到另一个环境。两个环境都需已接管。默认 dry-run 预览；需 confirm:true 才执行。执行前会自动给目标环境存保险快照。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source_home": {"type": "string", "description": "来源环境路径"},
                    "target_home": {"type": "string", "description": "目标环境路径"},
                    "include_sessions": {"type": "boolean", "description": "含对话，默认 true"},
                    "include_skills": {"type": "boolean", "description": "含技能，默认 false"},
                    "include_config": {"type": "boolean", "description": "含配置，默认 false"},
                    "include_memories": {"type": "boolean", "description": "含记忆，默认 false"},
                    "confirm": {"type": "boolean", "description": "必须显式传 true 才会真正执行切换"}
                },
                "required": ["source_home", "target_home"]
            }
        }
    ])
}

fn result_ok(id: Value, text: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{"type": "text", "text": text}],
            "isError": false
        }
    })
}

fn result_err(id: Value, msg: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{"type": "text", "text": msg}],
            "isError": true
        }
    })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn handle_tools_call(id: Value, params: &Value) -> Value {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let repo = crate::scanner::default_repo_dir();

    match name {
        "scan_homes" => {
            let deep = args.get("deep").and_then(|v| v.as_bool()).unwrap_or(false);
            let homes = crate::multiscan::multi_scan(!deep);
            let summary: Vec<Value> = homes.iter().map(|h| json!({
                "label": h.label,
                "path": h.path,
                "sessions": h.sessions.total,
                "healthy_sessions": h.sessions.ok,
                "variant": h.variant,
            })).collect();
            result_ok(id, serde_json::to_string_pretty(&json!({
                "count": homes.len(),
                "homes": summary,
            })).unwrap_or_default())
        }
        "get_status" => {
            let homes = crate::scanner::discover_homes();
            let summary: Vec<Value> = homes.iter().map(|h| {
                let adopted = crate::adopt::is_adopted(&PathBuf::from(&h.path));
                json!({
                    "label": h.label,
                    "path": h.path,
                    "adopted": adopted,
                    "sessions": h.sessions.total,
                })
            }).collect();
            result_ok(id, serde_json::to_string_pretty(&json!({
                "repo": repo.to_string_lossy(),
                "homes": summary,
            })).unwrap_or_default())
        }
        "create_backup" => {
            let confirm = args.get("confirm").and_then(|v| v.as_bool()).unwrap_or(false);
            if !confirm {
                // dry-run：报告将要备份多少
                let homes = crate::scanner::discover_homes();
                let total_sessions: u32 = homes.iter().map(|h| h.sessions.total).sum();
                return result_ok(id, format!(
                    "【预览】将为 {} 个环境创建备份，共 {} 个会话。如确认执行，请用 confirm:true 再次调用。",
                    homes.len(), total_sessions
                ));
            }
            let note = args.get("note").and_then(|v| v.as_str()).unwrap_or("");
            match crate::repo::run_backup(&repo.to_string_lossy(), note, false, None) {
                Ok(r) => result_ok(id, format!(
                    "备份完成：{} 个环境、{} 个文件、{} 字节。",
                    r.homes, r.files, r.bytes
                )),
                Err(e) => result_err(id, format!("备份失败：{e}")),
            }
        }
        "switch_env" => {
            let source = args.get("source_home").and_then(|v| v.as_str()).unwrap_or("");
            let target = args.get("target_home").and_then(|v| v.as_str()).unwrap_or("");
            let confirm = args.get("confirm").and_then(|v| v.as_bool()).unwrap_or(false);
            if source.is_empty() || target.is_empty() {
                return result_err(id, "缺少 source_home 或 target_home".into());
            }
            if !confirm {
                return result_ok(id, format!(
                    "【预览】将把「{source}」的内容切换到「{target}」。目标原件会先存保险快照。如确认执行，请用 confirm:true 再次调用，并确保所有 DSH 窗口已关闭。"
                ));
            }
            let inc_sessions = args.get("include_sessions").and_then(|v| v.as_bool()).unwrap_or(true);
            let inc_skills = args.get("include_skills").and_then(|v| v.as_bool()).unwrap_or(false);
            let inc_config = args.get("include_config").and_then(|v| v.as_bool()).unwrap_or(false);
            let inc_mem = args.get("include_memories").and_then(|v| v.as_bool()).unwrap_or(false);
            // v4.1：实验性 preset 默认不跨版本带（官方版不认识会报错）
            let inc_presets = args.get("include_presets").and_then(|v| v.as_bool()).unwrap_or(false);
            match crate::adopt::switch_links(
                &repo,
                &PathBuf::from(source),
                &PathBuf::from(target),
                inc_sessions, inc_skills, inc_config, inc_mem, inc_presets,
                false, // 合并模式（MCP 默认安全：不删除目标端内容）
            ) {
                Ok(r) => result_ok(id, format!(
                    "切换完成：{} 类内容已切换。目标原件已存保险快照「{}」。",
                    r.switched_links, r.backup_snapshot
                )),
                Err(e) => result_err(id, format!("切换失败：{e}")),
            }
        }
        _ => rpc_error(id, -32601, "未知工具"),
    }
}

fn handle_request(req: &Value) -> Option<Value> {
    let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    match method {
        "initialize" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "dsh-vault", "version": crate::model::VAULT_VERSION}
            }
        })),
        "notifications/initialized" | "initialized" => None, // 通知无需响应
        "ping" => Some(json!({"jsonrpc": "2.0", "id": id, "result": {}})),
        "tools/list" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"tools": tool_defs()}
        })),
        "tools/call" => {
            let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
            Some(handle_tools_call(id, &params))
        }
        _ => {
            if id.is_null() {
                None // 未知通知
            } else {
                Some(rpc_error(id, -32601, "方法不存在"))
            }
        }
    }
}

/// 启动 MCP stdio 服务（阻塞，直到 stdin 关闭）。
pub fn run_mcp_server() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(resp) = handle_request(&req) {
            let _ = writeln!(out, "{}", serde_json::to_string(&resp).unwrap_or_default());
            let _ = out.flush();
        }
    }
}
