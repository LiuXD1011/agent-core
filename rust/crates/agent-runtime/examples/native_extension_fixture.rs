//! Offline native extension fixture; intentionally std-only so hosts can compile
//! it with rustc into a temporary executable without another Cargo invocation.
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::process::{self, Command};
use std::thread;
use std::time::Duration;

fn quoted(value: &str) -> String {
    let mut result = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => result.push_str("\\\\"),
            '"' => result.push_str("\\\""),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            value if value.is_control() => result.push_str(&format!("\\u{:04x}", value as u32)),
            value => result.push(value),
        }
    }
    result.push('"');
    result
}

fn field<'a>(source: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{}:", quoted(key));
    let start = source.find(&needle)? + needle.len();
    let start = start + source[start..].len() - source[start..].trim_start().len();
    let mut depth = 0;
    let mut string = false;
    let mut escape = false;
    for (offset, byte) in source.as_bytes()[start..].iter().copied().enumerate() {
        if string {
            if escape {
                escape = false;
            } else if byte == b'\\' {
                escape = true;
            } else if byte == b'"' {
                string = false;
                if depth == 0 {
                    return Some(&source[start..start + offset + 1]);
                }
            }
            continue;
        }
        match byte {
            b'"' => string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    return Some(&source[start..start + offset + 1]);
                }
            }
            b',' | b'}' | b']' if depth == 0 => return Some(source[start..start + offset].trim()),
            _ => {}
        }
    }
    Some(source[start..].trim())
}

fn result(id: &str, payload: &str, kind: &str, version: u8) {
    println!("{{\"protocol\":\"agent-core-extension\",\"version\":{version},\"id\":{id},\"type\":\"{kind}\",\"success\":true,\"result\":{payload}}}");
    io::stdout().flush().unwrap();
}

fn record(path: Option<&str>, line: &str) {
    if let Some(path) = path {
        writeln!(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap(),
            "{line}"
        )
        .unwrap();
    }
}

