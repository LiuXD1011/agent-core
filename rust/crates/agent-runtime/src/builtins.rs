use crate::process::run_command;
use crate::search::{find, grep};
use agent_core::tools::AgentTool;
use agent_core::types::{CancellationToken, Content, ExecutionMode, ToolResult};
use base64::{engine::general_purpose::STANDARD, Engine};
use image::{
    imageops::FilterType, metadata::Orientation, DynamicImage, ImageDecoder, ImageFormat,
    ImageReader,
};
use serde_json::{json, Value};
use similar::{ChangeTag, TextDiff};
use std::collections::HashMap;
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use unicode_normalization::UnicodeNormalization;

pub(crate) const MAX_OUTPUT_BYTES: usize = 51_200;
const MAX_READ_BYTES: u64 = 16_777_216;
const MAX_IMAGE_BASE64: usize = 4_718_592;
type MutationLocks = Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>;
static MUTATION_LOCKS: OnceLock<MutationLocks> = OnceLock::new();

pub struct BuiltinTool {
    name: &'static str,
    workspace: PathBuf,
}

/// Shared by CLI, RPC and desktop; effectful tools remain opt-in.
pub fn create_tools(
    workspace: &Path,
    allow_changes: bool,
    allow_commands: bool,
) -> Vec<Box<dyn AgentTool>> {
    let mut names = vec!["read", "ls", "grep", "find"];
    if allow_changes {
        names.extend(["write", "edit"]);
    }
    if allow_commands {
        names.push("run_command");
    }
    names
        .into_iter()
        .map(|name| {
            Box::new(BuiltinTool {
                name,
                workspace: workspace.to_owned(),
            }) as Box<dyn AgentTool>
        })
        .collect()
}

pub fn tool_definitions(tools: &[Box<dyn AgentTool>]) -> Vec<Value> {
    tools.iter().map(|tool| json!({"type":"function","function":{"name":tool.name(),"description":tool.description(),"parameters":tool.parameters()}})).collect()
}

pub(crate) fn string_argument<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{key} must be a string"))
}

pub(crate) fn check_cancelled(signal: &CancellationToken) -> Result<(), String> {
    if signal.is_cancelled() {
        Err("Operation aborted".into())
    } else {
        Ok(())
    }
}

pub(crate) fn positive_limit(args: &Value, key: &str, default: usize) -> Result<usize, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_f64()
            .filter(|number| {
                number.is_finite()
                    && *number >= 1.0
                    && number.fract() == 0.0
                    && *number < usize::MAX as f64
            })
            .map(|number| number as usize)
            .ok_or_else(|| format!("{key} must be a positive integer")),
    }
}

/// Retains the TS conventions for @file, tilde and Unicode spaces.
pub(crate) fn resolve_path(input: &str, workspace: &Path) -> PathBuf {
    let normalized: String = input
        .strip_prefix('@')
        .unwrap_or(input)
        .chars()
        .map(|character| {
            if matches!(
                character,
                '\u{00a0}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
            ) {
                ' '
            } else {
                character
            }
        })
        .collect();
    if normalized == "~" || normalized.starts_with("~/") || normalized.starts_with("~\\") {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return PathBuf::from(home).join(normalized.get(2..).unwrap_or(""));
        }
    }
    #[cfg(target_os = "windows")]
    let normalized = normalize_windows_shell_path(&normalized);
    let path = PathBuf::from(normalized);
    if path.is_absolute() {
        path
    } else {
        workspace.join(path)
    }
}

#[cfg(target_os = "windows")]
fn normalize_windows_shell_path(input: &str) -> String {
    let suffix = input
        .strip_prefix("/mnt/")
        .or_else(|| input.strip_prefix("/cygdrive/"))
        .or_else(|| input.strip_prefix('/'));
    if !input.starts_with("//") && !input.contains('\\') {
        if let Some(suffix) = suffix {
            let bytes = suffix.as_bytes();
            if bytes.first().is_some_and(u8::is_ascii_alphabetic)
                && (bytes.len() == 1 || bytes[1] == b'/')
            {
                return format!(
                    "{}:\\{}",
                    (bytes[0] as char).to_ascii_uppercase(),
                    suffix.get(2..).unwrap_or("").replace('/', "\\")
                );
            }
        }
    }
    input.to_owned()
}

