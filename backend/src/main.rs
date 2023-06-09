pub mod gateway;
pub mod macros;
pub mod models;
pub mod rest;
pub mod utils;
use std::process::ExitCode;

use models::appstate::APP;
use tokio::signal::ctrl_c;
use tracing::level_filters::LevelFilter;
use warp::Filter;

#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};

#[cfg(unix)]
async fn handle_signals() {
    let mut sigterm = signal(SignalKind::terminate()).expect("Failed to create SIGTERM signal listener");

    tokio::select! {
        _ = sigterm.recv() => {
            tracing::info!("Received SIGTERM, terminating...");
        }
        _ = ctrl_c() => {
            tracing::info!("Received keyboard interrupt, terminating...");
        }
    };
}

#[cfg(not(unix))]
async fn handle_signals() {
    ctrl_c().await.expect("Failed to create CTRL+C signal listener");
}

#[tokio::main]
async fn main() -> ExitCode {
    #[cfg(debug_assertions)]
    let subscriber = tracing_subscriber::fmt()
        .compact()
        .with_target(false)
        .with_max_level(LevelFilter::DEBUG)
        .without_time()
        .finish();

    #[cfg(not(debug_assertions))]
    let subscriber = tracing_subscriber::fmt()
        .compact()
        .with_target(false)
        .without_time()
        .finish();

    /* console_subscriber::init(); */
    tracing::subscriber::set_global_default(subscriber).expect("Failed to set subscriber");

    let gateway_routes = gateway::handler::get_routes();
    let rest_routes = rest::routes::get_routes();

    // Initialize the database
    if let Err(e) = APP.init().await {
        tracing::error!(message = "Failed initializing application", error = %e);
        return ExitCode::FAILURE;
    }

    tokio::select!(
        _ = handle_signals() => {},
        _ = warp::serve(gateway_routes.or(rest_routes))
            .run(APP.config().listen_addr()) => {}
    );
    APP.close().await;

    ExitCode::SUCCESS
}
