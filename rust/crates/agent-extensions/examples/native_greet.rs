//! Run offline: cargo run -p agent-extensions --example native_greet --offline
use agent_core::tools::AgentTool;
use agent_core::types::{CancellationToken, ToolResult, Value};
use agent_extensions::{
    CommandResult, Extension, ExtensionCommand, ExtensionContext, SharedRegistry,
};
use serde_json::json;
use std::sync::Arc;

struct Greet;

impl AgentTool for Greet {
    fn name(&self) -> &str {
        "greet"
    }
    fn description(&self) -> &str {
        "Greet a person by name"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"name":{"type":"string","minLength":1}},"required":["name"],"additionalProperties":false})
    }
    fn execute(
        &self,
        _id: &str,
        args: &Value,
        signal: &CancellationToken,
        _update: &mut dyn FnMut(ToolResult),
    ) -> Result<ToolResult, String> {
        if signal.is_cancelled() {
            return Err("Operation aborted".into());
        }
        let name = args["name"].as_str().ok_or("Missing name")?;
        Ok(ToolResult::text(format!("Hello, {name}!")))
    }
}

impl ExtensionCommand for Greet {
    fn name(&self) -> &str {
        "hello"
    }
    fn execute(
        &self,
        args: &str,
        context: &ExtensionContext,
        _signal: &CancellationToken,
    ) -> Result<CommandResult, String> {
        let previous = context
            .states
            .get("native-greet")
            .and_then(|state| state["count"].as_u64())
            .unwrap_or(0);
        Ok(CommandResult {
            text: format!("Hello, {}!", if args.is_empty() { "world" } else { args }),
            state: json!({"count": previous + 1}),
            ..CommandResult::default()
        })
    }
}

struct GreetExtension;

impl Extension for GreetExtension {
    fn id(&self) -> &str {
        "native-greet"
    }
    fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        vec![Arc::new(Greet)]
    }
    fn commands(&self) -> Vec<Arc<dyn ExtensionCommand>> {
        vec![Arc::new(Greet)]
    }
}

fn main() -> Result<(), String> {
    let registry = SharedRegistry::default();
    registry.register(Arc::new(GreetExtension))?;
    let signal = CancellationToken::new();
    let result =
        registry.dispatch_command("hello", "Rust", &ExtensionContext::default(), &signal)?;
    println!("{}", result.text);
    let tools = registry.tools();
    let tool = &tools[0];
    let args = json!({"name":"Ada"});
    tool.validate_arguments(&args)?;
    let result = tool.execute("example", &args, &signal, &mut |_| {})?;
    if let Some(agent_core::types::Content::Text { text }) = result.content.first() {
        println!("{text}");
    }
    Ok(())
}
