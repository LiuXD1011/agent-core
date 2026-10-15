use agent_core::types::ThinkingLevel;
use agent_runtime::runtime::RunOptions;
use agent_tui::terminal::TerminalMode;
use std::path::PathBuf;

pub const HELP: &str = "Agent Core — Rust native CLI\n\
Usage: agent-core [options] [@files...] [message]\n\
  --provider demo|openai|openai-responses|openai-codex|anthropic|google|azure|vertex|bedrock|mistral|pi-messages\n\
  --base-url URL       API endpoint base\n\
  --model ID           Provider model ID\n\
  --thinking LEVEL     off|minimal|low|medium|high|xhigh|max (clamped to capabilities)\n\
  --list-models [QUERY] Offline model catalog with native protocol markers\n\
  --api-key KEY        Overrides environment key for this process\n\
  --mode text|json|rpc Output text, native events, or JSONL RPC\n\
  --print, -p          Run once and exit\n\
  --line              Use the original native line interface\n\
  --tui-mode regular|fullscreen  Native terminal viewport\n\
  --keybindings PATH  JSON shortcut overrides\n\
  --tui-smoke-test    Render an offline terminal fixture and exit\n\
  --auto-compact / --retry  Opt-in request-boundary automation\n\
  --auth-dir PATH     Native credential directory (Windows DPAPI)\n\
  --extension PATH    Explicit native extension manifest (repeatable)\n\
  --azure-resource NAME / --azure-deployment NAME / --azure-api-version VERSION\n\
  --azure-token TOKEN Azure bearer token instead of an API key\n\
  --vertex-project ID / --vertex-location REGION / --vertex-credentials PATH\n\
  --vertex-token TOKEN Explicit Google Cloud bearer token\n\
  --aws-region REGION / --aws-profile NAME\n\
  --aws-credentials-file PATH / --aws-config-file PATH\n\
  --aws-sts-endpoint URL Native STS endpoint (optional)\n\
  --aws-sso-cache-dir PATH Native AWS SSO cache directory (optional)\n\
  --aws-sso-endpoint URL / --aws-sso-oidc-endpoint URL Native SSO endpoints (optional)\n\
  --aws-access-key-id ID / --aws-secret-access-key KEY / --aws-session-token TOKEN\n\
  --bedrock-token TOKEN / --bedrock-max-tokens N / --bedrock-temperature 0..1\n\
  --bedrock-cache none|short|long\n\
  --context-window N / --max-output-tokens N Explicit budget for unknown deployments\n\
  auth status|login PROVIDER|logout PROVIDER|import PATH\n\
  --cwd PATH          Workspace (defaults to the session workspace)\n\
  --session ID        Restore native session ID\n\
  --continue, -c      Restore latest native session in this workspace\n\
  --session-dir PATH  Native session directory\n\
  --no-session        Do not persist a session\n\
  --name NAME         Set session name\n\
  --import-session PATH [--leaf ENTRY_ID]  Import legacy JSONL branch\n\
  --allow-changes     Enable write/edit\n\
  --allow-commands    Enable run_command\n\
  --tools NAMES       Comma-separated tool allowlist\n\
  --exclude-tools NAMES / --no-tools\n\
  --system-prompt TEXT / --append-system-prompt TEXT\n\
  --skill PATH / --prompt-template PATH / --agent-dir PATH\n\
  --no-skills / --no-prompt-templates / --no-context-files\n\
  --export-jsonl PATH Export native messages without running a model\n\
  --export-html PATH  Export a self-contained native session page\n\
  --max-turns N       Provider request limit (1..1024)\n\
  --compact           Summarize old turns and retain their full archive\n\
  --keep-recent N     User turns retained during compaction (default 2)\n\
  --help / --version\n\
Default provider is explicit offline demo. API keys are never saved in sessions.\n\
Interactive Rust TUI (or --line): /help, /exit, /new, /sessions, /resume ID, /model PROVIDER MODEL.\n";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputMode {
    #[default]
    Text,
    Json,
    Rpc,
}

