//! Kraken Futures REST client.
//!
//! Implements [`rt_execution::Broker`] for order routing. Every
//! authenticated request goes through [`crate::auth::sign_rest_request`]
//! and attaches `APIKey` + `Authent` + `Nonce` headers.

use crate::auth::{sign_rest_request, NonceGenerator};
use crate::messages::{
    CancelOrderResponse, KrakenOrderType, KrakenSide, OpenOrdersResponse, ResponseEnvelope,
    SendOrderRequest, SendOrderResponse,
};
use crate::{Credentials, KrakenError, KrakenResult};
use async_trait::async_trait;
use chrono::Utc;
use rt_core::order::{Order, OrderStatus, OrderType, Side};
use rt_core::Broker;
use rt_execution::{
    Broker as ExecutionBroker, ExecutionError, OpenOrderSummary, SubmissionResult,
};
use reqwest::{Client, StatusCode};
use rust_decimal::Decimal;
use std::time::Duration;
use tracing::{debug, instrument, warn};

const REST_API_PREFIX: &str = "/derivatives";

/// The production Kraken Futures REST client.
pub struct KrakenFuturesRestClient {
    base_url: String,
    credentials: Option<Credentials>,
    http: Client,
    nonces: NonceGenerator,
}

impl KrakenFuturesRestClient {
    pub fn new(base_url: impl Into<String>, credentials: Option<Credentials>) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("razortrade/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client builder is infallible with these settings");

