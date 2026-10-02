//! The agent's HTTP API: the payment operation and the status, for the app and the dashboard.
//!
//! Every answer is JSON and none is cached.
//!
//! * `GET /api/status` answers 200 with a [`Status`](crate::status::Status). A node that cannot be
//!   asked is not an error: the states are the ones last recorded, `tip_height` is `null` and a
//!   `warning` says why.
//! * `POST /api/pay` takes a [`PayRequest`], as JSON with the content type `application/json`
//!   (a page of another origin cannot send that without the agent's consent). It waits up to 30
//!   seconds for the redemption to be confirmed, and answers 200 with
//!   `{"txid": "<hex>", "vouchers": ["<id>"], "state": "redemption_pending|redeemed"}`, where
//!   `redemption_pending` means that no block holds the transaction yet. A payment that stops
//!   answers `{"error": "<message>", "stage": "<stage>"}`, plus the `txid` when one is involved:
//!   400 for the stages `input`, `policy` and `selection`, which the client can fix, 500 for
//!   `store`, `prover` and `signer`, and 502 for `node` and for `unknown`, where a node that did
//!   not answer a submission leaves it open whether the transaction was taken.
//! * `GET /api/delegated-key` answers `{"public": "<hex>", "secret": "<hex>"}`, and exists only
//!   when the router is built to reveal the key. **It is for the demonstration only**: it hands
//!   the delegated secret key to anyone who can reach the server, so that a judge can try to
//!   misuse the key and see the chain refuse. Without it the path is a 404, like any other.
//!
//! The API has no authentication and sends no CORS headers: a browser page cannot read its
//! answers from another origin, and the dashboard reaches it through its own server. It belongs
//! on a loopback address.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use serde_json::json;

use crate::Agent;
use crate::pay::{PayError, PayRequest, Payment, Stage};

/// The API's routes, for the agent `agent`.
///
/// With `reveal_key` the router also serves `GET /api/delegated-key`, which gives away the
/// delegated secret key: only for the demonstration.
pub fn router(agent: Arc<Agent>, reveal_key: bool) -> Router {
    let mut api = Router::new()
        .route("/api/status", get(status))
        .route("/api/pay", post(pay));
    if reveal_key {
        api = api.route("/api/delegated-key", get(delegated_key));
    }
    api.fallback(not_found)
        .layer(middleware::map_response(no_store))
        .with_state(agent)
}

async fn status(State(agent): State<Arc<Agent>>) -> Response {
    match agent.status().await {
        Ok(status) => Json(status).into_response(),
        Err(error) => failed(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn pay(State(agent): State<Arc<Agent>>, headers: HeaderMap, body: Bytes) -> Response {
    let request = match read_request(&headers, &body) {
        Ok(request) => request,
        Err((status, error)) => return refused(status, &error),
    };
    // The payment runs in a task of its own. A client that hangs up makes the server drop this
    // handler, and that must not cancel a payment between reserving the vouchers and recording
    // what became of them.
    let payment = tokio::spawn(async move { agent.pay(&request, Some(WAIT)).await }).await;
    match payment {
        Ok(Ok(payment)) => {
            println!(
                "paid {} sparks in transaction {}: {}",
                payment.amount(),
                hex::encode(payment.txid.0),
                payment.state
            );
            accepted(&payment)
        }
        Ok(Err(error)) => {
            eprintln!("refused at the {} stage: {}", error.stage, error.message);
            refused(status_of(error.stage), &error)
        }
        Err(error) => {
            let message = format!("the payment task failed: {error}");
            eprintln!("{message}");
            refused(
                StatusCode::INTERNAL_SERVER_ERROR,
                &PayError::new(Stage::Unknown, message),
            )
        }
    }
}

/// Gives away the delegated secret key. Only routed when the key is to be revealed, which is for
/// the demonstration only.
async fn delegated_key(State(agent): State<Arc<Agent>>) -> Response {
    match agent.files().read().await {
        Ok(records) => Json(json!({
            "public": hex::encode(records.public().to_bytes()),
            "secret": hex::encode(records.delegate_key.to_bytes()),
        }))
        .into_response(),
        Err(error) => failed(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn not_found() -> Response {
    failed(StatusCode::NOT_FOUND, "there is no such endpoint")
}

/// Keeps browsers and proxies from storing an answer, which may hold the delegated key.
async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The payment request in `body`, or the answer that refuses it.
fn read_request(headers: &HeaderMap, body: &[u8]) -> Result<PayRequest, (StatusCode, PayError)> {
    let sent_as_json = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_json);
    if !sent_as_json {
        let error = PayError::new(Stage::Input, "the request must be sent as application/json");
        return Err((StatusCode::UNSUPPORTED_MEDIA_TYPE, error));
    }
    serde_json::from_slice(body).map_err(|error| {
        let message = format!("the request body is not a payment request: {error}");
        (
            StatusCode::BAD_REQUEST,
            PayError::new(Stage::Input, message),
        )
    })
}

/// Whether a `Content-Type` header value names JSON, with or without parameters.
fn is_json(value: &str) -> bool {
    value
        .split(';')
        .next()
        .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("application/json"))
}

/// The HTTP status of a payment that stopped at `stage`.
fn status_of(stage: Stage) -> StatusCode {
    match stage {
        Stage::Input | Stage::Policy | Stage::Selection => StatusCode::BAD_REQUEST,
        Stage::Store | Stage::Prover | Stage::Signer => StatusCode::INTERNAL_SERVER_ERROR,
        Stage::Node | Stage::Unknown => StatusCode::BAD_GATEWAY,
    }
}

fn accepted(payment: &Payment) -> Response {
    let vouchers: Vec<String> = payment
        .vouchers
        .iter()
        .map(|voucher| hex::encode(voucher.id))
        .collect();
    let mut body = json!({
        "txid": hex::encode(payment.txid.0),
        "vouchers": vouchers,
        "state": payment.state.as_str(),
    });
    if !payment.warnings.is_empty() {
        body["warnings"] = json!(payment.warnings);
    }
    Json(body).into_response()
}

fn refused(status: StatusCode, error: &PayError) -> Response {
    let mut body = json!({"error": error.message, "stage": error.stage.as_str()});
    if let Some(txid) = error.txid {
        body["txid"] = json!(hex::encode(txid.0));
    }
    (status, Json(body)).into_response()
}

fn failed(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": message}))).into_response()
}

/// How long a payment waits for its redemption to be confirmed.
const WAIT: Duration = Duration::from_secs(30);
