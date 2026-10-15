//! Native file search: ignore-aware traversal without external rg/fd processes.

use crate::builtins::{
    check_cancelled, positive_limit, read_cancellable, resolve_path, string_argument,
    truncate_head, MAX_OUTPUT_BYTES,
};
use agent_core::types::{CancellationToken, ToolResult};
use globset::{GlobBuilder, GlobMatcher};
use ignore::{Walk, WalkBuilder};
use regex::RegexBuilder;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

fn walk(path: &Path, is_find: bool, signal: &CancellationToken) -> Walk {
    let inside_repo = path.ancestors().any(|parent| parent.join(".git").exists());
    let cancelled = signal.clone();
    let mut builder = WalkBuilder::new(path);
    builder
        .hidden(false)
        .follow_links(false)
        .require_git(if is_find { inside_repo } else { true })
        .sort_by_file_name(|left, right| left.cmp(right))
        .filter_entry(move |entry| {
            !cancelled.is_cancelled() && (entry.depth() == 0 || entry.file_name() != ".git")
        });
    builder.build()
}

fn matcher(pattern: &str, smart_case: bool) -> Result<GlobMatcher, String> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .case_insensitive(smart_case && !pattern.chars().any(char::is_uppercase))
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|error| format!("Invalid glob: {error}"))
}

fn relative_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

