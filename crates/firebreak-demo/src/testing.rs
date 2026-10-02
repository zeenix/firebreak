//! Fixtures shared by the tests: public files, a fake agent and free local ports.

use std::fs;
use std::net::TcpListener as StdListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use tokio::net::TcpListener;

/// A string that stands for a secret: it sits in the fixtures where no secret belongs, and no
/// answer of the dashboard may contain it.
pub const SECRET: &str = "5ec2e7-never-shown";

/// An owner snapshot in the shape the spec gives, with a secret where none belongs.
pub fn owner_status() -> String {
    json!({
        "updated": 1_696_000_000,
        "tip_height": 12,
        "owner_key": SECRET,
        "wallet": {
            "balance": "900",
            "outputs": [{"id": "aa".repeat(32), "qty": "900", "state": "unspent"}],
        },
        "allowances": [{
            "allowance": "0123456789abcdef",
            "merchant": "tf1merchant",
            "delegate": "dd".repeat(32),
            "total": "100",
            "funding_txid": "ff".repeat(32),
            "vouchers": [
                {"id": "11".repeat(32), "qty": "50", "state": "redeemed", "txid": "ee".repeat(32)},
                {"id": "22".repeat(32), "qty": "20", "state": "unspent", "txid": null,
                 "opening": SECRET},
            ],
            "recovery": [{
                "txid": "cc".repeat(32), "vouchers": ["22".repeat(32)], "state": "pending",
            }],
        }],
    })
    .to_string()
}

/// A merchant snapshot in the shape the spec gives.
pub fn merchant_status() -> String {
    json!({
        "updated": 1_696_000_001,
        "tip_height": 12,
        "address": "tf1merchant",
        "receipts": [{"id": "33".repeat(32), "qty": "50", "txid": "ee".repeat(32), "height": 7,
                      "memo": "coffee", "spent": false, "spent_txid": null}],
        "balance": "50",
        "spends": [],
    })
    .to_string()
}

/// A journal of two submissions, a line that is not JSON and a blank line.
pub fn journal() -> String {
    let rejected = json!({
        "time": 200, "actor": "attacker", "action": "attack:redirect", "txid": null, "tx": null,
        "inputs": ["22".repeat(32)], "stage": "prover", "outcome": "rejected",
        "error": "the prover refused to release the token", "note": "pay the attacker instead",
    });
    let accepted = json!({
        "time": 100, "actor": "owner", "action": "fund", "txid": "bb".repeat(32),
        "tx": "not hex", "inputs": [], "stage": "confirmed", "outcome": "accepted",
        "error": null, "note": "fund 100 sparks",
    });
    format!("{accepted}\nthis line is not json\n\n{rejected}\n")
}

/// Writes `contents` to `<data_dir>/public/<name>`.
pub fn write_public(data_dir: &Path, name: &str, contents: &str) {
    let public = data_dir.join("public");
    fs::create_dir_all(&public).expect("create the public directory");
    fs::write(public.join(name), contents).expect("write the file");
}

/// The URL of a local port that nothing listens on.
pub fn unused_url() -> String {
    let listener = StdListener::bind("127.0.0.1:0").expect("a free port");
    let port = listener.local_addr().expect("an address").port();
    format!("http://127.0.0.1:{port}")
}

/// Serves `router` on a free local port, and gives the URL it answers on.
pub async fn serve(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a local port");
    let address = listener.local_addr().expect("an address");
    tokio::spawn(async move { axum::serve(listener, router).await.expect("serve") });
    format!("http://{address}")
}

/// What a fake agent saw.
#[derive(Default)]
pub struct Seen {
    /// The body of the last `POST /api/pay`.
    pub pay_body: Mutex<Option<Vec<u8>>>,
    /// How many times its key was asked for.
    pub key_requests: Mutex<usize>,
}

/// An agent that reports one allowance, refuses every payment the way the real one does when the
/// vouchers do not cover it, and reveals a key when asked.
pub fn fake_agent(seen: Arc<Seen>) -> Router {
    let pay = Arc::clone(&seen);
    let key = Arc::clone(&seen);
    Router::new()
        .route(
            "/api/status",
            get(|| async {
                Json(json!({
                    "delegate": "dd".repeat(32),
                    "secret": SECRET,
                    "tip_height": 12,
                    "allowances": [{
                        "allowance": "0123456789abcdef",
                        "merchant": "tf1merchant",
                        "total": "100",
                        "vouchers": [
                            {"id": "11".repeat(32), "qty": "50", "state": "unspent", "txid": null},
                        ],
                    }],
                }))
            }),
        )
        .route(
            "/api/pay",
            post(move |body: Bytes| async move {
                *pay.pay_body.lock().expect("not poisoned") = Some(body.to_vec());
                let refusal = json!({"error": "insufficient authority", "stage": "selection"});
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    [(CONTENT_TYPE, "application/json")],
                    refusal.to_string(),
                )
            }),
        )
        .route(
            "/api/delegated-key",
            get(move || async move {
                *key.key_requests.lock().expect("not poisoned") += 1;
                Json(json!({"public": "dd".repeat(32), "secret": "ab".repeat(32)}))
            }),
        )
}
