mod db;
mod routes;
mod session;

use axum::Router;
use std::sync::Arc;
use tower_http::services::ServeDir;

pub struct AppState {
    pub db: db::Db,
    pub channels: session::Channels,
}

pub type SharedState = Arc<AppState>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,trace_gpx=debug".into()),
        )
        .init();

    let db_path = std::env::var("DB_PATH").unwrap_or_else(|_| "data/trace.db".into());
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let db = db::Db::open(&db_path)?;
    db.migrate()?;

    let state: SharedState = Arc::new(AppState {
        db,
        channels: session::Channels::new(),
    });

    // Purge expired sessions every 10 minutes
    let purge_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(600));
        loop {
            interval.tick().await;
            if let Err(e) = purge_state.db.purge_expired() {
                tracing::warn!("purge error: {e}");
            }
        }
    });

    let app = Router::new()
        .nest("/api", routes::api_router())
        .fallback_service(ServeDir::new("static"))
        .with_state(state);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    tracing::info!("listening on :{port}");
    axum::serve(listener, app).await?;
    Ok(())
}
