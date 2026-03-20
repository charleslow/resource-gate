mod api;
mod budget;
mod config;
mod dispatcher;
mod models;
mod provider_bridge;
mod store;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    tracing::info!("resource-gate starting");

    // TODO: load config, init store, start dispatcher, serve API
}