        Self {
            base_url: base_url.into(),
            credentials,
            http,
            nonces: NonceGenerator::new(),
        }
    }

    pub fn production(credentials: Option<Credentials>) -> Self {
        Self::new(crate::KRAKEN_FUTURES_REST_BASE, credentials)
    }

    pub fn demo(credentials: Option<Credentials>) -> Self {
        Self::new(crate::KRAKEN_FUTURES_DEMO_REST_BASE, credentials)
    }

    fn creds(&self) -> Result<&Credentials, ExecutionError> {
        self.credentials
            .as_ref()
            .ok_or_else(|| ExecutionError::Authentication("no credentials configured".to_string()))
    }

    /// Perform an authenticated GET. `path` is the endpoint suffix
    /// (e.g. `/api/v3/openorders`); `query` is the raw query string
    /// without the leading `?`, or empty for no params.
    async fn authed_get(&self, path: &str, query: &str) -> KrakenResult<String> {
        let creds = self.creds().map_err(|e| KrakenError::Api {
            code: None,
            message: e.to_string(),
        })?;
        let nonce = self.nonces.next()?;

        // The full endpoint path for the HTTP request URL.
        let endpoint_path = format!("{}{}", REST_API_PREFIX, path);

        // Kraken's signing requires the path WITHOUT the /derivatives prefix,
        // even though the URL includes it. The official Python client
        // (cfRestApiV3.py::sign_message) strips '/derivatives' before hashing.
        // For GET /accounts Kraken tolerates either variant, but for POST
        // endpoints like /sendorder it does not — signing with the full
        // path yields `authenticationError`. To be safe and consistent,
        // always sign the stripped path.
        let authent = sign_rest_request(&creds.api_secret_b64, query, &nonce, path)?;

        let mut url = format!("{}{}", self.base_url, endpoint_path);
        if !query.is_empty() {
            url.push('?');
            url.push_str(query);
        }

        debug!(%url, "authenticated GET");
        let response = self
            .http
            .get(&url)
            .header("APIKey", &creds.api_key)
            .header("Nonce", &nonce)
            .header("Authent", &authent)
            .send()
            .await?;

        self.read_response(response).await
    }

    /// Perform an authenticated POST with a JSON body.
    /// Authenticated POST with a JSON body. Used only by endpoints that
    /// Kraken actually documents as accepting JSON (currently just the
    /// batchorder endpoint, which takes a JSON string in the `json` query
    /// parameter — not the HTTP body). Most trading endpoints (sendorder,
    /// cancelorder, etc.) use [`Self::authed_post_form`] instead because
    /// Kraken Futures signs and expects form-encoded post data per
    /// <https://docs.kraken.com/api/docs/guides/futures-rest/>.
    #[allow(dead_code)]
    async fn authed_post_json(&self, path: &str, body_json: &str) -> KrakenResult<String> {
        let creds = self.creds().map_err(|e| KrakenError::Api {
            code: None,
            message: e.to_string(),
        })?;
        let nonce = self.nonces.next()?;
        let endpoint_path = format!("{}{}", REST_API_PREFIX, path);
        // See note in authed_get: sign the path WITHOUT /derivatives prefix.
        let authent =
            sign_rest_request(&creds.api_secret_b64, body_json, &nonce, path)?;

        let url = format!("{}{}", self.base_url, endpoint_path);
        debug!(%url, "authenticated POST (JSON)");
        let response = self
            .http
            .post(&url)
            .header("APIKey", &creds.api_key)
            .header("Nonce", &nonce)
            .header("Authent", &authent)
            .header("Content-Type", "application/json")
            .body(body_json.to_string())
            .send()
            .await?;

        self.read_response(response).await
    }

    /// Authenticated POST with form-encoded body.
    ///
    /// This is the correct shape for Kraken Futures trading endpoints like
    /// `sendorder` and `cancelorder`. The signed `postData` is the exact
    /// form-encoded string that is also sent as the request body.
    async fn authed_post_form(&self, path: &str, form_body: &str) -> KrakenResult<String> {
        let creds = self.creds().map_err(|e| KrakenError::Api {
            code: None,
            message: e.to_string(),
        })?;
        let nonce = self.nonces.next()?;
        let endpoint_path = format!("{}{}", REST_API_PREFIX, path);
        // See note in authed_get: sign the path WITHOUT /derivatives prefix.
        let authent =
            sign_rest_request(&creds.api_secret_b64, form_body, &nonce, path)?;

        let url = format!("{}{}", self.base_url, endpoint_path);
        debug!(%url, body = form_body, "authenticated POST (form)");
        let response = self
            .http
            .post(&url)
            .header("APIKey", &creds.api_key)
            .header("Nonce", &nonce)
            .header("Authent", &authent)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(form_body.to_string())
            .send()
            .await?;

        self.read_response(response).await
    }

    /// Unauthenticated public GET (e.g. /instruments).
    pub async fn public_get(&self, path: &str) -> KrakenResult<String> {
        let url = format!("{}{}{}", self.base_url, REST_API_PREFIX, path);
        debug!(%url, "public GET");
        let response = self.http.get(&url).send().await?;
        self.read_response(response).await
    }

    async fn read_response(&self, response: reqwest::Response) -> KrakenResult<String> {
        let status = response.status();
        let body = response.text().await?;

        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(KrakenError::Api {
                code: Some("rate_limited".to_string()),
                message: body,
            });
        }

        if !status.is_success() {
            warn!(%status, body, "non-2xx response from kraken");
            return Err(KrakenError::Api {
                code: Some(status.as_u16().to_string()),
                message: body,
            });
        }

        // Kraken returns 200 OK with a JSON body even on API-level errors,
        // distinguished by the `result` field. The caller is responsible
        // for parsing and inspecting that field.
        Ok(body)
    }

    // =============================================================
    // Typed endpoint wrappers
    // =============================================================

    #[instrument(skip(self))]
    pub async fn send_order(&self, req: &SendOrderRequest) -> KrakenResult<SendOrderResponse> {
        // Kraken Futures sendorder expects form-encoded body, NOT JSON.
        // The auth signature is computed over the same form-encoded string.
        let body = serde_urlencoded::to_string(req).map_err(|e| KrakenError::Api {
            code: None,
            message: format!("failed to urlencode SendOrderRequest: {e}"),
        })?;
        let text = self.authed_post_form("/api/v3/sendorder", &body).await?;
        let parsed: SendOrderResponse = serde_json::from_str(&text)?;
        if !matches!(parsed.result.as_str(), "success") {
            return Err(KrakenError::Api {
                code: Some(parsed.result.clone()),
                message: parsed.error.clone().unwrap_or_default(),
            });
        }
        Ok(parsed)
    }

    #[instrument(skip(self))]
    pub async fn cancel_order_raw(&self, order_id: &str) -> KrakenResult<CancelOrderResponse> {
        // cancelorder also uses form-encoded body.
        let body = serde_urlencoded::to_string([("order_id", order_id)]).map_err(|e| {
            KrakenError::Api {
                code: None,
                message: format!("failed to urlencode cancel params: {e}"),
            }
        })?;
        let text = self.authed_post_form("/api/v3/cancelorder", &body).await?;
        let parsed: CancelOrderResponse = serde_json::from_str(&text)?;
        if !matches!(parsed.result.as_str(), "success") {
            return Err(KrakenError::Api {
                code: Some(parsed.result.clone()),
                message: parsed.error.clone().unwrap_or_default(),
            });
        }
        Ok(parsed)
    }

    #[instrument(skip(self))]
    pub async fn open_orders_raw(&self) -> KrakenResult<OpenOrdersResponse> {
        let text = self.authed_get("/api/v3/openorders", "").await?;
        let parsed: OpenOrdersResponse = serde_json::from_str(&text)?;
        if !matches!(parsed.result.as_str(), "success") {
            return Err(KrakenError::Api {
                code: Some(parsed.result.clone()),
                message: parsed.error.clone().unwrap_or_default(),
            });
        }
        Ok(parsed)
    }

    #[instrument(skip(self))]
    pub async fn server_time(&self) -> KrakenResult<ResponseEnvelope> {
        let text = self.public_get("/api/v3/accounts").await?;
        let parsed: ResponseEnvelope = serde_json::from_str(&text)?;
        Ok(parsed)
    }

    /// Fetch the full `/accounts` payload, parsed into a typed
    /// [`AccountsResponse`]. Used by the equity writer (Drop 12) to
    /// snapshot cash and margin state once per minute.
    ///
    /// Unlike [`Self::server_time`] which also targets `/accounts`
    /// for its side effect of being cheap, this method actually
    /// deserialises the response into strongly-typed structs so
    /// callers get `Option<FlexAccount>` / `Option<MarginAccount>`
    /// rather than raw JSON.
    #[instrument(skip(self))]
    pub async fn get_accounts(&self) -> KrakenResult<crate::accounts::AccountsResponse> {
        let text = self.authed_get("/api/v3/accounts", "").await?;
        let parsed: crate::accounts::AccountsResponse = serde_json::from_str(&text)?;
        if !matches!(parsed.result.as_str(), "success") {
            return Err(KrakenError::Api {
                code: Some(parsed.result.clone()),
                message: "accounts endpoint returned non-success result".to_string(),
            });
        }
        Ok(parsed)
    }
}

