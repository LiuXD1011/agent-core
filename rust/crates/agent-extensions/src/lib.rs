//! Native extensions use Rust traits, not a JavaScript or Node compatibility runtime.
//! The registry snapshots registration before calling user code and never holds its lock
//! through a callback. Ordinary event failures are diagnostic; tool preflight is fail-closed.
//!
//! # Host integration
//!
//! Pass a cloned [`SharedRegistry`] to a native runtime SDK. Rebuild [`ExtensionContext`]
//! from the active native session before each callback. Fill `states[id]` from that
//! branch's `runtime_state("extension:{id}")`; never copy state from another branch.
//! [`CommandResult::text`] is presentation text, not a synthesized assistant message.
//! Apply validated User/Custom messages and non-null state to a candidate session, then
//! save atomically. Use [`SharedRegistry::resolve_command`] to obtain the originating ID.
//! A returned session name normalizes CR/LF and trims; an empty name explicitly clears it.
//! Presentation requests are typed notifications/status updates. Blocking select,
//! confirm, input and editor callbacks use [`UiBroker`] and the current context's
//! [`UiHandle`]; a frontend polls requests and sends typed [`UiReply`] responses.
//! Bind an enabled broker only after the reader/UI loop runs. Cancellation, timeout,
//! host close and stale registration/session scopes settle pending dialogs.
//!
//! Hosts route registered commands before [`SharedRegistry::emit_input`]. Input hooks
//! then run once on raw incoming text/images, before templates, skills and queue delivery.
//! [`InputResult::Handled`] stops the chain and creates no transcript or provider request.
//! Transforms are ordered; omitted/null images preserve attachments, while an empty
//! image vector explicitly clears them. Failed transforms are diagnostic and leave the
//! preceding value intact. Source and streaming behavior remain fixed through the chain.
//! Native incoming/transformed input is bounded to 1MiB of text, 16 images and
//! 64MiB of image payload. Explicit process transport frame limits remain separate.
//!
//! `before_agent_start` runs once per submitted prompt before the core loop. Chain its
//! system prompt for that run only; append its Custom messages without emitting the user
//! prompt twice. At each request, perform the registry's nonpersistent `context` transform
//! before custom-to-LLM conversion. [`custom_message_content`] implements the observed
//! custom `data.content` string/text-image array shape. Display=false still enters LLM
//! context; a Custom message without content is UI/state-only.
//!
//! Use [`SharedRegistry::tool_hooks`] for built-in and extension tools alike. Core validates
//! and normalizes arguments before the before-tool chain. Mutations then affect execution
//! without revalidation. A block short-circuits; an error/panic blocks the tool. After-tool
//! patches see previous content/details/usage/error/terminate changes. After-tool failures
//! are diagnostic and preserve completed output. Core handles execution lifecycle and
//! ordered tool-call/result pairing. Emit [`LifecycleEvent::Agent`] for actual core events
//! and [`LifecycleEvent::AgentSettled`] only after retry, compaction and queues finish.
//! Terminal cleanup events run even when the supplied cancellation token is already set.
//! Emit SessionShutdown before unregistering extensions or replacing a session runtime.
//! Initialize replacement candidates with [`SharedRegistry::emit_event_checked`] before
//! closing the old runtime; best-effort observation is not proof of successful startup.
//! [`LifecycleEvent::ModelSelect`] is a post-commit notification: validate and commit
//! the selected model first, then use best-effort [`SharedRegistry::emit_event`].
//! Ordinary callback failures are diagnostic and do not veto the selection. Cancellation
//! may stop notification delivery, but must not roll back an already committed model.
//! Hosts suppress notifications when provider/model identities are unchanged. A restore
//! notification can carry no previous identity when initializing a replacement runtime.
//!
//! # Explicit native differences
//!
//! No TypeScript/Node loader, dynamic-library ABI, WASM runtime or package manager is
//! provided. A native [`Extension`] registers compile-time Rust trait objects; a host may
//! adapt an explicit process protocol to the same traits. Custom TUI
//! components, renderers, shortcuts, provider registration and transport/header mutation
//! are not claimed by this minimal API. Advanced JSON-schema references/patterns/formats
//! are rejected during registration, rather than silently skipped. Supported structural
//! keywords are the core's types, objects/arrays, unions, bounds, enums and constants.
//!
//! Tool names across extensions preserve the TS first-registration-wins order; a later
//! winner becomes visible after unregistering the first. Built-in tool replacement is
//! permitted, so a host must merge extension tools first and report overrides. Duplicate
//! commands use the original runner's ordered `name:1`, `name:2` invocation resolution.
//! Duplicate names within one native extension and duplicate extension IDs are rejected
//! transactionally instead of silently overwriting a mutable JS map. Built-in command
//! names are reserved; hosts can reserve additional tool/command names before loading.
//! Captured tool wrappers become stale after unregister/reload. Callback cancellation is
//! cooperative; a native callback that ignores its token is not forcefully interrupted.
//! Diagnostics are bounded to the latest 1024 entries and drained through the shared handle.

mod registry;
mod schema;
mod types;
mod ui;

pub use registry::{RegistryToolHooks, SharedRegistry};
pub use schema::validate_tool_schema;
pub use types::*;
pub use ui::{UiBroker, UiDialog, UiDialogRequest, UiHandle, UiReply};