fn resolve_read_path(input: &str, workspace: &Path) -> PathBuf {
    let path = resolve_path(input, workspace);
    if path.exists() {
        return path;
    }
    let raw = path.to_string_lossy();
    let nfd: String = raw.nfd().collect();
    for candidate in [
        raw.replace(" AM.", "\u{202f}AM.")
            .replace(" PM.", "\u{202f}PM."),
        nfd.clone(),
        raw.replace('\'', "\u{2019}"),
        nfd.replace('\'', "\u{2019}"),
    ] {
        let candidate = PathBuf::from(candidate);
        if candidate.exists() {
            return candidate;
        }
    }
    path
}

fn mutation_lock(path: &Path) -> Result<Arc<Mutex<()>>, String> {
    let key = fs::canonicalize(path).unwrap_or_else(|_| {
        let parent = path.parent().unwrap_or(Path::new("."));
        fs::canonicalize(parent)
            .unwrap_or_else(|_| parent.to_owned())
            .join(path.file_name().unwrap_or_default())
    });
    let mut locks = MUTATION_LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| "File mutation registry is poisoned")?;
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    Ok(lock)
}

pub(crate) struct Truncation {
    pub content: String,
    pub truncated: bool,
    pub output_lines: usize,
    pub first_line_exceeds: bool,
    details: Value,
}
impl Truncation {
    pub(crate) fn details(&self) -> Value {
        self.details.clone()
    }
}

pub(crate) fn truncate_head(text: &str, max_lines: usize) -> Truncation {
    let mut lines: Vec<&str> = if text.is_empty() {
        vec![]
    } else {
        text.split('\n').collect()
    };
    if text.ends_with('\n') {
        lines.pop();
    }
    let mut output = String::new();
    let mut output_lines = 0;
    let mut truncated_by = "lines";
    for line in lines.iter().take(max_lines) {
        let newline = usize::from(output_lines > 0);
        if output
            .len()
            .saturating_add(line.len())
            .saturating_add(newline)
            > MAX_OUTPUT_BYTES
        {
            truncated_by = "bytes";
            break;
        }
        if newline > 0 {
            output.push('\n');
        }
        output.push_str(line);
        output_lines += 1;
    }
    let truncated = output_lines < lines.len() || text.len() > MAX_OUTPUT_BYTES;
    let first_line_exceeds = lines
        .first()
        .is_some_and(|line| line.len() > MAX_OUTPUT_BYTES);
    if !truncated {
        output = text.to_owned();
    }
    let details = json!({"content":output,"truncated":truncated,"truncatedBy":if truncated { Some(truncated_by) } else { None },
        "totalLines":lines.len(),"totalBytes":text.len(),"outputLines":output_lines,"outputBytes":output.len(),
        "lastLinePartial":false,"firstLineExceedsLimit":first_line_exceeds,"maxLines":max_lines,"maxBytes":MAX_OUTPUT_BYTES});
    Truncation {
        content: output,
        truncated,
        output_lines,
        first_line_exceeds,
        details,
    }
}

fn prepare_edit_arguments(input: &Value) -> Result<Value, String> {
    let mut result = input.clone();
    let Some(args) = result.as_object_mut() else {
        return Ok(result);
    };
    if let Some(value) = args.get("edits").cloned() {
        let value = if let Some(text) = value.as_str() {
            serde_json::from_str(text).unwrap_or(value)
        } else {
            value
        };
        let value = if value.is_object()
            && value.get("oldText").is_some_and(Value::is_string)
            && value.get("newText").is_some_and(Value::is_string)
        {
            Value::Array(vec![value])
        } else {
            value
        };
        args.insert("edits".into(), value);
    }
    if let (Some(old), Some(new)) = (
        args.get("oldText").and_then(Value::as_str),
        args.get("newText").and_then(Value::as_str),
    ) {
        let edit = json!({"oldText":old,"newText":new});
        let mut edits = args
            .get("edits")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        edits.push(edit);
        args.insert("edits".into(), Value::Array(edits));
        args.remove("oldText");
        args.remove("newText");
    }
    Ok(result)
}

