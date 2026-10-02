//! A node's JSON-RPC interface behind a proxy that fails submissions on demand.
//!
//! Everything the agent asks goes through to the real node, except the submissions that were
//! given a fault: those are refused, lost or answered wrongly the way a node or a network can.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// The JSON-RPC error code of a transaction the mempool refuses.
const MEMPOOL_REJECTED: i64 = -32002;

/// What the node says when a membership proof is out of date.
pub const STALE: &str = "Item proof is outdated and must be re-created against the new state";

/// How a submission goes wrong.
pub enum Fault {
    /// The node refuses the transaction with this message.
    Refuse(String),
    /// Something happens first, such as a block that spends a voucher, and then the node refuses
    /// the transaction with this message.
    RefuseAfter(String, Effect),
    /// The node takes the transaction, and the answer is lost on the way back.
    LoseAnswer,
    /// The request is lost on the way: the node never sees the transaction.
    LoseRequest,
}

/// Something that happens in the world while a submission is on its way.
pub type Effect = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

/// A proxy in front of a node.
pub struct Proxy {
    url: String,
    shared: Arc<Shared>,
}

struct Shared {
    node: String,
    faults: Mutex<VecDeque<Fault>>,
    calls: Mutex<Vec<String>>,
    client: Client<HttpConnector, Full<Bytes>>,
}

impl Proxy {
    /// A proxy in front of the node at `node`, serving on a free local port.
    pub async fn start(node: &str) -> Proxy {
        let shared = Arc::new(Shared {
            node: node.to_owned(),
            faults: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
            client: Client::builder(TokioExecutor::new())
                .pool_max_idle_per_host(0)
                .build_http(),
        });
        let router = Router::new()
            .route("/", post(relay))
            .with_state(Arc::clone(&shared));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a port");
        let address = listener.local_addr().expect("an address");
        tokio::spawn(async move { axum::serve(listener, router).await.expect("serve") });
        Proxy {
            url: format!("http://{address}"),
            shared,
        }
    }

    /// The URL to give the agent instead of the node's.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Makes the next submission that has no fault yet go wrong in the way `fault` says.
    pub fn inject(&self, fault: Fault) {
        self.shared
            .faults
            .lock()
            .expect("not poisoned")
            .push_back(fault);
    }

    /// The JSON-RPC methods the agent called, in order.
    pub fn calls(&self) -> Vec<String> {
        self.shared.calls.lock().expect("not poisoned").clone()
    }
}

async fn relay(State(shared): State<Arc<Shared>>, body: Bytes) -> Response {
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let method = request["method"].as_str().unwrap_or_default().to_owned();
    shared
        .calls
        .lock()
        .expect("not poisoned")
        .push(method.clone());
    let fault = if method == "submit_tx" {
        shared.faults.lock().expect("not poisoned").pop_front()
    } else {
        None
    };
    match fault {
        Some(Fault::Refuse(message)) => refusal(&request, &message),
        Some(Fault::RefuseAfter(message, effect)) => {
            effect().await;
            refusal(&request, &message)
        }
        Some(Fault::LoseAnswer) => {
            forward(&shared, body).await;
            StatusCode::BAD_GATEWAY.into_response()
        }
        Some(Fault::LoseRequest) => StatusCode::BAD_GATEWAY.into_response(),
        None => forward(&shared, body).await,
    }
}

/// The node's answer to `body`, exactly as it came.
async fn forward(shared: &Shared, body: Bytes) -> Response {
    let request = Request::builder()
        .method("POST")
        .uri(&shared.node)
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(body))
        .expect("a request");
    let response = shared
        .client
        .request(request)
        .await
        .expect("the node answers");
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.expect("a body").to_bytes();
    (parts.status, [(CONTENT_TYPE, "application/json")], bytes).into_response()
}

/// The JSON-RPC answer of a mempool that refuses the transaction of `request`.
fn refusal(request: &Value, message: &str) -> Response {
    let answer = json!({
        "jsonrpc": "2.0",
        "id": request["id"],
        "error": {"code": MEMPOOL_REJECTED, "message": message},
    });
    (
        StatusCode::OK,
        [(CONTENT_TYPE, "application/json")],
        answer.to_string(),
    )
        .into_response()
}
