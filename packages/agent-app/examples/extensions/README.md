# Extension Examples

The repository keeps a minimal tool example and examples required by regression tests.
The extension APIs remain available for custom integrations.

## Usage

```bash
# Run from packages/agent-app
agent-core --extension examples/extensions/hello.ts
```

## Included examples

| Example | Purpose |
|---|---|
| [hello.ts](hello.ts) | Minimal custom tool |
| [custom-compaction.ts](custom-compaction.ts) | Custom compaction hook; regression fixture |
| [git-merge-and-resolve.ts](git-merge-and-resolve.ts) | Git merge workflow; regression fixture |
| [input-transform-streaming.ts](input-transform-streaming.ts) | Streaming-aware input handling; regression fixture |
| [plan-mode/](plan-mode/) | Read-only planning and step tracking; regression fixture |
| [subagent/](subagent/) | Specialized subagents and project-trust regression fixture |
| [trigger-compact.ts](trigger-compact.ts) | Explicit compaction hook; regression fixture |
| [with-deps/](with-deps/) | Extension-local dependencies; discovery regression fixture |

See the [extension API reference](../../docs/extensions.md) for events, tools, UI, providers, and lifecycle behavior.
