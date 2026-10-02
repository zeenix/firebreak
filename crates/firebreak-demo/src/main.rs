//! The Firebreak demo dashboard: one local page and the small JSON API behind it.
//!
//! The page shows four views of one allowance: the owner, the app that holds the delegated key,
//! the merchant and a public observer. The server gathers their data (see [`state`]), relays the
//! page's purchase and key-reveal requests to the payment agent, and holds no key of its own.

mod agent;
mod decode;
mod explain;
mod journal;
mod node;
mod state;
mod status;
#[cfg(test)]
mod testing;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use serde_json::json;
use tokio::net::TcpListener;

use crate::agent::Reply;
use crate::state::App;

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("firebreak-demo: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let app = Arc::new(App::new(cli.data_dir.clone(), &cli.rpc, &cli.agent)?);
    let listener = TcpListener::bind(cli.listen)
        .await
        .map_err(|error| format!("cannot listen on {}: {error}", cli.listen))?;
    if !cli.listen.ip().is_loopback() {
        eprintln!(
            "warning: {} is not a loopback address; anyone who can reach it can make the agent \
             pay and ask it for the demo key",
            cli.listen
        );
    }
    println!(
        "LOCAL DEVNET dashboard on http://{} (data {}, node {}, agent {})",
        cli.listen,
        cli.data_dir.display(),
        cli.rpc,
        cli.agent
    );
    axum::serve(listener, router(app))
        .await
        .map_err(|error| format!("the server stopped: {error}"))
}

/// The page and its API.
fn router(app: Arc<App>) -> Router {
    let api = Router::new()
        .route("/api/state", get(dashboard_state))
        .route("/api/pay", post(pay))
        .route("/api/reveal-key", post(reveal_key))
        .layer(middleware::map_response(no_store));
    Router::new()
        .route("/", get(page))
        .merge(api)
        .with_state(app)
}

