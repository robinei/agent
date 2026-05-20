use std::path::{Path, PathBuf};

/// Full server configuration, loaded from `~/.agent/config.toml` and env vars.
#[derive(Clone, Debug)]
pub struct Config {
    pub server: ServerConfig,
    pub provider: ProviderConfig,
    pub summary: SummaryConfig,
    pub session: SessionConfig,
    pub logging: LoggingConfig,
    pub sandbox: SandboxConfig,
}

#[derive(Clone, Debug)]
pub struct SandboxConfig {
    pub enabled: bool,
    pub bwrap_path: Option<PathBuf>,
    pub defaults: SandboxDefaults,
}

#[derive(Clone, Debug, Default)]
pub struct SandboxDefaults {
    pub hide: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Clone, Debug)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// Send `thinking: {type: "enabled"}` in the request body. Required for
    /// DeepSeek and some other providers to emit reasoning content. Has no
    /// effect on models that don't support it.
    pub enable_thinking: bool,
    /// Value for the `reasoning_effort` field when `enable_thinking` is true.
    /// Maps to DeepSeek's reasoning_effort; typical values: "low", "medium", "high".
    /// Defaults to "medium".
    pub reasoning_effort: String,
    /// If set, sent as `max_tokens` in the request body. Leave unset to use
    /// the provider's default.
    pub max_tokens: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct SummaryConfig {
    pub model: String,
    pub base_url: String,
    pub api_key: String,
}

#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub soft_cap_pct: u8,
    pub hard_cap_pct: u8,
    pub max_tool_calls_per_turn: usize,
}

#[derive(Clone, Debug)]
pub struct LoggingConfig {
    pub level: String,
    pub to_file: Option<PathBuf>,
    pub to_stderr: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                host: "127.0.0.1".into(),
                port: 8080,
            },
            provider: ProviderConfig {
                base_url: "http://localhost:8080/v1".into(),
                api_key: String::new(),
                model: "qwen2.5-coder-7b-instruct".into(),
                enable_thinking: true,
                reasoning_effort: "medium".into(),
                max_tokens: None,
            },
            summary: SummaryConfig {
                model: "qwen2.5-coder-1.5b-instruct".into(),
                base_url: "http://localhost:8080/v1".into(),
                api_key: String::new(),
            },
            session: SessionConfig {
                soft_cap_pct: 65,
                hard_cap_pct: 85,
                max_tool_calls_per_turn: 25,
            },
            logging: LoggingConfig {
                level: "info".into(),
                to_file: Some("/tmp/agent-server.log".into()),
                to_stderr: true,
            },
            sandbox: SandboxConfig {
                enabled: true,
                bwrap_path: None,
                defaults: SandboxDefaults {
                    hide: vec![
                        PathBuf::from("~/.ssh"),
                        PathBuf::from("~/.aws"),
                        PathBuf::from("~/.azure"),
                        PathBuf::from("~/.config/gcloud"),
                        PathBuf::from("~/.config/heroku"),
                        PathBuf::from("~/.config/gh"),
                        PathBuf::from("~/.config/glab"),
                        PathBuf::from("~/.kube"),
                        PathBuf::from("~/.docker"),
                        PathBuf::from("~/.git-credentials"),
                        PathBuf::from("~/.netrc"),
                        PathBuf::from("~/.npmrc"),
                        PathBuf::from("~/.pypirc"),
                        PathBuf::from("~/.cargo/credentials.toml"),
                        PathBuf::from("~/.gnupg"),
                        PathBuf::from("~/.password-store"),
                        PathBuf::from("~/.local/share/keyrings"),
                        PathBuf::from("~/.config/keybase"),
                        PathBuf::from("~/.bash_history"),
                        PathBuf::from("~/.zsh_history"),
                        PathBuf::from("~/.local/share/fish/fish_history"),
                        PathBuf::from("~/.mozilla"),
                        PathBuf::from("~/.config/google-chrome"),
                        PathBuf::from("~/.config/chromium"),
                        PathBuf::from("~/.config/Slack"),
                        PathBuf::from("~/.config/discord"),
                    ],
                },
            },
        }
    }
}

