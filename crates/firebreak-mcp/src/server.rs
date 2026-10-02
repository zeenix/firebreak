//! The JSON-RPC loop of MCP's stdio transport: one message per line in, one answer per line out.

use std::io;

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::agent::Agent;
use crate::{log, tools};

/// Serves MCP over `reader` and `writer` until `reader` ends, and then returns `Ok`.
///
/// Every line read is one JSON-RPC message, and every answer is written as one line and flushed,
/// which is how MCP's stdio transport frames messages. Messages are handled one at a time, in
/// order, so a payment that takes the agent half a minute also holds up the messages behind it,
/// and this server never has more than one payment in flight. Only an I/O error on `reader` or
/// `writer` ends it early.
pub async fn serve<R, W, A>(mut reader: R, mut writer: W, agent: &A) -> io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
    A: Agent,
{
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).await? == 0 {
            return Ok(());
        }
        let Some(mut answer) = handle(agent, &line).await else {
            continue;
        };
        answer.push('\n');
        writer.write_all(answer.as_bytes()).await?;
        writer.flush().await?;
    }
}

/// Answers one line from the client with the line to write back, or `None` when it calls for no
/// answer.
async fn handle<A>(agent: &A, line: &[u8]) -> Option<String>
where
    A: Agent,
{
    if line.trim_ascii().is_empty() {
        return None;
    }
    let answer = match serde_json::from_slice::<Value>(line) {
        Ok(message) => dispatch(agent, message).await?,
        Err(error) => failure(Value::Null, PARSE_ERROR, &format!("Parse error: {error}")),
    };
    Some(answer.to_string())
}

/// Answers one JSON-RPC message, or gives `None` when it needs no answer.
async fn dispatch<A>(agent: &A, message: Value) -> Option<Value>
where
    A: Agent,
{
    let Value::Object(mut fields) = message else {
        let reason = "a message is one JSON object; batches are not supported";
        return Some(failure(Value::Null, INVALID_REQUEST, reason));
    };
    let id = match fields.remove("id") {
        None => None,
        Some(id @ (Value::String(_) | Value::Number(_))) => Some(id),
        Some(_) => {
            let reason = "an id is a string or a number";
            return Some(failure(Value::Null, INVALID_REQUEST, reason));
        }
    };
    let Some(Value::String(method)) = fields.remove("method") else {
        // A message with a result or an error is a response. This server makes no requests, so
        // there is nothing for it to answer.
        if fields.contains_key("result") || fields.contains_key("error") {
            return None;
        }
        let id = id.unwrap_or(Value::Null);
        return Some(failure(id, INVALID_REQUEST, "a message needs a method"));
    };
    let Some(id) = id else {
        // A message without an id is a notification. It is never answered, not even to refuse it,
        // and it never runs anything: a payment whose result could not be reported must not be
        // made.
        if !method.starts_with("notifications/") {
            log::line(&format!("ignoring {method:?}, which came without an id"));
        }
        return None;
    };
    let params = fields.remove("params").unwrap_or(Value::Null);
    Some(match route(agent, &method, params).await {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => failure(id, error.code, &error.message),
    })
}

/// The result of the request `method`, or the error that says why it has none.
async fn route<A>(agent: &A, method: &str, params: Value) -> Result<Value, RpcError>
where
    A: Agent,
{
    match method {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools::list()),
        "tools/call" => call(agent, params).await,
        _ => Err(RpcError::new(
            METHOD_NOT_FOUND,
            format!("Method not found: {method}"),
        )),
    }
}

/// The result of `initialize`: the protocol version to speak, and what this server is and does.
fn initialize(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = SUPPORTED_VERSIONS
        .into_iter()
        .find(|version| Some(*version) == requested)
        .unwrap_or(SUPPORTED_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "firebreak", "version": env!("CARGO_PKG_VERSION")},
        "instructions": INSTRUCTIONS,
    })
}

