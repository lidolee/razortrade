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
    extract::{Query, State},
    http::HeaderMap,
    response::{sse::{Event, KeepAlive, Sse}, Html, Json},
    routing::get,
    Router,
};
use chrono::{DateTime, Duration, Utc};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{FromRow, SqlitePool};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
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
// Live log streaming (Server-Sent Events)
//
// Shells out to `journalctl -f -u rt-daemon -u razortrade-signals.service
//   -u razortrade-backup.service --output=short-iso --no-pager`
// and streams each line to the browser as an SSE event. The browser's
// EventSource auto-reconnects if the stream drops, so we don't have to
// worry about restart logic on our end.
//
// We deliberately DON'T use --output=json here — plain short-iso is
// human-readable, small, and doesn't need client-side parsing for the
// common case. Heavy parsing would belong in a different endpoint.
// =========================================================================

#[derive(Deserialize)]
struct LogsQuery {
    /// How many historical lines to include before going live. Default 50.
    #[serde(default = "default_tail")]
    tail: u32,
}

fn default_tail() -> u32 {
    50
}

async fn logs_stream(
    State(_): State<AppState>,
    Query(q): Query<LogsQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    use async_stream::stream;

    let tail = q.tail.min(500); // cap to avoid abuse

    // Spawn journalctl as a subprocess. We watch three units and emit
    // the last `tail` lines first, then follow live.
    let stream = stream! {
        let mut child = match Command::new("journalctl")
            .args([
                "--follow",
                "--no-pager",
                "--output=short-iso",
                "-n",
                &tail.to_string(),
                "-u", "rt-daemon.service",
                "-u", "razortrade-signals.service",
                "-u", "razortrade-backup.service",
                "-u", "rt-dashboard.service",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "failed to spawn journalctl");
                yield Ok(Event::default().event("error").data(format!("spawn failed: {}", e)));
                return;
            }
        };

        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                yield Ok(Event::default().event("error").data("no stdout on journalctl"));
                return;
            }
        };

        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();

        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    yield Ok(Event::default().data(line));
                }
                Ok(None) => {
                    // journalctl exited; terminate the SSE stream. Client
                    // will auto-reconnect via EventSource.
                    break;
                }
                Err(e) => {
                    warn!(error = %e, "reading journalctl output");
                    break;
                }
            }
        }

        // Best-effort kill. The SSE client disconnecting should trigger
        // Drop of the stream, which drops `child`, which kills the
        // subprocess via Tokio's kill_on_drop semantics if enabled; we
        // don't enable that, so we reap explicitly.
        let _ = child.kill().await;
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// =========================================================================
// Drop 18B — Widget endpoints
// =========================================================================

/// GET /api/user
///
/// Returns the currently authenticated user's email and display name,
/// extracted from the trusted headers injected by oauth2-proxy.
///
/// Header provenance: `pass_user_headers=true` + `set_xauthrequest=true`
/// in oauth2-proxy.cfg causes the proxy to set `X-Forwarded-Email`,
/// `X-Forwarded-User`, and `X-Forwarded-Preferred-Username` on every
/// upstream request. These are trusted because the rt-dashboard binary
/// listens on 127.0.0.1 only and there is no path into it that bypasses
/// oauth2-proxy.
#[derive(Serialize, Default)]
struct UserInfo {
    email: Option<String>,
    name: Option<String>,
    user: Option<String>,
}

fn header_string(h: &HeaderMap, name: &str) -> Option<String> {
    h.get(name).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
}

async fn user_info(headers: HeaderMap) -> Json<UserInfo> {
    Json(UserInfo {
        email: header_string(&headers, "x-auth-request-email"),
        // Google's preferred_username is the human-facing display name
        // when present (for workspace accounts) or the email local-part
        // otherwise. We surface it as `name` but fall back to the
        // username at render time on the frontend.
        name: header_string(&headers, "x-auth-request-preferred-username"),
        user: header_string(&headers, "x-auth-request-user"),
    })
}

/// GET /api/kill-switch
///
/// Returns the current kill-switch state:
///   - armed (no unresolved events, healthy)
///   - soft_triggered (realised budget exhausted OR nav drawdown)
///   - hard_triggered (effective P&L past hard threshold, panic-close
///     has fired)
/// plus the latest unresolved event detail (for the UI to show the
/// operator what happened).
#[derive(Serialize, FromRow)]
struct KillSwitchEventRow {
    id: i64,
    triggered_at: String,
    reason_kind: String,
    reason_detail_json: String,
    resolved_at: Option<String>,
}

