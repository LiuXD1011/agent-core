//! Native authentication commands. Only explicit runtime calls touch credentials.
use crate::interrupt::Interrupt;
use agent_core::types::CancellationToken;
use agent_runtime::auth::{CodexOAuthClient, NativeOAuthClient, NativeOAuthConfig, CODEX_PROVIDER};
pub use agent_runtime::auth_storage::default_auth_directory;
use agent_runtime::auth_storage::AuthStorage;
use serde_json::json;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;

pub fn auth_store_for(directory: &std::path::Path) -> AuthStorage {
    AuthStorage::new(directory.join("credentials.dat"))
}

pub fn run_auth_command(args: &[String]) -> Result<bool, String> {
    if args.first().map(String::as_str) != Some("auth") {
        return Ok(false);
    }
    let interrupt = Interrupt::install()?;
    let signal = interrupt.begin();
    let result = execute_auth_command_with(
        args,
        &CodexOAuthClient::default(),
        &signal,
        &mut io::stdout().lock(),
    );
    interrupt.end();
    result
}

/// Injectable flow for offline tests; production defaults never use fixtures.
pub fn execute_auth_command_with(
    args: &[String],
    client: &CodexOAuthClient,
    signal: &CancellationToken,
    output: &mut dyn Write,
) -> Result<bool, String> {
    execute_auth_command_with_clients(args, client, None, None, signal, output)
}

/// Explicit client and manual-response injection for native hosts and offline tests.
/// No endpoint override is accepted from an ordinary command-line login.
pub fn execute_auth_command_with_clients(
    args: &[String],
    client: &CodexOAuthClient,
    native: Option<&NativeOAuthClient>,
    manual: Option<&Receiver<String>>,
    signal: &CancellationToken,
    output: &mut dyn Write,
) -> Result<bool, String> {
    if args.first().map(String::as_str) != Some("auth") {
        return Ok(false);
    }
    let mut directory = default_auth_directory();
    let mut positional = Vec::new();
    let mut enterprise = None;
    let mut args = args.iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--auth-dir" {
            directory = PathBuf::from(
                args.next()
                    .filter(|value| !value.is_empty())
                    .ok_or("--auth-dir needs a directory")?,
            );
        } else if let Some(path) = arg.strip_prefix("--auth-dir=") {
            if path.is_empty() {
                return Err("--auth-dir needs a directory".into());
            }
            directory = path.into();
        } else if arg == "--enterprise-domain" {
            enterprise = Some(
                args.next()
                    .filter(|value| !value.is_empty())
                    .ok_or("--enterprise-domain needs a domain")?
                    .as_str(),
            );
        } else if let Some(domain) = arg.strip_prefix("--enterprise-domain=") {
            if domain.is_empty() {
                return Err("--enterprise-domain needs a domain".into());
            }
            enterprise = Some(domain);
        } else if arg.starts_with('-') {
            return Err(format!("Unknown auth option: {arg}"));
        } else {
            positional.push(arg.as_str());
        }
    }
    if enterprise.is_some() && positional.as_slice() != ["login", "github-copilot"] {
        return Err("--enterprise-domain belongs to auth login github-copilot".into());
    }
    let store = auth_store_for(&directory);
    match positional.as_slice() {
        ["status"] | ["status",_] => {
            let metadata=store.list(signal)?.into_iter().filter(|entry| positional.len()==1 || entry.provider_id==positional[1]).collect::<Vec<_>>();
            writeln!(output,"{}",json!({"credentials":metadata})).map_err(|_| "Auth status output failed")?;
        }
        ["login",provider] if *provider==CODEX_PROVIDER => {
            let login=client.begin_login()?;
            writeln!(output,"{}",json!({"type":"auth_url","provider":CODEX_PROVIDER,"url":login.authorization_url})).map_err(|_| "Auth URL output failed")?;
            output.flush().map_err(|_| "Auth URL output failed")?;
            let credential=login.finish(signal)?;
            store.write(CODEX_PROVIDER,serde_json::to_value(credential).map_err(|_| "Credential serialization failed")?,&CancellationToken::new())?;
            if signal.is_cancelled() { return Err("Authentication committed; operation aborted".into()); }
            writeln!(output,"{}",json!({"type":"auth_complete","provider":CODEX_PROVIDER})).map_err(|_| "Auth completion output failed")?;
        }
        ["logout",provider] => {store.delete(provider,signal)?;writeln!(output,"{}",json!({"type":"auth_logout","provider":provider})).map_err(|_| "Auth logout output failed")?;}
        ["import",path] => {let report=store.import_legacy(std::path::Path::new(path),signal)?;writeln!(output,"{}",json!({"type":"auth_import","report":report})).map_err(|_| "Auth import output failed")?;}
        ["login",provider] => {
            let mut config=NativeOAuthConfig::for_provider(provider)?;
            if let Some(domain)=enterprise { config=config.with_enterprise_domain(domain)?; }
            let default_client=NativeOAuthClient::new(config);
            let native=native.unwrap_or(&default_client);
            if native.config.provider != *provider { return Err("OAuth client provider identity mismatch".into()); }
            let credential=if matches!(*provider,"anthropic"|"openrouter") {
                let login=native.begin_pkce()?;
                writeln!(output,"{}",json!({"type":"auth_url","provider":provider,"url":login.authorization_url})).map_err(|_|"Auth URL output failed")?;
                output.flush().map_err(|_|"Auth URL output failed")?;
                login.finish_with_manual(manual,signal)?
            } else {
                let login=native.begin_device(signal)?;
                writeln!(output,"{}",json!({"type":"auth_url","provider":provider,"url":login.verification_uri,"userCode":login.user_code,"expiresInSeconds":login.expires_in_seconds})).map_err(|_|"Auth URL output failed")?;
                output.flush().map_err(|_|"Auth URL output failed")?;
                login.finish(signal)?
            };
            store.write(provider,serde_json::to_value(&credential).map_err(|_|"Credential serialization failed")?,&CancellationToken::new())?;
            if signal.is_cancelled() { return Err("Authentication committed; operation aborted".into()); }
            writeln!(output,"{}",json!({"type":"auth_complete","provider":provider})).map_err(|_|"Auth completion output failed")?;
        }
        _ => return Err("Usage: auth status [PROVIDER] | login PROVIDER | logout PROVIDER | import AUTH.json [--auth-dir DIR]; login providers: openai-codex, anthropic, github-copilot, openrouter, kimi-coding, xai".into()),
    }
    output.flush().map_err(|_| "Auth output failed")?;
    Ok(true)
}
