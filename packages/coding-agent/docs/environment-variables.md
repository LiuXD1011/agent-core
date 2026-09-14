# Environment Variables

Agent Core uses environment variables in three ways:

- Variables such as `AGENT_CORE_OFFLINE` configure the Agent Core process.
- Agent Core sets process markers so child processes can identify Agent Core as the launching agent.
- Commands run by the LLM-callable shell tools receive `AGENT_CORE_*` variables describing the current session.

Provider API-key variables are documented separately in [Models and Providers](models.md#api-keys).

## Process Marker

The CLI and RPC entry points set two process markers:

- `AI_AGENT=agent-core` is a generic marker that lets tooling identify Agent Core as the agent that launched the process.
- `AGENT_CORE_CODING_AGENT=true` lets child processes detect that they run inside Agent Core.

Child processes inherit both markers. They are not session-specific and are not set automatically when Agent Core is embedded through the SDK.

## Shell Tool Session Environment

Commands run by the `bash` and `powershell` tools receive the current Agent Core session state:

| Variable | Description |
|----------|-------------|
| `AGENT_CORE_SESSION_ID` | Current session ID |
| `AGENT_CORE_SESSION_FILE` | Absolute path to the current session JSONL file; unset for ephemeral sessions |
| `AGENT_CORE_PROVIDER` | Currently selected model provider |
| `AGENT_CORE_MODEL` | Currently selected model ID |
| `AGENT_CORE_REASONING_LEVEL` | Current effective reasoning level: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `max` |

The values are resolved when each command starts. Switching models or changing the reasoning level therefore affects the next shell command without restarting Agent Core. `AGENT_CORE_PROVIDER` and `AGENT_CORE_MODEL` identify the selected Agent Core model, not a different upstream model that a router may choose internally.

When asked which model or provider is running, inspect these variables instead of inferring the answer from the system prompt:

```bash
printf '%s/%s\n' "$AGENT_CORE_PROVIDER" "$AGENT_CORE_MODEL"
printf 'reasoning=%s session=%s\n' "$AGENT_CORE_REASONING_LEVEL" "$AGENT_CORE_SESSION_ID"
```

The session file can be inspected directly when the session is persistent:

```bash
if [ -n "$AGENT_CORE_SESSION_FILE" ]; then
  tail -n 1 "$AGENT_CORE_SESSION_FILE"
fi
```

These variables are injected into the LLM-callable `bash` and `powershell` tools. They are not injected into user-entered `!` or `!!` commands.

### Custom Shell Tools

Tools created with `createBashTool()` or `createPowerShellTool()` expose the session environment by default when registered with Agent Core. Injection happens before `spawnHook`, so a hook receives the variables in `ctx.env`:

```typescript
const bashTool = createBashTool(cwd, {
  spawnHook: (ctx) => ({
    ...ctx,
    env: { ...ctx.env, CI: "1" },
  }),
});
```

Disable session metadata independently of the spawn hook:

```typescript
const powershellTool = createPowerShellTool(cwd, {
  exposeSessionEnvironment: false,
  spawnHook: (ctx) => ctx,
});
```

When disabled, Agent Core removes inherited values for these variables so nested Agent Core processes do not expose stale parent-session metadata.

## Agent Core Process Configuration

These variables are read by Agent Core itself:

| Variable | Description |
|----------|-------------|
| `AGENT_CORE_CODING_AGENT_DIR` | Override the config directory; default is `~/.agent-core/agent` |
| `AGENT_CORE_CODING_AGENT_SESSION_DIR` | Override session storage; overridden by `--session-dir` |
| `AGENT_CORE_PACKAGE_DIR` | Override the package directory, useful for Nix/Guix store paths |
| `AGENT_CORE_OFFLINE` | Disable all startup network operations, including the optional version check, package update checks, and model catalog refreshes |
| `AGENT_CORE_VERSION_CHECK_URL` | Opt in to a version check feed returning `{ "packageName": "...", "version": "..." }`; unset by default, which means no version request is made |
| `AGENT_CORE_SKIP_VERSION_CHECK` | Skip the version check for this run, even when `AGENT_CORE_VERSION_CHECK_URL` is configured |
| `AGENT_CORE_TELEMETRY` | Controls optional provider attribution headers (OpenRouter, Cloudflare, NVIDIA NIM): `1`/`true`/`yes` or `0`/`false`/`no`. Install/update telemetry is disabled in Agent Core and sends nothing |
| `AGENT_CORE_CACHE_RETENTION` | Set to `long` for extended provider prompt caching where supported |
| `AGENT_CORE_HARDWARE_CURSOR` | Set to `1` to show the hardware cursor; see [Terminal setup](terminal-setup.md) |
| `AGENT_CORE_HYPERLINKS` | Override OSC 8 hyperlink detection with `1`, `0`, or `auto` |
| `AGENT_CORE_IMAGE_PROTOCOL` | Override inline image detection with `kitty`, `iterm2`, `none`, or `auto` |
| `AGENT_CORE_TRUE_COLOR` | Override truecolor detection with `1`, `0`, or `auto` |
| `AGENT_CORE_TUI_ESC_TIMEOUT` | How long to wait after a lone ESC before treating it as Escape, in milliseconds; defaults to `100` over SSH and `10` otherwise. Increase if Alt-key input is misread as Escape |
| `VISUAL`, `EDITOR` | External editor fallback when `externalEditor` is unset |
| `HTTP_PROXY`, `HTTPS_PROXY` | Proxy outbound HTTP requests |

Provider credentials such as `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, and cloud-provider configuration are listed in [Models and Providers](models.md#api-keys).
