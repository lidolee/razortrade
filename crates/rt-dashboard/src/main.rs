//! rt-dashboard — read-only portfolio view.
//!
//! Binds to 127.0.0.1:8080 by default. Access it via SSH tunnel:
//!   ssh -L 8080:localhost:8080 linda@razortrade-prod
//! then open http://localhost:8080 in the browser.
//!
//! Opens the SQLite database in `?mode=ro` so we cannot
//! accidentally corrupt the daemon's state. Polling frequency
//! is driven by the browser (fetch + setInterval), not by the
//! server — the server just answers HTTP requests.

use axum::{
    extract::State,
    response::{Html, Json},
    routing::get,
    Router,
};
use chrono::{Duration, Utc};
use serde::Serialize;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{FromRow, SqlitePool};
use std::net::SocketAddr;
use tracing::{info, warn};

#[derive(Clone)]
struct AppState {
    pool: SqlitePool,
}

// ------------------ /api/summary ------------------

#[derive(Serialize, FromRow, Default)]
struct SummaryResponse {
    total_equity_chf: Option<String>,
    realized_pnl_chf: Option<String>,
    drawdown: Option<String>,
    nav_per_unit: Option<String>,
    updated_at: Option<String>,
}

async fn summary(State(state): State<AppState>) -> Json<SummaryResponse> {
    // SELECT the latest row, aliased to the serde field names so
    // FromRow does the mapping for us. All fields are Option<String>
    // so an empty table produces a row of NULLs gracefully.
    let row: Option<SummaryResponse> = sqlx::query_as(
        r#"
        SELECT
            total_equity_chf                  AS total_equity_chf,
            realized_pnl_leverage_lifetime    AS realized_pnl_chf,
            drawdown_fraction                 AS drawdown,
            nav_per_unit                      AS nav_per_unit,
            timestamp                         AS updated_at
        FROM equity_snapshots
        ORDER BY id DESC
        LIMIT 1
        "#,
    )
    .fetch_optional(&state.pool)
    .await
    .unwrap_or_else(|e| {
        warn!(error = %e, "summary query failed");
        None
    });

    Json(row.unwrap_or_default())
}

// ------------------ /api/positions ----------------

#[derive(Serialize, FromRow)]
struct PositionRow {
    id: i64,
    instrument_symbol: String,
    broker: String,
    sleeve: String,
    quantity: String,
    avg_entry_price: Option<String>,
    realized_pnl_btc: String,
    opened_at: String,
}

async fn positions(State(state): State<AppState>) -> Json<Vec<PositionRow>> {
    let rows: Vec<PositionRow> = sqlx::query_as(
        r#"
        SELECT id, instrument_symbol, broker, sleeve,
               quantity, avg_entry_price, realized_pnl_btc, opened_at
        FROM positions
        WHERE closed_at IS NULL
        ORDER BY opened_at DESC
        "#,
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_else(|e| {
        warn!(error = %e, "positions query failed");
        Vec::new()
    });

    Json(rows)
}

// ------------------ /api/equity-history -----------

#[derive(Serialize, FromRow)]
struct EquityPoint {
    timestamp: String,
    total_equity_chf: String,
}

async fn equity_history(State(state): State<AppState>) -> Json<Vec<EquityPoint>> {
    // Last 24h of equity snapshots. If the daemon writes sparsely
    // (e.g. once per cycle), the chart will be sparse — that is a
    // feature of the chart, not a bug of the dashboard.
    let since = (Utc::now() - Duration::hours(24)).to_rfc3339();
    let rows: Vec<EquityPoint> = sqlx::query_as(
        r#"
        SELECT timestamp, total_equity_chf
        FROM equity_snapshots
        WHERE timestamp > ?
        ORDER BY timestamp ASC
        "#,
    )
    .bind(&since)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_else(|e| {
        warn!(error = %e, "equity_history query failed");
        Vec::new()
    });

    Json(rows)
}

// ------------------ /api/recent-orders ------------

#[derive(Serialize, FromRow)]
struct RecentOrder {
    id: i64,
    created_at: String,
    instrument_symbol: String,
    side: String,
    status: String,
    quantity: String,
    filled_quantity: Option<String>,
    avg_fill_price: Option<String>,
}

async fn recent_orders(State(state): State<AppState>) -> Json<Vec<RecentOrder>> {
    let rows: Vec<RecentOrder> = sqlx::query_as(
        r#"
        SELECT id, created_at, instrument_symbol, side, status,
               quantity, filled_quantity, avg_fill_price
        FROM orders
        ORDER BY id DESC
        LIMIT 20
        "#,
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_else(|e| {
        warn!(error = %e, "recent_orders query failed");
        Vec::new()
    });

    Json(rows)
}

// ------------------ /  (HTML) ---------------------

const INDEX_HTML: &str = include_str!("index.html");

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

// ------------------ bootstrap ---------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Default points at the production path inside the
    // StateDirectory of the rt-daemon systemd service. Dev
    // override via DATABASE_URL env var.
    //
    // `mode=ro` is important: the dashboard opens the file
    // read-only so it can never corrupt the daemon's writes.
    let db_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        "sqlite:///var/lib/razortrade/razortrade.sqlite?mode=ro".to_string()
    });

    info!(db = %db_url, "opening database");

    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect(&db_url)
        .await?;

    let state = AppState { pool };

    let app = Router::new()
        .route("/", get(index))
        .route("/api/summary", get(summary))
        .route("/api/positions", get(positions))
        .route("/api/equity-history", get(equity_history))
        .route("/api/recent-orders", get(recent_orders))
        .with_state(state);

    let bind_addr = std::env::var("RT_DASHBOARD_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let addr: SocketAddr = bind_addr.parse()?;

    info!(bind = %addr, "rt-dashboard listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