impl AgentTool for BuiltinTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        match self.name {
            "read" => "Read text or image files. Text is capped at 2000 complete lines or 50KB; continue using offset/limit. PNG/JPEG/GIF/WebP images are attached; BMP is converted to PNG.",
            "ls" => "List a directory including dotfiles, sorted case-insensitively with '/' after directories. Defaults to 500 entries; supports limit.",
            "find" => "Find files, directories and symlinks by glob. Includes hidden entries, respects ignore files, never follows directory symlinks. Defaults to 1000 results or 50KB.",
            "grep" => "Search file contents using regex or literal text. Respects ignore files, supports glob/ignoreCase/context/limit, and returns paths and line numbers. Defaults to 100 matches or 50KB; long lines are capped at 500 characters.",
            "write" => "Write UTF-8 text to a file, creating parent directories if needed.",
            "edit" => "Apply unique non-overlapping exact-text replacements against the original file using edits[]. CRLF and BOM are preserved; returns a numbered diff and unified patch.",
            "run_command" => "Run a PowerShell command on Windows (sh on Unix) in the workspace. Timeout is capped at 120 seconds.", _ => "",
        }
    }
    fn execution_mode(&self) -> Option<ExecutionMode> {
        Some(if matches!(self.name, "read" | "ls" | "grep" | "find") {
            ExecutionMode::Parallel
        } else {
            ExecutionMode::Sequential
        })
    }
    fn parameters(&self) -> Value {
        match self.name {
            "read" => {
                json!({"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1}},"required":["path"]})
            }
            "ls" => {
                json!({"type":"object","properties":{"path":{"type":"string"},"limit":{"type":"integer","minimum":1}}})
            }
            "find" => {
                json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"limit":{"type":"integer","minimum":1}},"required":["pattern"]})
            }
            "grep" => {
                json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"glob":{"type":"string"},"ignoreCase":{"type":"boolean"},"literal":{"type":"boolean"},"context":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1}},"required":["pattern"]})
            }
            "write" => {
                json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]})
            }
            "edit" => {
                json!({"type":"object","properties":{"path":{"type":"string"},"edits":{"type":"array","minItems":1,"items":{"type":"object","properties":{"oldText":{"type":"string"},"newText":{"type":"string"}},"required":["oldText","newText"]}}},"required":["path","edits"]})
            }
            "run_command" => {
                json!({"type":"object","properties":{"command":{"type":"string"},"timeout":{"type":"integer","minimum":1,"maximum":120}},"required":["command"]})
            }
            _ => json!({"type":"object"}),
        }
    }
    fn prepare_arguments(&self, args: &Value) -> Result<Value, String> {
        if self.name == "edit" {
            prepare_edit_arguments(args)
        } else {
            Ok(args.clone())
        }
    }
    fn execute(
        &self,
        _call_id: &str,
        args: &Value,
        signal: &CancellationToken,
        _on_update: &mut dyn FnMut(ToolResult),
    ) -> Result<ToolResult, String> {
        check_cancelled(signal)?;
        match self.name {
            "read" => read_file(args, &self.workspace, signal),
            "ls" => list_directory(args, &self.workspace, signal),
            "find" => find(args, &self.workspace, signal),
            "grep" => grep(args, &self.workspace, signal),
            "write" => {
                let path = resolve_path(string_argument(args, "path")?, &self.workspace);
                let lock = mutation_lock(&path)?;
                let _guard = lock.lock().map_err(|_| "File mutation lock is poisoned")?;
                check_cancelled(signal)?;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                }
                check_cancelled(signal)?;
                let content = string_argument(args, "content")?;
                fs::write(&path, content).map_err(|error| error.to_string())?;
                check_cancelled(signal)?;
                Ok(ToolResult::text(format!(
                    "Wrote {} bytes to {}",
                    content.len(),
                    path.display()
                )))
            }
            "edit" => edit_file(args, &self.workspace, signal),
            "run_command" => run_command(
                string_argument(args, "command")?,
                &self.workspace,
                Duration::from_secs(
                    args["timeout"].as_f64().unwrap_or(60.0).clamp(1.0, 120.0) as u64
                ),
                signal,
            ),
            _ => Err("Unknown builtin".into()),
        }
    }
}