fn host_dialog(
    inputs: &mut impl Iterator<Item = io::Result<String>>,
    record_path: Option<&str>,
    id: &str,
    request_id: &str,
    method: &str,
    params: &str,
) -> String {
    println!("{{\"protocol\":\"agent-core-extension\",\"version\":1,\"id\":{id},\"type\":\"host_request\",\"requestId\":{},\"method\":{},\"params\":{params}}}", quoted(request_id), quoted(method));
    io::stdout().flush().unwrap();
    let response = inputs.next().expect("host_result expected").unwrap();
    record(record_path, &response);
    assert_eq!(field(&response, "id"), Some(id));
    assert_eq!(field(&response, "type"), Some("\"host_result\""));
    assert_eq!(
        field(&response, "requestId"),
        Some(quoted(request_id).as_str())
    );
    assert_eq!(field(&response, "success"), Some("true"));
    field(&response, "result").unwrap().to_owned()
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|arg| arg == name);
    let argument = |prefix: &str| args.iter().find_map(|arg| arg.strip_prefix(prefix));
    if let Some(path) = argument("--pid=") {
        fs::write(path, process::id().to_string()).unwrap();
    }
    if flag("--hold") || flag("--no-read") {
        if let Some(path) = argument("--pid=") {
            fs::write(path, process::id().to_string()).unwrap();
        }
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }
    if let Some(path) = argument("--argv=") {
        fs::write(
            path,
            args.iter()
                .map(|arg| quoted(arg))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
    }
    if let Some(path) = argument("--child=") {
        let child = Command::new(env::current_exe().unwrap())
            .args(["--hold", &format!("--pid={path}")])
            .spawn()
            .unwrap();
        fs::write(format!("{path}.parent"), child.id().to_string()).unwrap();
        // The child inherits the fixture's pipes and native process-tree job.
        drop(child);
    }
    let mut own_id = String::from("fixture");
    let stdin = io::stdin();
    let mut inputs = stdin.lock().lines();
    while let Some(input) = inputs.next() {
        let line = input.unwrap();
        record(argument("--record="), &line);
        let id = field(&line, "id").unwrap_or("0");
        let method = field(&line, "method").unwrap_or("\"missing\"");
        if flag("--hang")
            || (flag("--hang-execute") && method == "\"execute\"")
            || (flag("--hang-command") && method == "\"command\"")
            || (flag("--hang-input") && method == "\"input\"")
        {
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        }
        if flag("--crash") {
            process::exit(7);
        }
        if flag("--malformed") {
            println!("not-json");
            io::stdout().flush().unwrap();
            continue;
        }
        if flag("--invalid-utf8") {
            io::stdout().write_all(&[0xff, b'\n']).unwrap();
            io::stdout().flush().unwrap();
            continue;
        }
        if flag("--oversize") {
            println!("{}", "x".repeat(2 * 1024 * 1024));
            io::stdout().flush().unwrap();
            continue;
        }
        if flag("--stderr-flood") {
            io::stderr().write_all(&vec![b'e'; 1024 * 1024]).unwrap();
            io::stderr().flush().unwrap();
        }
        let version = if flag("--wrong-version") { 2 } else { 1 };
        let tool_name = argument("--tool=").unwrap_or("fixture_echo");
        let command_name = argument("--command=").unwrap_or("fixture_echo");
        let params = field(&line, "params").unwrap_or("null");
        if method == "\"ui\"" {
            let dialog = field(params, "dialog").unwrap_or("{\"method\":\"input\"}");
            let ui_method = field(dialog, "method")
                .unwrap_or("\"input\"")
                .trim_matches('"');
            let dialog_params = field(params, "options").unwrap_or("{\"title\":\"Fixture input\"}");
            if flag("--ui-bad-parent")
                || flag("--ui-overlap")
                || flag("--ui-result-early")
                || flag("--ui-crash")
            {
                let parent = if flag("--ui-bad-parent") { "0" } else { id };
                println!("{{\"protocol\":\"agent-core-extension\",\"version\":1,\"id\":{parent},\"type\":\"host_request\",\"requestId\":\"ui-1\",\"method\":{},\"params\":{dialog_params}}}",quoted(ui_method));
                io::stdout().flush().unwrap();
                if flag("--ui-overlap") {
                    println!("{{\"protocol\":\"agent-core-extension\",\"version\":1,\"id\":{id},\"type\":\"host_request\",\"requestId\":\"ui-2\",\"method\":{},\"params\":{dialog_params}}}",quoted(ui_method));
                    io::stdout().flush().unwrap();
                }
                if flag("--ui-result-early") {
                    result(id, "null", "result", version);
                }
                if flag("--ui-crash") {
                    process::exit(7);
                }
                loop {
                    thread::sleep(Duration::from_secs(60));
                }
            }
            let reply = host_dialog(
                &mut inputs,
                argument("--record="),
                id,
                "ui-1",
                ui_method,
                dialog_params,
            );
            if flag("--ui-duplicate") {
                let _ = host_dialog(
                    &mut inputs,
                    argument("--record="),
                    id,
                    "ui-1",
                    ui_method,
                    dialog_params,
                );
            }
            result(id, &reply, "result", version);
            continue;
        }
        if (flag("--ui-command") && method == "\"command\"")
            || (flag("--ui-tool") && method == "\"execute\"")
        {
            let mut replies = Vec::new();
            for (index, (method, params)) in [
                (
                    "select",
                    "{\"title\":\"Pick one\",\"options\":[\"red\",\"蓝🙂\"]}",
                ),
                (
                    "confirm",
                    "{\"title\":\"Confirm\",\"message\":\"Continue?\"}",
                ),
                (
                    "input",
                    "{\"title\":\"Enter text\",\"placeholder\":\"placeholder\"}",
                ),
                (
                    "editor",
                    "{\"title\":\"Edit text\",\"prefill\":\"original\\ntext\"}",
                ),
            ]
            .iter()
            .enumerate()
            {
                replies.push(host_dialog(
                    &mut inputs,
                    argument("--record="),
                    id,
                    &format!("dialog-{index}"),
                    method,
                    params,
                ));
            }
            let text = quoted(&format!("[{}]", replies.join(",")));
            let payload = if method == "\"command\"" {
                format!("{{\"text\":{text},\"messages\":[],\"sessionName\":null,\"state\":null,\"ui\":[]}}")
            } else {
                format!("{{\"content\":[{{\"type\":\"text\",\"text\":{text}}}],\"details\":null,\"usage\":null,\"addedToolNames\":[],\"terminate\":false}}")
            };
            if method == "\"execute\"" {
                result(id, &payload, "update", version);
            }
            result(id, &payload, "result", version);
            continue;
        }
        let event_type = field(field(params, "event").unwrap_or("{}"), "type");
        if flag("--ui-startup")
            && method == "\"lifecycle\""
            && event_type == Some("\"session_start\"")
        {
            let _ = host_dialog(
                &mut inputs,
                argument("--record="),
                id,
                "startup-dialog",
                "input",
                "{\"title\":\"Extension startup input\",\"placeholder\":\"startup reply\"}",
            );
            result(id, "null", "result", version);
            continue;
        }
        let before_switch =
            method == "\"session_before\"" && event_type == Some("\"session_before_switch\"");
        let before_fork =
            method == "\"session_before\"" && event_type == Some("\"session_before_fork\"");
        if (before_switch && flag("--hang-before-switch"))
            || (before_fork && flag("--hang-before-fork"))
            || (method == "\"lifecycle\""
                && event_type == Some("\"session_info_changed\"")
                && flag("--hang-session-info"))
        {
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        }
        let model_select = method == "\"lifecycle\""
            && field(field(params, "event").unwrap_or("{}"), "type") == Some("\"model_select\"");
        let model_source = field(field(params, "event").unwrap_or("{}"), "source");
        let thinking_select =
            method == "\"lifecycle\"" && event_type == Some("\"thinking_level_select\"");
        if thinking_select && flag("--hang-thinking-select") {
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        }
        if thinking_select && flag("--thinking-select-error") {
            println!("{{\"protocol\":\"agent-core-extension\",\"version\":1,\"id\":{id},\"type\":\"result\",\"success\":false,\"error\":\"thinking_level_select fixture error\"}}");
            io::stdout().flush().unwrap();
            continue;
        }
        if model_select
            && ((flag("--hang-model-select") && model_source == Some("\"set\""))
                || (flag("--hang-model-restore") && model_source == Some("\"restore\"")))
        {
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        }
        if model_select && flag("--model-select-error") {
            println!("{{\"protocol\":\"agent-core-extension\",\"version\":1,\"id\":{id},\"type\":\"result\",\"success\":false,\"error\":\"model_select fixture error\"}}");
            io::stdout().flush().unwrap();
            continue;
        }
        if flag("--hang-lifecycle") && method == "\"lifecycle\"" {
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        }
        if method == "\"command\"" {
            if let Some(delay) =
                argument("--slow-command-ms=").and_then(|value| value.parse::<u64>().ok())
            {
                thread::sleep(Duration::from_millis(delay));
            }
        }
        if method == "\"input\"" {
            if let Some(delay) =
                argument("--slow-input-ms=").and_then(|value| value.parse::<u64>().ok())
            {
                thread::sleep(Duration::from_millis(delay));
            }
        }
        match method {
            "\"initialize\"" => {
                let extension_id = field(params, "extensionId").unwrap_or("\"fixture\"");
                own_id = extension_id.trim_matches('"').to_owned();
                let session_hook=if args.iter().any(|arg|arg.contains("before-switch")||arg.contains("before-fork")){",\"session_before\""}else{""};
                result(id, &format!("{{\"id\":{extension_id},\"tools\":[{{\"name\":{},\"description\":\"Offline native fixture\",\"parameters\":{{\"type\":\"object\"}},\"executionMode\":\"sequential\"}}],\"commands\":[{{\"name\":{},\"description\":\"Offline command\"}}],\"hooks\":[\"input\",\"before_agent_start\",\"context\",\"before_tool_call\",\"after_tool_call\",\"lifecycle\"{session_hook}]}}", quoted(tool_name),quoted(command_name)), "result", version);
                if flag("--exit-after-init") { return; }
            }
            "\"execute\"" => {
                let details = if flag("--echo-context") { field(params, "context").unwrap_or("null") } else { "{\"native\":true}" };
                let text = field(field(params, "arguments").unwrap_or("{}"), "text").unwrap_or("\"native fixture\"");
                let payload = format!("{{\"content\":[{{\"type\":\"text\",\"text\":{text}}}],\"details\":{details},\"usage\":null,\"addedToolNames\":[],\"terminate\":false}}");
                result(id, &payload, "update", version);
                result(id, if flag("--bad-tool-result") { "{}" } else { &payload }, "result", version);
            }
            "\"command\"" => {
                let context = field(params,"context").unwrap_or("{}");
                let states = field(context,"states").unwrap_or("{}");
                let previous = field(states,&own_id).unwrap_or("{}");
                let count = field(previous,"count").and_then(|value|value.parse::<u64>().ok()).unwrap_or(0).saturating_add(1);
                let name=quoted(argument("--session-name=").unwrap_or("Native Fixture"));
                result(id, &format!("{{\"text\":\"native command\",\"messages\":[{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"native command message\"}}],\"timestamp\":1}}],\"sessionName\":{name},\"state\":{{\"count\":{count}}},\"ui\":[{{\"type\":\"notification\",\"level\":\"info\",\"text\":\"native notice\"}}]}}"), "result", version);
            }
            "\"session_before\"" if (before_switch&&flag("--before-switch-error"))||(before_fork&&flag("--before-fork-error"))=>{
                println!("{{\"protocol\":\"agent-core-extension\",\"version\":1,\"id\":{id},\"type\":\"result\",\"success\":false,\"error\":\"before session fixture error\"}}");io::stdout().flush().unwrap();
            }
            "\"session_before\""=>result(id,if (before_switch&&flag("--before-switch-cancel"))||(before_fork&&flag("--before-fork-cancel")){"{\"cancel\":true}"}else{"{}"},"result",version),
            "\"input\"" if flag("--input-handled") => result(id,"{\"action\":\"handled\"}","result",version),
            "\"input\"" if flag("--input-error") => {
                println!("{{\"protocol\":\"agent-core-extension\",\"version\":1,\"id\":{id},\"type\":\"result\",\"success\":false,\"error\":\"input fixture error\"}}");io::stdout().flush().unwrap();
            }
            "\"input\"" if flag("--input-malformed") => result(id,"{\"action\":\"unknown\"}","result",version),
            "\"input\"" if argument("--input-prefix=").is_some() || flag("--input-clear-images") || flag("--input-replace-images") || flag("--input-invalid-images") => {
                let event=field(params,"event").unwrap_or("{}");
                let original=field(event,"text").unwrap_or("\"\"");let prefix=quoted(argument("--input-prefix=").unwrap_or(""));
                let text=format!("{}{}",prefix.strip_suffix('"').unwrap(),original.strip_prefix('"').unwrap());
                let images=if flag("--input-clear-images"){",\"images\":[]"}else if flag("--input-replace-images"){",\"images\":[{\"type\":\"image\",\"data\":\"native image\",\"mimeType\":\"image/jpeg\"}]"}else if flag("--input-invalid-images"){",\"images\":[{\"type\":\"text\",\"text\":\"not an image\"}]"}else{""};
                result(id,&format!("{{\"action\":\"transform\",\"text\":{text}{images}}}"),"result",version);
            }
            "\"input\"" => result(id,"null","result",version),
            "\"before_agent_start\"" if flag("--inject") => result(id, "{\"messages\":[{\"role\":\"custom\",\"kind\":\"fixture\",\"data\":{\"content\":\"native injected\",\"display\":false,\"details\":null},\"timestamp\":1}],\"systemPrompt\":\"native system\"}", "result", version),
            "\"before_agent_start\"" => result(id, "{}", "result", version),
            "\"before_tool_call\"" if flag("--mutate") => result(id, "{\"args\":{\"text\":\"mutated by native\"}}", "result", version),
            "\"before_tool_call\"" => result(id, "{}", "result", version),
            "\"after_tool_call\"" if flag("--patch") => result(id, "{\"details\":null,\"isError\":true}", "result", version),
            "\"error\"" => { println!("{{\"protocol\":\"agent-core-extension\",\"version\":1,\"id\":{id},\"type\":\"result\",\"success\":false,\"error\":\"fixture error\"}}"); io::stdout().flush().unwrap(); }
            "\"echo\"" => result(id, params, "result", version),
            _ => result(id, "null", "result", version),
        }
    }
}