#[derive(Default)]
pub struct Args {
    pub help: bool,
    pub version: bool,
    pub print: bool,
    pub line: bool,
    pub tui_mode: TerminalMode,
    pub tui_smoke_test: bool,
    pub keybindings: Option<PathBuf>,
    pub auth_dir: Option<PathBuf>,
    pub extensions: Vec<PathBuf>,
    pub azure_resource: Option<String>,
    pub azure_deployment: Option<String>,
    pub azure_api_version: Option<String>,
    pub azure_token: Option<String>,
    pub vertex_project: Option<String>,
    pub vertex_location: Option<String>,
    pub vertex_credentials: Option<PathBuf>,
    pub vertex_token: Option<String>,
    pub aws_region: Option<String>,
    pub aws_profile: Option<String>,
    pub aws_credentials_file: Option<PathBuf>,
    pub aws_config_file: Option<PathBuf>,
    pub aws_sts_endpoint: Option<String>,
    pub aws_sso_cache_dir: Option<PathBuf>,
    pub aws_sso_endpoint: Option<String>,
    pub aws_sso_oidc_endpoint: Option<String>,
    pub aws_access_key_id: Option<String>,
    pub aws_secret_access_key: Option<String>,
    pub aws_session_token: Option<String>,
    pub bedrock_token: Option<String>,
    pub bedrock_max_tokens: Option<u32>,
    pub bedrock_temperature: Option<f64>,
    pub bedrock_cache: Option<String>,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub compact: bool,
    pub keep_recent: Option<usize>,
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    pub list_models: Option<String>,
    pub api_key: Option<String>,
    pub mode: OutputMode,
    pub cwd: Option<PathBuf>,
    pub session_dir: Option<PathBuf>,
    pub session: Option<String>,
    pub continue_session: bool,
    pub no_session: bool,
    pub name: Option<String>,
    pub import_session: Option<PathBuf>,
    pub leaf: Option<String>,
    pub export_jsonl: Option<PathBuf>,
    pub export_html: Option<PathBuf>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Vec<String>,
    pub skills: Vec<PathBuf>,
    pub prompts: Vec<PathBuf>,
    pub agent_dir: Option<PathBuf>,
    pub no_skills: bool,
    pub no_prompts: bool,
    pub no_context_files: bool,
    pub messages: Vec<String>,
    pub files: Vec<PathBuf>,
    pub run: RunOptions,
}

impl std::fmt::Debug for Args {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Args")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("mode", &self.mode)
            .field("extension_count", &self.extensions.len())
            .field("message_count", &self.messages.len())
            .finish_non_exhaustive()
    }
}

