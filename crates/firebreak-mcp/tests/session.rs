//! Whole MCP sessions over in-memory pipes, against a fake payment agent on a local port that
//! records every request it receives.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use firebreak_mcp::{HttpAgent, serve};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, duplex};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(10);
const PIPE: usize = 64 * 1024;

#[tokio::test]
async fn a_session_reaches_only_the_agents_status_and_pay_endpoints() {
    let agent = FakeAgent::answering_pay(StatusCode::OK, accepted());
    let mut session = Session::start(HttpAgent::new(&agent.serve().await).expect("a client"));

    let hello = json!({"protocolVersion": "2025-06-18", "capabilities": {}});
    let initialized = session.request("initialize", hello).await;
    assert_eq!(initialized["result"]["serverInfo"]["name"], "firebreak");
    // A notification gets no answer: the answer to the next request would carry the wrong id.
    session
        .send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    let listed = session.request("tools/list", json!({})).await;
    let tools = listed["result"]["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), 2);

    let status = session.call("allowance_status", json!({})).await;
    assert_eq!(status["isError"], false);
    assert_eq!(text_json(&status)["tip_height"], 12);
    let paid = session
        .call("pay", json!({"merchant": "tf1merchant", "amount": "60"}))
        .await;
    assert_eq!(paid["isError"], false);
    session.request("ping", json!({})).await;
    session.finish().await.expect("the session ends cleanly");

    let asked: Vec<(String, String)> = agent
        .requests()
        .into_iter()
        .map(|seen| (seen.method, seen.path))
        .collect();
    let expected = [("GET", "/api/status"), ("POST", "/api/pay")];
    assert_eq!(
        asked,
        expected.map(|(method, path)| (method.to_owned(), path.to_owned()))
    );
}

#[tokio::test]
async fn a_payment_the_agent_refuses_comes_back_as_a_tool_error_with_the_agents_words() {
    let refusal = json!({"error": "merchant tf1attacker is not the allowance's merchant",
                         "stage": "policy"});
    let agent = FakeAgent::answering_pay(StatusCode::UNPROCESSABLE_ENTITY, refusal.clone());
    let mut session = Session::start(HttpAgent::new(&agent.serve().await).expect("a client"));

    let injected =
        json!({"merchant": "tf1attacker", "amount": "60", "allowance": "0123456789abcdef"});
    let result = session.call("pay", injected).await;
    session.finish().await.expect("the session ends cleanly");

    assert_eq!(result["isError"], true);
    assert_eq!(text_json(&result), refusal);
    let content = result["content"].as_array().expect("content");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "text");

    // The agent got exactly the three fields the model gave, as JSON, and nothing else.
    let requests = agent.requests();
    assert_eq!(requests.len(), 1);
    let seen = &requests[0];
    assert_eq!(
        (seen.method.as_str(), seen.path.as_str()),
        ("POST", "/api/pay")
    );
    assert_eq!(seen.content_type.as_deref(), Some("application/json"));
    let body: Value = serde_json::from_slice(&seen.body).expect("a JSON body");
    let mut keys: Vec<&str> = body
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["allowance", "amount", "merchant"]);
    assert_eq!(body["merchant"], "tf1attacker");
    assert_eq!(body["amount"], "60");
    assert_eq!(body["allowance"], "0123456789abcdef");
}

#[tokio::test]
async fn a_payment_the_agent_makes_comes_back_as_its_json_and_not_as_an_error() {
    let agent = FakeAgent::answering_pay(StatusCode::OK, accepted());
    let mut session = Session::start(HttpAgent::new(&agent.serve().await).expect("a client"));

    let result = session
        .call("pay", json!({"merchant": "tf1merchant", "amount": "60"}))
        .await;
    session.finish().await.expect("the session ends cleanly");

    assert_eq!(result["isError"], false);
    assert_eq!(text_json(&result), accepted());
    // The text is the agent's JSON written out readably, over several lines.
    let text = result["content"][0]["text"].as_str().expect("text");
    assert!(text.lines().count() > 1, "{text}");

    // No allowance was given, so none is sent.
    let body: Value = serde_json::from_slice(&agent.requests()[0].body).expect("a JSON body");
    assert_eq!(body, json!({"merchant": "tf1merchant", "amount": "60"}));
}