/// The result of `tools/call`: the tool's own, or a protocol error when the call names no tool.
///
/// Whatever the tool reports, even a refusal by the agent, is a result: only a malformed call is
/// an error here.
async fn call<A>(agent: &A, params: Value) -> Result<Value, RpcError>
where
    A: Agent,
{
    let Value::Object(mut fields) = params else {
        let reason = "tools/call takes an object with the tool's name and its arguments";
        return Err(RpcError::new(INVALID_PARAMS, reason));
    };
    let Some(Value::String(name)) = fields.remove("name") else {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "tools/call needs the name of a tool",
        ));
    };
    let arguments = fields.remove("arguments").unwrap_or(Value::Null);
    match tools::call(agent, &name, arguments).await {
        Some(result) => Ok(result.into_json()),
        None => Err(RpcError::new(
            INVALID_PARAMS,
            format!("Unknown tool: {name}"),
        )),
    }
}

/// A JSON-RPC error response to the request `id`.
fn failure(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Why a request has no result.
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn new<M>(code: i64, message: M) -> RpcError
    where
        M: Into<String>,
    {
        RpcError {
            code,
            message: message.into(),
        }
    }
}

/// The protocol versions this server speaks, newest first. A client that asks for any other
/// version is answered with the newest, and may disconnect if it cannot speak that.
const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// What the server tells the model about itself when the session starts.
const INSTRUCTIONS: &str = "Firebreak gives you a limited spending allowance on the Flame \
    blockchain, through two tools. allowance_status shows the allowance: its one merchant and its \
    vouchers, which are fixed-value, single-use parts of the total. pay pays that merchant and no \
    one else: the app refuses any other address and the blockchain would refuse it too, so do not \
    try to pay anyone else. Vouchers cannot be split, so the amount of a payment must equal an \
    exact sum of the remaining unspent vouchers: check allowance_status first, and when a payment \
    is refused, change the request rather than repeating it. You hold no keys and cannot recover \
    unspent funds; only the owner can.";

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use http::StatusCode;

    use super::*;
    use crate::testing::{Asked, Fake};

    const REFUSED: &str = r#"{"error":"not the allowance's merchant","stage":"policy"}"#;

    /// An agent that would accept anything, for tests that must not reach it.
    fn idle() -> Fake {
        Fake::answering(StatusCode::OK, "{}")
    }

    fn request(id: Value, method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    fn pay_call(arguments: Value) -> Value {
        request(
            json!(1),
            "tools/call",
            json!({"name": "pay", "arguments": arguments}),
        )
    }

    /// What the server writes for `line`, which is always one JSON line, or `None` for nothing.
    async fn answer(agent: &Fake, line: &[u8]) -> Option<Value> {
        let answer = handle(agent, line).await?;
        assert!(
            !answer.contains('\n'),
            "an answer is a single line: {answer:?}"
        );
        Some(serde_json::from_str(&answer).expect("an answer is JSON"))
    }

    /// What the server writes for `message`.
    async fn ask(agent: &Fake, message: &Value) -> Value {
        let answer = answer(agent, message.to_string().as_bytes()).await;
        answer.expect("the message is answered")
    }

    fn error_code(answer: &Value) -> i64 {
        assert!(answer.get("result").is_none(), "{answer}");
        answer["error"]["code"].as_i64().expect("an error code")
    }

    #[tokio::test]
    async fn initialize_answers_with_the_clients_version_when_the_server_speaks_it() {
        for version in ["2025-06-18", "2025-03-26", "2024-11-05"] {
            let params = json!({"protocolVersion": version, "capabilities": {}});
            let answer = ask(&idle(), &request(json!(0), "initialize", params)).await;
            assert_eq!(answer["result"]["protocolVersion"], version);
        }
    }

    #[tokio::test]
    async fn initialize_answers_with_the_newest_version_when_the_client_asks_for_another() {
        let asked = [
            json!({"protocolVersion": "2099-01-01"}),
            json!({"protocolVersion": "2024-11-04"}),
            json!({"protocolVersion": ""}),
            json!({"protocolVersion": 20250618}),
            json!({}),
            Value::Null,
        ];
        for params in asked {
            let answer = ask(&idle(), &request(json!(0), "initialize", params.clone())).await;
            assert_eq!(
                answer["result"]["protocolVersion"], "2025-06-18",
                "{params}"
            );
        }
        let without_params = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize"});
        let answer = ask(&idle(), &without_params).await;
        assert_eq!(answer["result"]["protocolVersion"], "2025-06-18");
    }

    #[tokio::test]
    async fn initialize_describes_the_server_and_offers_only_tools() {
        let answer = ask(&idle(), &request(json!(0), "initialize", json!({}))).await;
        let result = &answer["result"];

        assert_eq!(
            result["capabilities"],
            json!({"tools": {"listChanged": false}})
        );
        assert_eq!(result["serverInfo"]["name"], "firebreak");
        assert_eq!(result["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
        let instructions = result["instructions"].as_str().expect("instructions");
        assert!(!instructions.contains('\n'), "one paragraph");
        for wanted in [
            "allowance_status",
            "pay",
            "merchant",
            "no one else",
            "exact sum",
            "vouchers",
        ] {
            assert!(
                instructions.contains(wanted),
                "the instructions lack {wanted:?}"
            );
        }
    }

    #[tokio::test]
    async fn ping_is_answered_with_an_empty_result_and_the_id_as_it_was_sent() {
        for id in [json!(7), json!(0), json!("seven"), json!(""), json!(1.5)] {
            let answer = ask(&idle(), &request(id.clone(), "ping", Value::Null)).await;
            assert_eq!(answer, json!({"jsonrpc": "2.0", "id": id, "result": {}}));
        }
    }

    #[tokio::test]
    async fn tools_list_offers_pay_and_allowance_status() {
        let answer = ask(&idle(), &request(json!(2), "tools/list", Value::Null)).await;
        let tools = answer["result"]["tools"]
            .as_array()
            .expect("a list of tools");
        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool["name"].as_str().expect("a name"))
            .collect();
        assert_eq!(names, ["pay", "allowance_status"]);
        for tool in tools {
            assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
        }
    }

    #[tokio::test]
    async fn an_unknown_method_is_a_method_not_found_error() {
        let agent = idle();
        for method in [
            "resources/list",
            "prompts/list",
            "tools/run",
            "",
            "initialize ",
        ] {
            let answer = ask(&agent, &request(json!(7), method, json!({}))).await;
            assert_eq!(error_code(&answer), -32601, "{method:?}");
            assert_eq!(answer["id"], 7);
            let message = answer["error"]["message"].as_str().expect("a message");
            assert!(message.contains(method), "{message}");
        }
        assert!(agent.asked().is_empty());
    }

    #[tokio::test]
    async fn malformed_json_is_a_parse_error_with_a_null_id() {
        let lines: [&[u8]; 5] = [
            b"{not json",
            br#"{"jsonrpc":"2.0","id":1,"method":"ping""#,
            b"ping",
            b"\xff\xfe{}",
            b"{\"id\":1,\"method\":\"ping\"} trailing",
        ];
        for line in lines {
            let answer = answer(&idle(), line).await.expect("an error answer");
            assert_eq!(error_code(&answer), -32700, "{line:?}");
            assert_eq!(answer["id"], Value::Null);
            assert_eq!(answer["jsonrpc"], "2.0");
        }
    }

    #[tokio::test]
    async fn json_that_is_not_a_request_is_an_invalid_request_error() {
        let agent = idle();
        let null = Value::Null;
        // Each message with the id its error answer must carry: the id when it could be read,
        // null when there is none or it is not a string or a number.
        let invalid = [
            (json!([]), null.clone()),
            (
                json!([request(json!(1), "ping", Value::Null)]),
                null.clone(),
            ),
            (json!(42), null.clone()),
            (json!("ping"), null.clone()),
            (null.clone(), null.clone()),
            (json!({}), null.clone()),
            (json!({"jsonrpc": "2.0", "params": {}}), null.clone()),
            (json!({"jsonrpc": "2.0", "id": 1, "method": 5}), json!(1)),
            (json!({"jsonrpc": "2.0", "id": "abc"}), json!("abc")),
            (
                json!({"jsonrpc": "2.0", "id": null, "method": "ping"}),
                null.clone(),
            ),
            (
                json!({"jsonrpc": "2.0", "id": true, "method": "ping"}),
                null.clone(),
            ),
            (
                json!({"jsonrpc": "2.0", "id": {"n": 1}, "method": "ping"}),
                null.clone(),
            ),
            (json!({"jsonrpc": "2.0", "id": [1], "method": "ping"}), null),
        ];
        for (message, id) in invalid {
            let answer = ask(&agent, &message).await;
            assert_eq!(error_code(&answer), -32600, "{message}");
            assert_eq!(answer["id"], id, "{message}");
        }
        assert!(agent.asked().is_empty());
    }

    #[tokio::test]
    async fn notifications_get_no_answer_and_run_nothing() {
        let agent = Fake::answering(StatusCode::OK, "{}");
        let notifications = [
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                   "params": {"requestId": 1}}),
            json!({"jsonrpc": "2.0", "method": "notifications/unheard-of"}),
            json!({"jsonrpc": "2.0", "method": "no/such/method", "params": {}}),
            json!({"jsonrpc": "2.0", "method": "ping"}),
            json!({"jsonrpc": "2.0", "method": "initialize", "params": {}}),
            json!({"jsonrpc": "2.0", "method": "tools/list"}),
            json!({"jsonrpc": "2.0", "method": "tools/call",
                   "params": {"name": "pay", "arguments": {"merchant": "tf1m", "amount": "60"}}}),
            json!({"jsonrpc": "2.0", "method": "tools/call",
                   "params": {"name": "allowance_status"}}),
        ];
        for notification in notifications {
            let reply = answer(&agent, notification.to_string().as_bytes()).await;
            assert!(
                reply.is_none(),
                "{notification} was answered with {reply:?}"
            );
        }
        assert!(agent.asked().is_empty(), "a notification reached the agent");
    }

    #[tokio::test]
    async fn responses_from_the_client_are_ignored() {
        let responses = [
            json!({"jsonrpc": "2.0", "id": 1, "result": {}}),
            json!({"jsonrpc": "2.0", "id": "x", "error": {"code": -1, "message": "no"}}),
        ];
        for response in responses {
            let reply = answer(&idle(), response.to_string().as_bytes()).await;
            assert!(reply.is_none(), "{response} was answered with {reply:?}");
        }
    }

    #[tokio::test]
    async fn blank_lines_are_skipped_and_windows_line_endings_are_accepted() {
        let agent = idle();
        for blank in [&b""[..], b"\n", b"\r\n", b"  \t \r\n"] {
            assert!(answer(&agent, blank).await.is_none(), "{blank:?}");
        }
        let ping = format!("{}\r\n", request(json!(3), "ping", Value::Null));
        let reply = answer(&agent, ping.as_bytes()).await.expect("an answer");
        assert_eq!(reply["id"], 3);
    }

    #[tokio::test]
    async fn a_call_that_names_no_known_tool_is_an_invalid_params_error() {
        let agent = idle();
        let calls = [
            Value::Null,
            json!({}),
            json!({"arguments": {}}),
            json!({"name": 5}),
            json!({"name": "delegated_key"}),
            json!({"name": "reclaim", "arguments": {}}),
            json!("pay"),
        ];
        for params in calls {
            let answer = ask(&agent, &request(json!(4), "tools/call", params.clone())).await;
            assert_eq!(error_code(&answer), -32602, "{params}");
            assert_eq!(answer["id"], 4);
        }
        let unknown = request(json!(4), "tools/call", json!({"name": "delegated_key"}));
        let message = ask(&agent, &unknown).await["error"]["message"].clone();
        assert_eq!(message, "Unknown tool: delegated_key");
        assert!(agent.asked().is_empty());
    }

    #[tokio::test]
    async fn a_payment_the_agent_refuses_is_a_result_and_never_a_protocol_error() {
        let agent = Fake::answering(StatusCode::UNPROCESSABLE_ENTITY, REFUSED);
        let arguments = json!({"merchant": "tf1attacker", "amount": "60"});
        let answer = ask(&agent, &pay_call(arguments.clone())).await;

        assert!(answer.get("error").is_none(), "{answer}");
        let result = &answer["result"];
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["type"], "text");
        let text = result["content"][0]["text"].as_str().expect("text");
        let shown: Value = serde_json::from_str(text).expect("the agent's JSON");
        assert_eq!(shown["error"], "not the allowance's merchant");
        assert_eq!(shown["stage"], "policy");
        assert_eq!(agent.asked(), [Asked::Pay(arguments)]);
    }

    #[tokio::test]
    async fn a_payment_the_agent_makes_is_a_result_that_is_not_an_error() {
        let paid = r#"{"txid":"ab","vouchers":["11"],"state":"redeemed"}"#;
        let agent = Fake::answering(StatusCode::OK, paid);
        let answer = ask(
            &agent,
            &pay_call(json!({"merchant": "tf1m", "amount": "50"})),
        )
        .await;

        assert_eq!(answer["result"]["isError"], false);
        let text = answer["result"]["content"][0]["text"]
            .as_str()
            .expect("text");
        assert!(text.contains("redeemed"), "{text}");
    }

    #[tokio::test]
    async fn invalid_payment_arguments_are_a_result_and_the_agent_is_not_asked() {
        let agent = idle();
        let invalid = [
            json!({"merchant": "tf1merchant"}),
            json!({"merchant": "tf1merchant", "amount": "sixty"}),
            json!({"merchant": "tf1merchant", "amount": "60.5"}),
            json!({"merchant": "tf1merchant", "amount": 60}),
            json!({"amount": "60"}),
        ];
        for arguments in invalid {
            let answer = ask(&agent, &pay_call(arguments.clone())).await;
            assert!(answer.get("error").is_none(), "{arguments}: {answer}");
            assert_eq!(answer["result"]["isError"], true, "{arguments}");
            let text = answer["result"]["content"][0]["text"]
                .as_str()
                .expect("text");
            assert!(
                text.contains("amount") || text.contains("merchant"),
                "{text}"
            );
        }
        assert!(agent.asked().is_empty());
    }

    #[tokio::test]
    async fn serve_answers_each_line_in_order_and_ends_with_its_input() {
        let agent = Fake::answering(StatusCode::OK, "{}");
        let input = [
            request(
                json!(1),
                "initialize",
                json!({"protocolVersion": "2025-06-18"}),
            )
            .to_string(),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
            String::new(),
            request(json!("two"), "ping", Value::Null).to_string(),
            "{broken".to_owned(),
            request(json!(3), "tools/call", json!({"name": "allowance_status"})).to_string(),
        ];
        // The last line has no newline: the end of the input ends it.
        let input = input.join("\n");
        let mut output = Vec::new();

        let ended = serve(input.as_bytes(), &mut output, &agent).await;

        assert!(ended.is_ok(), "{ended:?}");
        let output = String::from_utf8(output).expect("UTF-8");
        assert!(output.ends_with('\n'));
        let answers: Vec<Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is JSON"))
            .collect();
        let ids: Vec<&Value> = answers.iter().map(|answer| &answer["id"]).collect();
        assert_eq!(ids, [&json!(1), &json!("two"), &Value::Null, &json!(3)]);
        assert_eq!(error_code(&answers[2]), -32700);
        assert_eq!(answers[3]["result"]["isError"], false);
        assert_eq!(agent.asked(), [Asked::Status]);
    }

    #[tokio::test]
    async fn serve_stops_with_the_error_of_an_output_that_fails() {
        let input = request(json!(1), "ping", Value::Null).to_string() + "\n";

        let ended = serve(input.as_bytes(), Closed, &idle()).await;

        let error = ended.expect_err("the pipe is closed");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    /// An output that fails every write, as a closed pipe does.
    struct Closed;

    impl AsyncWrite for Closed {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
}
