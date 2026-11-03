use std::sync::Arc;

use client::{Client, UserStore};
use gpui::{Entity, IntoElement, ParentElement};
use language_model::{LanguageModelRegistry, ZED_CLOUD_PROVIDER_ID};
use ui::prelude::*;

use crate::{AgentPanelOnboardingCard, ApiKeysWithoutProviders};

/// Sands has no hosted models or plans, so onboarding only asks for a provider API key.
pub struct AgentPanelOnboarding {
    has_configured_providers: bool,
}

impl AgentPanelOnboarding {
    pub fn new(
        _user_store: Entity<UserStore>,
        _client: Arc<Client>,
        _continue_with_zed_ai: impl Fn(&mut Window, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.subscribe(
            &LanguageModelRegistry::global(cx),
            |this: &mut Self, _registry, event: &language_model::Event, cx| match event {
                language_model::Event::ProviderStateChanged(_)
                | language_model::Event::AddedProvider(_)
                | language_model::Event::RemovedProvider(_)
                | language_model::Event::ProvidersChanged => {
                    this.has_configured_providers = Self::has_configured_providers(cx);
                    cx.notify();
                }
                _ => {}
            },
        )
        .detach();

        Self {
            has_configured_providers: Self::has_configured_providers(cx),
        }
    }

    fn has_configured_providers(cx: &App) -> bool {
        LanguageModelRegistry::read_global(cx)
            .visible_providers()
            .iter()
            .any(|provider| provider.is_authenticated(cx) && provider.id() != ZED_CLOUD_PROVIDER_ID)
    }
}

impl Render for AgentPanelOnboarding {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        AgentPanelOnboardingCard::new().when(!self.has_configured_providers, |this| {
            this.child(ApiKeysWithoutProviders::new())
        })
    }
}