fn names(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut result = Args::default();
    let mut args = args.into_iter().peekable();
    let mut positional = false;
    while let Some(arg) = args.next() {
        if arg == "--" && !positional {
            positional = true;
            continue;
        }
        if positional || !arg.starts_with('-') {
            if let Some(path) = arg.strip_prefix('@') {
                if path.is_empty() {
                    return Err("Empty attachment path".into());
                }
                result.files.push(PathBuf::from(path));
            } else {
                result.messages.push(arg);
            }
            continue;
        }
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(a, b)| (a, Some(b.to_owned())));
        let mut value = || {
            inline
                .clone()
                .or_else(|| args.next())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag {
            "--help" | "-h" => result.help = true,
            "--version" | "-v" => result.version = true,
            "--print" | "-p" => result.print = true,
            "--line" => result.line = true,
            "--tui-smoke-test" => result.tui_smoke_test = true,
            "--tui-mode" => {
                result.tui_mode = match value()?.as_str() {
                    "regular" => TerminalMode::Regular,
                    "fullscreen" => TerminalMode::Fullscreen,
                    _ => return Err("--tui-mode must be regular or fullscreen".into()),
                }
            }
            "--keybindings" => result.keybindings = Some(value()?.into()),
            "--auth-dir" => result.auth_dir = Some(value()?.into()),
            "--extension" => result.extensions.push(value()?.into()),
            "--azure-resource" => result.azure_resource = Some(value()?),
            "--azure-deployment" => result.azure_deployment = Some(value()?),
            "--azure-api-version" => result.azure_api_version = Some(value()?),
            "--azure-token" => result.azure_token = Some(value()?),
            "--vertex-project" => result.vertex_project = Some(value()?),
            "--vertex-location" => result.vertex_location = Some(value()?),
            "--vertex-credentials" => result.vertex_credentials = Some(value()?.into()),
            "--vertex-token" => result.vertex_token = Some(value()?),
            "--aws-region" => result.aws_region = Some(value()?),
            "--aws-profile" => result.aws_profile = Some(value()?),
            "--aws-credentials-file" => result.aws_credentials_file = Some(value()?.into()),
            "--aws-config-file" => result.aws_config_file = Some(value()?.into()),
            "--aws-sts-endpoint" => result.aws_sts_endpoint = Some(value()?),
            "--aws-sso-cache-dir" => result.aws_sso_cache_dir = Some(value()?.into()),
            "--aws-sso-endpoint" => result.aws_sso_endpoint = Some(value()?),
            "--aws-sso-oidc-endpoint" => result.aws_sso_oidc_endpoint = Some(value()?),
            "--aws-access-key-id" => result.aws_access_key_id = Some(value()?),
            "--aws-secret-access-key" => result.aws_secret_access_key = Some(value()?),
            "--aws-session-token" => result.aws_session_token = Some(value()?),
            "--bedrock-token" => result.bedrock_token = Some(value()?),
            "--bedrock-max-tokens" => {
                let n = value()?
                    .parse::<u32>()
                    .map_err(|_| "--bedrock-max-tokens must be a positive integer")?;
                if n == 0 {
                    return Err("--bedrock-max-tokens must be positive".into());
                }
                result.bedrock_max_tokens = Some(n);
            }
            "--bedrock-temperature" => {
                let n = value()?
                    .parse::<f64>()
                    .map_err(|_| "--bedrock-temperature must be 0..1")?;
                if !n.is_finite() || !(0.0..=1.0).contains(&n) {
                    return Err("--bedrock-temperature must be 0..1".into());
                }
                result.bedrock_temperature = Some(n);
            }
            "--bedrock-cache" => {
                let cache = value()?;
                if !matches!(cache.as_str(), "none" | "short" | "long") {
                    return Err("--bedrock-cache must be none, short or long".into());
                }
                result.bedrock_cache = Some(cache);
            }
            "--context-window" => {
                let window = value()?
                    .parse::<u64>()
                    .map_err(|_| "--context-window must be a positive integer")?;
                if window == 0 {
                    return Err("--context-window must be positive".into());
                }
                result.context_window = Some(window);
            }
            "--max-output-tokens" => {
                let max = value()?
                    .parse::<u64>()
                    .map_err(|_| "--max-output-tokens must be a positive integer")?;
                if max == 0 {
                    return Err("--max-output-tokens must be positive".into());
                }
                result.max_output_tokens = Some(max);
            }
            "--auto-compact" => result.run.automation.enabled = true,
            "--retry" => result.run.automation.retry.enabled = true,
            "--compact" => result.compact = true,
            "--keep-recent" => {
                let keep = value()?
                    .parse::<usize>()
                    .map_err(|_| "--keep-recent must be an integer".to_string())?;
                if keep == 0 {
                    return Err("--keep-recent must be at least 1".into());
                }
                result.keep_recent = Some(keep);
            }
            "--provider" => result.provider = Some(value()?),
            "--base-url" => result.base_url = Some(value()?),
            "--model" => result.model = Some(value()?),
            "--thinking" => result.thinking = Some(value()?.parse()?),
            "--list-models" => {
                result.list_models = Some(inline.unwrap_or_else(|| {
                    if args.peek().is_some_and(|item| !item.starts_with('-')) {
                        args.next().unwrap()
                    } else {
                        String::new()
                    }
                }))
            }
            "--api-key" => result.api_key = Some(value()?),
            "--mode" => {
                result.mode = match value()?.as_str() {
                    "text" => OutputMode::Text,
                    "json" => OutputMode::Json,
                    "rpc" => OutputMode::Rpc,
                    _ => return Err("--mode must be text, json or rpc".into()),
                }
            }
            "--cwd" => result.cwd = Some(value()?.into()),
            "--session-dir" => result.session_dir = Some(value()?.into()),
            "--session" => result.session = Some(value()?),
            "--continue" | "-c" => result.continue_session = true,
            "--no-session" => result.no_session = true,
            "--name" | "-n" => result.name = Some(value()?),
            "--import-session" => result.import_session = Some(value()?.into()),
            "--leaf" => result.leaf = Some(value()?),
            "--export-jsonl" => result.export_jsonl = Some(value()?.into()),
            "--export-html" => result.export_html = Some(value()?.into()),
            "--system-prompt" => result.system_prompt = Some(value()?),
            "--append-system-prompt" => result.append_system_prompt.push(value()?),
            "--allow-changes" => result.run.allow_changes = true,
            "--allow-commands" => result.run.allow_commands = true,
            "--no-tools" | "-nt" => result.run.no_tools = true,
            "--tools" | "-t" => result.run.tools = Some(names(&value()?)),
            "--exclude-tools" | "-xt" => result.run.exclude_tools = names(&value()?),
            "--skill" => result.skills.push(value()?.into()),
            "--prompt-template" => result.prompts.push(value()?.into()),
            "--agent-dir" => result.agent_dir = Some(value()?.into()),
            "--no-skills" => result.no_skills = true,
            "--no-prompt-templates" => result.no_prompts = true,
            "--no-context-files" => result.no_context_files = true,
            "--max-turns" => {
                result.run.max_turns = value()?
                    .parse()
                    .map_err(|_| "--max-turns must be an integer".to_owned())?;
                if !(1..=1024).contains(&result.run.max_turns) {
                    return Err("--max-turns must be 1..1024".into());
                }
            }
            _ => return Err(format!("Unknown option: {flag}")),
        }
    }
    if result.leaf.is_some() && result.import_session.is_none() {
        return Err("--leaf requires --import-session".into());
    }
    if result.session.is_some() && result.import_session.is_some() {
        return Err("Choose --session or --import-session".into());
    }
    if result.mode == OutputMode::Rpc && (!result.messages.is_empty() || !result.files.is_empty()) {
        return Err("RPC prompts are JSON lines on stdin, not positional arguments".into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(items: &[&str]) -> Result<Args, String> {
        parse(items.iter().map(|s| (*s).into()))
    }
    #[test]
    fn parses_values_modes_files_and_tool_policy() {
        let result = args(&[
            "--mode=json",
            "--provider",
            "anthropic",
            "--allow-changes",
            "--tools",
            "read, edit",
            "@file.txt",
            "你好",
        ])
        .unwrap();
        assert_eq!(result.mode, OutputMode::Json);
        assert_eq!(result.provider.as_deref(), Some("anthropic"));
        assert_eq!(result.files, vec![PathBuf::from("file.txt")]);
        assert_eq!(result.messages, vec!["你好"]);
        assert_eq!(result.run.tools.unwrap(), vec!["read", "edit"]);
        assert!(result.run.allow_changes);
    }
    #[test]
    fn stops_parsing_flags_at_double_dash() {
        let result = args(&["-p", "--", "--looks-like-a-flag", "@file"]).unwrap();
        assert_eq!(result.messages, vec!["--looks-like-a-flag"]);
        assert!(result.print);
    }
    #[test]
    fn validates_missing_values_conflicts_and_limits() {
        for input in [
            vec!["--model"],
            vec!["--mode", "bad"],
            vec!["--max-turns", "0"],
            vec!["--max-turns", "1025"],
            vec!["--leaf", "id"],
            vec!["--session", "s", "--import-session", "f"],
            vec!["--mode", "rpc", "hi"],
            vec!["--unknown"],
        ] {
            assert!(args(&input).is_err(), "{input:?}");
        }
    }
    #[test]
    fn parses_explicit_extension_and_native_cloud_configuration() {
        let result = args(&[
            "--extension",
            "one.json",
            "--extension=two.json",
            "--azure-resource",
            "resource",
            "--azure-deployment",
            "deployment",
            "--azure-api-version",
            "v1",
            "--vertex-project",
            "project",
            "--vertex-location",
            "global",
            "--vertex-credentials",
            "adc.json",
        ])
        .unwrap();
        assert_eq!(
            result.extensions,
            vec![PathBuf::from("one.json"), PathBuf::from("two.json")]
        );
        assert_eq!(result.azure_deployment.as_deref(), Some("deployment"));
        assert_eq!(result.vertex_project.as_deref(), Some("project"));
        assert_eq!(result.vertex_credentials, Some(PathBuf::from("adc.json")));
        assert!(args(&["--extension"]).is_err());
    }

    #[test]
    fn parses_explicit_sso_paths_and_endpoints_without_debugging_credentials() {
        let result = args(&[
            "--aws-sso-cache-dir",
            "cache with spaces",
            "--aws-sso-endpoint=http://127.0.0.1:1/sso",
            "--aws-sso-oidc-endpoint",
            "http://127.0.0.1:1/oidc",
            "--aws-secret-access-key",
            "synthetic-secret-do-not-print",
            "--aws-session-token",
            "synthetic-session-do-not-print",
            "--bedrock-token",
            "synthetic-bearer-do-not-print",
        ])
        .unwrap();
        assert_eq!(
            result.aws_sso_cache_dir,
            Some(PathBuf::from("cache with spaces"))
        );
        assert_eq!(
            result.aws_sso_endpoint.as_deref(),
            Some("http://127.0.0.1:1/sso")
        );
        assert_eq!(
            result.aws_sso_oidc_endpoint.as_deref(),
            Some("http://127.0.0.1:1/oidc")
        );
        assert!(!format!("{result:?}").contains("do-not-print"));
        for flag in [
            "--aws-sso-cache-dir",
            "--aws-sso-endpoint",
            "--aws-sso-oidc-endpoint",
        ] {
            assert_eq!(args(&[flag]).unwrap_err(), format!("{flag} needs a value"));
            assert_eq!(
                args(&[&format!("{flag}=")]).unwrap_err(),
                format!("{flag} needs a value")
            );
            assert!(HELP.contains(flag));
        }
    }
}
