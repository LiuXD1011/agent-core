//! Native skills and prompt-template discovery. Files are loaded, not executed.

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResourceScope {
    User,
    Project,
    Path,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticKind {
    Warning,
    Collision,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResourceDiagnostic {
    pub kind: DiagnosticKind,
    pub message: String,
    pub path: PathBuf,
    pub winner_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    pub source: ResourceScope,
    pub disable_model_invocation: bool,
    pub frontmatter: Value,
    pub content: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub content: String,
    pub file_path: PathBuf,
    pub source: ResourceScope,
}

#[derive(Clone, Debug)]
pub struct ResourceOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub skill_paths: Vec<PathBuf>,
    pub prompt_paths: Vec<PathBuf>,
    pub include_defaults: bool,
}

impl Default for ResourceOptions {
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_default(),
            agent_dir: std::env::var_os("AGENT_CORE_CODING_AGENT_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home_directory().join(".agent-core").join("agent")),
            skill_paths: vec![],
            prompt_paths: vec![],
            include_defaults: true,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ResourceLoad {
    pub skills: Vec<Skill>,
    pub prompts: Vec<PromptTemplate>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Frontmatter {
    pub fields: Value,
    pub body: String,
}

pub fn parse_frontmatter(content: &str) -> Result<Frontmatter, String> {
    let normalized = content
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    if let Some(after_delimiter) = normalized.strip_prefix("---") {
        if let Some(end) = after_delimiter.find("\n---").map(|end| end + 3) {
            let yaml = normalized.get(4..end).unwrap_or_default();
            let fields: Value = if yaml.is_empty() {
                Value::Object(Map::new())
            } else {
                serde_yaml::from_str(yaml)
                    .map_err(|error| format!("Invalid frontmatter: {error}"))?
            };
            let fields = if fields.is_null() {
                Value::Object(Map::new())
            } else {
                fields
            };
            if !fields.is_object() {
                return Err("Frontmatter must be a mapping".into());
            }
            return Ok(Frontmatter {
                fields,
                body: normalized[end + 4..].trim().into(),
            });
        }
    }
    Ok(Frontmatter {
        fields: Value::Object(Map::new()),
        body: normalized,
    })
}

fn home_directory() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

fn resolve_path(path: &Path, cwd: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    let path = if text == "~" {
        home_directory()
    } else if text.starts_with("~/") || text.starts_with("~\\") {
        home_directory().join(&text[2..])
    } else {
        PathBuf::from(text.trim())
    };
    let path = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    fs::canonicalize(&path).unwrap_or(path)
}

fn warning(diagnostics: &mut Vec<ResourceDiagnostic>, path: &Path, message: impl Into<String>) {
    diagnostics.push(ResourceDiagnostic {
        kind: DiagnosticKind::Warning,
        message: message.into(),
        path: path.to_owned(),
        winner_path: None,
    });
}

pub fn load_skill_file(path: &Path, source: ResourceScope) -> Result<Skill, String> {
    let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let Frontmatter { fields, body } = parse_frontmatter(&text)?;
    let description = fields["description"]
        .as_str()
        .filter(|description| !description.trim().is_empty())
        .ok_or("description is required")?
        .to_owned();
    let base_dir = path
        .parent()
        .ok_or("Skill has no parent directory")?
        .to_owned();
    let name = fields["name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            base_dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
    Ok(Skill {
        name,
        description,
        file_path: path.to_owned(),
        base_dir,
        source,
        disable_model_invocation: fields["disable-model-invocation"].as_bool() == Some(true),
        frontmatter: fields,
        content: body,
    })
}

fn load_skill(path: &Path, source: ResourceScope, loaded: &mut ResourceLoad) {
    match load_skill_file(path, source) {
        Ok(skill) => {
            let name = &skill.name;
            if name.len() > 64 {
                warning(&mut loaded.diagnostics, path, "name exceeds 64 characters");
            }
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            {
                warning(
                    &mut loaded.diagnostics,
                    path,
                    "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)",
                );
            }
            if name.starts_with('-') || name.ends_with('-') {
                warning(
                    &mut loaded.diagnostics,
                    path,
                    "name must not start or end with a hyphen",
                );
            }
            if name.contains("--") {
                warning(
                    &mut loaded.diagnostics,
                    path,
                    "name must not contain consecutive hyphens",
                );
            }
            if skill.description.encode_utf16().count() > 1024 {
                warning(
                    &mut loaded.diagnostics,
                    path,
                    "description exceeds 1024 characters",
                );
            }
            loaded.skills.push(skill);
        }
        Err(error) => {
            // Ordinary markdown files without descriptions are not declared skills.
            if path.file_name().is_some_and(|name| name == "SKILL.md")
                || error != "description is required"
            {
                warning(&mut loaded.diagnostics, path, error);
            }
        }
    }
}

fn ignored(path: &Path, is_directory: bool, matchers: &[Gitignore]) -> bool {
    let mut ignored = false;
    for matcher in matchers {
        let matched = matcher.matched_path_or_any_parents(path, is_directory);
        if matched.is_ignore() {
            ignored = true;
        } else if matched.is_whitelist() {
            ignored = false;
        }
    }
    ignored
}

fn scan_skills(
    directory: &Path,
    source: ResourceScope,
    root_files: bool,
    matchers: &mut Vec<Gitignore>,
    visited: &mut BTreeSet<PathBuf>,
    loaded: &mut ResourceLoad,
) {
    if !directory.exists() {
        return;
    }
    let canonical = match fs::canonicalize(directory) {
        Ok(path) => path,
        Err(error) => {
            warning(&mut loaded.diagnostics, directory, error.to_string());
            return;
        }
    };
    if !visited.insert(canonical.clone()) {
        return;
    }
    let mut builder = GitignoreBuilder::new(directory);
    for name in [".gitignore", ".ignore", ".fdignore"] {
        let path = directory.join(name);
        if path.is_file() {
            if let Some(error) = builder.add(&path) {
                warning(&mut loaded.diagnostics, &path, error.to_string());
            }
        }
    }
    match builder.build() {
        Ok(matcher) => matchers.push(matcher),
        Err(error) => {
            warning(&mut loaded.diagnostics, directory, error.to_string());
            visited.remove(&canonical);
            return;
        }
    }
    let declared = directory.join("SKILL.md");
    if declared.is_file() && !ignored(&declared, false, matchers) {
        load_skill(&declared, source, loaded);
    } else {
        match fs::read_dir(directory) {
            Ok(entries) => {
                let mut paths: Vec<PathBuf> = entries
                    .filter_map(|entry| match entry {
                        Ok(entry) => Some(entry.path()),
                        Err(error) => {
                            warning(&mut loaded.diagnostics, directory, error.to_string());
                            None
                        }
                    })
                    .collect();
                paths.sort();
                for path in paths {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    if name.starts_with('.') || name == "node_modules" {
                        continue;
                    }
                    if ignored(&path, path.is_dir(), matchers) {
                        continue;
                    }
                    if path.is_dir() {
                        scan_skills(&path, source, false, matchers, visited, loaded);
                    } else if root_files
                        && path.extension().is_some_and(|extension| extension == "md")
                        && path.is_file()
                    {
                        load_skill(&path, source, loaded);
                    }
                }
            }
            Err(error) => warning(&mut loaded.diagnostics, directory, error.to_string()),
        }
    }
    matchers.pop();
    visited.remove(&canonical);
}

fn load_prompt(path: &Path, source: ResourceScope, loaded: &mut ResourceLoad) {
    let result = fs::read_to_string(path)
        .map_err(|error| error.to_string())
        .and_then(|content| parse_frontmatter(&content));
    match result {
        Ok(Frontmatter { fields, body }) => {
            let description = fields["description"]
                .as_str()
                .filter(|description| !description.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    let first = body
                        .lines()
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or_default();
                    let units: Vec<u16> = first.encode_utf16().collect();
                    let mut description = String::from_utf16_lossy(&units[..units.len().min(60)]);
                    if units.len() > 60 {
                        description.push_str("...");
                    }
                    description
                });
            loaded.prompts.push(PromptTemplate {
                name: path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                description,
                argument_hint: fields["argument-hint"]
                    .as_str()
                    .filter(|hint| !hint.is_empty())
                    .map(str::to_owned),
                content: body,
                file_path: path.to_owned(),
                source,
            });
        }
        Err(error) => warning(&mut loaded.diagnostics, path, error),
    }
}

fn scan_prompts(directory: &Path, source: ResourceScope, loaded: &mut ResourceLoad) {
    if !directory.exists() {
        return;
    }
    match fs::read_dir(directory) {
        Ok(entries) => {
            let mut paths: Vec<_> = entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .collect();
            paths.sort();
            for path in paths {
                if path.is_file() && path.extension().is_some_and(|extension| extension == "md") {
                    load_prompt(&path, source, loaded);
                }
            }
        }
        Err(error) => warning(&mut loaded.diagnostics, directory, error.to_string()),
    }
}

pub fn load_resources(options: &ResourceOptions) -> ResourceLoad {
    let cwd = resolve_path(&options.cwd, &std::env::current_dir().unwrap_or_default());
    let agent_dir = resolve_path(&options.agent_dir, &cwd);
    let user_skills = agent_dir.join("skills");
    let project_skills = cwd.join(".agent-core").join("skills");
    let user_prompts = agent_dir.join("prompts");
    let project_prompts = cwd.join(".agent-core").join("prompts");
    let mut loaded = ResourceLoad::default();
    if options.include_defaults {
        scan_skills(
            &user_skills,
            ResourceScope::User,
            true,
            &mut vec![],
            &mut BTreeSet::new(),
            &mut loaded,
        );
        scan_skills(
            &project_skills,
            ResourceScope::Project,
            true,
            &mut vec![],
            &mut BTreeSet::new(),
            &mut loaded,
        );
        scan_prompts(&user_prompts, ResourceScope::User, &mut loaded);
        scan_prompts(&project_prompts, ResourceScope::Project, &mut loaded);
    }
    for raw_path in &options.skill_paths {
        let path = resolve_path(raw_path, &cwd);
        let source = if !options.include_defaults && path.starts_with(&user_skills) {
            ResourceScope::User
        } else if !options.include_defaults && path.starts_with(&project_skills) {
            ResourceScope::Project
        } else {
            ResourceScope::Path
        };
        if path.is_dir() {
            scan_skills(
                &path,
                source,
                true,
                &mut vec![],
                &mut BTreeSet::new(),
                &mut loaded,
            );
        } else if path.is_file() && path.extension().is_some_and(|extension| extension == "md") {
            load_skill(&path, source, &mut loaded);
        } else {
            warning(
                &mut loaded.diagnostics,
                &path,
                "skill path does not exist or is not a markdown file",
            );
        }
    }
    for raw_path in &options.prompt_paths {
        let path = resolve_path(raw_path, &cwd);
        let source = if path.starts_with(&user_prompts) {
            ResourceScope::User
        } else if path.starts_with(&project_prompts) {
            ResourceScope::Project
        } else {
            ResourceScope::Path
        };
        if path.is_dir() {
            scan_prompts(&path, source, &mut loaded);
        } else if path.is_file() && path.extension().is_some_and(|extension| extension == "md") {
            load_prompt(&path, source, &mut loaded);
        } else {
            warning(
                &mut loaded.diagnostics,
                &path,
                "prompt path does not exist or is not a markdown file",
            );
        }
    }
    let mut names: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut paths = BTreeSet::new();
    loaded.skills.retain(|skill| {
        let canonical =
            fs::canonicalize(&skill.file_path).unwrap_or_else(|_| skill.file_path.clone());
        if paths.contains(&canonical) {
            return false;
        }
        if let Some(winner) = names.get(&skill.name) {
            loaded.diagnostics.push(ResourceDiagnostic {
                kind: DiagnosticKind::Collision,
                message: format!("skill name {:?} collision", skill.name),
                path: skill.file_path.clone(),
                winner_path: Some(winner.clone()),
            });
            false
        } else {
            names.insert(skill.name.clone(), skill.file_path.clone());
            paths.insert(canonical);
            true
        }
    });
    names.clear();
    paths.clear();
    loaded.prompts.retain(|prompt| {
        let canonical =
            fs::canonicalize(&prompt.file_path).unwrap_or_else(|_| prompt.file_path.clone());
        if paths.contains(&canonical) {
            return false;
        }
        if let Some(winner) = names.get(&prompt.name) {
            loaded.diagnostics.push(ResourceDiagnostic {
                kind: DiagnosticKind::Collision,
                message: format!("prompt name {:?} collision", prompt.name),
                path: prompt.file_path.clone(),
                winner_path: Some(winner.clone()),
            });
            false
        } else {
            names.insert(prompt.name.clone(), prompt.file_path.clone());
            paths.insert(canonical);
            true
        }
    });
    loaded
}

pub fn parse_command_args(text: &str) -> Vec<String> {
    let mut args = vec![];
    let mut current = String::new();
    let mut quote = None;
    for character in text.chars() {
        if let Some(in_quote) = quote {
            if character == in_quote {
                quote = None;
            } else {
                current.push(character);
            }
        } else if character == '\'' || character == '"' {
            quote = Some(character);
        } else if character.is_whitespace() {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
        } else {
            current.push(character);
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

pub fn substitute_args(content: &str, args: &[String]) -> String {
    let all_args = args.join(" ");
    let positional = |target: &str| {
        target
            .parse::<usize>()
            .ok()
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| args.get(index))
            .map(String::as_str)
            .unwrap_or("")
    };
    let mut output = String::new();
    let mut cursor = 0;
    while cursor < content.len() {
        let tail = &content[cursor..];
        if let Some(pattern) = tail.strip_prefix('$') {
            if let Some(braced) = pattern.strip_prefix('{') {
                if let Some(brace_end) = braced.find('}') {
                    let expression = &braced[..brace_end];
                    if let Some((target, default)) = expression.split_once(":-") {
                        if target == "@"
                            || target == "ARGUMENTS"
                            || (!target.is_empty()
                                && target.bytes().all(|byte| byte.is_ascii_digit()))
                        {
                            let value = if target == "@" || target == "ARGUMENTS" {
                                all_args.as_str()
                            } else {
                                positional(target)
                            };
                            output.push_str(if value.is_empty() { default } else { value });
                            cursor += brace_end + 3;
                            continue;
                        }
                    } else if let Some(slice) = expression.strip_prefix("@:") {
                        let parts: Vec<_> = slice.split(':').collect();
                        if (1..=2).contains(&parts.len())
                            && parts.iter().all(|part| {
                                !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
                            })
                        {
                            let start = parts[0]
                                .parse::<usize>()
                                .unwrap_or(usize::MAX)
                                .saturating_sub(1)
                                .min(args.len());
                            let end = if parts.len() == 2 {
                                start
                                    .saturating_add(parts[1].parse::<usize>().unwrap_or(usize::MAX))
                                    .min(args.len())
                            } else {
                                args.len()
                            };
                            output.push_str(&args[start..end].join(" "));
                            cursor += brace_end + 3;
                            continue;
                        }
                    }
                }
            } else if pattern.starts_with("ARGUMENTS") {
                output.push_str(&all_args);
                cursor += 10;
                continue;
            } else if pattern.starts_with('@') {
                output.push_str(&all_args);
                cursor += 2;
                continue;
            } else {
                let length = pattern.bytes().take_while(u8::is_ascii_digit).count();
                if length > 0 {
                    output.push_str(positional(&pattern[..length]));
                    cursor += length + 1;
                    continue;
                }
            }
        }
        let character = tail.chars().next().expect("nonempty tail");
        output.push(character);
        cursor += character.len_utf8();
    }
    output
}

pub fn expand_prompt_template(text: &str, templates: &[PromptTemplate]) -> String {
    let Some(command) = text.strip_prefix('/') else {
        return text.into();
    };
    let split = command.find(char::is_whitespace).unwrap_or(command.len());
    let name = &command[..split];
    match templates.iter().find(|template| template.name == name) {
        Some(template) => {
            substitute_args(&template.content, &parse_command_args(&command[split..]))
        }
        None => text.into(),
    }
}

/// Expand explicit skills and templates identically for CLI, RPC, and desktop.
pub fn prepare_prompt(text: &str, resources: &ResourceLoad) -> Result<String, String> {
    if let Some(invocation) = text.strip_prefix("/skill:") {
        let (name, arguments) = invocation
            .split_once(char::is_whitespace)
            .unwrap_or((invocation, ""));
        let skill = resources
            .skills
            .iter()
            .find(|skill| skill.name == name)
            .ok_or_else(|| format!("Skill not found: {name}"))?;
        let content = fs::read_to_string(&skill.file_path)
            .map_err(|error| format!("{}: {error}", skill.file_path.display()))?;
        let body = parse_frontmatter(&content)?.body;
        let mut expanded = format!(
            "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
            escape_xml(&skill.name),
            escape_xml(&skill.file_path.to_string_lossy()),
            skill.base_dir.display(),
            body.trim()
        );
        if !arguments.trim().is_empty() {
            expanded.push_str("\n\n");
            expanded.push_str(arguments.trim());
        }
        Ok(expanded)
    } else {
        Ok(expand_prompt_template(text, &resources.prompts))
    }
}

pub fn format_skills_for_prompt(skills: &[Skill]) -> String {
    let visible: Vec<_> = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut output = "\n\nThe following skills provide specialized instructions for specific tasks.\nUse the read tool to load a skill's file when the task matches its description.\nWhen a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.\n\n<available_skills>".to_owned();
    for skill in visible {
        output.push_str(&format!("\n  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    <location>{}</location>\n  </skill>", escape_xml(&skill.name), escape_xml(&skill.description), escape_xml(&skill.file_path.to_string_lossy())));
    }
    output.push_str("\n</available_skills>");
    output
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
