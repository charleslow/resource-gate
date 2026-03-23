// Configuration loading from TOML

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub harness: HarnessConfig,
    pub budget: BudgetConfig,
    #[serde(default)]
    pub providers: ProvidersConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HarnessConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_db_path")]
    pub db_path: String,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_seconds: u64,
    #[serde(default = "default_workspace")]
    pub workspace_dir: String,
    pub agent_token: Option<String>,
    pub admin_token: Option<String>,
    pub python_bin: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BudgetConfig {
    #[serde(default = "default_daily_limit")]
    pub daily_limit_usd: f64,
    #[serde(default = "default_alert_threshold")]
    pub alert_threshold_pct: u32,
    #[serde(default)]
    pub auto_pause_on_limit: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub local: Option<LocalProviderConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LocalProviderConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}
fn default_port() -> u16 {
    8420
}
fn default_db_path() -> String {
    "./data/harness.db".to_string()
}
fn default_poll_interval() -> u64 {
    30
}
fn default_daily_limit() -> f64 {
    0.0 // 0 means unlimited — resource-gate is opt-in so no ceiling by default
}
fn default_alert_threshold() -> u32 {
    80
}
fn default_workspace() -> String {
    "./workspace".to_string()
}
fn default_true() -> bool {
    true
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    pub fn default_config() -> Self {
        Config {
            harness: HarnessConfig {
                host: default_host(),
                port: default_port(),
                db_path: default_db_path(),
                poll_interval_seconds: default_poll_interval(),
                workspace_dir: default_workspace(),
                agent_token: None,
                admin_token: None,
                python_bin: None,
            },
            budget: BudgetConfig {
                daily_limit_usd: default_daily_limit(), // 0 = unlimited
                alert_threshold_pct: default_alert_threshold(),
                auto_pause_on_limit: false,
            },
            providers: ProvidersConfig::default(),
        }
    }

    pub fn python_bin(&self) -> &str {
        self.harness
            .python_bin
            .as_deref()
            .unwrap_or("python3")
    }
}
