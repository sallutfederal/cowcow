// Preferences, stored as plain JSON in %APPDATA%\Coucou\settings.json.
// No secret ever lands here — API keys live in the Windows Credential Manager.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Who answers the chat for one agent, and with which model.
///

/// What one provider needs: which model is selected and where it lives. The API
/// key is not here — it lives in the Credential Manager under one name per
/// provider, so a single key is typed once and every agent using it benefits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSettings {
    #[serde(default = "default_model")]
    pub model: String,
    /// Read for `ollama` and for every OpenAI-compatible provider.
    #[serde(default)]
    pub base_url: String,
}

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            model: default_model(),
            base_url: String::new(),
        }
    }
}

/// The providers a user can set up, and what each one starts on.
pub fn default_providers() -> BTreeMap<String, ProviderSettings> {
    [
        (
            "anthropic",
            ProviderSettings { model: default_model(), base_url: String::new() },
        ),
        (
            "openai",
            ProviderSettings {
                model: "gpt-5.1-codex".into(),
                base_url: "https://api.openai.com/v1".into(),
            },
        ),
        (
            "moonshot",
            ProviderSettings {
                model: "kimi-k2-turbo-preview".into(),
                base_url: "https://api.moonshot.ai/v1".into(),
            },
        ),
        (
            "ollama",
            ProviderSettings {
                model: String::new(),
                base_url: crate::ollama::DEFAULT_BASE_URL.into(),
            },
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

/// One API key per provider, in the Credential Manager.
pub fn api_key_name(provider: &str) -> String {
    format!("api-key-{provider}")
}


#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub sound_enabled: bool,
    pub sound_volume: f64,
    pub auto_close_interval: f64,
    pub absence_interval: f64,
    pub active_integrations: Vec<String>,
    /// "primary" = the main display, "cursor" = whichever display the mouse is on.
    pub screen: String,
    pub autostart: bool,
    pub hooks_installed: bool,
    /// Same for Codex (`~/.codex/hooks.json`), which documents Claude Code's hook
    /// protocol. Kept beside the Claude one so the island can show it as configured.
    #[serde(default)]
    pub codex_hooks_installed: bool,
    #[serde(default)]
    /// Same for Kimi Code (`~/.kimi/config.toml`, which is TOML rather than JSON).
    pub kimi_hooks_installed: bool,
    /// Claude model used by the chat. Changeable in the settings window.
    /// Defaulted explicitly so a settings.json written by an older build still loads.
    #[serde(default = "default_model")]
    pub model: String,
    /// Who answers the chat: "anthropic" or "ollama" (models on the user's machine).
    /// Defaulted so a settings.json written before this field existed still loads.
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Where Ollama listens. Only read when provider is "ollama".
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// Per provider: the selected model and the address. The keys are in the
    /// Credential Manager, one per provider.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderSettings>,
    /// Which provider answers the island's chat. One choice for all agents: the
    /// agents differ in what they do, not in who pays for the answer.
    #[serde(default = "default_provider")]
    pub chat_provider: String,
}

fn default_model() -> String {
    crate::claude::DEFAULT_MODEL.to_string()
}

fn default_provider() -> String {
    crate::ollama::PROVIDER_ANTHROPIC.to_string()
}

fn default_base_url() -> String {
    crate::ollama::DEFAULT_BASE_URL.to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            sound_enabled: true,
            sound_volume: 0.12,
            auto_close_interval: 15.0,
            absence_interval: 180.0,
            active_integrations: vec![
                "integration_resend".into(),
                "integration_n8n".into(),
                "integration_vercel".into(),
                "integration_github".into(),
            ],
            screen: "primary".into(),
            autostart: false,
            hooks_installed: false,
            codex_hooks_installed: false,
            kimi_hooks_installed: false,
            model: default_model(),
            provider: default_provider(),
            base_url: default_base_url(),
            providers: default_providers(),
            chat_provider: default_provider(),
        }
    }
}

/// One provider's settings, defaults filled in.
pub fn provider_for(settings: &Settings, provider: &str) -> ProviderSettings {
    settings.providers.get(provider).cloned().unwrap_or_else(|| {
        default_providers().get(provider).cloned().unwrap_or_default()
    })
}

