# Context and tool execution

These are ordinary Agent Core behaviors, not an extension pack or separate mode. A normal session needs no activation flags. `--no-extensions` disables third-party extensions, not the agent's context management. Explicit tool lists, exclusions and `--no-tools` still apply.

## A normal run

The loop sends the current model context, executes validated tools, records results, and requests the next response. Short tasks need no plan or compaction.

- `write` and `edit` may carry `then_run: { command, timeout? }`. The mutation completes first. A failed command keeps the edit and reports failure; a failed mutation never starts the command. Shell restrictions and custom tool policies take precedence.
- Large repeated successful text results are stored and later replaced only in the model view. `obs_recall` retrieves exact pages; disabled recall or an in-memory session leaves the original text visible.
- Eligible diagnostic logs can use the current session model to produce checked evidence. Source bytes remain authoritative. Invalid evidence, unavailable models, sensitive content or timeout preserve the original output. The auxiliary call consumes model tokens and may not save total cost.
- `update_plan` records progress when a task needs a plan. Completed boundaries and context pressure can request compaction within the same run. Successful compaction rebuilds the next request; cancellation does not secretly restart the agent.
- The application uses conservative scheduling: at most four explicitly parallel read operations, while unknown or effectful batches run sequentially. Tool results retain their model call order.

## Session evidence and export

Archives live in `<sessionDir>/context/<sessionId>/`. JSONL export carries archives by default; HTML embeds downloadable evidence. Export fails explicitly if a file exceeds 4 MiB or total archived bytes exceed 16 MiB, rather than silently producing an incomplete file. Import validates paths, sizes, encoding and checksums before restoring files. Model-visible diagnostic references are relocated; historical messages remain unchanged.

## Advanced configuration

Defaults are part of normal assembly. Optional overrides live in `<agentDir>/context.json` or a trusted project's `.agent-core/context.json`; a project file replaces the global file. Untrusted project overrides are ignored. Restart or `/reload` after a change. Internal policy overrides are intended for diagnosis and controlled experiments, not initial setup.

The former `efficiency.json` configuration and its old field names are retired. The current project's values have been migrated; the original configuration is backed up outside the repository. Existing independent evaluation runtime archives have not been replaced. This development migration does not promise automatic conversion of historical external archives.

## Third-party license

Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

Permission is hereby granted, free of charge, to any person obtaining a
copy of this software and associated documentation files (the "Software"),
to deal in the Software without restriction, including without limitation
the rights to use, copy, modify, merge, publish, distribute, sublicense,
and/or sell copies of the Software, and to permit persons to whom the
Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL
THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
