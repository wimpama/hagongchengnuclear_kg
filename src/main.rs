mod ai_trace;
mod auth;
mod catalog;
mod chat_context;
mod chat_intent;
mod config;
mod courseware;
mod db;
mod document_visuals;
mod embedding;
mod error;
mod extract;
mod gateway;
mod indexing;
mod ingest;
mod knowledge;
mod lexical;
mod local_runtime;
mod media;
mod models;
mod qdrant;
mod question_attachments;
mod routes;
mod startup;
mod state;
mod storage;
mod storage_migration;
mod tickets;

use anyhow::Context;
use axum::Router;
use config::AppConfig;
use state::AppState;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let migration = arguments
        .first()
        .is_some_and(|command| command == "migrate-storage");
    let local_backend = if migration {
        None
    } else {
        match startup::configure(&arguments)? {
            Some(options) => options.local_backend,
            None => return Ok(()),
        }
    };
    init_tracing();
    // Read env-file settings before Tokio starts worker threads.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("create Tokio runtime")?;
    runtime.block_on(async {
        if migration {
            storage_migration::run(&arguments[1..]).await
        } else {
            run_server(local_backend).await
        }
    })
}

async fn run_server(local_backend: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let signals = tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });
    let result = run_services(local_backend, shutdown_rx).await;
    signals.abort();
    result
}

async fn run_services(
    local_backend: Option<std::path::PathBuf>,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut dependency = if let Some(backend) = local_backend {
        tokio::select! {
            result = local_runtime::LocalRuntime::start(&backend) => Some(result?),
            _ = wait_shutdown(shutdown.clone()) => return Ok(()),
        }
    } else {
        None
    };
    if let Some(local) = dependency.as_mut() {
        let result = tokio::select! {
            result = serve(shutdown) => result,
            status = local.exited() => Err(anyhow::anyhow!("local Qdrant stopped unexpectedly: {}",status?)),
        };
        let stopped = local.stop().await;
        result.and(stopped)
    } else {
        serve(shutdown).await
    }
}

async fn wait_shutdown(mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stopped| *stopped).await;
}

async fn serve(shutdown: tokio::sync::watch::Receiver<bool>) -> anyhow::Result<()> {
    let config = AppConfig::from_env().context("load server configuration")?;
    let production = AppConfig::for_environment(config::Environment::Production)?;
    if config.database_url == production.database_url {
        anyhow::bail!("development and production must use different SQLite databases");
    }
    let bind_addr = config.bind_addr;
    let development = build_tenant(config).await?;
    let production = build_tenant(production).await?;
    let app = gateway::router(development, production);
    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("bind {bind_addr}"))?;
    tracing::info!(address = %bind_addr, "knowledge service listening for development and production sites");
    axum::serve(listener, app)
        .with_graceful_shutdown(wait_shutdown(shutdown))
        .await
        .context("serve HTTP requests")?;
    Ok(())
}

async fn build_tenant(config: AppConfig) -> anyhow::Result<Router> {
    let pool = db::connect(&config.database_url)
        .await
        .context("open SQLite database")?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("run SQLite migrations")?;
    sqlx::query("INSERT OR IGNORE INTO kb_instance_environment(singleton,environment) VALUES(1,?)")
        .bind(config.environment.name())
        .execute(&pool)
        .await?;
    let bound: String =
        sqlx::query_scalar("SELECT environment FROM kb_instance_environment WHERE singleton=1")
            .fetch_one(&pool)
            .await?;
    if bound != config.environment.name() {
        anyhow::bail!("database is already assigned to a different site environment");
    }

    let qdrant = qdrant::QdrantAdapter::new(config.qdrant.clone())?;
    let auth = auth::AuthUpstream::new(config.auth.clone())?;
    let state = AppState::new(config.clone(), pool, qdrant, auth);
    lexical::backfill(&state.db)
        .await
        .map_err(|_| anyhow::anyhow!("build lexical index"))?;
    indexing::resume_jobs(state.clone()).await;
    question_attachments::start_cleanup(state.clone());
    storage::start_cache_cleanup(state.storage.clone());

    let app = Router::new()
        .merge(routes::health_router())
        .merge(ingest::router())
        .merge(courseware::router())
        .merge(models::router())
        .merge(knowledge::router())
        .merge(question_attachments::router())
        .merge(indexing::router())
        .merge(tickets::router())
        .with_state(state)
        .layer(TraceLayer::new_for_http().make_span_with(
            |request: &axum::http::Request<axum::body::Body>| {
                let route = request
                    .extensions()
                    .get::<axum::extract::MatchedPath>()
                    .map_or("unmatched", |p| p.as_str());
                tracing::info_span!("http", method = %request.method(), route)
            },
        ));

    Ok(app)
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("course_kb_server=info,tower_http=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(%error, "failed to install Ctrl-C handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => tracing::warn!(%error, "failed to install SIGTERM handler"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
