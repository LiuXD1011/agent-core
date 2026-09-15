# Built-in efficiency mechanisms

Agent Core integrates the four MIT-licensed SoL-Pi mechanisms into its default resource loader. No external package installation is needed. Interactive, print, JSON and stdio RPC modes use this loader, as does the default SDK session. A completely custom ResourceLoader controls its own integrations.

## Configuration

The trusted project's `.agent-core/sol-pi.json` replaces the global `<agentDir>/sol-pi.json`. Untrusted project configuration is ignored. No file means all four flags are false. Unknown fields and invalid values are errors. `--no-extensions` disables automatic efficiency loading. Restart or `/reload` after editing configuration.

```json
{
  "version": 1,
  "actionFusion": true,
  "observationPack": true,
  "evidencePreservingReducer": true,
  "evidencePreservingReducerProvider": "$current",
  "evidencePreservingReducerModel": "$current",
  "onlineContextCompact": true,
  "cacheWriteReadRatio": 12.5
}
```

Log reduction is enabled in this project with explicit user authorization to process diagnostic content using the current session model. Both route fields must use `$current` to select the active session model on each reduction, or name an explicit configured provider/model pair. ModelRuntime owns authentication and endpoints. Reduction adds a model call and does not guarantee net cost savings. Do not enable it for logs that must remain local. UI byte/token savings are not monetary savings.

## Execution behavior

- **Action Fusion** adds optional `then_run: { command, timeout? }` to active built-in edit/write tools. Mutation completes before validation. Failure preserves the edit and reports an error. Cancellation or a detected intervening change skips validation. The host shell path/prefix is inherited. Allowlists, denylists, custom tools and interception policies retain precedence; these policies use separate calls rather than a hidden shell. A late interceptor blocks fused execution too.
- **ObservationPack** archives successful pure-text results over 10 KiB. The first two projections send the full result; later projections use a stable handle. Session history stays unchanged. `obs_recall` returns exact UTF-8 pages with `next_offset`. Errors, images and reducer receipts are not packed. Missing storage or disabled recall preserves originals.
- **Evidence-Preserving Reducer** processes eligible build/test/check logs of at least 4 KiB. It archives the source, invokes ModelRegistry.complete, and verifies source hash, observed status and every quote. Invalid receipts, unavailable models, likely secrets or failures preserve the original result. Verified quotes do not prove completeness; original logs remain authoritative and the agent owns diagnosis and final adjudication. Source-file reads are not delegated.
- **Online Context Compact** exposes `update_plan`. Completed steps can trigger economic/window-pressure checks using the actual retained-tail setting. The existing native compaction runs and successful compaction continues the parent task. The original prompt waits for continuation settlement. Cancellation/failed compaction schedules no continuation. Native threshold compaction remains separate; set `onlineContextCompact` false to disable this boundary policy.

Archives remain at `<sessionDir>/sol-pi/<sessionId>/` after exit. Resume in the same directory preserves recall. Do not remove archives for sessions still in use. No automatic retention policy is added.

## Disable

Set the four flags to false and reload for ordinary execution and context projection. Existing archives and session records remain. This does not undo file edits. No dependency migration is involved.

## Upstream attribution and license

Adapted from https://github.com/NVlabs/SoL-Pi at commit `2b791687a489a1d24da816cf1634d8ae1d36befd`.
Adaptations include native imports, direct ModelRegistry completion, trusted automatic loading, host shell settings, tool-policy guards and the real compaction tail. The fallback for older Pi releases is not included.

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