pub fn find(
    args: &Value,
    workspace: &Path,
    signal: &CancellationToken,
) -> Result<ToolResult, String> {
    check_cancelled(signal)?;
    let path = resolve_path(
        args.get("path").and_then(Value::as_str).unwrap_or("."),
        workspace,
    );
    let metadata =
        fs::metadata(&path).map_err(|_| format!("Path not found: {}", path.display()))?;
    if !metadata.is_dir() {
        return Err(format!("Not a directory: {}", path.display()));
    }
    let pattern = string_argument(args, "pattern")?;
    let path_pattern = pattern.contains('/');
    let effective = if path_pattern && !pattern.starts_with('/') && !pattern.starts_with("**/") {
        format!("**/{pattern}")
    } else {
        pattern.to_owned()
    };
    let pattern_matcher = matcher(&effective, true)?;
    let limit = positive_limit(args, "limit", 1000)?;
    let mut paths = Vec::new();
    let mut bytes = 0usize;
    let mut bytes_reached = false;
    let mut limit_reached = false;
    for entry in walk(&path, true, signal) {
        check_cancelled(signal)?;
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.depth() == 0 && path.is_dir() {
            continue;
        }
        let relative = relative_path(entry.path(), &path);
        let candidate = if pattern.starts_with('/') {
            entry.path().to_string_lossy().replace('\\', "/")
        } else if path_pattern {
            relative.clone()
        } else {
            entry.file_name().to_string_lossy().into_owned()
        };
        if !pattern_matcher.is_match(&candidate) {
            continue;
        }
        let is_directory = entry.file_type().is_some_and(|kind| kind.is_dir());
        let item = format!("{relative}{}", if is_directory { "/" } else { "" });
        if bytes
            .saturating_add(item.len())
            .saturating_add(usize::from(!paths.is_empty()))
            > MAX_OUTPUT_BYTES
        {
            bytes_reached = true;
            break;
        }
        bytes += item.len() + usize::from(!paths.is_empty());
        paths.push(item);
        if paths.len() >= limit {
            limit_reached = true;
            break;
        }
    }
    check_cancelled(signal)?;
    if paths.is_empty() && !bytes_reached {
        return Ok(ToolResult::text("No files found matching pattern"));
    }
    let mut output = paths.join("\n");
    let mut notices = Vec::new();
    if limit_reached {
        notices.push(format!(
            "{limit} results limit reached. Use limit={} for more, or refine pattern",
            limit.saturating_mul(2)
        ));
    }
    if bytes_reached {
        notices.push("50.0KB limit reached".into());
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    Ok(ToolResult {
        content: ToolResult::text(output).content,
        details: json!({"resultLimitReached":if limit_reached { Some(limit) } else { None },"byteLimitReached":bytes_reached}),
        ..ToolResult::default()
    })
}

struct GrepOutput {
    lines: Vec<String>,
    bytes: usize,
    bytes_reached: bool,
    lines_truncated: bool,
}
impl GrepOutput {
    fn push(&mut self, path: &str, line: usize, text: &str, is_match: bool) {
        let text = text.replace('\r', "");
        let mut short: String = text.chars().take(500).collect();
        if short.len() < text.len() {
            short.push_str("... [truncated]");
            self.lines_truncated = true;
        }
        let output = if is_match {
            format!("{path}:{line}: {short}")
        } else {
            format!("{path}-{line}- {short}")
        };
        let additional = output.len() + usize::from(!self.lines.is_empty());
        if self.bytes.saturating_add(additional) > MAX_OUTPUT_BYTES {
            self.bytes_reached = true;
            return;
        }
        self.bytes += additional;
        self.lines.push(output);
    }
}

pub fn grep(
    args: &Value,
    workspace: &Path,
    signal: &CancellationToken,
) -> Result<ToolResult, String> {
    check_cancelled(signal)?;
    let pattern = string_argument(args, "pattern")?;
    let pattern = if args
        .get("literal")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        regex::escape(pattern)
    } else {
        pattern.to_owned()
    };
    let regex = RegexBuilder::new(&pattern)
        .case_insensitive(
            args.get("ignoreCase")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        )
        .build()
        .map_err(|error| format!("Invalid regex: {error}"))?;
    let root = resolve_path(
        args.get("path").and_then(Value::as_str).unwrap_or("."),
        workspace,
    );
    let metadata =
        fs::metadata(&root).map_err(|_| format!("Path not found: {}", root.display()))?;
    let directory = metadata.is_dir();
    let limit = positive_limit(args, "limit", 100)?;
    let context = match args.get("context") {
        None | Some(Value::Null) => 0,
        Some(value) => value
            .as_f64()
            .filter(|number| {
                number.is_finite()
                    && *number >= 0.0
                    && number.fract() == 0.0
                    && *number < usize::MAX as f64
            })
            .map(|number| number as usize)
            .ok_or("context must be a nonnegative integer")?,
    };
    let filter = args
        .get("glob")
        .and_then(Value::as_str)
        .map(|pattern| {
            let invert = pattern.starts_with('!');
            let pattern = pattern.strip_prefix('!').unwrap_or(pattern);
            matcher(pattern, false).map(|matcher| (matcher, pattern.contains('/'), invert))
        })
        .transpose()?;
    let mut output = GrepOutput {
        lines: Vec::new(),
        bytes: 0,
        bytes_reached: false,
        lines_truncated: false,
    };
    let mut matches = 0;
    let mut limit_reached = false;
    for entry in walk(&root, false, signal) {
        check_cancelled(signal)?;
        let entry = entry.map_err(|error| error.to_string())?;
        if directory && !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let display = if directory {
            relative_path(entry.path(), &root)
        } else {
            root.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        if let Some((filter, path_pattern, invert)) = &filter {
            let candidate = if *path_pattern {
                display.as_str()
            } else {
                entry.file_name().to_str().unwrap_or("")
            };
            if filter.is_match(candidate) == *invert {
                continue;
            }
        }
        let bytes = read_cancellable(entry.path(), signal)?;
        if bytes.contains(&0) {
            continue;
        } // ripgrep skips binary files by default.
        let text = String::from_utf8_lossy(&bytes)
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        let lines: Vec<&str> = text.split('\n').collect();
        let match_lines = if text.is_empty() {
            0
        } else {
            lines.len() - usize::from(text.ends_with('\n'))
        };
        for (index, line) in lines.iter().take(match_lines).enumerate() {
            check_cancelled(signal)?;
            if !regex.is_match(line) {
                continue;
            }
            matches += 1;
            let start = index.saturating_sub(context);
            let end = index
                .saturating_add(context)
                .saturating_add(1)
                .min(lines.len());
            for (offset, line) in lines[start..end].iter().enumerate() {
                check_cancelled(signal)?;
                output.push(&display, start + offset + 1, line, start + offset == index);
                if output.bytes_reached {
                    break;
                }
            }
            if matches >= limit {
                limit_reached = true;
                break;
            }
            if output.bytes_reached {
                break;
            }
        }
        if limit_reached || output.bytes_reached {
            break;
        }
    }
    check_cancelled(signal)?;
    if matches == 0 {
        return Ok(ToolResult::text("No matches found"));
    }
    let truncation = truncate_head(&output.lines.join("\n"), usize::MAX);
    let mut text = truncation.content.clone();
    let mut notices = Vec::new();
    if limit_reached {
        notices.push(format!(
            "{limit} matches limit reached. Use limit={} for more, or refine pattern",
            limit.saturating_mul(2)
        ));
    }
    if output.bytes_reached {
        notices.push("50.0KB limit reached".into());
    }
    if output.lines_truncated {
        notices.push("Some lines truncated to 500 chars. Use read tool to see full lines".into());
    }
    if !notices.is_empty() {
        text.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    Ok(ToolResult {
        content: ToolResult::text(text).content,
        details: json!({"matchLimitReached":if limit_reached { Some(limit) } else { None },"byteLimitReached":output.bytes_reached,"linesTruncated":output.lines_truncated,"matchCount":matches}),
        ..ToolResult::default()
    })
}
