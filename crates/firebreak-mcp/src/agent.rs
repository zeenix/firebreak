//! The payment agent, reduced to the two requests this server may make.

use std::error::Error;
use std::time::Duration;

use bytes::Bytes;
use http::header::CONTENT_TYPE;
use http::{Request, StatusCode, Uri};
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde::{Deserialize, Serialize};

/// What the model can make the payment agent do: report its status, and pay its merchant.
///
/// The trait has these two methods and no others, so a server written against it can ask nothing
/// else of the agent. `Err` means the agent could not be asked or its answer could not be read,
/// and says why; an answer with any HTTP status, a refusal included, is `Ok`.
pub trait Agent {
    /// Asks for the agent's allowances and their vouchers.
    fn status(&self) -> impl Future<Output = Result<Answer, String>> + Send;

    /// Asks the agent to pay `request` from one of its allowances.
    fn pay(&self, request: &PayRequest) -> impl Future<Output = Result<Answer, String>> + Send;
}

/// A payment as the agent's `POST /api/pay` takes it, and as the `pay` tool's arguments spell it.
///
/// These three fields are all the agent ever receives from the model.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PayRequest {
    /// The merchant's `tf1...` address. The agent refuses any but its allowance's merchant.
    pub merchant: String,
    /// The amount in sparks, as a decimal string.
    pub amount: String,
    /// The allowance to pay from, for an agent that holds more than one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowance: Option<String>,
}

/// What the agent answered: an HTTP status and the body that came with it.
#[derive(Debug)]
pub struct Answer {
    pub status: StatusCode,
    pub body: Bytes,
}

/// The payment agent at one base URL, reached over plain HTTP.
///
/// It asks for exactly two URLs, `GET /api/status` and `POST /api/pay`, and holds no other: the
/// agent's remaining endpoints, such as the demo's key reveal, are not reachable through it.
pub struct HttpAgent {
    status: Uri,
    pay: Uri,
    limit: Duration,
    client: Client<HttpConnector, Full<Bytes>>,
}

impl HttpAgent {
    /// A client for the agent at `base`, such as `http://127.0.0.1:7741`, that gives up on a
    /// request after [`REQUEST_TIMEOUT`].
    pub fn new(base: &str) -> Result<HttpAgent, String> {
        // Every request opens its own connection, so a restarted agent never meets a stale one.
        let client = Client::builder(TokioExecutor::new())
            .pool_max_idle_per_host(0)
            .build_http();
        Ok(HttpAgent {
            status: endpoint(base, "/api/status")?,
            pay: endpoint(base, "/api/pay")?,
            limit: REQUEST_TIMEOUT,
            client,
        })
    }

    /// The same client, giving up on a request after `limit` instead.
    pub fn with_timeout(mut self, limit: Duration) -> HttpAgent {
        self.limit = limit;
        self
    }
}

impl Agent for HttpAgent {
    async fn status(&self) -> Result<Answer, String> {
        let request = Request::get(&self.status).body(Full::new(Bytes::new()));
        send(&self.client, self.limit, request).await
    }

    async fn pay(&self, request: &PayRequest) -> Result<Answer, String> {
        let body = serde_json::to_vec(request)
            .map_err(|error| format!("cannot encode the payment: {error}"))?;
        let request = Request::post(&self.pay)
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body)));
        send(&self.client, self.limit, request).await
    }
}

/// How long the agent gets to answer a request. A payment waits up to 30 seconds for its
/// confirmation, so this is longer than that.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

/// The largest answer that is read. The agent's answers are a few kilobytes at most.
const MAX_ANSWER: usize = 1 << 20;

/// The URL of `path` on the agent at `base`, which must be a plain `http://` URL. A slash that
/// ends `base` is not doubled.
fn endpoint(base: &str, path: &str) -> Result<Uri, String> {
    let invalid = || format!("the agent URL must look like http://127.0.0.1:7741, not {base:?}");
    let url = format!("{}{path}", base.trim_end_matches('/'));
    let uri: Uri = url.parse().map_err(|_| invalid())?;
    if uri.scheme_str() != Some("http") || uri.authority().is_none() {
        return Err(invalid());
    }
    Ok(uri)
}

/// Sends `request` and reads the whole answer, waiting at most `limit` for all of it.
async fn send(
    client: &Client<HttpConnector, Full<Bytes>>,
    limit: Duration,
    request: Result<Request<Full<Bytes>>, http::Error>,
) -> Result<Answer, String> {
    let request = request.map_err(|error| format!("invalid request: {error}"))?;
    let exchange = async {
        let response = client
            .request(request)
            .await
            .map_err(|error| explain(&error))?;
        let (parts, body) = response.into_parts();
        let body = Limited::new(body, MAX_ANSWER)
            .collect()
            .await
            .map_err(|error| explain(error.as_ref()))?;
        Ok(Answer {
            status: parts.status,
            body: body.to_bytes(),
        })
    };
    match tokio::time::timeout(limit, exchange).await {
        Ok(result) => result,
        Err(_) => Err(format!("no answer within {limit:?}")),
    }
}

