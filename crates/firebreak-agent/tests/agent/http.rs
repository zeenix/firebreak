//! An HTTP client for the agent's API, and a way to serve the API on a real socket.

use std::sync::Arc;

use axum::body::Bytes;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use firebreak_agent::{Agent, api};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::{self, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use serde_json::Value;
use tokio::net::TcpListener;

/// A client of one server.
pub struct Client {
    base: String,
    client: legacy::Client<HttpConnector, Full<Bytes>>,
}

/// What the server answered.
pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
    pub json: Value,
}

impl Client {
    /// Serves the agent's API on a free port of the loopback interface, and gives a client for
    /// it. The server lives as long as the test's runtime does.
    pub async fn serve(agent: Arc<Agent>, reveal_key: bool) -> Client {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a port");
        let address = listener.local_addr().expect("an address");
        let router = api::router(agent, reveal_key);
        tokio::spawn(async move { axum::serve(listener, router).await.expect("serve") });
        Client::at(&format!("http://{address}"))
    }

    /// A client of the server at `base`, such as `http://127.0.0.1:7741`.
    pub fn at(base: &str) -> Client {
        Client {
            base: base.to_owned(),
            client: legacy::Client::builder(TokioExecutor::new())
                .pool_max_idle_per_host(0)
                .build_http(),
        }
    }

    pub async fn get(&self, path: &str) -> Reply {
        self.send(Method::GET, path, None, "").await
    }

    /// Posts `body` as JSON.
    pub async fn post(&self, path: &str, body: &Value) -> Reply {
        self.send(
            Method::POST,
            path,
            Some("application/json"),
            &body.to_string(),
        )
        .await
    }

    /// Posts `body` with the given content type, or with none.
    pub async fn post_as(&self, path: &str, content_type: Option<&str>, body: &str) -> Reply {
        self.send(Method::POST, path, content_type, body).await
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        content_type: Option<&str>,
        body: &str,
    ) -> Reply {
        let mut request = Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.base));
        if let Some(content_type) = content_type {
            request = request.header(CONTENT_TYPE, content_type);
        }
        let request = request
            .body(Full::new(Bytes::copy_from_slice(body.as_bytes())))
            .expect("a request");
        let response = self.client.request(request).await.expect("an answer");
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.expect("a body").to_bytes();
        let body = String::from_utf8(bytes.to_vec()).expect("UTF-8");
        let json = serde_json::from_str(&body).unwrap_or(Value::Null);
        Reply {
            status: parts.status,
            headers: parts.headers,
            body,
            json,
        }
    }
}