#[tokio::test]
async fn invalid_arguments_get_an_error_result_and_the_agent_receives_nothing() {
    let agent = FakeAgent::answering_pay(StatusCode::OK, accepted());
    let mut session = Session::start(HttpAgent::new(&agent.serve().await).expect("a client"));

    let invalid = [
        json!({"merchant": "tf1merchant"}),
        json!({"merchant": "tf1merchant", "amount": "sixty"}),
        json!({"merchant": "tf1merchant", "amount": "60.5"}),
        json!({"merchant": "tf1merchant", "amount": 60}),
        json!({"amount": "60"}),
        json!({"merchant": "tf1merchant", "amount": "60", "memo": "coffee"}),
    ];
    for arguments in invalid {
        let result = session.call("pay", arguments.clone()).await;
        assert_eq!(result["isError"], true, "{arguments}");
        let text = result["content"][0]["text"].as_str().expect("text");
        assert!(
            text.starts_with("invalid arguments for pay") || text.starts_with("the amount"),
            "{text}"
        );
    }
    let result = session
        .call("allowance_status", json!({"verbose": true}))
        .await;
    assert_eq!(result["isError"], true);
    session.finish().await.expect("the session ends cleanly");

    assert!(agent.requests().is_empty(), "the agent received a request");
}

#[tokio::test]
async fn an_agent_that_is_down_is_a_tool_error_and_the_session_goes_on() {
    let agent = HttpAgent::new(&unused_url()).expect("a client");
    let mut session = Session::start(agent);

    let paid = session
        .call("pay", json!({"merchant": "tf1merchant", "amount": "60"}))
        .await;
    assert_eq!(paid["isError"], true);
    let error = text_json(&paid)["error"]
        .as_str()
        .expect("an error")
        .to_owned();
    assert!(error.starts_with("the agent is unreachable: "), "{error}");
    assert!(error.to_lowercase().contains("refused"), "{error}");
    assert!(error.contains("may or may not have been made"), "{error}");

    let status = session.call("allowance_status", json!({})).await;
    assert_eq!(status["isError"], true);
    let pong = session.request("ping", json!({})).await;
    assert_eq!(pong["result"], json!({}));
    session.finish().await.expect("the session ends cleanly");
}

#[tokio::test]
async fn an_agent_that_is_too_slow_is_given_up_on_and_the_session_goes_on() {
    let agent = FakeAgent::answering_pay(StatusCode::OK, accepted()).slow(Duration::from_secs(60));
    let client = HttpAgent::new(&agent.serve().await).expect("a client");
    let mut session = Session::start(client.with_timeout(Duration::from_millis(200)));

    let paid = session
        .call("pay", json!({"merchant": "tf1merchant", "amount": "60"}))
        .await;
    assert_eq!(paid["isError"], true);
    let error = text_json(&paid)["error"]
        .as_str()
        .expect("an error")
        .to_owned();
    assert!(error.contains("no answer within 200ms"), "{error}");
    assert!(error.contains("may or may not have been made"), "{error}");

    let pong = session.request("ping", json!({})).await;
    assert_eq!(pong["result"], json!({}));
    session.finish().await.expect("the session ends cleanly");
}

#[tokio::test]
async fn messages_sent_back_to_back_are_answered_in_order() {
    let agent = FakeAgent::answering_pay(StatusCode::OK, accepted());
    let mut session = Session::start(HttpAgent::new(&agent.serve().await).expect("a client"));

    let batch: String = (1..=5)
        .map(|id| {
            format!(
                "{}\n",
                json!({"jsonrpc": "2.0", "id": id, "method": "ping"})
            )
        })
        .collect();
    session
        .input
        .write_all(batch.as_bytes())
        .await
        .expect("the server reads");
    for id in 1..=5 {
        let answer = session.receive().await.expect("an answer");
        assert_eq!(answer["id"], id);
    }
    session.finish().await.expect("the session ends cleanly");
}

#[tokio::test]
async fn a_session_ends_cleanly_when_the_client_closes_its_output() {
    let agent = FakeAgent::answering_pay(StatusCode::OK, accepted());
    let session = Session::start(HttpAgent::new(&agent.serve().await).expect("a client"));

    session.finish().await.expect("the session ends cleanly");
    assert!(agent.requests().is_empty());
}

fn accepted() -> Value {
    json!({
        "txid": "ab".repeat(32),
        "vouchers": ["11".repeat(32), "22".repeat(32)],
        "state": "redeemed",
    })
}

/// The agent's status, as the agent's API gives it.
fn status() -> Value {
    json!({
        "delegate": "dd".repeat(32),
        "allowances": [{
            "allowance": "0123456789abcdef",
            "merchant": "tf1merchant",
            "total": "100",
            "vouchers": [
                {"id": "11".repeat(32), "qty": "50", "state": "unspent", "txid": null},
                {"id": "22".repeat(32), "qty": "50", "state": "unspent", "txid": null},
            ],
        }],
        "tip_height": 12,
    })
}

