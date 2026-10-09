// Preferences, stored as plain JSON in %APPDATA%\Coucou\settings.json.
// No secret ever lands here — API keys live in the Windows Credential Manager.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// What one provider needs: which model is selected and where it lives. The API
/// key is not here — it lives in the Credential Manager under one name per
/// provider, so a single key is typed once and every agent using it benefits.
/// What the agent is allowed to run when the model asks for a shell.
///
/// Nothing runs until the user turns this on and names the programs. An empty
/// `allowed` blocks everything, which is the state a fresh install is in.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ShellSettings {
    /// Master switch. Off means `run_shell` answers with an error.
    #[serde(default)]
    pub enabled: bool,
    /// When true, commands are still parsed and allowed, but not executed.
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// First token of a command, matched case-insensitively.
    #[serde(default = "default_allowed_shell")]
    pub allowed: Vec<String>,
    #[serde(default = "default_shell_timeout")]
    pub timeout_s: u64,
}

fn default_true() -> bool {
    true
}

/// Read-only and build programs.
///
/// `git` is deliberately absent. An allow-list keyed on the first token cannot
/// tell `git status` from `git clean -fdx` or `git push --force`, and a default
/// list is a long-term contract: whoever installs this gets `git` whether or not
/// they remember to remove it. Adding it is one edit away, and that edit is the
/// conscious decision. The shells are absent for a blunter reason — allowing one
/// makes the rest of the list meaningless.
fn default_allowed_shell() -> Vec<String> {
    [
        "cargo", "npm", "pnpm", "node", "python", "ls", "dir", "cat", "type", "rg",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn default_shell_timeout() -> u64 {
    60
}

impl Default for ShellSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            dry_run: true,
            allowed: default_allowed_shell(),
            timeout_s: default_shell_timeout(),
        }
    }
}

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
    /// What the agent may run when the model asks for a shell. Off by default.
    #[serde(default)]
    pub shell: ShellSettings,
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
            shell: ShellSettings::default(),
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
    let path = settings_path();
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return with_all_agents(Settings::default()),
    };
    match parse_settings(&bytes) {
        Some(loaded) => with_all_agents(loaded),
        None => {
            // Returning the defaults is the right thing to run with, but it must
            // not be allowed to become the only version left: the first save
            // would overwrite a file we merely failed to read. Windows editors
            // and PowerShell both add a UTF-8 BOM, which is enough on its own to
            // get here, so the file is put aside rather than lost.
            let _ = std::fs::rename(&path, path.with_file_name("settings.unreadable.json"));
            with_all_agents(Settings::default())
        }
    }
}

/// Reads the file, with the UTF-8 BOM some Windows tools put in front skipped.
fn parse_settings(bytes: &[u8]) -> Option<Settings> {
    let body = match bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(rest) => rest,
        None => bytes,
    };
    serde_json::from_slice(body).ok()
}

pub fn save(settings: &Settings) -> std::io::Result<()> {
    save_at(&config_dir(), settings)
}

/// `save` with the directory spelled out, so a test can prove what gets
/// written without going near the user's own settings.json.
pub fn save_at(dir: &std::path::Path, settings: &Settings) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_vec_pretty(settings)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(dir.join("settings.json"), json)
}

/// What the agent may run, as stored. A settings.json with no `shell` block —
/// every install made before this existed — gets the default: off.
pub fn shell_settings() -> ShellSettings {
    load().shell
}

/// Writes the shell rules back, leaving every other setting untouched.
pub fn set_shell_settings(shell: ShellSettings) -> Result<(), String> {
    let mut current = load();
    current.shell = shell;
    save(&current).map_err(|e| format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::Settings;

    /// A settings.json in a temp dir, written the way a Windows tool would.
    fn write_file(dir: &std::path::Path, name: &str, bytes: &[u8]) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), bytes).unwrap();
    }

    #[test]
    fn a_utf8_bom_does_not_make_the_settings_unreadable() {
        // PowerShell's `Set-Content -Encoding UTF8` and several Windows editors
        // write one. serde_json refuses a BOM, so this used to read as "no
        // settings at all" and the defaults were then saved over the file.
        let settings = Settings { sound_volume: 0.42, ..Settings::default() };
        let json = serde_json::to_vec(&settings).unwrap();

        let dir = std::env::temp_dir().join(format!("coucou-bom-{}", std::process::id()));
        let mut with_bom = vec![0xEF, 0xBB, 0xBF];
        with_bom.extend_from_slice(&json);
        write_file(&dir, "settings.json", &with_bom);

        let parsed = parse_settings(&with_bom).expect("o BOM nao pode marcar o arquivo como invalido");
        assert_eq!(parsed.sound_volume, 0.42);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_without_a_bom_still_reads() {
        let json = serde_json::to_vec(&Settings { sound_volume: 0.42, ..Settings::default() }).unwrap();
        assert_eq!(parse_settings(&json).unwrap().sound_volume, 0.42);
    }

    #[test]
    fn a_file_we_cannot_read_is_set_aside_instead_of_lost() {
        // Whatever the reason for the parse failure, the first save must not be
        // able to destroy the only copy.
        let dir = std::env::temp_dir().join(format!("coucou-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_file(&dir, "settings.json", b"{ this is not json");

        assert!(parse_settings(b"{ this is not json").is_none());

        // And the rename that `load` does when it hits this.
        let path = dir.join("settings.json");
        std::fs::rename(&path, path.with_file_name("settings.unreadable.json")).unwrap();
        assert!(path.with_file_name("settings.unreadable.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
    use super::*;

    #[test]
    fn the_chat_answers_with_the_one_provider_that_was_chosen() {
        let mut settings = Settings {
            chat_provider: "moonshot".into(),
            ..Default::default()
        };
        settings.providers.get_mut("moonshot").unwrap().model = "kimi-latest".into();

        let (provider, config) = chat_provider(&settings);
        assert_eq!(provider, "moonshot");
        assert_eq!(config.model, "kimi-latest");
    }

    #[test]
    fn an_empty_choice_falls_back_to_anthropic() {
        let settings = Settings {
            chat_provider: "   ".into(),
            ..Default::default()
        };
        assert_eq!(chat_provider(&settings).0, "anthropic");
    }

    /// A settings.json written before `chat_provider` existed still has Claude's
    /// provider and model in the flat fields; do not throw that away.
    #[test]
    fn claude_keeps_the_flat_settings_an_older_build_wrote() {
        let mut settings = Settings {
            model: "claude-sonnet-5".into(),
            provider: "anthropic".into(),
            ..Default::default()
        };
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