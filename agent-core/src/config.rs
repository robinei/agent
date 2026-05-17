use std::path::PathBuf;

/// Full server configuration, loaded from `~/.agent/config.toml` and env vars.
#[derive(Clone, Debug)]
pub struct Config {
    pub server: ServerConfig,
    pub provider: ProviderConfig,
    pub summary: SummaryConfig,
    pub session: SessionConfig,
    pub logging: LoggingConfig,
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
        }
    }
}

/// Load config from the default path (`~/.agent/config.toml`), falling back to
/// defaults and applying environment variable overrides.
pub fn load_config() -> Config {
    let mut cfg = Config::default();

    // Try to load from file
    let config_path = agent_dir().join("config.toml");
    if let Ok(content) = std::fs::read_to_string(&config_path) {
        if !content.trim().is_empty() {
            let parsed = parse_toml(&content);
            apply_toml(&mut cfg, &parsed);
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

// ── TOML parsing (minimal, no toml crate dependency) ──
//
// We parse the TOML the config manually to avoid pulling in the `toml` crate.
// The format is a subset of TOML: `[section]\nkey = "value"\nkey = 123`.
// This is enough for our config file. If more complex TOML is needed later,
// swap to the `toml` crate.

type TomlTable = std::collections::HashMap<String, TomlSection>;
type TomlSection = std::collections::HashMap<String, TomlValue>;

#[derive(Debug)]
enum TomlValue {
    String(String),
    Integer(i64),
    Bool(bool),
}

fn parse_toml(content: &str) -> TomlTable {
    let mut table = TomlTable::new();
    let mut current_section: Option<String> = None;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let section = line.trim_matches('[').trim_matches(']').trim().to_string();
            table.entry(section.clone()).or_default();
            current_section = Some(section);
            continue;
        }
        if let Some(eq_pos) = line.find('=') {
            let key = line[..eq_pos].trim().to_string();
            let raw_val = line[eq_pos + 1..].trim();

            let value = if raw_val.starts_with('"') && raw_val.ends_with('"') {
                TomlValue::String(raw_val[1..raw_val.len() - 1].to_string())
            } else if raw_val == "true" {
                TomlValue::Bool(true)
            } else if raw_val == "false" {
                TomlValue::Bool(false)
            } else if let Ok(n) = raw_val.parse::<i64>() {
                TomlValue::Integer(n)
            } else {
                // Fallback: treat as string without quotes
                TomlValue::String(raw_val.to_string())
            };

            if let Some(section) = &current_section {
                table.entry(section.clone()).or_default().insert(key, value);
            }
        }
    }

    table
}

fn apply_toml(cfg: &mut Config, table: &TomlTable) {
    // [server]
    if let Some(section) = table.get("server") {
        if let Some(TomlValue::String(v)) = section.get("host") {
            cfg.server.host = v.clone();
        }
        if let Some(TomlValue::Integer(v)) = section.get("port") {
            cfg.server.port = *v as u16;
        }
    }

    // [provider]
    if let Some(section) = table.get("provider") {
        if let Some(TomlValue::String(v)) = section.get("base_url") {
            cfg.provider.base_url = v.clone();
        }
        if let Some(TomlValue::String(v)) = section.get("api_key") {
            cfg.provider.api_key = v.clone();
        }
        if let Some(TomlValue::String(v)) = section.get("model") {
            cfg.provider.model = v.clone();
        }
    }

    // [summary]
    if let Some(section) = table.get("summary") {
        if let Some(TomlValue::String(v)) = section.get("model") {
            cfg.summary.model = v.clone();
        }
        if let Some(TomlValue::String(v)) = section.get("base_url") {
            cfg.summary.base_url = v.clone();
        }
        if let Some(TomlValue::String(v)) = section.get("api_key") {
            cfg.summary.api_key = v.clone();
        }
    }

    // [session]
    if let Some(section) = table.get("session") {
        if let Some(TomlValue::Integer(v)) = section.get("soft_cap_pct") {
            cfg.session.soft_cap_pct = *v as u8;
        }
        if let Some(TomlValue::Integer(v)) = section.get("hard_cap_pct") {
            cfg.session.hard_cap_pct = *v as u8;
        }
        if let Some(TomlValue::Integer(v)) = section.get("max_tool_calls_per_turn") {
            cfg.session.max_tool_calls_per_turn = *v as usize;
        }
    }

    // [logging]
    if let Some(section) = table.get("logging") {
        if let Some(TomlValue::String(v)) = section.get("level") {
            cfg.logging.level = v.clone();
        }
        if let Some(TomlValue::String(v)) = section.get("to_file") {
            cfg.logging.to_file = Some(PathBuf::from(v));
        }
        if let Some(TomlValue::Bool(v)) = section.get("to_stderr") {
            cfg.logging.to_stderr = *v;
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
        let table = parse_toml(content);
        let srv = table.get("server").unwrap();
        assert_eq!(srv.get("host").map(|v| match v { TomlValue::String(s) => s.as_str(), _ => "" }), Some("0.0.0.0"));
        let prv = table.get("provider").unwrap();
        assert_eq!(prv.get("model").map(|v| match v { TomlValue::String(s) => s.as_str(), _ => "" }), Some("test-model"));
        assert_eq!(prv.get("api_key").map(|v| match v { TomlValue::String(s) => s.as_str(), _ => "" }), Some("sk-123"));
    }
}