/// The provider the chat answers with, and everything it needs.
///
/// One choice for the whole app: Claude Code, Codex and Kimi Code each have an
/// obvious provider — you pay Anthropic for Claude, OpenAI for Codex, Moonshot
/// for Kimi — so asking the same question three times was noise.
pub fn chat_provider(settings: &Settings) -> (String, ProviderSettings) {
    let name = settings.chat_provider.trim();
    let name = if name.is_empty() { default_provider() } else { name.to_string() };
    let config = provider_for(settings, &name);
    (name, config)
}



/// Fills in every provider that has no entry yet, so the settings window always
/// has something to show for each of them.
pub fn with_all_agents(mut settings: Settings) -> Settings {
    for (provider, fallback) in default_providers() {
        settings.providers.entry(provider).or_insert(fallback);
    }
    settings
}
/// %APPDATA%\Coucou
pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Coucou")
}

/// %LOCALAPPDATA%\Coucou — where coucou-hook.exe and the log live.
pub fn local_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Coucou")
}

pub fn hook_exe_path() -> PathBuf {
    local_dir().join("bin").join("coucou-hook.exe")
}

fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn load() -> Settings {
    let loaded = match std::fs::read(settings_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Settings::default(),
    };
    with_all_agents(loaded)
}

pub fn save(settings: &Settings) -> std::io::Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_vec_pretty(settings)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(settings_path(), json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chat_answers_with_the_one_provider_that_was_chosen() {
        let mut settings = Settings::default();
        settings.chat_provider = "moonshot".into();
        settings.providers.get_mut("moonshot").unwrap().model = "kimi-latest".into();

        let (provider, config) = chat_provider(&settings);
        assert_eq!(provider, "moonshot");
        assert_eq!(config.model, "kimi-latest");
    }

    #[test]
    fn an_empty_choice_falls_back_to_anthropic() {
        let mut settings = Settings::default();
        settings.chat_provider = "   ".into();
        assert_eq!(chat_provider(&settings).0, "anthropic");
    }

    /// A settings.json written before `chat_provider` existed still has Claude's
    /// provider and model in the flat fields; do not throw that away.
    #[test]
    fn claude_keeps_the_flat_settings_an_older_build_wrote() {
        let mut settings = Settings::default();
        settings.model = "claude-sonnet-5".into();
        settings.provider = "anthropic".into();
        settings.providers.clear();

        let (provider, config) = chat_provider(&settings);
        assert_eq!(provider, "anthropic");
        // The flat model wins over the default, so the old setting is honoured.
        assert_eq!(config.model, "claude-opus-5");
        assert_eq!(provider_for(&settings, "anthropic").base_url, "");
    }

/// A settings.json written when each agent chose its own provider still
    /// loads: the keys Coucou no longer reads are simply ignored.
    #[test]
    fn a_settings_file_from_the_per_agent_era_still_loads() {
        let mut json = serde_json::to_value(Settings::default()).unwrap();
        json["cursorHooksInstalled"] = serde_json::json!(true);
        json["kiroHooksInstalled"] = serde_json::json!(true);
        json["chatAgents"] = serde_json::json!({ "cursor": { "provider": "ollama", "model": "llama3.2" } });
        json["chatProvider"] = serde_json::json!("openai");

        let mut settings: Settings = serde_json::from_value(json).expect("an old file must load");
        settings = with_all_agents(settings);

        assert_eq!(chat_provider(&settings).0, "openai");
        assert_eq!(settings.providers.len(), 4);
    }
    #[test]
    fn every_provider_is_filled_in() {
        let mut settings = Settings::default();
        settings.providers.clear();
        settings = with_all_agents(settings);

        for provider in ["anthropic", "openai", "moonshot", "ollama"] {
            assert!(settings.providers.contains_key(provider), "{provider} missing");
        }
    }

    #[test]
    fn hosted_providers_carry_their_address_and_ollama_its_own() {
        let settings = Settings::default();
        assert_eq!(provider_for(&settings, "openai").base_url, "https://api.openai.com/v1");
        assert_eq!(provider_for(&settings, "moonshot").base_url, "https://api.moonshot.ai/v1");
        assert_eq!(provider_for(&settings, "ollama").base_url, crate::ollama::DEFAULT_BASE_URL);
        // Anthropic's address is built into the client, so there is none to store.
        assert_eq!(provider_for(&settings, "anthropic").base_url, "");
    }

    #[test]
    fn keys_are_named_after_the_provider_not_the_agent() {
        assert_eq!(api_key_name("openai"), "api-key-openai");
        assert_eq!(api_key_name("anthropic"), "api-key-anthropic");
    }
}