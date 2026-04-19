//! rt-dashboard v2 — trading view + system health.
//!
//! Public HTTPS via Caddy → oauth2-proxy → this binary on 127.0.0.1:8080.
//! The binary itself has no auth; it trusts that Caddy only forwards
//! authorised requests.
//!
//! All SQLite access is `?mode=ro` so the dashboard can never corrupt
//! the daemon's DB. Systemctl probes shell out to the `systemctl show`
//! command; parsing KEY=VALUE is easier than wiring dbus for a dozen
//! properties and avoids a new runtime dependency.
//!
//! The browser polls every 5 s via setInterval. All "freshness" figures
//! are computed server-side so the client doesn't need to know anything
//! about our tick cadences.

use axum::{
    extract::State,
    response::{Html, Json},
    routing::get,
    Router,
};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{FromRow, SqlitePool};
use std::collections::HashMap;
use std::net::SocketAddr;
use tokio::process::Command;
use tracing::{info, warn};

#[derive(Clone)]
struct AppState {
    pool: SqlitePool,
}

// =========================================================================
// Trading tab endpoints
// =========================================================================

#[derive(Serialize, FromRow, Default)]
struct SummaryResponse {
    total_equity_chf: Option<String>,
    cash_chf: Option<String>,
    crypto_leverage_value_chf: Option<String>,
    unrealized_pnl_leverage_chf: Option<String>,
    realized_pnl_chf: Option<String>,
    drawdown: Option<String>,
    nav_per_unit: Option<String>,
    updated_at: Option<String>,
}