// =============================================================
// rt_execution::Broker implementation
// =============================================================

fn to_kraken_order_type(ot: OrderType) -> Result<KrakenOrderType, ExecutionError> {
    Ok(match ot {
        OrderType::Market => KrakenOrderType::Mkt,
        OrderType::Limit => KrakenOrderType::Lmt,
        OrderType::PostOnly => KrakenOrderType::Post,
        OrderType::StopMarket => KrakenOrderType::Stp,
        // StopLimit maps to TakeProfit in Kraken's taxonomy, but only for a
        // specific sub-case. Rather than silently coerce, we refuse for now
        // and will add a dedicated mapping when we actually need it.
        OrderType::StopLimit => {
            return Err(ExecutionError::Unsupported(
                "StopLimit not yet mapped to Kraken Futures".into(),
            ))
        }
    })
}

fn to_kraken_side(s: Side) -> KrakenSide {
    match s {
        Side::Buy => KrakenSide::Buy,
        Side::Sell => KrakenSide::Sell,
    }
}

fn from_kraken_status(s: &str) -> OrderStatus {
    match s {
        "placed" | "received" => OrderStatus::Acknowledged,
        "fullyExecuted" | "filled" => OrderStatus::Filled,
        "partiallyFilled" => OrderStatus::PartiallyFilled,
        "cancelled" => OrderStatus::Cancelled,
        "rejected" | "invalid" => OrderStatus::Rejected,
        _ => OrderStatus::Submitted,
    }
}

#[async_trait]
impl ExecutionBroker for KrakenFuturesRestClient {
    fn id(&self) -> Broker {
        Broker::KrakenFutures
    }

    async fn submit_order(&self, order: &Order) -> Result<SubmissionResult, ExecutionError> {
        let req = SendOrderRequest {
            order_type: to_kraken_order_type(order.order_type)?,
            symbol: order.instrument.symbol.clone(),
            side: to_kraken_side(order.side),
            size: order.quantity,
            limit_price: order.limit_price,
            stop_price: order.stop_price,
            // CV-1: send cli_ord_id so a REST timeout on submit is
            // recoverable. Kraken echoes this in fills and open-orders
            // updates, letting fill_reconciler attach fills to this
            // order even without the broker_order_id from the
            // (potentially lost) HTTP response.
            cli_ord_id: order.cli_ord_id.clone(),
            reduce_only: None,
            trigger_signal: None,
        };
        let resp = self
            .send_order(&req)
            .await
            .map_err(|e| map_api_error(e, "submit_order"))?;
        let status = resp
            .send_status
            .as_ref()
            .ok_or_else(|| ExecutionError::InvalidResponse("missing sendStatus".into()))?;
        let status_str = status.status.as_deref().unwrap_or("");

        // Kraken only returns an order_id when the order was actually placed
        // on the book. If order_id is absent, the request was syntactically
        // valid but Kraken refused to place the order. Common reasons:
        //   "insufficientAvailableFunds"
        //   "post_order_failed_because_it_would_be_filled"
        //   "marketSuspended" / "priceRejected"
        // These are business-logic rejections, not transport errors — we
        // surface them as ExecutionError::Rejected so the signal_processor
        // can mark the signal with broker_rejected rather than
        // broker_invalid_response.
        let broker_order_id = match &status.order_id {
            Some(id) => id.clone(),
            None => {
                return Err(ExecutionError::Rejected(format!(
                    "kraken did not place order: status='{status_str}'"
                )));
            }
        };

        Ok(SubmissionResult {
            broker_order_id,
            status: from_kraken_status(status_str),
            submitted_at: Utc::now(),
        })
    }