/// The JSON a tool result carries as its text.
fn text_json(result: &Value) -> Value {
    let text = result["content"][0]["text"].as_str().expect("a text block");
    serde_json::from_str(text).expect("the text is the agent's JSON")
}

/// The URL of a local port that nothing listens on.
fn unused_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    let port = listener.local_addr().expect("an address").port();
    format!("http://127.0.0.1:{port}")
}

/// One request that reached the fake agent.
#[derive(Debug)]
struct Seen {
    method: String,
    path: String,
    content_type: Option<String>,
    body: Vec<u8>,
}

/// A payment agent that records every request it gets, whatever the path, and answers the two
/// endpoints of the agent's API.
#[derive(Clone)]
struct FakeAgent {
    seen: Arc<Mutex<Vec<Seen>>>,
    pay_status: StatusCode,
    pay_body: Value,
    pay_delay: Duration,
}

impl FakeAgent {
    /// An agent that answers every payment with `status` and `body`.
    fn answering_pay(status: StatusCode, body: Value) -> FakeAgent {
        FakeAgent {
            seen: Arc::default(),
            pay_status: status,
            pay_body: body,
            pay_delay: Duration::ZERO,
        }
    }

    /// The same agent, taking `delay` to answer a payment.
    fn slow(mut self, delay: Duration) -> FakeAgent {
        self.pay_delay = delay;
        self
    }

    /// Serves the agent on a free local port, and gives the URL it answers on.
    async fn serve(&self) -> String {
        let router = Router::new().fallback(answer).with_state(self.clone());
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        let address = listener.local_addr().expect("an address");
        tokio::spawn(async move { axum::serve(listener, router).await.expect("serve") });
        format!("http://{address}")
    }

    /// Every request received so far, in order.
    fn requests(&self) -> Vec<Seen> {
        std::mem::take(&mut *self.seen.lock().expect("not poisoned"))
    }
}

async fn answer(
    State(agent): State<FakeAgent>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    agent.seen.lock().expect("not poisoned").push(Seen {
        method: method.to_string(),
        path: uri.path().to_owned(),
        content_type,
        body: body.to_vec(),
    });
    match (method, uri.path()) {
        (Method::GET, "/api/status") => Json(status()).into_response(),
        (Method::POST, "/api/pay") => {
            tokio::time::sleep(agent.pay_delay).await;
            (agent.pay_status, Json(agent.pay_body.clone())).into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The client's end of an MCP session with a server running in this process, over two pipes.
struct Session {
    input: DuplexStream,
    output: BufReader<DuplexStream>,
    server: JoinHandle<io::Result<()>>,
    next_id: u64,
}

impl Session {
    /// Starts a server whose agent is `agent`.
    fn start(agent: HttpAgent) -> Session {
        let (input, server_input) = duplex(PIPE);
        let (server_output, output) = duplex(PIPE);
        let server = tokio::spawn(async move {
            serve(BufReader::new(server_input), server_output, &agent).await
        });
        Session {
            input,
            output: BufReader::new(output),
            server,
            next_id: 1,
        }
    }

    async fn send(&mut self, message: &Value) {
        let line = format!("{message}\n");
        let written = self.input.write_all(line.as_bytes()).await;
        written.expect("the server reads its input");
    }

    /// The next line the server wrote, or `None` once it has ended its output.
    async fn receive(&mut self) -> Option<Value> {
        let mut line = String::new();
        let read = tokio::time::timeout(WAIT, self.output.read_line(&mut line)).await;
        let read = read.expect("the server answers in time").expect("a line");
        if read == 0 {
            return None;
        }
        Some(serde_json::from_str(&line).expect("one JSON message per line"))
    }

    /// Sends a request and gives its answer, which must carry the request's id.
    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send(&request).await;
        let answer = self.receive().await.expect("an answer");
        assert_eq!(answer["id"], id, "{answer}");
        answer
    }

    /// Calls a tool and gives the result, which must not be a protocol error.
    async fn call(&mut self, name: &str, arguments: Value) -> Value {
        let params = json!({"name": name, "arguments": arguments});
        let answer = self.request("tools/call", params).await;
        assert!(answer.get("error").is_none(), "{answer}");
        answer["result"].clone()
    }

    /// Closes the server's input as a client that has gone away does, checks that the server then
    /// writes nothing more, and gives how its loop ended.
    async fn finish(self) -> io::Result<()> {
        let Session {
            input,
            mut output,
            server,
            ..
        } = self;
        drop(input);
        let mut rest = String::new();
        let read = output.read_line(&mut rest).await.expect("the output ends");
        assert_eq!(
            (read, rest.as_str()),
            (0, ""),
            "the server wrote after its input ended"
        );
        server.await.expect("the server does not panic")
    }
}