async fn summary(State(state): State<AppState>) -> Json<SummaryResponse> {
    let row: Option<SummaryResponse> = sqlx::query_as(
        r#"
        SELECT
            total_equity_chf                  AS total_equity_chf,
            cash_chf                          AS cash_chf,
            crypto_leverage_value_chf         AS crypto_leverage_value_chf,
            unrealized_pnl_leverage_chf       AS unrealized_pnl_leverage_chf,
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

#[derive(Serialize, FromRow)]
struct EquityPoint {
    timestamp: String,
    total_equity_chf: String,
}

async fn equity_history(State(state): State<AppState>) -> Json<Vec<EquityPoint>> {
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

#[derive(Serialize, FromRow)]
struct RecentSignal {
    id: i64,
    created_at: String,
    instrument_symbol: String,
    side: String,
    signal_type: String,
    sleeve: String,
    status: String,
    rejection_reason: Option<String>,
}

async fn recent_signals(State(state): State<AppState>) -> Json<Vec<RecentSignal>> {
    let rows: Vec<RecentSignal> = sqlx::query_as(
        r#"
        SELECT id, created_at, instrument_symbol, side, signal_type,
               sleeve, status, rejection_reason
        FROM signals
        ORDER BY id DESC
        LIMIT 20
        "#,
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_else(|e| {
        warn!(error = %e, "recent_signals query failed");
        Vec::new()
    });

    Json(rows)
}

// =========================================================================
// System status tab endpoints
// =========================================================================

#[derive(Serialize)]
struct ServiceStatus {
    name: String,
    active_state: String,
    sub_state: String,
    since_iso: Option<String>,
    uptime_seconds: Option<i64>,
    memory_bytes: Option<u64>,
    last_exit_status: Option<String>,
}

async fn systemctl_show(unit: &str, props: &[&str]) -> HashMap<String, String> {
    let mut cmd = Command::new("systemctl");
    cmd.arg("show").arg(unit);
    for p in props {
        cmd.arg(format!("--property={}", p));
    }
    cmd.arg("--no-pager");

    let output = match cmd.output().await {
        Ok(o) => o,
        Err(e) => {
            warn!(error = %e, unit, "systemctl show failed to spawn");
            return HashMap::new();
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter_map(|line| {
            line.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect()
}

async fn probe_service(unit: &str) -> ServiceStatus {
    let props = systemctl_show(
        unit,
        &[
            "ActiveState",
            "SubState",
            "ActiveEnterTimestamp",
            "MemoryCurrent",
            "ExecMainStatus",
        ],
    )
    .await;

    let active_state = props.get("ActiveState").cloned().unwrap_or_default();
    let sub_state = props.get("SubState").cloned().unwrap_or_default();

    let since_iso = props
        .get("ActiveEnterTimestamp")
        .and_then(|ts| parse_systemd_timestamp(ts));
    let uptime_seconds = since_iso
        .as_ref()
        .and_then(|iso| DateTime::parse_from_rfc3339(iso).ok())
        .map(|ts| (Utc::now() - ts.with_timezone(&Utc)).num_seconds());

    let memory_bytes = props
        .get("MemoryCurrent")
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&v| v > 0);

    let last_exit_status = props.get("ExecMainStatus").cloned();

    ServiceStatus {
        name: unit.to_string(),
        active_state,
        sub_state,
        since_iso,
        uptime_seconds,
        memory_bytes,
        last_exit_status,
    }
}

/// Parse systemd's default timestamp "Day YYYY-MM-DD HH:MM:SS TZ" into
/// RFC-3339. Returns None if empty / unparseable.
fn parse_systemd_timestamp(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() || s == "n/a" {
        return None;
    }
    let rest = s.splitn(2, ' ').nth(1).unwrap_or(s);
    chrono::DateTime::parse_from_str(rest, "%Y-%m-%d %H:%M:%S %Z")
        .ok()
        .or_else(|| chrono::DateTime::parse_from_str(rest, "%Y-%m-%d %H:%M:%S %z").ok())
        .map(|dt| dt.with_timezone(&Utc).to_rfc3339())
}

async fn system_status(State(_): State<AppState>) -> Json<Vec<ServiceStatus>> {
    let units = [
        "rt-daemon.service",
        "rt-dashboard.service",
        "razortrade-signals.timer",
        "razortrade-backup.timer",
        "caddy.service",
        "oauth2-proxy.service",
    ];
    let mut results = Vec::with_capacity(units.len());
    for u in &units {
        results.push(probe_service(u).await);
    }
    Json(results)
}

#[derive(Serialize)]
struct TimerStatus {
    name: String,
    next_elapse_iso: Option<String>,
    last_trigger_iso: Option<String>,
    seconds_until_next: Option<i64>,
    seconds_since_last: Option<i64>,
}

async fn probe_timer(unit: &str) -> TimerStatus {
    let props = systemctl_show(unit, &["NextElapseUSecRealtime", "LastTriggerUSec"]).await;

    let next_elapse_iso = props
        .get("NextElapseUSecRealtime")
        .and_then(|v| parse_systemd_timestamp(v));
    let last_trigger_iso = props
        .get("LastTriggerUSec")
        .and_then(|v| parse_systemd_timestamp(v));

    let seconds_until_next = next_elapse_iso
        .as_ref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|ts| (ts.with_timezone(&Utc) - Utc::now()).num_seconds());
    let seconds_since_last = last_trigger_iso
        .as_ref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|ts| (Utc::now() - ts.with_timezone(&Utc)).num_seconds());

    TimerStatus {
        name: unit.to_string(),
        next_elapse_iso,
        last_trigger_iso,
        seconds_until_next,
        seconds_since_last,
    }
}

async fn timers(State(_): State<AppState>) -> Json<Vec<TimerStatus>> {
    let units = ["razortrade-signals.timer", "razortrade-backup.timer"];
    let mut results = Vec::with_capacity(units.len());
    for u in &units {
        results.push(probe_timer(u).await);
    }
    Json(results)
}

#[derive(Serialize)]
struct Freshness {
    latest_equity_snapshot_iso: Option<String>,
    seconds_since_equity_snapshot: Option<i64>,
    equity_snapshot_count_total: i64,
    latest_signal_iso: Option<String>,
    seconds_since_last_signal: Option<i64>,
    signal_count_total: i64,
    applied_fills_total: i64,
    unresolved_kill_switch_events: i64,
}

async fn freshness(State(state): State<AppState>) -> Json<Freshness> {
    let now = Utc::now();

    let eq_row: Option<(String, i64)> = sqlx::query_as(
        "SELECT timestamp, (SELECT COUNT(*) FROM equity_snapshots) FROM equity_snapshots ORDER BY id DESC LIMIT 1"
    )
    .fetch_optional(&state.pool)
    .await
    .unwrap_or(None);
    let (latest_equity_snapshot_iso, equity_snapshot_count_total) = match eq_row {
        Some((ts, count)) => (Some(ts), count),
        None => (None, 0),
    };
    let seconds_since_equity_snapshot = latest_equity_snapshot_iso
        .as_ref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|ts| (now - ts.with_timezone(&Utc)).num_seconds());

    let sig_row: Option<(String, i64)> = sqlx::query_as(
        "SELECT created_at, (SELECT COUNT(*) FROM signals) FROM signals ORDER BY id DESC LIMIT 1"
    )
    .fetch_optional(&state.pool)
    .await
    .unwrap_or(None);
    let (latest_signal_iso, signal_count_total) = match sig_row {
        Some((ts, count)) => (Some(ts), count),
        None => (None, 0),
    };
    let seconds_since_last_signal = latest_signal_iso
        .as_ref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|ts| (now - ts.with_timezone(&Utc)).num_seconds());

    let applied_fills_total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM applied_fills")
        .fetch_one(&state.pool)
        .await
        .unwrap_or(0);

    let unresolved_kill_switch_events: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM kill_switch_events WHERE resolved_at IS NULL")
            .fetch_one(&state.pool)
            .await
            .unwrap_or(0);

    Json(Freshness {
        latest_equity_snapshot_iso,
        seconds_since_equity_snapshot,
        equity_snapshot_count_total,
        latest_signal_iso,
        seconds_since_last_signal,
        signal_count_total,
        applied_fills_total,
        unresolved_kill_switch_events,
    })
}

#[derive(Serialize)]
struct DiskUsage {
    path: String,
    used_bytes: u64,
    total_bytes: u64,
    available_bytes: u64,
    use_percent: u8,
}

async fn disk_usage(State(_): State<AppState>) -> Json<Vec<DiskUsage>> {
    let targets = ["/var/lib/razortrade", "/"];
    let mut results = Vec::with_capacity(targets.len());

    for path in targets {
        let output = Command::new("df")
            .args(["-B1", "--output=target,used,size,avail,pcent", path])
            .output()
            .await;
        let Ok(o) = output else { continue };
        let stdout = String::from_utf8_lossy(&o.stdout);
        let Some(line) = stdout.lines().nth(1) else {
            continue;
        };
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 5 {
            continue;
        }
        results.push(DiskUsage {
            path: fields[0].to_string(),
            used_bytes: fields[1].parse().unwrap_or(0),
            total_bytes: fields[2].parse().unwrap_or(0),
            available_bytes: fields[3].parse().unwrap_or(0),
            use_percent: fields[4].trim_end_matches('%').parse().unwrap_or(0),
        });
    }

    Json(results)
}

#[derive(Serialize)]
struct BackupStatus {
    last_run_iso: Option<String>,
    last_exit_status: Option<String>,
    seconds_since_last_run: Option<i64>,
    seconds_until_next_run: Option<i64>,
    local_backup_count: u32,
    local_backup_latest_iso: Option<String>,
    local_backup_latest_bytes: Option<u64>,
}

async fn backup_status(State(_): State<AppState>) -> Json<BackupStatus> {
    let svc_props = systemctl_show(
        "razortrade-backup.service",
        &["ExecMainExitTimestamp", "ExecMainStatus"],
    )
    .await;
    let last_run_iso = svc_props
        .get("ExecMainExitTimestamp")
        .and_then(|v| parse_systemd_timestamp(v));
    let last_exit_status = svc_props.get("ExecMainStatus").cloned();
    let seconds_since_last_run = last_run_iso
        .as_ref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|ts| (Utc::now() - ts.with_timezone(&Utc)).num_seconds());

    let timer_props =
        systemctl_show("razortrade-backup.timer", &["NextElapseUSecRealtime"]).await;
    let seconds_until_next_run = timer_props
        .get("NextElapseUSecRealtime")
        .and_then(|v| parse_systemd_timestamp(v))
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|ts| (ts.with_timezone(&Utc) - Utc::now()).num_seconds());

    // Count local backups + find latest via `ls -l` on the backup dir.
    let output = Command::new("sh")
        .arg("-c")
        .arg("ls -l /var/lib/razortrade/backups/*.sqlite 2>/dev/null | tail -1; ls /var/lib/razortrade/backups/*.sqlite 2>/dev/null | wc -l")
        .output()
        .await;
    let (local_backup_count, local_backup_latest_iso, local_backup_latest_bytes) = match output {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            let lines: Vec<&str> = s.lines().collect();
            let count = lines
                .last()
                .and_then(|l| l.trim().parse::<u32>().ok())
                .unwrap_or(0);
            let (size, date) = lines
                .first()
                .filter(|l| !l.trim().is_empty())
                .and_then(|l| {
                    let fields: Vec<&str> = l.split_whitespace().collect();
                    if fields.len() >= 9 {
                        let size = fields[4].parse::<u64>().ok();
                        let fname = fields.last().copied().unwrap_or("");
                        let date_iso = extract_date_from_filename(fname);
                        Some((size, date_iso))
                    } else {
                        None
                    }
                })
                .unwrap_or((None, None));
            (count, date, size)
        }
        Err(_) => (0, None, None),
    };

    Json(BackupStatus {
        last_run_iso,
        last_exit_status,
        seconds_since_last_run,
        seconds_until_next_run,
        local_backup_count,
        local_backup_latest_iso,
        local_backup_latest_bytes,
    })
}

fn extract_date_from_filename(path: &str) -> Option<String> {
    let fname = path.rsplit('/').next()?;
    let stem = fname.strip_suffix(".sqlite")?;
    if stem.len() == 10 && stem.chars().nth(4) == Some('-') && stem.chars().nth(7) == Some('-') {
        Some(stem.to_string())
    } else {
        None
    }
}

// =========================================================================
// HTML root
// =========================================================================

const INDEX_HTML: &str = include_str!("index.html");

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

// =========================================================================
// Bootstrap
// =========================================================================

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

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
        .route("/api/recent-signals", get(recent_signals))
        .route("/api/system-status", get(system_status))
        .route("/api/timers", get(timers))
        .route("/api/freshness", get(freshness))
        .route("/api/disk", get(disk_usage))
        .route("/api/backup-status", get(backup_status))
        .with_state(state);

    let bind_addr = std::env::var("RT_DASHBOARD_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let addr: SocketAddr = bind_addr.parse()?;

    info!(bind = %addr, "rt-dashboard listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
