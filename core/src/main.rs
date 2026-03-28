mod api;
mod budget;
mod config;
mod dispatcher;
mod models;
mod provider_bridge;
mod store;

#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use config::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("resource-gate starting");

    // Load config (use default if no config file)
    let config = match Path::new("config.toml").exists() {
        true => Config::load(Path::new("config.toml"))?,
        false => {
            tracing::info!("no config.toml found, using defaults");
            Config::default_config()
        }
    };

    // Init store
    let store = store::Store::open(Path::new(&config.harness.db_path))?;
    tracing::info!("database opened at {}", config.harness.db_path);

    // Seed budget config into DB
    store.set_budget_config(
        config.budget.daily_limit_usd,
        config.budget.alert_threshold_pct,
        config.budget.auto_pause_on_limit,
    )?;

    // Init budget enforcer
    let budget = Arc::new(budget::BudgetEnforcer::new(
        config.budget.clone(),
        store.clone(),
    ));

    // Create workspace directory
    let workspace_dir = std::path::PathBuf::from(&config.harness.workspace_dir);
    std::fs::create_dir_all(&workspace_dir)?;
    let workspace_abs = std::fs::canonicalize(&workspace_dir)?;
    tracing::info!("workspace at {}", workspace_abs.display());

    // Init provider bridge
    let integrations_path = std::env::current_dir()?
        .parent()
        .unwrap_or(Path::new("."))
        .join("integrations")
        .join("src");
    let bridge = Arc::new(provider_bridge::ProviderBridge::new(
        config.python_bin().to_string(),
        integrations_path.to_string_lossy().to_string(),
        workspace_abs.to_string_lossy().to_string(),
    ));

    // Preflight: check that Docker is available for the local provider
    if config
        .providers
        .local
        .as_ref()
        .map_or(true, |l| l.enabled)
    {
        tracing::info!("running Docker preflight check");
        bridge.preflight("local").await?;
        tracing::info!("Docker preflight check passed");
    }

    // Start dispatcher
    let concurrency_limits = config.concurrency_limits();
    let dispatcher = Arc::new(dispatcher::Dispatcher::new(
        store.clone(),
        Arc::clone(&budget),
        Arc::clone(&bridge),
        config.harness.poll_interval_seconds,
        concurrency_limits.clone(),
    ));
    let paused_flag = dispatcher.paused_flag();

    let dispatcher_handle = Arc::clone(&dispatcher);
    tokio::spawn(async move {
        dispatcher_handle.run().await;
    });

    // Build API router
    let state = api::AppState {
        store,
        budget,
        bridge,
        dispatcher,
        paused: paused_flag,
        agent_token: config.harness.agent_token.clone(),
        admin_token: config.harness.admin_token.clone(),
        concurrency_limits,
    };
    let app = api::router(state);

    // Serve
    let addr: SocketAddr = format!("{}:{}", config.harness.host, config.harness.port)
        .parse()?;
    tracing::info!("listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