/// Load config from a given path, falling back to defaults and applying
/// environment variable overrides.
pub fn load_config_from_path(config_path: &Path) -> Config {
    let mut cfg = Config::default();

    if let Ok(content) = std::fs::read_to_string(config_path) {
        if !content.trim().is_empty() {
            if let Ok(table) = content.parse::<toml::Table>() {
                apply_toml(&mut cfg, &table);
            }
        }
    }

    // Environment variable overrides
    if let Ok(val) = std::env::var("AGENT_SERVER_HOST") {
        cfg.server.host = val;
    }
    if let Ok(val) = std::env::var("AGENT_SERVER_PORT") {
        if let Ok(port) = val.parse::<u16>() {
            cfg.server.port = port;
        }
    }
    if let Ok(val) = std::env::var("LLM_API_KEY") {
        cfg.provider.api_key = val;
    }
    if let Ok(val) = std::env::var("AGENT_MODEL") {
        cfg.provider.model = val;
    }
    if let Ok(val) = std::env::var("AGENT_BASE_URL") {
        cfg.provider.base_url = val;
    }

    cfg
}

/// Load config from the default path (`~/.agent/config.toml`), falling back to
/// defaults and applying environment variable overrides.
pub fn load_config() -> Config {
    load_config_from_path(&agent_dir().join("config.toml"))
}

/// Return the agent data directory (`~/.agent`).
pub fn agent_dir() -> PathBuf {
    if let Ok(val) = std::env::var("AGENT_DIR") {
        return PathBuf::from(val);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".agent")
}

// ── TOML parsing via toml crate ──