    async fn cancel_order(&self, broker_order_id: &str) -> Result<(), ExecutionError> {
        self.cancel_order_raw(broker_order_id)
            .await
            .map_err(|e| map_api_error(e, "cancel_order"))?;
        Ok(())
    }

    async fn get_order(&self, broker_order_id: &str) -> Result<OrderStatus, ExecutionError> {
        // Kraken Futures does not have a cheap single-order fetch; we
        // reconcile by listing open orders and falling back to "Filled or
        // Cancelled" if the id is not present.
        let open = self
            .open_orders_raw()
            .await
            .map_err(|e| map_api_error(e, "get_order"))?;
        for o in open.open_orders {
            if o.order_id == broker_order_id {
                return Ok(from_kraken_status(o.status.as_deref().unwrap_or("received")));
            }
        }
        // Not in open list: the daemon's reconciliation logic decides whether
        // that means filled or cancelled by consulting its fills history.
        Ok(OrderStatus::Filled)
    }

    async fn get_order_by_cli_ord_id(
        &self,
        cli_ord_id: &str,
    ) -> Result<Option<OpenOrderSummary>, ExecutionError> {
        // Drop 19 — CV-A1a: Kraken Futures' v3 API has no direct
        // "lookup-by-cli-ord-id" endpoint. We fetch the full open-orders
        // list and filter client-side. At our volume (≤ 3 concurrent
        // orders) this is one REST call per uncertain-resubmit and
        // cheap.
        let resp = self
            .open_orders_raw()
            .await
            .map_err(|e| map_api_error(e, "get_order_by_cli_ord_id"))?;
        for o in resp.open_orders {
            if o.cli_ord_id.as_deref() == Some(cli_ord_id) {
                let filled = o.filled_size.unwrap_or(Decimal::ZERO);
                let remaining = o.unfilled_size.unwrap_or(Decimal::ZERO);
                return Ok(Some(OpenOrderSummary {
                    broker_order_id: o.order_id,
                    instrument_symbol: o.symbol,
                    status: from_kraken_status(o.status.as_deref().unwrap_or("received")),
                    quantity: filled + remaining,
                    filled_quantity: filled,
                    limit_price: o.limit_price,
                    cli_ord_id: o.cli_ord_id,
                }));
            }
        }
        Ok(None)
    }

    async fn open_orders(&self) -> Result<Vec<OpenOrderSummary>, ExecutionError> {
        let resp = self
            .open_orders_raw()
            .await
            .map_err(|e| map_api_error(e, "open_orders"))?;
        let summaries = resp
            .open_orders
            .into_iter()
            .map(|o| {
                let filled = o.filled_size.unwrap_or(Decimal::ZERO);
                let remaining = o.unfilled_size.unwrap_or(Decimal::ZERO);
                OpenOrderSummary {
                    broker_order_id: o.order_id,
                    instrument_symbol: o.symbol,
                    status: from_kraken_status(o.status.as_deref().unwrap_or("received")),
                    quantity: filled + remaining,
                    filled_quantity: filled,
                    limit_price: o.limit_price,
                    cli_ord_id: o.cli_ord_id,
                }
            })
            .collect();
        Ok(summaries)
    }

    async fn health_check(&self) -> Result<(), ExecutionError> {
        // The cheapest authenticated endpoint is GET /accounts (list
        // collateral accounts). Errors map to Authentication for auth-level
        // failures and Network for anything else.
        match self.authed_get("/api/v3/accounts", "").await {
            Ok(_) => Ok(()),
            Err(e) => Err(map_api_error(e, "health_check")),
        }
    }
}

fn map_api_error(e: KrakenError, context: &str) -> ExecutionError {
    match e {
        KrakenError::Api { code, message } => match code.as_deref() {
            Some("rate_limited") => ExecutionError::RateLimited {
                retry_after_ms: 1000,
            },
            Some(c) if c.starts_with("4") => ExecutionError::Rejected(format!("{context}: {message}")),
            _ => ExecutionError::Rejected(format!("{context}: {message}")),
        },
        KrakenError::Http(e) => {
            if e.is_timeout() {
                ExecutionError::Timeout { seconds: 10 }
            } else {
                ExecutionError::Network(e.to_string())
            }
        }
        KrakenError::Auth(e) => ExecutionError::Authentication(e.to_string()),
        KrakenError::Json(e) => ExecutionError::InvalidResponse(e.to_string()),
        other => ExecutionError::Network(other.to_string()),
    }
}
