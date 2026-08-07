use remtene_domain::{AsrEngine, AsrPreference};
use thiserror::Error;

use crate::ports::EngineHealth;

/// The semantic ASR choice frozen for one Session.
///
/// Concrete model packages, quantization and platform runtimes stay behind adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedAsrRoute {
    engine: AsrEngine,
}

impl ResolvedAsrRoute {
    #[must_use]
    pub(crate) const fn new(engine: AsrEngine) -> Self {
        Self { engine }
    }

    #[must_use]
    pub const fn engine(self) -> AsrEngine {
        self.engine
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AsrRouteError {
    #[error("no healthy local ASR engine matches the frozen preference")]
    NoHealthyEngine,
}

pub fn resolve_asr_route(
    preference: AsrPreference,
    qwen_health: EngineHealth,
    whisper_health: EngineHealth,
) -> Result<ResolvedAsrRoute, AsrRouteError> {
    let engine = match preference {
        AsrPreference::Qwen if qwen_health == EngineHealth::Healthy => AsrEngine::Qwen,
        AsrPreference::Whisper if whisper_health == EngineHealth::Healthy => AsrEngine::Whisper,
        AsrPreference::Qwen | AsrPreference::Whisper => {
            return Err(AsrRouteError::NoHealthyEngine);
        }
    };

    Ok(ResolvedAsrRoute::new(engine))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qwen_selection_uses_healthy_qwen() {
        assert_eq!(
            resolve_asr_route(
                AsrPreference::Qwen,
                EngineHealth::Healthy,
                EngineHealth::Healthy,
            ),
            Ok(ResolvedAsrRoute::new(AsrEngine::Qwen))
        );
    }

    #[test]
    fn qwen_selection_never_falls_back_to_whisper() {
        assert_eq!(
            resolve_asr_route(
                AsrPreference::Qwen,
                EngineHealth::Unhealthy,
                EngineHealth::Healthy,
            ),
            Err(AsrRouteError::NoHealthyEngine)
        );
    }

    #[test]
    fn whisper_selection_never_selects_qwen() {
        assert_eq!(
            resolve_asr_route(
                AsrPreference::Whisper,
                EngineHealth::Healthy,
                EngineHealth::Missing,
            ),
            Err(AsrRouteError::NoHealthyEngine)
        );
    }

    #[test]
    fn no_healthy_engine_rejects_route_resolution() {
        assert_eq!(
            resolve_asr_route(
                AsrPreference::Qwen,
                EngineHealth::Unhealthy,
                EngineHealth::Incompatible,
            ),
            Err(AsrRouteError::NoHealthyEngine)
        );
    }
}
