//! Entry point.
//!
//! Ported from `cmd.ts`. Each command runs against either the minimal runtime
//! (database only) or the full one (logging, configuration, torrent clients),
//! matching the original's `withMinimalRuntime` / `withFullRuntime` split.

use clap::Parser;
use crust_seed::cli::{Cli, Command};
use crust_seed::config::ConfigOverrides;
use crust_seed::errors::CrustSeedError;
use crust_seed::logger::Label;
use serde_json::json;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(error) = run(cli).await {
        // A CrustSeedError is a problem the user can fix; print it plainly.
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), CrustSeedError> {
    match cli.command {
        // ─── Full runtime ───────────────────────────────────────────────
        Command::Daemon {
            port,
            host,
            base_path,
            no_port,
            verbose,
        } => {
            let mut overrides = ConfigOverrides::new();
            if let Some(port) = port {
                overrides.insert("port".into(), json!(port));
            }
            if let Some(host) = &host {
                overrides.insert("host".into(), json!(host));
            }
            if !base_path.is_empty() {
                overrides.insert("basePath".into(), json!(base_path));
            }
            let (pool, config, _guards) =
                crust_seed::startup::init_full_runtime(verbose, &overrides).await?;

            crust_seed::server::routers::start_signup_window();
            tokio::spawn(crust_seed::log_watcher::watch_logs());

            // Startup indexing runs a beat after the listener comes up so the
            // UI is reachable while a large library is scanned.
            let index_pool = pool.clone();
            let index_config = config.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                if let Err(e) = crust_seed::torrent::index::index_torrents_and_data_dirs(
                    &index_pool,
                    &index_config,
                    true,
                )
                .await
                {
                    tracing::error!(label = Label::Index.as_str(), "Indexing failed: {e}");
                }
                crust_seed::jobs::jobs_loop(index_pool).await;
            });

            if no_port {
                // `--no-port` runs the jobs without exposing HTTP at all.
                std::future::pending::<()>().await;
                return Ok(());
            }
            let port = config.port.unwrap_or(2468);
            let host = config.host.clone().unwrap_or_else(|| "0.0.0.0".into());
            let base_path = config.base_path.clone().unwrap_or_default();
            crust_seed::server::serve(pool, port, &host, &base_path).await
        }

        Command::Search { torrents, verbose } => {
            let mut overrides = ConfigOverrides::new();
            if !torrents.is_empty() {
                overrides.insert("torrents".into(), json!(torrents));
            }
            let (pool, config, _guards) =
                crust_seed::startup::init_full_runtime(verbose, &overrides).await?;
            let _ = crust_seed::torrent::index::index_torrents_and_data_dirs(&pool, &config, true)
                .await;
            crust_seed::pipeline::bulk_search(&pool, &config).await;
            Ok(())
        }

        Command::Rss { verbose } => {
            let (pool, config, _guards) =
                crust_seed::startup::init_full_runtime(verbose, &ConfigOverrides::new()).await?;
            let _ = crust_seed::torrent::index::index_torrents_and_data_dirs(&pool, &config, true)
                .await;
            let last_run =
                crust_seed::jobs::get_job_last_run(&pool, crust_seed::jobs::JobName::Rss)
                    .await
                    .unwrap_or(0);
            crust_seed::pipeline::scan_rss_feeds(&pool, &config, last_run).await;
            Ok(())
        }

        Command::Inject {
            inject_dir,
            ignore_titles,
            no_ignore_titles,
            verbose,
        } => {
            let (pool, config, _guards) =
                crust_seed::startup::init_full_runtime(verbose, &ConfigOverrides::new()).await?;
            if config.action != crust_seed::constants::Action::Inject {
                return Err(CrustSeedError::new(
                    "`crust-seed inject` requires the 'inject' action.",
                ));
            }
            let _ = crust_seed::torrent::index::index_torrents_and_data_dirs(&pool, &config, true)
                .await;
            // An explicit --no-ignore-titles must beat a configured `true`.
            let ignore = if no_ignore_titles {
                Some(false)
            } else if ignore_titles {
                Some(true)
            } else {
                None
            };
            crust_seed::inject::inject_saved_torrents(&pool, &config, ignore, inject_dir).await;
            Ok(())
        }

        Command::Restore { verbose } => {
            let (pool, config, _guards) =
                crust_seed::startup::init_full_runtime(verbose, &ConfigOverrides::new()).await?;
            crust_seed::inject::restore_from_torrent_cache(&pool, &config).await;
            Ok(())
        }

        Command::TestNotification { verbose } => {
            let (_pool, config, _guards) =
                crust_seed::startup::init_full_runtime(verbose, &ConfigOverrides::new()).await?;
            let results =
                crust_seed::push_notifier::send_test_notification(config.notification_webhook_urls)
                    .await;
            if results.iter().any(|result| !result.ok) {
                return Err(CrustSeedError::new(
                    "At least one webhook failed; see the log for details.",
                ));
            }
            Ok(())
        }

        // ─── Minimal runtime (database only) ────────────────────────────
        Command::Diff {
            searchee,
            candidate,
        } => {
            crust_seed::logger::initialize_bootstrap_logger();
            let _pool = crust_seed::startup::init_minimal_runtime().await?;
            let config = crust_seed::config::default_runtime_config();
            let output = crust_seed::inject::diff_torrents(
                std::path::Path::new(&searchee),
                std::path::Path::new(&candidate),
                &config,
            )
            .await
            .map_err(CrustSeedError::new)?;
            println!("{output}");
            Ok(())
        }

        Command::Tree { torrent } => {
            crust_seed::logger::initialize_bootstrap_logger();
            let bytes = tokio::fs::read(&torrent)
                .await
                .map_err(|e| CrustSeedError::new(format!("{torrent}: {e}")))?;
            let meta = crust_seed::torrent::Metafile::decode(&bytes)
                .map_err(|e| CrustSeedError::new(e.to_string()))?;
            println!("Use `crust-seed diff` to compare two .torrent files");
            println!("name: {}", meta.name);
            println!("title: {}", meta.title);
            println!("infoHash: {}", meta.info_hash);
            println!("length: {}", meta.length);
            println!("pieceLength: {}", meta.piece_length);
            println!("trackers: {}", meta.trackers.join(", "));
            for file in &meta.files {
                println!("  {} ({} bytes)", file.path, file.length);
            }
            Ok(())
        }

        Command::UpdateTorrentCacheTrackers {
            old_announce_url,
            new_announce_url,
        } => {
            crust_seed::logger::initialize_bootstrap_logger();
            let _pool = crust_seed::startup::init_minimal_runtime().await?;
            let updated = crust_seed::torrent::cache::update_torrent_cache_trackers(
                &old_announce_url,
                &new_announce_url,
            )
            .await;
            println!("Updated {updated} torrents in the cache");
            Ok(())
        }

        Command::ClearIndexerFailures => {
            crust_seed::logger::initialize_bootstrap_logger();
            let pool = crust_seed::startup::init_minimal_runtime().await?;
            crust_seed::indexers::clear_indexer_failures(&pool)
                .await
                .map_err(|e| CrustSeedError::new(e.to_string()))?;
            println!("Cleared indexer failures");
            Ok(())
        }

        Command::ClearCache => {
            crust_seed::logger::initialize_bootstrap_logger();
            let pool = crust_seed::startup::init_minimal_runtime().await?;
            println!("Clearing cache...");
            // Rows WITHOUT an info hash are the ones that never resulted in a
            // snatch, so clearing them cannot cause a re-download.
            for sql in [
                "DELETE FROM decision WHERE info_hash IS NULL",
                "DELETE FROM timestamp",
            ] {
                sqlx::query(sql)
                    .execute(&pool)
                    .await
                    .map_err(|e| CrustSeedError::new(e.to_string()))?;
            }
            Ok(())
        }

        Command::ClearClientCache => {
            crust_seed::logger::initialize_bootstrap_logger();
            let pool = crust_seed::startup::init_minimal_runtime().await?;
            println!("Clearing client cache...");
            for table in ["torrent", "client_searchee", "data", "ensemble"] {
                sqlx::query(&format!("DELETE FROM {table}"))
                    .execute(&pool)
                    .await
                    .map_err(|e| CrustSeedError::new(e.to_string()))?;
            }
            Ok(())
        }

        Command::ApiKey { api_key } => {
            crust_seed::logger::initialize_bootstrap_logger();
            let pool = crust_seed::startup::init_minimal_runtime().await?;
            let key = match api_key {
                Some(api_key) => crust_seed::user_auth::set_api_key(&pool, &api_key).await?,
                None => crust_seed::user_auth::get_api_key(&pool).await?,
            };
            println!("{key}");
            Ok(())
        }

        Command::ResetApiKey => {
            crust_seed::logger::initialize_bootstrap_logger();
            let pool = crust_seed::startup::init_minimal_runtime().await?;
            let key = crust_seed::user_auth::reset_api_key(&pool).await?;
            println!("{key}");
            Ok(())
        }

        Command::ResetUser => {
            crust_seed::logger::initialize_bootstrap_logger();
            let pool = crust_seed::startup::init_minimal_runtime().await?;
            let message = crust_seed::user_auth::reset_users(&pool)
                .await
                .map_err(|e| CrustSeedError::new(e.to_string()))?;
            println!("{message}");
            Ok(())
        }

        Command::DevLogin {
            user,
            origin,
            redirect_to,
        } => {
            crust_seed::logger::initialize_bootstrap_logger();
            let pool = crust_seed::startup::init_minimal_runtime().await?;
            let username = user.unwrap_or_else(|| "dev".to_string());
            let url =
                crust_seed::user_auth::create_dev_login(&pool, &username, &origin, &redirect_to)
                    .await?;
            println!("{url}");
            println!("Start the daemon with CRUST_SEED_DEV_LOGIN=true for this URL to work.");
            Ok(())
        }
    }
}