async fn page() -> impl IntoResponse {
    (
        [
            (CONTENT_TYPE, "text/html; charset=utf-8"),
            (CONTENT_SECURITY_POLICY, PAGE_POLICY),
            (X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (REFERRER_POLICY, "no-referrer"),
            (CACHE_CONTROL, "no-store"),
        ],
        PAGE,
    )
}

async fn dashboard_state(State(app): State<Arc<App>>) -> Response {
    match app.state().await {
        Ok(state) => Json(state).into_response(),
        Err(message) => {
            let body = Json(json!({ "error": message }));
            (StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
        }
    }
}

/// Relays a purchase request to the agent, body and answer untouched.
async fn pay(State(app): State<Arc<App>>, body: Bytes) -> Response {
    relay(app.agent().post("/api/pay", body, PAY_TIMEOUT).await)
}

/// Asks the agent for its delegated key. The page calls this only when the person clicks the
/// reveal button, and the answer, which holds the secret, goes to that page and nowhere else.
async fn reveal_key(State(app): State<Arc<App>>) -> Response {
    relay(app.agent().get("/api/delegated-key", REVEAL_TIMEOUT).await)
}

/// The agent's answer exactly as it came, or an error in the agent's own shape when there was
/// none.
fn relay(answer: Result<Reply, String>) -> Response {
    match answer {
        Ok(reply) => {
            let content_type = reply
                .content_type
                .unwrap_or_else(|| HeaderValue::from_static("application/json"));
            (reply.status, [(CONTENT_TYPE, content_type)], reply.body).into_response()
        }
        Err(message) => {
            let body = Json(json!({
                "error": format!("the agent is unreachable: {message}"),
                "stage": "unknown",
            }));
            (StatusCode::BAD_GATEWAY, body).into_response()
        }
    }
}

/// Keeps browsers and proxies from storing an API answer, which may hold the delegated key.
async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The local demo dashboard for Firebreak.
#[derive(Debug, Parser)]
#[command(version)]
struct Cli {
    /// The data directory the roles write their public files to.
    #[arg(long, default_value = ".firebreak")]
    data_dir: PathBuf,

    /// The Flame node's JSON-RPC URL.
    #[arg(long, default_value = "http://127.0.0.1:7740")]
    rpc: String,

    /// The payment agent's HTTP URL.
    #[arg(long, default_value = "http://127.0.0.1:7741")]
    agent: String,

    /// The address to serve the dashboard on.
    #[arg(long, default_value = "127.0.0.1:7742")]
    listen: SocketAddr,
}

/// The page, embedded so the dashboard needs nothing but this binary.
const PAGE: &str = include_str!("index.html");

/// What the page may load and run: its own inline script and style, and requests to this server.
/// Nothing is fetched from anywhere else.
const PAGE_POLICY: &str = "default-src 'none'; script-src 'unsafe-inline'; \
    style-src 'unsafe-inline'; connect-src 'self'; img-src data:; base-uri 'none'; \
    form-action 'none'; frame-ancestors 'none'";

/// How long a purchase may take. The agent waits up to 30 seconds for confirmation.
const PAY_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the agent gets to reveal its key.
const REVEAL_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests {
    use std::path::Path;

    use axum::body::{self, Body};
    use tempfile::tempdir;

    use super::*;
    use crate::agent::Agent;
    use crate::testing::{self, SECRET, Seen};

    const REQUEST: &[u8] =
        br#"{"allowance":"0123456789abcdef","merchant":"tf1merchant","amount":"110"}"#;
    const WAIT: Duration = Duration::from_secs(10);

    /// A running dashboard over `data_dir` whose agent is at `agent`, and a client for it.
    async fn dashboard(data_dir: &Path, agent: &str) -> Agent {
        let app = App::new(data_dir.to_path_buf(), &testing::unused_url(), agent);
        let url = testing::serve(router(Arc::new(app.expect("an app")))).await;
        Agent::new(&url)
    }

    #[tokio::test]
    async fn the_page_is_one_document_that_loads_nothing_from_elsewhere() {
        let response = page().await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let header = |name| response.headers()[name].to_str().expect("text").to_owned();
        assert_eq!(header(CONTENT_TYPE), "text/html; charset=utf-8");
        let policy = header(CONTENT_SECURITY_POLICY);
        assert!(policy.starts_with("default-src 'none'"), "{policy}");
        assert!(policy.contains("connect-src 'self'"), "{policy}");
        assert!(!policy.contains("http"), "{policy}");

        let bytes = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a body");
        let text = String::from_utf8(bytes.to_vec()).expect("UTF-8");
        for title in ["Owner", "App", "Merchant", "Public observer"] {
            assert!(
                text.contains(&format!(">{title} <span class=\"who\">")),
                "no {title} view"
            );
        }
        for wanted in [
            "LOCAL DEVNET — not production",
            "Reveal delegated key",
            "amount: not public",
        ] {
            assert!(text.contains(wanted), "the page lacks {wanted:?}");
        }
        // No CDN, no font host, no linked script or style, no import.
        for external in [
            "http://",
            "https://",
            "//cdn",
            "src=",
            "@import",
            "rel=\"stylesheet\"",
        ] {
            assert!(!text.contains(external), "the page refers to {external:?}");
        }
    }

    #[tokio::test]
    async fn the_page_is_served_at_the_root() {
        let dir = tempdir().expect("a directory");
        let client = dashboard(dir.path(), &testing::unused_url()).await;
        let reply = client.get("/", WAIT).await.expect("a reply");

        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.body.starts_with(b"<!doctype html>"));
        let kind = reply.content_type.expect("a content type");
        assert_eq!(kind, "text/html; charset=utf-8");
    }

    #[tokio::test]
    async fn a_purchase_is_relayed_to_the_agent_and_its_answer_comes_back_untouched() {
        let dir = tempdir().expect("a directory");
        let seen = Arc::new(Seen::default());
        let agent = testing::serve(testing::fake_agent(Arc::clone(&seen))).await;
        let client = dashboard(dir.path(), &agent).await;
        let reply = client
            .post("/api/pay", Bytes::from_static(REQUEST), WAIT)
            .await;
        let reply = reply.expect("a reply");

        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
        let refusal = br#"{"error":"insufficient authority","stage":"selection"}"#;
        assert_eq!(&reply.body[..], &refusal[..]);
        assert_eq!(
            reply.content_type.expect("a content type"),
            "application/json"
        );
        let sent = seen.pay_body.lock().expect("not poisoned").clone();
        assert_eq!(
            sent.as_deref(),
            Some(REQUEST),
            "the agent got the body as it was"
        );
    }

    #[tokio::test]
    async fn the_delegated_key_is_asked_for_only_by_an_explicit_post() {
        let dir = tempdir().expect("a directory");
        let seen = Arc::new(Seen::default());
        let agent = testing::serve(testing::fake_agent(Arc::clone(&seen))).await;
        let client = dashboard(dir.path(), &agent).await;
        let asked = || *seen.key_requests.lock().expect("not poisoned");

        // Neither a GET nor the state the page polls every two seconds touches the key.
        let reply = client.get("/api/reveal-key", WAIT).await.expect("a reply");
        assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
        let reply = client.get("/api/state", WAIT).await.expect("a reply");
        assert!(!String::from_utf8_lossy(&reply.body).contains(&"ab".repeat(32)));
        assert_eq!(asked(), 0);

        let reply = client
            .post("/api/reveal-key", Bytes::new(), WAIT)
            .await
            .expect("a reply");
        assert_eq!(reply.status, StatusCode::OK);
        let key: serde_json::Value = serde_json::from_slice(&reply.body).expect("JSON");
        assert_eq!(key["secret"], "ab".repeat(32));
        assert_eq!(key["public"], "dd".repeat(32));
        assert_eq!(asked(), 1);
    }

    #[tokio::test]
    async fn an_unreachable_agent_is_a_bad_gateway_in_the_agents_own_error_shape() {
        let dir = tempdir().expect("a directory");
        let client = dashboard(dir.path(), &testing::unused_url()).await;

        for reply in [
            client
                .post("/api/pay", Bytes::from_static(REQUEST), WAIT)
                .await,
            client.post("/api/reveal-key", Bytes::new(), WAIT).await,
        ] {
            let reply = reply.expect("a reply");
            assert_eq!(reply.status, StatusCode::BAD_GATEWAY);
            let answer: serde_json::Value = serde_json::from_slice(&reply.body).expect("JSON");
            let error = answer["error"].as_str().expect("an error message");
            assert!(error.starts_with("the agent is unreachable: "), "{error}");
            assert_eq!(answer["stage"], "unknown");
        }
    }

    #[tokio::test]
    async fn the_state_endpoint_answers_with_json_whatever_is_down() {
        let dir = tempdir().expect("a directory");
        testing::write_public(dir.path(), "owner-status.json", &testing::owner_status());
        testing::write_public(dir.path(), "journal.jsonl", &testing::journal());
        let client = dashboard(dir.path(), &testing::unused_url()).await;
        let reply = client.get("/api/state", WAIT).await.expect("a reply");

        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            reply.content_type.expect("a content type"),
            "application/json"
        );
        let text = String::from_utf8(reply.body.to_vec()).expect("UTF-8");
        assert!(!text.contains(SECRET), "{text}");
        let state: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert!(state["generated"].as_u64().expect("a time") > 1_696_000_000);
        let states: Vec<&str> = ["node", "agent", "owner", "merchant", "observer"]
            .iter()
            .map(|source| state[*source]["state"].as_str().expect("a state"))
            .collect();
        assert_eq!(states, ["error", "error", "ok", "missing", "ok"]);
        assert_eq!(state["merchant"]["message"], "not initialised yet");
        assert_eq!(state["owner"]["data"]["wallet"]["balance"], "900");
        let entries = state["observer"]["data"]["entries"]
            .as_array()
            .expect("entries");
        assert_eq!(
            entries[0]["error"],
            "the prover refused to release the token"
        );
    }

    #[tokio::test]
    async fn an_answer_of_the_api_is_never_stored() {
        let response = no_store(Response::new(Body::empty())).await;
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
    }
}