fn read_file(
    args: &Value,
    workspace: &Path,
    signal: &CancellationToken,
) -> Result<ToolResult, String> {
    let path = resolve_read_path(string_argument(args, "path")?, workspace);
    if fs::metadata(&path)
        .map_err(|error| error.to_string())?
        .len()
        > MAX_READ_BYTES
    {
        return Err("File exceeds 16MB read limit".into());
    }
    let bytes = read_cancellable(&path, signal)?;
    if let Ok(format) = image::guess_format(&bytes) {
        if matches!(
            format,
            ImageFormat::Png
                | ImageFormat::Jpeg
                | ImageFormat::Gif
                | ImageFormat::WebP
                | ImageFormat::Bmp
        ) {
            return read_image(bytes, format, signal);
        }
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.split('\n').collect();
    let offset = positive_limit(args, "offset", 1)?;
    if offset > lines.len() {
        return Err(format!(
            "Offset {offset} is beyond end of file ({} lines total)",
            lines.len()
        ));
    }
    let limit = positive_limit(args, "limit", lines.len())?;
    let selected = lines[offset - 1..]
        .iter()
        .take(limit)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let truncated = truncate_head(&selected, 2000);
    let mut output = truncated.content.clone();
    let next = offset.saturating_add(truncated.output_lines);
    if truncated.first_line_exceeds {
        output = format!("[Line {offset} exceeds the 50KB output limit. Use run_command to inspect a bounded portion of this line.]");
    } else if truncated.truncated {
        output.push_str(&format!(
            "\n\n[Showing lines {offset}-{} of {}. Use offset={next} to continue.]",
            next.saturating_sub(1),
            lines.len()
        ));
    } else if args.get("limit").is_some()
        && offset.saturating_sub(1).saturating_add(limit) < lines.len()
    {
        output.push_str(&format!(
            "\n\n[{} more lines in file. Use offset={} to continue.]",
            lines.len() - (offset - 1 + limit),
            offset + limit
        ));
    }
    Ok(ToolResult {
        content: ToolResult::text(output).content,
        details: json!({"totalLines":lines.len(),"shownLines":truncated.output_lines,"truncated":truncated.truncated,
        "truncation":if truncated.truncated { Some(truncated.details()) } else { None }}),
        ..ToolResult::default()
    })
}

pub(crate) fn read_cancellable(path: &Path, signal: &CancellationToken) -> Result<Vec<u8>, String> {
    let mut file = fs::File::open(path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 65_536];
    loop {
        check_cancelled(signal)?;
        let count = match file.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.to_string()),
        };
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    check_cancelled(signal)?;
    Ok(bytes)
}

fn read_image(
    bytes: Vec<u8>,
    format: ImageFormat,
    signal: &CancellationToken,
) -> Result<ToolResult, String> {
    let mut decoder = ImageReader::with_format(Cursor::new(&bytes), format)
        .into_decoder()
        .map_err(|error| format!("Image decode failed: {error}"))?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut decoded = DynamicImage::from_decoder(decoder)
        .map_err(|error| format!("Image decode failed: {error}"))?;
    decoded.apply_orientation(orientation);
    check_cancelled(signal)?;
    let (original_width, original_height) = (decoded.width(), decoded.height());
    let mut output_format = format;
    let mut output = bytes;
    let mut transformed = format == ImageFormat::Bmp;
    if original_width > 2000 || original_height > 2000 {
        decoded = decoded.resize(2000, 2000, FilterType::Lanczos3);
        transformed = true;
    }
    if transformed || output.len().div_ceil(3) * 4 >= MAX_IMAGE_BASE64 {
        loop {
            check_cancelled(signal)?;
            let mut encoded = Cursor::new(Vec::new());
            decoded
                .write_to(&mut encoded, ImageFormat::Png)
                .map_err(|error| error.to_string())?;
            output = encoded.into_inner();
            output_format = ImageFormat::Png;
            if output.len().div_ceil(3) * 4 < MAX_IMAGE_BASE64 {
                break;
            }
            if decoded.width() == 1 && decoded.height() == 1 {
                return Err("Image exceeds inline image byte limit".into());
            }
            decoded = decoded.resize(
                (decoded.width() * 3 / 4).max(1),
                (decoded.height() * 3 / 4).max(1),
                FilterType::Lanczos3,
            );
            transformed = true;
        }
    }
    let mime = match output_format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Gif => "image/gif",
        ImageFormat::WebP => "image/webp",
        _ => "image/png",
    };
    let mut note = format!("Read image file [{mime}]");
    if format == ImageFormat::Bmp {
        note.push_str("\n[Image converted from image/bmp to image/png.]");
    }
    if transformed && (original_width != decoded.width() || original_height != decoded.height()) {
        note.push_str(&format!("\n[Image: original {original_width}x{original_height}, displayed at {}x{}. Multiply coordinates by {:.2} to map to original image.]", decoded.width(), decoded.height(), f64::from(original_width) / f64::from(decoded.width())));
    }
    check_cancelled(signal)?;
    Ok(ToolResult {
        content: vec![
            Content::text(note),
            Content::Image {
                data: STANDARD.encode(output),
                mime_type: mime.into(),
            },
        ],
        details: json!({"originalWidth":original_width,"originalHeight":original_height,"width":decoded.width(),"height":decoded.height()}),
        ..ToolResult::default()
    })
}

