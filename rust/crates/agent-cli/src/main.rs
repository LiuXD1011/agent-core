use agent_cli::args::{parse, OutputMode, HELP};
use agent_cli::client::Client;
use agent_cli::interrupt::Interrupt;
use agent_core::types::CancellationToken;
use agent_core::types::{AgentEvent, Message};
use agent_runtime::catalog::search;
use agent_runtime::session::SessionStore;
use std::io::{self, BufRead, IsTerminal, Read, Write};

fn run() -> Result<(), String> {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    if agent_cli::auth_commands::run_auth_command(&raw_args)? {
        return Ok(());
    }
    let args = parse(raw_args)?;
    if args.help {
        print!("{HELP}");
        return Ok(());
    }
    if args.version {
        println!("agent-core {} (Rust)", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.tui_smoke_test {
        println!("{}", agent_cli::terminal_client::smoke_test()?);
        return Ok(());
    }
    if let Some(query) = &args.list_models {
        let models = search(query);
        if args.mode == OutputMode::Json {
            println!(
                "{}",
                serde_json::json!({"models":models.iter().map(|model|serde_json::json!({"model":model,"nativeProtocol":model.is_supported()})).collect::<Vec<_>>()})
            );
        } else {
            for model in models {
                println!(
                    "{}\t{}\t{}\twindow={} output={}",
                    model.canonical_id(),
                    model.api,
                    if model.is_supported() {
                        "native"
                    } else {
                        "pending"
                    },
                    model.context_window,
                    model.max_tokens
                );
            }
        }
        return Ok(());
    }
    let interrupt = Interrupt::install()?;
    let signal = interrupt.begin();
    let result = Client::from_args_with_signal(&args, &signal);
    interrupt.end();
    let mut client = result?;
    if let Some(path) = &args.export_jsonl {
        SessionStore::export_jsonl(&client.session, path)?;
        println!("{}", path.display());
        return Ok(());
    }
    if let Some(path) = &args.export_html {
        SessionStore::export_html(&client.session, path)?;
        println!("{}", path.display());
        return Ok(());
    }
    if args.mode == OutputMode::Rpc {
        let signal = interrupt.begin();
        let result = agent_cli::rpc::run_rpc(
            io::BufReader::new(io::stdin()),
            &mut io::stdout().lock(),
            client,
            &signal,
        );
        interrupt.end();
        return result;
    }
    if args.compact {
        let signal = interrupt.begin();
        let result = client.compact(args.keep_recent.unwrap_or(2), &signal);
        interrupt.end();
        let result = result?;
        if args.mode == OutputMode::Json {
            println!(
                "{}",
                serde_json::json!({"type":"compaction_end","result":result})
            );
        } else {
            println!("Context compacted; original messages retained in the session archive.");
        }
        if args.messages.is_empty() && args.files.is_empty() {
            return Ok(());
        }
    }
    let mut initial = args.messages.join("\n");
    let interactive = io::stdin().is_terminal() && !args.print;
    if !interactive && initial.is_empty() && args.files.is_empty() {
        io::stdin()
            .take(1_048_577)
            .read_to_string(&mut initial)
            .map_err(|e| e.to_string())?;
        if initial.len() > 1_048_576 {
            return Err("Stdin prompt exceeds 1MB".into());
        }
    }
    if interactive && io::stdout().is_terminal() && args.mode == OutputMode::Text && !args.line {
        let initial = if !initial.trim().is_empty() || !args.files.is_empty() {
            Some(client.message(&initial, &args.files)?)
        } else {
            None
        };
        return agent_cli::terminal_client::run_terminal(
            client,
            initial,
            args.tui_mode,
            args.keybindings.as_deref(),
            &interrupt,
        );
    }
    if !initial.trim().is_empty() || !args.files.is_empty() {
        let message = client.message(&initial, &args.files)?;
        let signal = interrupt.begin();
        let result = run_message(&mut client, message, args.mode, &signal);
        interrupt.end();
        result?;
    } else if !interactive {
        return Err("Prompt is empty".into());
    }
    if !interactive {
        return Ok(());
    }
    println!("Agent Core · Rust line mode. /exit, /new, /sessions, /resume ID");
    let input = io::stdin();
    let mut input = input.lock();
    loop {
        print!("> ");
        io::stdout().flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        if input.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            break;
        }
        let line = line.trim();
        let result = match line {
            "" => continue,
            "/exit" | "/quit" => break,
            "/new" => {
                let result = client.new_session_with_signal(&interrupt.begin());
                interrupt.end();
                result
            }
            "/compact" => {
                let signal = interrupt.begin();
                let result = client.compact(2, &signal).map(|_| ());
                interrupt.end();
                result
            }
            "/sessions" => {
                if let Some(store) = &client.store {
                    for session in store.list()? {
                        println!(
                            "{}  {}  {}",
                            session.id,
                            session.name,
                            session.workspace.display()
                        );
                    }
                }
                Ok(())
            }
            _ if line.starts_with("/resume ") => {
                let result = client.resume_with_signal(line[8..].trim(), &interrupt.begin());
                interrupt.end();
                result
            }
            _ => client.message(line, &[]).and_then(|message| {
                let signal = interrupt.begin();
                let result = run_message(&mut client, message, args.mode, &signal);
                interrupt.end();
                result
            }),
        };
        if let Err(error) = result {
            eprintln!("{error}");
        }
    }
    client.save()
}

fn run_message(
    client: &mut Client,
    message: Message,
    mode: OutputMode,
    signal: &CancellationToken,
) -> Result<(), String> {
    let mut output = io::stdout().lock();
    let mut output_error = None;
    let result = client.run(message, signal, &mut |event: AgentEvent| {
        if output_error.is_some() {
            return;
        }
        let record = if mode == OutputMode::Json {
            serde_json::to_string(&event)
                .map(Some)
                .map_err(|e| e.to_string())
        } else if let AgentEvent::MessageEnd {
            message: Message::Assistant(message),
        } = event
        {
            let text = Message::Assistant(message).text();
            Ok((!text.is_empty()).then_some(text))
        } else {
            Ok(None)
        };
        let written = record.and_then(|record| {
            if let Some(record) = record {
                writeln!(output, "{record}")
                    .and_then(|_| output.flush())
                    .map_err(|e| e.to_string())
            } else {
                Ok(())
            }
        });
        if let Err(error) = written {
            output_error = Some(error);
            signal.cancel();
        }
    });
    if output_error.is_none() {
        if client.input_handled {
            let record = if mode == OutputMode::Json {
                "{\"type\":\"input_handled\"}"
            } else {
                "Input handled by extension"
            };
            if let Err(error) = writeln!(output, "{record}").and_then(|_| output.flush()) {
                output_error = Some(error.to_string());
                signal.cancel();
            }
        }
        if let Some(command) = &client.last_command {
            let record = if mode == OutputMode::Json {
                serde_json::json!({"type":"extension_command_result","result":command}).to_string()
            } else {
                command.text.clone()
            };
            if !record.is_empty() {
                if let Err(error) = writeln!(output, "{record}").and_then(|_| output.flush()) {
                    output_error = Some(error.to_string());
                    signal.cancel();
                }
            }
            if mode != OutputMode::Json {
                for ui in &command.ui {
                    eprintln!("{}", serde_json::to_string(ui).map_err(|e| e.to_string())?);
                }
            }
        }
        for diagnostic in client.options.extensions.drain_diagnostics() {
            eprintln!(
                "Extension {} / {}: {}",
                diagnostic.extension_id, diagnostic.event, diagnostic.error
            );
        }
    }
    if let Some(error) = output_error {
        Err(format!("Output failed: {error}"))
    } else {
        result
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