#[derive(Serialize)]
struct KillSwitchStateResponse {
    /// "armed" | "soft_triggered" | "hard_triggered"
    state: String,
    /// The unresolved event if any, oldest-first wins the read.
    active_event: Option<KillSwitchEventRow>,
    /// Last N events (resolved + unresolved, newest first).
    recent_events: Vec<KillSwitchEventRow>,
}

async fn kill_switch_state(
    State(state): State<AppState>,
) -> Json<KillSwitchStateResponse> {
    let active: Option<KillSwitchEventRow> = sqlx::query_as(
        r#"
        SELECT id, triggered_at, reason_kind, reason_detail_json, resolved_at
          FROM kill_switch_events
         WHERE resolved_at IS NULL
         ORDER BY triggered_at ASC
         LIMIT 1
        "#,
    )
    .fetch_optional(&state.pool)
    .await
    .unwrap_or_else(|e| {
        warn!(error = %e, "kill_switch active query failed");
        None
    });

    let recent: Vec<KillSwitchEventRow> = sqlx::query_as(
        r#"
        SELECT id, triggered_at, reason_kind, reason_detail_json, resolved_at
          FROM kill_switch_events
         ORDER BY triggered_at DESC
         LIMIT 10
        "#,
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_else(|e| {
        warn!(error = %e, "kill_switch recent query failed");
        Vec::new()
    });

    let kind = active.as_ref().map(|e| e.reason_kind.as_str()).unwrap_or("");
    let state_label = if active.is_none() {
        "armed"
    } else if kind.starts_with("hard_") {
        "hard_triggered"
    } else {
        "soft_triggered"
    };

    Json(KillSwitchStateResponse {
        state: state_label.to_string(),
        active_event: active,
        recent_events: recent,
    })
}

/// GET /api/event-timeline
///
/// Unified chronological feed of operationally interesting events:
///   - signals (created)
///   - orders (submitted, filled, closed_externally)
///   - applied_fills (reconciled)
///   - kill_switch_events (triggered, resolved)
///
/// All normalised to `{ at, kind, summary, severity }` so the frontend
/// can render them identically.
#[derive(Serialize)]
struct TimelineEntry {
    at: String,
    kind: String,
    summary: String,
    /// "info" | "ok" | "warn" | "err"
    severity: String,
}

async fn event_timeline(
    State(state): State<AppState>,
) -> Json<Vec<TimelineEntry>> {
    let mut entries: Vec<TimelineEntry> = Vec::new();

    // Signals (last 20). Status drives severity.
    if let Ok(rows) = sqlx::query_as::<_, (String, String, String, String)>(
        r#"
        SELECT created_at, instrument_symbol, signal_type, status
          FROM signals
         ORDER BY created_at DESC
         LIMIT 20
        "#,
    )
    .fetch_all(&state.pool)
    .await
    {
        for (at, sym, sig_type, status) in rows {
            let sev = match status.as_str() {
                "approved" | "processed" => "ok",
                "rejected" => "warn",
                "expired" => "warn",
                _ => "info",
            };
            entries.push(TimelineEntry {
                at,
                kind: "signal".into(),
                summary: format!("{sym} · {sig_type} · {status}"),
                severity: sev.into(),
            });
        }
    }

    // Orders (last 20). Status drives severity.
    if let Ok(rows) = sqlx::query_as::<_, (String, String, String, String, String)>(
        r#"
        SELECT created_at, instrument_symbol, side, status, quantity
          FROM orders
         ORDER BY created_at DESC
         LIMIT 20
        "#,
    )
    .fetch_all(&state.pool)
    .await
    {
        for (at, sym, side, status, qty) in rows {
            let sev = match status.as_str() {
                "filled" => "ok",
                "rejected" | "failed" | "closed_externally" => "warn",
                "cancelled" => "info",
                _ => "info",
            };
            entries.push(TimelineEntry {
                at,
                kind: "order".into(),
                summary: format!("{sym} · {side} {qty} · {status}"),
                severity: sev.into(),
            });
        }
    }

    // Kill-switch events.
    if let Ok(rows) = sqlx::query_as::<_, (String, String, Option<String>)>(
        r#"
        SELECT triggered_at, reason_kind, resolved_at
          FROM kill_switch_events
         ORDER BY triggered_at DESC
         LIMIT 20
        "#,
    )
    .fetch_all(&state.pool)
    .await
    {
        for (at, kind, resolved) in rows {
            let sev = if resolved.is_some() { "info" } else { "err" };
            let summary = if resolved.is_some() {
                format!("{kind} · resolved")
            } else {
                format!("{kind} · active")
            };
            entries.push(TimelineEntry {
                at,
                kind: "kill_switch".into(),
                summary,
                severity: sev.into(),
            });
        }
    }

    // Sort newest first, cap at 50.
    entries.sort_by(|a, b| b.at.cmp(&a.at));
    entries.truncate(50);

    Json(entries)
}

/// GET /api/fill-stats
///
/// Drop-18 health metrics. These are extracted from the daemon's
/// journal via `journalctl --no-pager --since "24 hours ago"` and
/// counted by grep. The log stream itself is structured JSON so we
/// count the `message` field values via a fixed set of probes.
///
/// The reason we shell out rather than instrument the daemon directly
/// is that rt-dashboard runs as a separate user (read-only DB access)
/// and adding a shared metrics channel for a low-volume counter would
/// be over-engineering.
#[derive(Serialize, Default)]
struct FillStats {
    applied_fills_24h: i64,
    orphan_catcher_24h: i64,
    unknown_order_warnings_24h: i64,
    ws_deser_failures_24h: i64,
    ws_reconnects_24h: i64,
    applied_fills_total: i64,
}

async fn fill_stats(State(state): State<AppState>) -> Json<FillStats> {
    // Total from DB (cheap).
    let total: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM applied_fills",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap_or((0,));

    // The rest comes from journalctl. Running a single journalctl
    // invocation with a grep is cheap because `--since 24 hours ago`
    // bounds the work. We parse stdout line-counts per pattern.
    async fn count_journal_lines(pattern: &str) -> i64 {
        let out = Command::new("journalctl")
            .args([
                "-u", "rt-daemon",
                "--since", "24 hours ago",
                "--no-pager",
                "--output=cat",
                "-g", pattern,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await;
        match out {
            Ok(o) if o.status.success() => {
                o.stdout.iter().filter(|&&b| b == b'\n').count() as i64
            }
            _ => 0,
        }
    }

    // These patterns match log *messages* emitted by the daemon.
    let applied = count_journal_lines(r#""fill applied to order""#).await;
    let orphan = count_journal_lines(r"orphan-catcher: matched fill").await;
    let unknown = count_journal_lines(
        r#""fill received for unknown order""#,
    ).await;
    let deser = count_journal_lines(
        r"WS message did not match any known variant",
    ).await;
    let reconnect = count_journal_lines(
        r"connection lost",
    ).await;

    Json(FillStats {
        applied_fills_24h: applied,
        orphan_catcher_24h: orphan,
        unknown_order_warnings_24h: unknown,
        ws_deser_failures_24h: deser,
        ws_reconnects_24h: reconnect,
        applied_fills_total: total.0,
    })
}

// =========================================================================
// Turn 4 — additional dashboard endpoints
// =========================================================================

/// GET /api/trade-pipeline
///
/// Returns recent signals alongside any orders that plausibly came
/// from them. This implementation deliberately does NOT rely on a
/// `signal_id` foreign key on the orders table, because that column's
/// existence has not been verified against the live schema. Instead,
/// it joins on a time-proximity window plus instrument and side:
/// an order created within 120 s of a signal on the same instrument
/// and side is treated as belonging to that signal.
///
/// This is **approximate** but safe — the alternative would be to
/// guess at a column name and crash the endpoint on schema mismatch.
/// If the daemon later gains a hard signal_id foreign key we'll
/// replace this with an exact join.
#[derive(Serialize, FromRow)]
struct PipelineOrder {
    id: i64,
    side: String,
    status: String,
    quantity: Option<String>,
    filled_quantity: Option<String>,
    avg_fill_price: Option<String>,
    created_at: String,
}
#[derive(Serialize)]
struct PipelineSignal {
    id: i64,
    created_at: String,
    instrument_symbol: String,
    side: String,
    signal_type: String,
    status: String,
    expires_at: Option<String>,
    rejection_reason: Option<String>,
    orders: Vec<PipelineOrder>,
}

async fn trade_pipeline(
    State(state): State<AppState>,
) -> Json<Vec<PipelineSignal>> {
    // Pull last 15 signals. A signal may have 0 orders (rejected,
    // expired) or 1+ orders (approved, one per broker submit).
    let signals: Vec<(i64, String, String, String, String, String, Option<String>, Option<String>)> =
        sqlx::query_as(
            r#"
            SELECT id, created_at, instrument_symbol, side, signal_type,
                   status, expires_at, rejection_reason
              FROM signals
             ORDER BY id DESC
             LIMIT 15
            "#,
        )
        .fetch_all(&state.pool)
        .await
        .unwrap_or_else(|e| {
            warn!(error = %e, "pipeline signals query failed");
            Vec::new()
        });

    let mut out: Vec<PipelineSignal> = Vec::with_capacity(signals.len());
    for (id, created_at, instrument_symbol, side, signal_type, status, expires_at, rejection_reason) in signals {
        // Approximate link to downstream orders: same instrument, same side,
        // within -10s / +120s of signal creation. Uses only columns
        // confirmed in recent_orders handler.
        let orders: Vec<PipelineOrder> = sqlx::query_as(
            r#"
            SELECT id, side, status, quantity, filled_quantity,
                   avg_fill_price, created_at
              FROM orders
             WHERE instrument_symbol = ?
               AND side = ?
               AND created_at BETWEEN
                   datetime(?, '-10 seconds')
                   AND datetime(?, '+120 seconds')
             ORDER BY id ASC
            "#,
        )
        .bind(&instrument_symbol)
        .bind(&side)
        .bind(&created_at)
        .bind(&created_at)
        .fetch_all(&state.pool)
        .await
        .unwrap_or_else(|e| {
            warn!(error = %e, signal_id = id, "pipeline orders query failed");
            Vec::new()
        });

        out.push(PipelineSignal {
            id,
            created_at,
            instrument_symbol,
            side,
            signal_type,
            status,
            expires_at,
            rejection_reason,
            orders,
        });
    }

    Json(out)
}

/// GET /api/signal-stats
///
/// Aggregates of the signals table: how many pending/approved/rejected/
/// processed/expired overall and for the last 24h. Used by the
/// success-rate widget.
#[derive(Serialize, Default, FromRow)]
struct SignalStats {
    total: i64,
    approved: i64,
    rejected: i64,
    processed: i64,
    expired: i64,
    pending: i64,
    last_24h_total: i64,
    last_24h_approved: i64,
    last_24h_rejected: i64,
}

async fn signal_stats(State(state): State<AppState>) -> Json<SignalStats> {
    // One row with all counts. Cheaper than eight queries.
    let row: Option<SignalStats> = sqlx::query_as(
        r#"
        SELECT
            COUNT(*)                                                           AS total,
            SUM(CASE WHEN status = 'approved'  THEN 1 ELSE 0 END)              AS approved,
            SUM(CASE WHEN status = 'rejected'  THEN 1 ELSE 0 END)              AS rejected,
            SUM(CASE WHEN status = 'processed' THEN 1 ELSE 0 END)              AS processed,
            SUM(CASE WHEN status = 'expired'   THEN 1 ELSE 0 END)              AS expired,
            SUM(CASE WHEN status = 'pending'   THEN 1 ELSE 0 END)              AS pending,
            SUM(CASE WHEN created_at >= datetime('now','-24 hours') THEN 1 ELSE 0 END)                                      AS last_24h_total,
            SUM(CASE WHEN created_at >= datetime('now','-24 hours') AND status = 'approved' THEN 1 ELSE 0 END)              AS last_24h_approved,
            SUM(CASE WHEN created_at >= datetime('now','-24 hours') AND status = 'rejected' THEN 1 ELSE 0 END)              AS last_24h_rejected
          FROM signals
        "#,
    )
    .fetch_optional(&state.pool)
    .await
    .unwrap_or_else(|e| {
        warn!(error = %e, "signal_stats query failed");
        None
    });

    Json(row.unwrap_or_default())
}

// =========================================================================
// HTML root
// =========================================================================

const INDEX_HTML: &str = include_str!("index.html");
const LOGO_DARK:  &[u8] = include_bytes!("assets/logo-dark.svg");
const LOGO_LIGHT: &[u8] = include_bytes!("assets/logo-light.svg");

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}
async fn logo_dark() -> ([(axum::http::HeaderName, &'static str); 2], &'static [u8]) {
    (
        [
            (axum::http::header::CONTENT_TYPE, "image/svg+xml"),
            (axum::http::header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        LOGO_DARK,
    )
}
async fn logo_light() -> ([(axum::http::HeaderName, &'static str); 2], &'static [u8]) {
    (
        [
            (axum::http::header::CONTENT_TYPE, "image/svg+xml"),
            (axum::http::header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        LOGO_LIGHT,
    )
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
        .route("/assets/logo-dark.svg", get(logo_dark))
        .route("/assets/logo-light.svg", get(logo_light))
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
        .route("/api/logs/stream", get(logs_stream))
        .route("/api/user", get(user_info))
        .route("/api/kill-switch", get(kill_switch_state))
        .route("/api/event-timeline", get(event_timeline))
        .route("/api/fill-stats", get(fill_stats))
        .route("/api/trade-pipeline", get(trade_pipeline))
        .route("/api/signal-stats", get(signal_stats))
        .with_state(state);

    let bind_addr = std::env::var("RT_DASHBOARD_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let addr: SocketAddr = bind_addr.parse()?;

    info!(bind = %addr, "rt-dashboard listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