fn list_directory(
    args: &Value,
    workspace: &Path,
    signal: &CancellationToken,
) -> Result<ToolResult, String> {
    let path = resolve_path(
        args.get("path").and_then(Value::as_str).unwrap_or("."),
        workspace,
    );
    let metadata =
        fs::metadata(&path).map_err(|_| format!("Path not found: {}", path.display()))?;
    if !metadata.is_dir() {
        return Err(format!("Not a directory: {}", path.display()));
    }
    let limit = positive_limit(args, "limit", 500)?;
    let mut entries = fs::read_dir(&path)
        .map_err(|error| error.to_string())?
        .map(|entry| entry.map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| {
        (
            entry.file_name().to_string_lossy().to_lowercase(),
            entry.file_name(),
        )
    });
    let mut results = Vec::new();
    let mut reached = false;
    for entry in entries {
        check_cancelled(signal)?;
        if results.len() >= limit {
            reached = true;
            break;
        }
        let Ok(metadata) = fs::metadata(entry.path()) else {
            continue;
        };
        results.push(format!(
            "{}{}",
            entry.file_name().to_string_lossy(),
            if metadata.is_dir() { "/" } else { "" }
        ));
    }
    if results.is_empty() {
        return Ok(ToolResult::text("(empty directory)"));
    }
    let truncated = truncate_head(&results.join("\n"), usize::MAX);
    let mut output = truncated.content.clone();
    let mut notices = Vec::new();
    if reached {
        notices.push(format!(
            "{limit} entries limit reached. Use limit={} for more",
            limit.saturating_mul(2)
        ));
    }
    if truncated.truncated {
        notices.push("50.0KB limit reached".into());
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    Ok(ToolResult {
        content: ToolResult::text(output).content,
        details: json!({"entryLimitReached":if reached { Some(limit) } else { None },"truncation":if truncated.truncated { Some(truncated.details()) } else { None }}),
        ..ToolResult::default()
    })
}

fn normalize_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}
fn normalize_fuzzy(text: &str) -> String {
    let text: String = text.nfkc().collect();
    text.split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .map(|character| match character {
            '\u{2018}'..='\u{201b}' => '\'',
            '\u{201c}'..='\u{201f}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{00a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
            _ => character,
        })
        .collect()
}

struct Replacement {
    index: usize,
    length: usize,
    text: String,
    edit_index: usize,
}

