//! Telegram bot-API alert delivery (`AlertDispatchPort`): sends one message
//! per fired alert to the configured chat. The bot token is read from the
//! configured environment variable on every dispatch, so rotated tokens apply
//! without a restart.

use amtracker_core::{
    domain::AlertEvent,
    error::AlertDispatchError,
    ports::AlertDispatchPort,
};

/// Telegram bot-API dispatcher.
pub struct TelegramAlertDispatcher {
    agent: ureq::Agent,
    /// Environment variable holding the bot token.
    token_env: String,
    /// Environment variable holding the target chat id.
    chat_id_env: String,
}

impl TelegramAlertDispatcher {
    /// Builds the dispatcher; token and chat id are read from the given
    /// environment variables on every dispatch.
    #[must_use]
    pub fn new(token_env: impl Into<String>, chat_id_env: impl Into<String>) -> Self {
        Self {
            agent: ureq::Agent::new(),
            token_env: token_env.into(),
            chat_id_env: chat_id_env.into(),
        }
    }

    fn env_value(name: &str) -> Result<String, AlertDispatchError> {
        let value = std::env::var(name)
            .map_err(|_| {
                AlertDispatchError::Transport(format!("environment variable {name} is unset"))
            })?
            .trim()
            .to_string();
        if value.is_empty() {
            return Err(AlertDispatchError::Transport(format!(
                "environment variable {name} is empty"
            )));
        }
        Ok(value)
    }

    /// The message body for one event: plain text, target-tagged.
    fn text(event: &AlertEvent) -> String {
        format!("\u{1f514} [{}] {}", event.target.as_str(), event.message)
    }
}

impl AlertDispatchPort for TelegramAlertDispatcher {
    fn dispatch(&self, event: &AlertEvent) -> Result<(), AlertDispatchError> {
        let token = Self::env_value(&self.token_env)?;
        let chat_id = Self::env_value(&self.chat_id_env)?;
        let url = format!("https://api.telegram.org/bot{token}/sendMessage");
        let body = serde_json::json!({
            "chat_id": chat_id,
            "text": Self::text(event),
            // Plain text keeps item names verbatim; no parse mode surprises.
            "disable_web_page_preview": true,
        });
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
    fn missing_envs_are_transport_errors() {
        let dispatcher =
            TelegramAlertDispatcher::new("AMT_DEFINITELY_UNSET_TOKEN", "AMT_DEFINITELY_UNSET_CHAT");
        let error = dispatcher.dispatch(&event()).expect_err("unset envs must fail");
        assert!(matches!(error, AlertDispatchError::Transport(_)));
    }

    #[test]
    fn message_is_target_tagged_plain_text() {
        let text = TelegramAlertDispatcher::text(&event());
        assert!(text.contains("[us-4] "), "target tag present: {text}");
        assert!(text.contains("d1 median 9000"), "observed detail present: {text}");
    }
}
