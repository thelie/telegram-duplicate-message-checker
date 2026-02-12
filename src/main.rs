mod auth;
mod config;
mod handler;
mod marker;
mod tracker;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use grammers_client::client::UpdatesConfiguration;
use grammers_client::{Client, SenderPool};
use grammers_session::storages::SqliteSession;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{error, info};

use crate::config::Config;
use crate::marker::Marker;
use crate::tracker::DuplicateTracker;

/// 30 days in seconds
const CLEANUP_MAX_AGE: u64 = 30 * 24 * 60 * 60;
/// Save state every 5 minutes
const SAVE_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Cleanup interval (daily)
const CLEANUP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    dotenvy::dotenv().ok();
    let config = Config::from_env()?;
    config.ensure_dirs()?;

    info!("Starting Telegram duplicate message checker");

    // Set up session and connect
    let session = Arc::new(
        SqliteSession::open(config.session_path.to_str().unwrap_or("session.sqlite"))
            .await
            .context("Failed to open session")?,
    );

    let SenderPool {
        runner,
        handle,
        updates,
    } = SenderPool::new(Arc::clone(&session), config.api_id);
    // Client::new consumes the fat handle; we clone it first so we can
    // call handle.quit() later for graceful shutdown.
    let client = Client::new(handle.clone());
    let pool_task = tokio::spawn(runner.run());

    // Authenticate
    auth::ensure_authorized(&client, &config.api_hash, config.phone_number.as_deref()).await?;

    // Load or create tracker state
    let tracker = if config.state_path.exists() {
        match DuplicateTracker::load(&config.state_path) {
            Ok(t) => {
                info!("Loaded state from {}", config.state_path.display());
                t
            }
            Err(e) => {
                error!("Failed to load state, starting fresh: {}", e);
                DuplicateTracker::default()
            }
        }
    } else {
        info!("No existing state, starting fresh");
        DuplicateTracker::default()
    };

    let tracker = Arc::new(Mutex::new(tracker));

    // Build marker with peer cache
    let mut marker = Marker::new(client.clone());
    marker.build_peer_cache().await?;
    let marker = Arc::new(Mutex::new(marker));

    // Start update stream
    let mut update_stream = client
        .stream_updates(
            updates,
            UpdatesConfiguration {
                catch_up: false,
                ..Default::default()
            },
        )
        .await;

    info!("Listening for updates...");

    // Spawn periodic save task. Use interval_at to skip the immediate
    // first tick — no need to save/cleanup right at startup.
    let save_tracker = Arc::clone(&tracker);
    let save_path = config.state_path.clone();
    tokio::spawn(async move {
        let start = Instant::now();
        let mut save_interval =
            tokio::time::interval_at(start + SAVE_INTERVAL, SAVE_INTERVAL);
        let mut cleanup_interval =
            tokio::time::interval_at(start + CLEANUP_INTERVAL, CLEANUP_INTERVAL);
        loop {
            tokio::select! {
                _ = save_interval.tick() => {
                    let t = save_tracker.lock().await;
                    if let Err(e) = t.save(&save_path) {
                        error!("Failed to save state: {}", e);
                    } else {
                        info!("State saved");
                    }
                }
                _ = cleanup_interval.tick() => {
                    let mut t = save_tracker.lock().await;
                    t.cleanup(CLEANUP_MAX_AGE);
                }
            }
        }
    });

    // Main update loop — two-phase processing to avoid holding both locks
    // across network I/O. Phase 1 (plan) only holds the tracker lock.
    // Phase 2 (execute) only holds the marker lock.
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Received Ctrl+C, shutting down...");
                break;
            }
            result = update_stream.next() => {
                match result {
                    Ok(update) => {
                        // Phase 1: plan (tracker lock only)
                        let action = {
                            let mut t = tracker.lock().await;
                            handler::plan_update(&update, &mut t).await
                        };
                        // Phase 2: execute (marker lock only)
                        let mut m = marker.lock().await;
                        handler::execute_action(action, &mut m).await;
                    }
                    Err(e) => {
                        error!("Error receiving update: {}", e);
                    }
                }
            }
        }
    }

    // Shutdown: save state
    info!("Saving final state...");
    {
        let t = tracker.lock().await;
        if let Err(e) = t.save(&config.state_path) {
            error!("Failed to save final state: {}", e);
        }
    }

    // Sync update state and shut down gracefully
    update_stream.sync_update_state().await;
    handle.quit();
    let _ = pool_task.await;

    info!("Goodbye!");
    Ok(())
}