fn edit_file(
    args: &Value,
    workspace: &Path,
    signal: &CancellationToken,
) -> Result<ToolResult, String> {
    // Direct callers retain the existing single-edit shape.
    let args = prepare_edit_arguments(args)?;
    let name = string_argument(&args, "path")?;
    let edits = args
        .get("edits")
        .and_then(Value::as_array)
        .filter(|edits| !edits.is_empty())
        .ok_or("edits must contain at least one replacement")?;
    let path = resolve_path(name, workspace);
    let lock = mutation_lock(&path)?;
    let _guard = lock.lock().map_err(|_| "File mutation lock is poisoned")?;
    check_cancelled(signal)?;
    let raw = fs::read_to_string(&path)
        .map_err(|error| format!("Could not edit file: {name}. {error}"))?;
    check_cancelled(signal)?;
    let (bom, original) = raw
        .strip_prefix('\u{feff}')
        .map_or(("", raw.as_str()), |text| ("\u{feff}", text));
    let crlf = original
        .find("\r\n")
        .is_some_and(|position| original.find('\n').is_some_and(|line| position < line));
    let normalized = normalize_lf(original);
    let edits = edits
        .iter()
        .map(|edit| {
            Ok((
                normalize_lf(string_argument(edit, "oldText")?),
                normalize_lf(string_argument(edit, "newText")?),
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let fuzzy = edits.iter().any(|(old, _)| {
        !normalized.contains(old) && normalize_fuzzy(&normalized).contains(&normalize_fuzzy(old))
    });
    let base = if fuzzy {
        normalize_fuzzy(&normalized)
    } else {
        normalized.clone()
    };
    let mut replacements = Vec::new();
    for (edit_index, (old, new)) in edits.iter().enumerate() {
        check_cancelled(signal)?;
        if old.is_empty() {
            return Err(format!(
                "edits[{edit_index}].oldText must not be empty in {name}."
            ));
        }
        let needle = if base.contains(old) {
            old.clone()
        } else {
            normalize_fuzzy(old)
        };
        if needle.is_empty() {
            return Err(format!(
                "edits[{edit_index}].oldText normalizes to empty text"
            ));
        }
        let Some(index) = base.find(&needle) else {
            return Err(format!("Could not find edits[{edit_index}] in {name}. The oldText must match exactly including all whitespace and newlines."));
        };
        let count = normalize_fuzzy(&base)
            .match_indices(&normalize_fuzzy(old))
            .count();
        if count > 1 {
            return Err(format!("Found {count} occurrences of edits[{edit_index}] in {name}. Each oldText must be unique. Please provide more context to make it unique."));
        }
        replacements.push(Replacement {
            index,
            length: needle.len(),
            text: new.clone(),
            edit_index,
        });
    }
    replacements.sort_by_key(|edit| edit.index);
    for pair in replacements.windows(2) {
        if pair[0].index + pair[0].length > pair[1].index {
            return Err(format!("edits[{}] and edits[{}] overlap in {name}. Merge them into one edit or target disjoint regions.", pair[0].edit_index, pair[1].edit_index));
        }
    }
    let new = if fuzzy {
        apply_fuzzy_preserving_lines(&normalized, &base, &replacements)?
    } else {
        apply_replacements(&base, &replacements, 0)
    };
    if new == normalized {
        return Err(format!(
            "No changes made to {name}. The replacements produced identical content."
        ));
    }
    let ending_restored = if crlf {
        new.replace('\n', "\r\n")
    } else {
        new.clone()
    };
    let final_content = format!("{bom}{ending_restored}");
    let (diff, patch, first_changed) = edit_diff(name, &normalized, &new);
    check_cancelled(signal)?;
    fs::write(&path, final_content).map_err(|error| error.to_string())?;
    check_cancelled(signal)?;
    Ok(ToolResult {
        content: ToolResult::text(format!(
            "Successfully replaced {} block(s) in {name}.",
            edits.len()
        ))
        .content,
        details: json!({"diff":diff,"patch":patch,"firstChangedLine":first_changed}),
        ..ToolResult::default()
    })
}

fn apply_replacements(base: &str, replacements: &[Replacement], offset: usize) -> String {
    let mut result = base.to_owned();
    for replacement in replacements.iter().rev() {
        let start = replacement.index - offset;
        result.replace_range(start..start + replacement.length, &replacement.text);
    }
    result
}

fn apply_fuzzy_preserving_lines(
    original: &str,
    base: &str,
    replacements: &[Replacement],
) -> Result<String, String> {
    let original_lines: Vec<&str> = original.split_inclusive('\n').collect();
    let base_lines: Vec<&str> = base.split_inclusive('\n').collect();
    if original_lines.len() != base_lines.len() {
        return Err("Cannot preserve unchanged lines: normalized line counts differ".into());
    }
    let mut spans = Vec::new();
    let mut offset = 0;
    for line in &base_lines {
        spans.push((offset, offset + line.len()));
        offset += line.len();
    }
    let mut groups: Vec<(usize, usize, Vec<Replacement>)> = Vec::new();
    for replacement in replacements {
        let start = spans
            .iter()
            .position(|(start, end)| replacement.index >= *start && replacement.index < *end)
            .ok_or("Replacement range is outside the base content")?;
        let end = spans
            .iter()
            .position(|(_, end)| *end >= replacement.index + replacement.length)
            .ok_or("Replacement range is outside the base content")?
            + 1;
        let copy = Replacement {
            index: replacement.index,
            length: replacement.length,
            text: replacement.text.clone(),
            edit_index: replacement.edit_index,
        };
        if let Some((_, group_end, edits)) = groups
            .last_mut()
            .filter(|(_, group_end, _)| start < *group_end)
        {
            *group_end = (*group_end).max(end);
            edits.push(copy);
        } else {
            groups.push((start, end, vec![copy]));
        }
    }
    let mut result = String::new();
    let mut next = 0;
    for (start, end, edits) in groups {
        result.push_str(&original_lines[next..start].join(""));
        result.push_str(&apply_replacements(
            &base[spans[start].0..spans[end - 1].1],
            &edits,
            spans[start].0,
        ));
        next = end;
    }
    result.push_str(&original_lines[next..].join(""));
    Ok(result)
}

fn edit_diff(path: &str, old: &str, new: &str) -> (String, String, Option<usize>) {
    let diff = TextDiff::from_lines(old, new);
    let patch = diff
        .unified_diff()
        .context_radius(4)
        .header(path, path)
        .to_string();
    let mut rendered = Vec::new();
    let mut first = None;
    let mut next_new = 1;
    for group in diff.grouped_ops(4) {
        if !rendered.is_empty() {
            rendered.push(" ...".to_owned());
        }
        for operation in group {
            for change in diff.iter_changes(&operation) {
                let tag = match change.tag() {
                    ChangeTag::Equal => ' ',
                    ChangeTag::Delete => '-',
                    ChangeTag::Insert => '+',
                };
                if tag != ' ' && first.is_none() {
                    first = Some(change.new_index().map_or(next_new, |index| index + 1));
                }
                if let Some(index) = change.new_index() {
                    next_new = index + 2;
                }
                let line = change
                    .old_index()
                    .or(change.new_index())
                    .map_or(1, |index| index + 1);
                rendered.push(format!(
                    "{tag}{line} {}",
                    change.value().trim_end_matches('\n')
                ));
            }
        }
    }
    (rendered.join("\n"), patch, first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::types::timestamp;
    #[test]
    fn builtin_read_write_edit_and_validation() {
        let dir = std::env::temp_dir().join(format!(
            "agent-rust-tools-{}-{}",
            std::process::id(),
            timestamp()
        ));
        fs::create_dir(&dir).unwrap();
        let tools = create_tools(&dir, true, false);
        let signal = CancellationToken::new();
        let write = tools.iter().find(|tool| tool.name() == "write").unwrap();
        let read = tools.iter().find(|tool| tool.name() == "read").unwrap();
        let edit = tools.iter().find(|tool| tool.name() == "edit").unwrap();
        let args = json!({"path":"sample.txt","content":"one\n中文\nthree\n"});
        write.validate_arguments(&args).unwrap();
        write.execute("write", &args, &signal, &mut |_| {}).unwrap();
        let result = read
            .execute(
                "read",
                &json!({"path":"sample.txt","offset":2,"limit":1}),
                &signal,
                &mut |_| {},
            )
            .unwrap();
        assert!(format!("{:?}", result.content).contains("中文"));
        edit.execute(
            "edit",
            &json!({"path":"sample.txt","oldText":"中文","newText":"changed"}),
            &signal,
            &mut |_| {},
        )
        .unwrap();
        assert!(fs::read_to_string(dir.join("sample.txt"))
            .unwrap()
            .contains("changed"));
        assert!(read.validate_arguments(&json!({"path":5})).is_err());
        assert!(edit
            .execute(
                "edit",
                &json!({"path":"sample.txt","oldText":"absent","newText":""}),
                &signal,
                &mut |_| {}
            )
            .is_err());
        signal.cancel();
        assert!(write.execute("write", &args, &signal, &mut |_| {}).is_err());
        fs::remove_file(dir.join("sample.txt")).unwrap();
        fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn effectful_tools_are_only_advertised_when_enabled() {
        let tools = create_tools(Path::new("."), false, false);
        assert_eq!(tools.len(), 4);
        assert!(tools
            .iter()
            .all(|tool| tool.execution_mode() == Some(ExecutionMode::Parallel)));
        let all = create_tools(Path::new("."), true, true);
        assert_eq!(all.len(), 7);
        assert_eq!(tool_definitions(&all).len(), 7);
    }
    #[test]
    fn shell_output_and_nonzero_exit() {
        let directory = std::env::current_dir().unwrap();
        #[cfg(target_os = "windows")]
        let command = "Write-Output 'offline-command'";
        #[cfg(not(target_os = "windows"))]
        let command = "printf offline-command";
        let result = run_command(
            command,
            &directory,
            Duration::from_secs(10),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(result.details["exitCode"], 0);
        assert!(result.details["stdout"]
            .as_str()
            .unwrap()
            .contains("offline-command"));
        assert!(run_command(
            "exit 3",
            &directory,
            Duration::from_secs(10),
            &CancellationToken::new()
        )
        .unwrap_err()
        .contains("exit 3"));
    }
}