/// An error together with the chain of causes behind it, joined by colons.
///
/// Transport errors keep their useful part, such as "Connection refused", in a source rather than
/// in their own message, so the top-level message alone would only say that a request failed.
fn explain(error: &(dyn Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {

    use axum::Router;
    use axum::routing::{get, post};
    use serde_json::{Value, json};
    use tokio::net::TcpListener;

    use super::*;

    const WAIT: Duration = Duration::from_secs(10);

    fn payment() -> PayRequest {
        PayRequest {
            merchant: "tf1merchant".to_owned(),
            amount: "60".to_owned(),
            allowance: None,
        }
    }

    /// The URL of a local port that nothing listens on.
    fn unused_url() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        let port = listener.local_addr().expect("an address").port();
        format!("http://127.0.0.1:{port}")
    }

    /// Serves `router` on a free local port, and gives the URL it answers on.
    async fn serve(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        let address = listener.local_addr().expect("an address");
        tokio::spawn(async move { axum::serve(listener, router).await.expect("serve") });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn an_agent_that_is_not_there_says_why() {
        let agent = HttpAgent::new(&unused_url()).expect("a client");
        let Err(error) = agent.status().await else {
            panic!("nothing listens there");
        };
        assert!(error.to_lowercase().contains("refused"), "{error}");
    }

    #[tokio::test]
    async fn an_agent_that_does_not_answer_is_given_up_on() {
        let slow = Router::new().route(
            "/api/pay",
            post(|| async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                "too late"
            }),
        );
        let url = serve(slow).await;
        let agent = HttpAgent::new(&url).expect("a client");
        let agent = agent.with_timeout(Duration::from_millis(100));

        let Err(error) = agent.pay(&payment()).await else {
            panic!("the agent is too slow");
        };
        assert_eq!(error, "no answer within 100ms");
    }

    #[test]
    fn requests_to_the_agent_time_out_after_45_seconds() {
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(45));
    }

    #[tokio::test]
    async fn a_refusal_is_an_answer_with_its_status_and_body() {
        let refusal = r#"{"error":"insufficient authority","stage":"selection"}"#;
        let router = Router::new().route(
            "/api/pay",
            post(move || async move { (StatusCode::UNPROCESSABLE_ENTITY, refusal) }),
        );
        let agent = HttpAgent::new(&serve(router).await).expect("a client");

        let answer = tokio::time::timeout(WAIT, agent.pay(&payment())).await;
        let Ok(Ok(answer)) = answer else {
            panic!("the agent answers: {answer:?}");
        };
        assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(&answer.body[..], refusal.as_bytes());
    }

    #[tokio::test]
    async fn a_base_url_with_a_trailing_slash_reaches_the_same_agent() {
        let router = Router::new().route("/api/status", get(|| async { "alive" }));
        let agent = HttpAgent::new(&format!("{}//", serve(router).await)).expect("a client");

        let Ok(answer) = agent.status().await else {
            panic!("the agent answers");
        };
        assert_eq!(&answer.body[..], b"alive");
    }

    #[test]
    fn a_url_that_is_not_a_plain_http_url_is_refused_when_the_client_is_made() {
        let urls = [
            "",
            "nonsense",
            "not a url",
            "127.0.0.1:7741",
            "https://127.0.0.1:7741",
            "ftp://127.0.0.1:7741",
            "http://",
        ];
        for url in urls {
            let error = HttpAgent::new(url).err().expect("the URL is refused");
            assert!(
                error.starts_with("the agent URL must look like http://127.0.0.1:7741"),
                "{error}"
            );
            assert!(error.contains(&format!("{url:?}")), "{error}");
        }
        for url in [
            "http://127.0.0.1:7741",
            "http://localhost:7741/",
            "http://[::1]:7741",
        ] {
            assert!(HttpAgent::new(url).is_ok(), "{url}");
        }
    }

    #[tokio::test]
    async fn an_answer_beyond_the_limit_is_not_read() {
        let huge = Router::new().route("/api/status", get(|| async { "x".repeat(2 << 20) }));
        let agent = HttpAgent::new(&serve(huge).await).expect("a client");

        let Err(error) = agent.status().await else {
            panic!("the answer is far over the limit");
        };
        assert!(error.contains("length limit exceeded"), "{error}");
    }

    #[test]
    fn a_payment_without_an_allowance_is_written_without_the_field() {
        let without = serde_json::to_value(payment()).expect("JSON");
        assert_eq!(without, json!({"merchant": "tf1merchant", "amount": "60"}));

        let with = PayRequest {
            allowance: Some("0123".to_owned()),
            ..payment()
        };
        let with = serde_json::to_value(with).expect("JSON");
        assert_eq!(
            with,
            json!({"merchant": "tf1merchant", "amount": "60", "allowance": "0123"})
        );
    }

    #[test]
    fn a_payment_is_read_from_exactly_its_three_fields() {
        let read = |json: Value| serde_json::from_value::<PayRequest>(json);
        assert!(read(json!({"merchant": "m", "amount": "1"})).is_ok());
        assert!(read(json!({"merchant": "m", "amount": "1", "allowance": null})).is_ok());
        assert!(read(json!({"merchant": "m", "amount": "1", "allowance": "a"})).is_ok());
        assert!(read(json!({"merchant": "m", "amount": "1", "memo": "x"})).is_err());
        assert!(read(json!({"merchant": "m"})).is_err());
    }
}