fn apply_toml(cfg: &mut Config, table: &toml::Table) {
    use toml::Value;

    // [server]
    if let Some(Value::Table(section)) = table.get("server") {
        if let Some(v) = section.get("host").and_then(|v| v.as_str()) {
            cfg.server.host = v.to_string();
        }
        if let Some(v) = section.get("port").and_then(|v| v.as_integer()) {
            cfg.server.port = v as u16;
        }
    }

    // [provider]
    if let Some(Value::Table(section)) = table.get("provider") {
        if let Some(v) = section.get("base_url").and_then(|v| v.as_str()) {
            cfg.provider.base_url = v.to_string();
        }
        if let Some(v) = section.get("api_key").and_then(|v| v.as_str()) {
            cfg.provider.api_key = v.to_string();
        }
        if let Some(v) = section.get("model").and_then(|v| v.as_str()) {
            cfg.provider.model = v.to_string();
        }
        if let Some(v) = section.get("enable_thinking").and_then(|v| v.as_bool()) {
            cfg.provider.enable_thinking = v;
        }
        if let Some(v) = section.get("reasoning_effort").and_then(|v| v.as_str()) {
            cfg.provider.reasoning_effort = v.to_string();
        }
        if let Some(v) = section.get("max_tokens").and_then(|v| v.as_integer()) {
            cfg.provider.max_tokens = Some(v as u32);
        }
    }

    // [summary]
    if let Some(Value::Table(section)) = table.get("summary") {
        if let Some(v) = section.get("model").and_then(|v| v.as_str()) {
            cfg.summary.model = v.to_string();
        }
        if let Some(v) = section.get("base_url").and_then(|v| v.as_str()) {
            cfg.summary.base_url = v.to_string();
        }
        if let Some(v) = section.get("api_key").and_then(|v| v.as_str()) {
            cfg.summary.api_key = v.to_string();
        }
    }

    // [session]
    if let Some(Value::Table(section)) = table.get("session") {
        if let Some(v) = section.get("soft_cap_pct").and_then(|v| v.as_integer()) {
            cfg.session.soft_cap_pct = v as u8;
        }
        if let Some(v) = section.get("hard_cap_pct").and_then(|v| v.as_integer()) {
            cfg.session.hard_cap_pct = v as u8;
        }
        if let Some(v) = section.get("max_tool_calls_per_turn").and_then(|v| v.as_integer()) {
            cfg.session.max_tool_calls_per_turn = v as usize;
        }
    }

    // [logging]
    if let Some(Value::Table(section)) = table.get("logging") {
        if let Some(v) = section.get("level").and_then(|v| v.as_str()) {
            cfg.logging.level = v.to_string();
        }
        if let Some(v) = section.get("to_file").and_then(|v| v.as_str()) {
            cfg.logging.to_file = Some(PathBuf::from(v));
        }
        if let Some(v) = section.get("to_stderr").and_then(|v| v.as_bool()) {
            cfg.logging.to_stderr = v;
        }
    }

    // [sandbox]
    if let Some(Value::Table(section)) = table.get("sandbox") {
        if let Some(v) = section.get("enabled").and_then(|v| v.as_bool()) {
            cfg.sandbox.enabled = v;
        }
        if let Some(v) = section.get("bwrap_path").and_then(|v| v.as_str()) {
            cfg.sandbox.bwrap_path = Some(PathBuf::from(v));
        }
        // [sandbox.defaults]
        if let Some(Value::Table(defaults)) = section.get("defaults") {
            if let Some(Value::Array(hide)) = defaults.get("hide") {
                let paths: Vec<PathBuf> = hide
                    .iter()
                    .filter_map(|v| v.as_str().map(PathBuf::from))
                    .collect();
                if !paths.is_empty() {
                    cfg.sandbox.defaults.hide = paths;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let cfg = Config::default();
        assert_eq!(cfg.server.port, 8080);
        assert_eq!(cfg.session.soft_cap_pct, 65);
        assert_eq!(cfg.session.hard_cap_pct, 85);
    }

    #[test]
    fn test_parse_toml() {
        let content = r#"
[server]
host = "0.0.0.0"
port = 9090

[provider]
model = "test-model"
api_key = "sk-123"
"#;
        let table: toml::Table = content.parse().unwrap();
        let srv = table.get("server").unwrap().as_table().unwrap();
        assert_eq!(srv.get("host").unwrap().as_str(), Some("0.0.0.0"));
        assert_eq!(srv.get("port").unwrap().as_integer(), Some(9090));
        let prv = table.get("provider").unwrap().as_table().unwrap();
        assert_eq!(prv.get("model").unwrap().as_str(), Some("test-model"));
        assert_eq!(prv.get("api_key").unwrap().as_str(), Some("sk-123"));
    }

    #[test]
    fn test_apply_toml() {
        let content = r#"
[server]
host = "0.0.0.0"
port = 9090

[provider]
base_url = "http://custom:8080/v1"
model = "custom-model"

[session]
soft_cap_pct = 50
hard_cap_pct = 90
max_tool_calls_per_turn = 10

[logging]
level = "debug"
to_file = "/tmp/custom.log"
to_stderr = false
"#;
        let table: toml::Table = content.parse().unwrap();
        let mut cfg = Config::default();
        apply_toml(&mut cfg, &table);

        assert_eq!(cfg.server.host, "0.0.0.0");
        assert_eq!(cfg.server.port, 9090);
        assert_eq!(cfg.provider.base_url, "http://custom:8080/v1");
        assert_eq!(cfg.provider.model, "custom-model");
        assert_eq!(cfg.session.soft_cap_pct, 50);
        assert_eq!(cfg.session.hard_cap_pct, 90);
        assert_eq!(cfg.session.max_tool_calls_per_turn, 10);
        assert_eq!(cfg.logging.level, "debug");
        assert_eq!(cfg.logging.to_file, Some("/tmp/custom.log".into()));
        assert!(!cfg.logging.to_stderr);
    }
}
