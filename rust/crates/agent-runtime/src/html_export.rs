//! Offline, self-contained HTML export. All branches and raw metadata are retained.
//! No JavaScript, terminal renderer, extension loader, or provider is needed.

use crate::session::Session;
use crate::session_tree::SessionEntry;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

const MAX_ARCHIVE_FILE: usize = 4 * 1024 * 1024;
const MAX_ARCHIVE_TOTAL: usize = 16 * 1024 * 1024;
const CSS: &str = r#"
:root{color-scheme:light dark;--bg:#f6f7fb;--card:#fff;--fg:#1a2535;--muted:#64748b;--border:#cbd5e1;--active:#2563eb}
@media(prefers-color-scheme:dark){:root{--bg:#121925;--card:#192435;--fg:#e3eaf5;--muted:#94a3b8;--border:#334155;--active:#7db3ff}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.6 system-ui,sans-serif}
header,main,footer{max-width:1100px;margin:auto;padding:24px}h1{font-size:26px;margin:0 0 8px}h2{font-size:19px}a{color:var(--active);overflow-wrap:anywhere}
nav{background:var(--card);padding:16px;border:1px solid var(--border);border-radius:8px}nav ol{padding-left:24px}nav li{margin:4px 0}
.entry{margin:20px 0;border:1px solid var(--border);background:var(--card);border-radius:8px;padding:18px;scroll-margin:16px}.entry.active{border-left:4px solid var(--active)}
.entry:target{outline:3px solid var(--active)}.entry h2{margin:0}.meta{font-size:12px;color:var(--muted);overflow-wrap:anywhere}.error{color:#d74747}
pre{font:13px/1.6 ui-monospace,Consolas,monospace;white-space:pre-wrap;overflow-wrap:anywhere;margin:12px 0}details{margin:12px 0}summary{cursor:pointer;font-weight:600}
img{max-width:100%;max-height:700px;object-fit:contain}figure{margin:16px 0}.tool{border-left:3px solid var(--muted);padding-left:14px}.diff{white-space:pre;overflow:auto}
.diff span{display:block}.added{color:#16803d}.removed{color:#c93c3c}.context{color:var(--muted)}footer{color:var(--muted);font-size:12px}
@media(max-width:600px){header,main,footer{padding:12px}.entry{padding:12px}}@media print{body{background:white;color:black}details> :not(summary){display:block}a{color:black}.entry{break-inside:avoid}}
"#;

pub fn escape_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

fn pre(output: &mut String, text: &str) {
    write!(output, "<pre>{}</pre>", escape_html(text)).expect("String write");
}

fn json_details(output: &mut String, label: &str, value: &Value) -> Result<(), String> {
    write!(output, "<details><summary>{}</summary>", escape_html(label)).expect("String write");
    pre(
        output,
        &serde_json::to_string_pretty(value).map_err(|error| error.to_string())?,
    );
    output.push_str("</details>");
    Ok(())
}

fn render_content(output: &mut String, content: &Value) -> Result<(), String> {
    if let Some(text) = content.as_str() {
        pre(output, text);
        return Ok(());
    }
    let Some(blocks) = content.as_array() else {
        return Ok(());
    };
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => pre(output, block["text"].as_str().unwrap_or_default()),
            Some("thinking") => {
                output.push_str("<details class=thinking><summary>推理 / Thinking</summary>");
                pre(output, block["thinking"].as_str().unwrap_or_default());
                output.push_str("</details>");
            }
            Some("image") => {
                let mime = block["mimeType"]
                    .as_str()
                    .ok_or("Image MIME type is missing")?;
                if !matches!(
                    mime,
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/bmp"
                ) {
                    return Err(format!("Unsupported export image MIME type: {mime}"));
                }
                let data = block["data"].as_str().ok_or("Image data is missing")?;
                let bytes = STANDARD
                    .decode(data)
                    .map_err(|error| format!("Invalid export image base64: {error}"))?;
                if STANDARD.encode(&bytes) != data {
                    return Err("Image base64 is not canonical".into());
                }
                write!(output,"<figure><img alt=\"会话附件\" src=\"data:{mime};base64,{data}\"><figcaption>{mime} · {} bytes</figcaption></figure>",bytes.len()).expect("String write");
            }
            Some("toolCall") => {
                write!(
                    output,
                    "<section class=tool><strong>工具调用：{}</strong><div class=meta>{}</div>",
                    escape_html(block["name"].as_str().unwrap_or("tool")),
                    escape_html(block["id"].as_str().unwrap_or_default())
                )
                .expect("String write");
                pre(
                    output,
                    &serde_json::to_string_pretty(&block["arguments"])
                        .map_err(|error| error.to_string())?,
                );
                output.push_str("</section>");
            }
            _ => json_details(output, "未知内容 / Unknown content", block)?,
        }
    }
    Ok(())
}

fn render_message(output: &mut String, message: &Value) -> Result<(), String> {
    render_content(output, &message["content"])?;
    if message["role"] == "bashExecution" {
        pre(output, message["command"].as_str().unwrap_or_default());
        pre(output, message["output"].as_str().unwrap_or_default());
    }
    if let Some(diff) = message["details"]["diff"].as_str() {
        output.push_str("<details open><summary>工具 Diff</summary><pre class=diff>");
        for line in diff.split('\n') {
            let class = if line.starts_with('+') {
                "added"
            } else if line.starts_with('-') {
                "removed"
            } else {
                "context"
            };
            writeln!(output, "<span class={class}>{}</span>", escape_html(line))
                .expect("String write");
        }
        output.push_str("</pre></details>");
    }
    if message["isError"] == true
        || matches!(message["stopReason"].as_str(), Some("error" | "aborted"))
    {
        write!(
            output,
            "<p class=error>{}: {}</p>",
            escape_html(message["stopReason"].as_str().unwrap_or("工具错误")),
            escape_html(message["errorMessage"].as_str().unwrap_or_default())
        )
        .expect("String write");
    }
    if let Some(details) = message.get("details").filter(|details| !details.is_null()) {
        json_details(output, "工具详情 / Tool details", details)?;
    }
    Ok(())
}

/// Validate portable archive records before creating any output file.
pub fn collect_archives(entries: &[SessionEntry]) -> Result<BTreeMap<String, String>, String> {
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    let mut total = 0usize;
    for entry in entries {
        if entry.entry_type != "custom" || entry.field("customType") != "export:context-archives" {
            continue;
        }
        let data = entry.field("data");
        if data["schema"] != "agent-session-archives/1" || !data["checksums"].is_object() {
            return Err("Invalid session archive schema".into());
        }
        let archive = data["files"]
            .as_object()
            .ok_or("Invalid session archive files")?;
        for (name, payload) in archive {
            let encoded = payload
                .as_str()
                .ok_or("Session archive payload must be base64 text")?;
            if name.contains(['\\', '\0', ':'])
                || name.starts_with('/')
                || name.split('/').any(|part| matches!(part, "" | "." | ".."))
                || encoded.len() > MAX_ARCHIVE_FILE.div_ceil(3) * 4
            {
                return Err(format!("Invalid session archive path or size: {name}"));
            }
            let bytes = STANDARD
                .decode(encoded)
                .map_err(|error| format!("Invalid archive base64: {error}"))?;
            let checksum = format!("{:x}", Sha256::digest(&bytes));
            if STANDARD.encode(&bytes) != encoded
                || bytes.len() > MAX_ARCHIVE_FILE
                || data["checksums"][name] != checksum
            {
                return Err(format!(
                    "Session archive checksum or encoding mismatch: {name}"
                ));
            }
            if let Some(previous) = files.get(name) {
                if previous != encoded {
                    return Err(format!("Conflicting session archive: {name}"));
                }
            } else {
                total = total.saturating_add(bytes.len());
                files.insert(name.clone(), encoded.into());
            }
            if total > MAX_ARCHIVE_TOTAL || files.len() > 4096 {
                return Err("Session archive exceeds export size limits".into());
            }
        }
    }
    Ok(files)
}

pub fn generate_session_html(session: &Session) -> Result<String, String> {
    let entries = session.get_entries()?;
    let branch: BTreeSet<_> = session
        .get_branch(None)?
        .iter()
        .map(|entry| entry.id.clone())
        .collect();
    let positions: BTreeMap<_, _> = entries
        .iter()
        .enumerate()
        .map(|(position, entry)| (entry.id.as_str(), position))
        .collect();
    let archives = collect_archives(&entries)?;
    let mut output = String::new();
    write!(output,"<!DOCTYPE html><html lang=zh-CN><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><meta http-equiv=Content-Security-Policy content=\"default-src 'none'; img-src data:; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'\"><title>{}</title><style>{CSS}</style></head><body><header><h1>{}</h1><p class=meta>会话 {} · 工作区 {} · {} 条记录 · 当前叶 {}</p></header><main>",escape_html(&session.name),escape_html(&session.name),escape_html(&session.id),escape_html(&session.workspace.display().to_string()),entries.len(),escape_html(session.get_leaf_id()?.as_deref().unwrap_or("root"))).expect("String write");
    json_details(
        &mut output,
        "系统提示词",
        &Value::String(session.context.system_prompt.clone()),
    )?;
    output.push_str("<nav aria-label=\"完整会话树\"><h2>完整会话树 / Timeline</h2><ol>");
    for (position, entry) in entries.iter().enumerate() {
        write!(
            output,
            "<li><a href=\"#entry-{position}\">{} · {}</a>{}</li>",
            escape_html(&entry.id),
            escape_html(&entry.entry_type),
            if branch.contains(&entry.id) {
                " · 当前分支"
            } else {
                ""
            }
        )
        .expect("String write");
    }
    output.push_str("</ol></nav>");
    let mut tokens = 0u64;
    for (position, entry) in entries.iter().enumerate() {
        let role = entry.field("message")["role"]
            .as_str()
            .unwrap_or(&entry.entry_type);
        write!(output,"<article id=\"entry-{position}\" class=\"entry{}\"><h2>{}</h2><p class=meta>ID {} · 时间 {}",if branch.contains(&entry.id){" active"}else{""},escape_html(role),escape_html(&entry.id),escape_html(&entry.timestamp.to_string())).expect("String write");
        if let Some(parent) = &entry.parent_id {
            write!(
                output,
                " · 父记录 <a href=\"#entry-{}\">{}</a>",
                positions[parent.as_str()],
                escape_html(parent)
            )
            .expect("String write");
        }
        output.push_str("</p>");
        if entry.entry_type == "message" {
            render_message(&mut output, entry.field("message"))?;
            tokens = tokens.saturating_add(
                entry.field("message")["usage"]["totalTokens"]
                    .as_u64()
                    .unwrap_or_default(),
            );
        } else if matches!(entry.entry_type.as_str(), "compaction" | "branch_summary") {
            pre(
                &mut output,
                entry.field("summary").as_str().unwrap_or_default(),
            );
        } else if entry.entry_type == "custom_message" {
            render_content(&mut output, entry.field("content"))?;
        }
        json_details(&mut output, "完整记录 / Raw record", &entry.to_value())?;
        output.push_str("</article>");
    }
    if !session.compactions.is_empty() {
        output.push_str("<section><h2>压缩归档 / Archived messages</h2>");
        for record in &session.compactions {
            write!(
                output,
                "<details><summary>{} · {} 条归档消息</summary>",
                escape_html(&record.id),
                record.archived_messages.len()
            )
            .expect("String write");
            for message in &record.archived_messages {
                render_message(
                    &mut output,
                    &serde_json::to_value(message).map_err(|error| error.to_string())?,
                )?;
            }
            json_details(
                &mut output,
                "完整压缩记录",
                &serde_json::to_value(record).map_err(|error| error.to_string())?,
            )?;
            output.push_str("</details>");
        }
        output.push_str("</section>");
    }
    if !archives.is_empty() {
        output.push_str("<section><h2>归档证据 / Evidence</h2><ul>");
        for (name, payload) in archives {
            write!(output,"<li><a download=\"{}\" href=\"data:application/octet-stream;base64,{payload}\">{}</a></li>",escape_html(&name),escape_html(&name)).expect("String write");
        }
        output.push_str("</ul></section>");
    }
    json_details(
        &mut output,
        "完整原生快照 / Native snapshot",
        &serde_json::to_value(session).map_err(|error| error.to_string())?,
    )?;
    write!(
        output,
        "</main><footer>全分支 · 自包含 · 无脚本 · Total tokens {tokens}</footer></body></html>"
    )
    .expect("String write");
    Ok(output)
}
