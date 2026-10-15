//! Explicit thinking selection is a metadata/configuration transaction. The
//! provider's pure capability resolver supplies the same clamp as its payload.
use crate::extensions::{
    extension_context, ExtensionMode, LifecycleEvent, ModelIdentity, SharedRegistry,
};
use crate::provider::ProviderSettings;
use crate::session::Session;
use agent_core::types::{CancellationToken, ThinkingLevel};

pub struct ThinkingSelection {
    pub session: Session,
    pub settings: ProviderSettings,
    pub level: ThinkingLevel,
    pub previous_level: ThinkingLevel,
}
pub fn prepare_thinking_selection(
    session: &Session,
    settings: &ProviderSettings,
    requested: ThinkingLevel,
    signal: &CancellationToken,
) -> Result<ThinkingSelection, String> {
    if signal.is_cancelled() {
        return Err("Thinking selection aborted before commit".into());
    }
    settings.validate_thinking_options()?;
    let level = settings.clamp_thinking_level(requested);
    let previous_level = settings.options.thinking_level.unwrap_or_default();
    let mut candidate = session.clone();
    candidate.append_thinking_level(level)?;
    let mut configured = settings.clone();
    configured.options.thinking_level = Some(level);
    Ok(ThinkingSelection {
        session: candidate,
        settings: configured,
        level,
        previous_level,
    })
}
pub fn notify_thinking_level_select(
    session: &Session,
    registry: &SharedRegistry,
    mode: ExtensionMode,
    model: Option<ModelIdentity>,
    level: ThinkingLevel,
    previous_level: ThinkingLevel,
    signal: &CancellationToken,
) -> Result<(), String> {
    if level == previous_level {
        return Ok(());
    }
    let mut context = extension_context(session, registry, mode, model)?;
    context.thinking_level = level;
    registry.emit_event(
        &LifecycleEvent::ThinkingLevelSelect {
            level,
            previous_level,
        },
        &context,
        signal,
    )
}
/// Restore only an observed branch entry. No entry leaves provider defaults alone.
pub fn restore_thinking_settings(
    session: &Session,
    settings: &mut ProviderSettings,
) -> Result<(), String> {
    settings.options.thinking_level = session
        .get_thinking_level()?
        .map(|level| settings.clamp_thinking_level(level));
    Ok(())
}
