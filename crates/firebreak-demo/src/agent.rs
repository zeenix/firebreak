//! A plain HTTP client for the payment agent's JSON API.

use std::time::Duration;

use axum::body::Bytes;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, Method, Request, StatusCode, Uri};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

use crate::explain::explain;

/// The agent at one base URL.
pub struct Agent {
    base: String,
    client: Client<HttpConnector, Full<Bytes>>,
}

/// What the agent answered.
pub struct Reply {
    pub status: StatusCode,
    pub content_type: Option<HeaderValue>,
    pub body: Bytes,
}

impl Agent {
    /// A client for the agent at `base`, such as `http://127.0.0.1:7741`.
    pub fn new(base: &str) -> Agent {
        // Every request opens its own connection, so a restarted agent never meets a stale one.
        let client = Client::builder(TokioExecutor::new())
            .pool_max_idle_per_host(0)
            .build_http();
        Agent {
            base: base.trim_end_matches('/').to_owned(),
            client,
        }
    }

    /// Sends `GET path`, waiting at most `limit` for the whole exchange.
    pub async fn get(&self, path: &str, limit: Duration) -> Result<Reply, String> {
        self.send(Method::GET, path, Bytes::new(), limit).await
    }

    /// Sends `POST path` with `body` as JSON, waiting at most `limit` for the whole exchange.
    pub async fn post(&self, path: &str, body: Bytes, limit: Duration) -> Result<Reply, String> {
        self.send(Method::POST, path, body, limit).await
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Bytes,
        limit: Duration,
    ) -> Result<Reply, String> {
        let url = format!("{}{path}", self.base);
        let uri: Uri = url
            .parse()
            .map_err(|error| format!("invalid agent URL {url}: {error}"))?;
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(body))
            .map_err(|error| format!("invalid request to {url}: {error}"))?;
        let exchange = async {
            let response = self
                .client
                .request(request)
                .await
                .map_err(|error| explain(&error))?;
            let (parts, body) = response.into_parts();
            let body = body.collect().await.map_err(|error| explain(&error))?;
            Ok(Reply {
                status: parts.status,
                content_type: parts.headers.get(CONTENT_TYPE).cloned(),
                body: body.to_bytes(),
            })
        };
        match tokio::time::timeout(limit, exchange).await {
            Ok(result) => result,
            Err(_) => Err(format!("no answer within {limit:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::routing::get;

    use super::*;
    use crate::testing;

    const WAIT: Duration = Duration::from_secs(10);

    #[tokio::test]
    async fn an_agent_that_is_not_there_says_why() {
        let agent = Agent::new(&testing::unused_url());
        let Err(error) = agent.get("/api/status", WAIT).await else {
            panic!("nothing listens there");
        };
        assert!(error.to_lowercase().contains("refused"), "{error}");
    }

    #[tokio::test]
    async fn an_agent_that_does_not_answer_is_given_up_on() {
        let slow = Router::new().route(
            "/api/status",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                "too late"
            }),
        );
        let agent = Agent::new(&testing::serve(slow).await);
        let Err(error) = agent.get("/api/status", Duration::from_millis(100)).await else {
            panic!("the agent is too slow");
        };
        assert_eq!(error, "no answer within 100ms");
    }

    #[tokio::test]
    async fn a_base_url_with_a_trailing_slash_reaches_the_same_agent() {
        let router = Router::new().route("/api/status", get(|| async { "alive" }));
        let agent = Agent::new(&format!("{}/", testing::serve(router).await));
        let Ok(reply) = agent.get("/api/status", WAIT).await else {
            panic!("the agent answers");
        };
        assert_eq!(&reply.body[..], b"alive");
    }

    #[tokio::test]
    async fn text_that_is_not_a_url_is_reported_not_sent() {
        let agent = Agent::new("not a url");
        let Err(error) = agent.get("/api/status", WAIT).await else {
            panic!("there is nowhere to send it");
        };
        assert!(
            error.starts_with("invalid agent URL not a url/api/status"),
            "{error}"
        );
    }
}
