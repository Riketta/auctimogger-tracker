//! Discord webhook alert delivery (`AlertDispatchPort`): posts one chat
//! message per fired alert. The webhook URL is read from the configured
//! environment variable on every dispatch, so rotated URLs apply without a
//! restart.

use amtracker_core::{
    domain::AlertEvent,
    error::AlertDispatchError,
    ports::AlertDispatchPort,
};

/// Discord webhook dispatcher.
pub struct DiscordAlertDispatcher {
    agent: ureq::Agent,
    /// Environment variable holding the webhook URL.
    webhook_env: String,
}

impl DiscordAlertDispatcher {
    /// Builds the dispatcher; the webhook URL is read from the given
    /// environment variable on every dispatch.
    #[must_use]
    pub fn new(webhook_env: impl Into<String>) -> Self {
        Self { agent: ureq::Agent::new(), webhook_env: webhook_env.into() }
    }

    /// The message body for one event: plain text, target-tagged.
    fn text(event: &AlertEvent) -> String {
        format!("[{}] {}", event.target.as_str(), event.message)
    }
}

impl AlertDispatchPort for DiscordAlertDispatcher {
    fn dispatch(&self, event: &AlertEvent) -> Result<(), AlertDispatchError> {
        let url = std::env::var(&self.webhook_env)
            .map_err(|_| {
                AlertDispatchError::Transport(format!(
                    "environment variable {} is unset",
                    self.webhook_env
                ))
            })?
            .trim()
            .to_string();
        if url.is_empty() {
            return Err(AlertDispatchError::Transport(format!(
                "environment variable {} is empty",
                self.webhook_env
            )));
        }
        let body = serde_json::json!({ "content": format!("🔔 {}", Self::text(event)) });
        // ureq 2 turns non-2xx answers into `Error::Status`, so `Ok` is the
        // delivered case.
        match self.agent.post(&url).send_json(body) {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(status, _)) => Err(AlertDispatchError::Status(status)),
            Err(error) => Err(AlertDispatchError::Transport(error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use amtracker_core::domain::{
        AlertCondition,
        ItemRef,
        TargetId,
    };

    use super::*;

    fn event() -> AlertEvent {
        AlertEvent {
            id: 1,
            rule_id: 7,
            target: TargetId("us-4".into()),
            item: ItemRef::Item { item_id: 2589 },
            condition: AlertCondition::PriceBelow { copper: 10_000 },
            observed: Some(9_000.0),
            message: "item 2589: d1 median 9000 copper (target ≤ 10000)".into(),
            fired_at: 1_000,
        }
    }

    #[test]
    fn missing_webhook_env_is_a_transport_error() {
        let dispatcher = DiscordAlertDispatcher::new("AMT_DEFINITELY_UNSET_WEBHOOK_ENV");
        let error = dispatcher.dispatch(&event()).expect_err("unset env must fail");
        assert!(matches!(error, AlertDispatchError::Transport(_)));
    }

    #[test]
    fn message_is_target_tagged_plain_text() {
        let text = DiscordAlertDispatcher::text(&event());
        assert!(text.starts_with("[us-4] "), "target tag first: {text}");
        assert!(text.contains("d1 median 9000"), "observed detail present: {text}");
    }
}
