//! The two tools the server offers, and what calling each one does.

use serde_json::{Value, json};

use crate::agent::{Agent, Answer, PayRequest};
use crate::log;

/// The tools, as the result of `tools/list` describes them.
pub fn list() -> Value {
    json!({
        "tools": [
            {
                "name": PAY,
                "description": PAY_DESCRIPTION,
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "merchant": {
                            "type": "string",
                            "minLength": 1,
                            "description": "The merchant's address, a tf1... string. It must be \
                                the allowance's merchant: any other address is refused.",
                        },
                        "amount": {
                            "type": "string",
                            "pattern": "^[0-9]+$",
                            "description": "The amount in sparks, as a string of decimal digits \
                                such as \"60\". It must equal an exact sum of the remaining \
                                unspent vouchers.",
                        },
                        "allowance": {
                            "type": "string",
                            "description": "The allowance ID, as allowance_status shows it. Give \
                                it when the app holds more than one allowance.",
                        },
                    },
                    "required": ["merchant", "amount"],
                    "additionalProperties": false,
                },
                "annotations": {
                    "readOnlyHint": false,
                    "destructiveHint": true,
                    "idempotentHint": false,
                    "openWorldHint": false,
                },
            },
            {
                "name": ALLOWANCE_STATUS,
                "description": STATUS_DESCRIPTION,
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                },
                "annotations": {
                    "readOnlyHint": true,
                    "openWorldHint": false,
                },
            },
        ],
    })
}

/// Calls the tool `name` with `arguments`, or gives `None` when there is no such tool.
///
/// A call the agent refuses, cannot be asked, or that has invalid arguments is still a result,
/// flagged as an error, so that the model reads why.
pub async fn call<A>(agent: &A, name: &str, arguments: Value) -> Option<ToolResult>
where
    A: Agent,
{
    // The arguments are logged as the model sent them. Stderr is the operator's record of what
    // authority the model used, and of what it asked for and was refused.
    let shown = if arguments.is_null() {
        String::new()
    } else {
        format!(" {arguments}")
    };
    let result = match name {
        PAY => pay(agent, arguments).await,
        ALLOWANCE_STATUS => allowance_status(agent, arguments).await,
        _ => return None,
    };
    let outcome = if result.is_error { "failed" } else { "ok" };
    log::line(&format!("{name}{shown}: {outcome}"));
    Some(result)
}

/// What a tool call came to: text for the model to read, and whether it reports a failure.
#[derive(Debug)]
pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
}

impl ToolResult {
    /// The result as the response to `tools/call` carries it.
    pub fn into_json(self) -> Value {
        json!({
            "content": [{"type": "text", "text": self.text}],
            "isError": self.is_error,
        })
    }
}

async fn pay<A>(agent: &A, arguments: Value) -> ToolResult
where
    A: Agent,
{
    let request = match pay_request(arguments) {
        Ok(request) => request,
        Err(message) => return failure(message),
    };
    match agent.pay(&request).await {
        Ok(answer) => relay(&answer),
        Err(error) => no_answer(&format!(
            "the agent is unreachable: {error}. The payment may or may not have been made: call \
             allowance_status to see which vouchers are spent before paying again"
        )),
    }
}

async fn allowance_status<A>(agent: &A, arguments: Value) -> ToolResult
where
    A: Agent,
{
    let takes_none = arguments.is_null()
        || arguments
            .as_object()
            .is_some_and(|fields| fields.is_empty());
    if !takes_none {
        return failure("allowance_status takes no arguments");
    }
    match agent.status().await {
        Ok(answer) => relay(&answer),
        Err(error) => no_answer(&format!("the agent is unreachable: {error}")),
    }
}

/// The payment the model asks for, if its arguments are well formed.
///
/// Only the form is checked here. Whether the merchant is the allowance's, and whether the amount
/// can be paid from the vouchers that remain, is the agent's to decide, and the model reads the
/// agent's own refusal.
fn pay_request(arguments: Value) -> Result<PayRequest, String> {
    // Serde would also read a list of values as the fields in order, which is not what pay's
    // schema says, so only an object is accepted. Arguments that are left out are no fields.
    let arguments = match arguments {
        Value::Null => json!({}),
        object @ Value::Object(_) => object,
        _ => {
            return Err(format!(
                "the arguments of pay must be a JSON object. {PAY_USAGE}"
            ));
        }
    };
    let request: PayRequest = serde_json::from_value(arguments)
        .map_err(|error| format!("invalid arguments for pay: {error}. {PAY_USAGE}"))?;
    if request.merchant.trim().is_empty() {
        return Err(format!("the merchant must not be empty. {PAY_USAGE}"));
    }
    if request.amount.is_empty() || !request.amount.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "the amount must be a non-empty string of decimal digits, such as \"60\": no sign, \
             decimal point, spaces or units. {PAY_USAGE}"
        ));
    }
    Ok(request)
}

/// A result that reports `text` as a failure.
fn failure<T>(text: T) -> ToolResult
where
    T: Into<String>,
{
    ToolResult {
        text: text.into(),
        is_error: true,
    }
}

/// The agent's answer as a result: its JSON written out readably, or, for an answer that is not
/// JSON, its text with its status. Any status but a success is an error.
fn relay(answer: &Answer) -> ToolResult {
    let text = match serde_json::from_slice::<Value>(&answer.body) {
        Ok(json) => format!("{json:#}"),
        Err(_) => {
            let body = String::from_utf8_lossy(&answer.body);
            match body.trim() {
                "" => format!("the agent answered {} with no body", answer.status),
                body => format!("the agent answered {}: {body}", answer.status),
            }
        }
    };
    ToolResult {
        text,
        is_error: !answer.status.is_success(),
    }
}

/// A result that reports the failure to get any answer, in the shape of the agent's own errors.
fn no_answer(message: &str) -> ToolResult {
    let error = json!({"error": message, "stage": "unknown"});
    failure(format!("{error:#}"))
}

const PAY: &str = "pay";

const ALLOWANCE_STATUS: &str = "allowance_status";

const PAY_DESCRIPTION: &str = "Pays the merchant of this app's spending allowance. The app's \
    payment authority is limited: it can pay only the one merchant its allowance was created for, \
    and never more than the vouchers that remain. A payment to any other merchant is refused by \
    the app and, even if the app let it through, by the blockchain. Vouchers are fixed-value and \
    single-use, so the amount must be an exact sum of the remaining unspent vouchers; any other \
    amount is refused. Check allowance_status first. On success the reply names the transaction \
    and the vouchers it spent, with state `redeemed`, or `redemption_pending` while it awaits \
    confirmation. A refusal comes back as an error that names the stage that refused it; if the \
    outcome is unknown, check allowance_status before paying again.";

const STATUS_DESCRIPTION: &str = "Shows the spending allowance this app holds: for each \
    allowance, its merchant (the only one that can be paid), its total, and its vouchers with \
    their amount in sparks, state and transaction, and the current block height. Read-only. Use \
    it before pay to see which amounts are possible: the exact sums of vouchers in state \
    `unspent`.";

const PAY_USAGE: &str = "pay takes merchant (a tf1... address), amount (a string of decimal \
    digits, in sparks) and, optionally, allowance.";

#[cfg(test)]
mod tests {
    use http::StatusCode;

    use super::*;
    use crate::testing::{Asked, Fake};

    const ACCEPTED: &str = r#"{"txid":"ab","vouchers":["11","22"],"state":"redeemed"}"#;
    const REFUSED: &str = r#"{"error":"not the allowance's merchant","stage":"policy"}"#;

    async fn call_tool(agent: &Fake, name: &str, arguments: Value) -> ToolResult {
        let result = call(agent, name, arguments).await;
        result.expect("a tool of that name")
    }

    /// The listed tool called `name`.
    fn tool(name: &str) -> Value {
        let tools = list();
        let listed = tools["tools"].as_array().expect("a list of tools");
        let found = listed.iter().find(|tool| tool["name"] == name);
        found.cloned().expect("the tool is listed")
    }

    fn json_of(text: &str) -> Value {
        serde_json::from_str(text).expect("JSON")
    }

    #[test]
    fn the_tools_are_pay_and_allowance_status_and_nothing_else() {
        let tools = list();
        let names: Vec<&str> = tools["tools"]
            .as_array()
            .expect("a list of tools")
            .iter()
            .map(|tool| tool["name"].as_str().expect("a name"))
            .collect();
        assert_eq!(names, ["pay", "allowance_status"]);
    }

    #[test]
    fn pay_takes_a_merchant_and_an_amount_of_digits_and_optionally_an_allowance() {
        let schema = &tool("pay")["inputSchema"];
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], json!(["merchant", "amount"]));
        assert_eq!(schema["additionalProperties"], false);

        let properties = schema["properties"].as_object().expect("properties");
        let mut names: Vec<&str> = properties.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["allowance", "amount", "merchant"]);
        for (name, property) in properties {
            assert_eq!(property["type"], "string", "{name}");
            let description = property["description"].as_str().expect("a description");
            assert!(!description.is_empty(), "{name}");
        }
        assert_eq!(properties["amount"]["pattern"], "^[0-9]+$");
    }

    #[test]
    fn allowance_status_takes_no_arguments() {
        let expected = json!({"type": "object", "properties": {}, "additionalProperties": false});
        assert_eq!(tool("allowance_status")["inputSchema"], expected);
    }

    #[test]
    fn the_tools_say_what_the_authority_is_and_what_it_refuses() {
        let pay = tool("pay");
        let description = pay["description"].as_str().expect("a description");
        for wanted in [
            "limited",
            "only the one merchant",
            "any other merchant is refused by the app",
            "by the blockchain",
            "exact sum of the remaining unspent vouchers",
            "any other amount is refused",
        ] {
            assert!(description.contains(wanted), "pay does not say {wanted:?}");
        }
        assert_eq!(
            tool("allowance_status")["annotations"]["readOnlyHint"],
            true
        );
        assert_eq!(pay["annotations"]["readOnlyHint"], false);
    }

    #[test]
    fn a_result_is_one_text_block_and_an_error_flag() {
        let result = ToolResult {
            text: "hello".to_owned(),
            is_error: true,
        };
        let expected = json!({"content": [{"type": "text", "text": "hello"}], "isError": true});
        assert_eq!(result.into_json(), expected);
    }

    #[tokio::test]
    async fn a_payment_is_forwarded_as_asked_and_the_agents_json_comes_back_readable() {
        let agent = Fake::answering(StatusCode::OK, ACCEPTED);
        let payment = json!({"merchant": "tf1merchant", "amount": "60", "allowance": "0123"});
        let result = call_tool(&agent, "pay", payment.clone()).await;

        assert!(!result.is_error);
        assert_eq!(json_of(&result.text), json_of(ACCEPTED));
        assert!(
            result.text.contains("\n  \"state\": \"redeemed\""),
            "{}",
            result.text
        );
        assert_eq!(agent.asked(), [Asked::Pay(payment)]);
    }

    #[tokio::test]
    async fn a_payment_without_an_allowance_is_forwarded_without_the_field() {
        let agent = Fake::answering(StatusCode::OK, ACCEPTED);
        let payment = json!({"merchant": "tf1merchant", "amount": "60"});
        for arguments in [
            payment.clone(),
            json!({"allowance": null, "merchant": "tf1merchant", "amount": "60"}),
        ] {
            call_tool(&agent, "pay", arguments).await;
        }
        assert_eq!(
            agent.asked(),
            [Asked::Pay(payment.clone()), Asked::Pay(payment)]
        );
    }

    #[tokio::test]
    async fn a_refusal_by_the_agent_is_an_error_result_in_the_agents_own_words() {
        let agent = Fake::answering(StatusCode::UNPROCESSABLE_ENTITY, REFUSED);
        let payment = json!({"merchant": "tf1attacker", "amount": "60"});
        let result = call_tool(&agent, "pay", payment).await;

        assert!(result.is_error);
        assert_eq!(json_of(&result.text), json_of(REFUSED));
    }

    #[tokio::test]
    async fn the_merchant_is_the_agents_to_judge_so_any_address_is_forwarded() {
        let agent = Fake::answering(StatusCode::UNPROCESSABLE_ENTITY, REFUSED);
        let merchants = ["tf1attacker", "bc1qsomeoneelse", "not an address"];
        for merchant in merchants {
            let payment = json!({"merchant": merchant, "amount": "60"});
            let result = call_tool(&agent, "pay", payment).await;
            assert!(result.is_error);
            assert_eq!(json_of(&result.text), json_of(REFUSED));
        }
        assert_eq!(agent.asked().len(), merchants.len());
    }

    #[tokio::test]
    async fn the_amount_is_the_agents_to_judge_so_any_digits_are_forwarded() {
        let agent = Fake::answering(StatusCode::UNPROCESSABLE_ENTITY, REFUSED);
        let amounts = ["0", "007", "123456789012345678901234567890"];
        for amount in amounts {
            let payment = json!({"merchant": "tf1merchant", "amount": amount});
            call_tool(&agent, "pay", payment).await;
        }
        let asked: Vec<Value> = agent
            .asked()
            .into_iter()
            .map(|asked| match asked {
                Asked::Pay(payment) => payment["amount"].clone(),
                Asked::Status => panic!("only payments were asked for"),
            })
            .collect();
        assert_eq!(asked, amounts);
    }

    #[tokio::test]
    async fn invalid_pay_arguments_are_an_error_result_that_never_reaches_the_agent() {
        let merchant = "tf1merchant";
        let mut cases = vec![
            (Value::Null, "missing field `merchant`"),
            (json!({}), "missing field `merchant`"),
            (json!({"merchant": merchant}), "missing field `amount`"),
            (json!({"amount": "60"}), "missing field `merchant`"),
            (
                json!({"merchant": merchant, "amount": 60}),
                "invalid type: integer `60`",
            ),
            (
                json!({"merchant": merchant, "amount": null}),
                "invalid type: null",
            ),
            (
                json!({"merchant": merchant, "amount": ["60"]}),
                "invalid type: sequence",
            ),
            (
                json!({"merchant": 7, "amount": "60"}),
                "invalid type: integer `7`",
            ),
            (
                json!({"merchant": "", "amount": "60"}),
                "the merchant must not be empty",
            ),
            (
                json!({"merchant": "  ", "amount": "60"}),
                "the merchant must not be empty",
            ),
            (
                json!({"merchant": merchant, "amount": "60", "allowance": 5}),
                "invalid type: integer `5`",
            ),
            (
                json!({"merchant": merchant, "amount": "60", "memo": "x"}),
                "unknown field `memo`",
            ),
            (json!("pay 60"), "must be a JSON object"),
            (json!([merchant, "60"]), "must be a JSON object"),
            (json!(60), "must be a JSON object"),
            (json!(true), "must be a JSON object"),
        ];
        let not_digits = [
            "",
            " 60",
            "60 ",
            "6O",
            "60.5",
            "-60",
            "+60",
            "1e2",
            "0x3c",
            "60 sparks",
        ];
        for amount in not_digits
            .into_iter()
            .chain(["\u{0666}\u{0660}", "\u{516d}\u{5341}"])
        {
            let arguments = json!({"merchant": merchant, "amount": amount});
            cases.push((
                arguments,
                "the amount must be a non-empty string of decimal digits",
            ));
        }

        let agent = Fake::answering(StatusCode::OK, ACCEPTED);
        for (arguments, expected) in cases {
            let result = call_tool(&agent, "pay", arguments.clone()).await;
            assert!(result.is_error, "{arguments}");
            assert!(
                result.text.contains(expected),
                "{arguments}: {}",
                result.text
            );
            assert!(
                result.text.contains("pay takes merchant"),
                "{arguments}: {}",
                result.text
            );
        }
        let asked = agent.asked();
        assert!(asked.is_empty(), "the agent was asked: {asked:?}");
    }

    #[tokio::test]
    async fn an_agent_that_cannot_be_reached_is_an_error_result_that_says_why() {
        let agent = Fake::unreachable("connection refused");
        let payment = json!({"merchant": "tf1merchant", "amount": "60"});
        let paid = call_tool(&agent, "pay", payment).await;

        assert!(paid.is_error);
        let shown = json_of(&paid.text);
        assert_eq!(shown["stage"], "unknown");
        let error = shown["error"].as_str().expect("an error message");
        assert!(
            error.starts_with("the agent is unreachable: connection refused"),
            "{error}"
        );
        assert!(error.contains("may or may not have been made"), "{error}");

        let status = call_tool(&agent, "allowance_status", Value::Null).await;
        assert!(status.is_error);
        let shown = json_of(&status.text);
        assert_eq!(
            shown["error"],
            "the agent is unreachable: connection refused"
        );
        assert_eq!(shown["stage"], "unknown");
    }

    #[tokio::test]
    async fn an_answer_that_is_not_json_is_shown_as_it_came_with_its_status() {
        let gateway = Fake::answering(StatusCode::BAD_GATEWAY, "<html>bad gateway</html>\n");
        let result = call_tool(&gateway, "allowance_status", Value::Null).await;
        assert!(result.is_error);
        assert_eq!(
            result.text,
            "the agent answered 502 Bad Gateway: <html>bad gateway</html>"
        );

        let silent = Fake::answering(StatusCode::INTERNAL_SERVER_ERROR, "");
        let result = call_tool(&silent, "allowance_status", Value::Null).await;
        assert!(result.is_error);
        assert_eq!(
            result.text,
            "the agent answered 500 Internal Server Error with no body"
        );

        let plain = Fake::answering(StatusCode::OK, "alive\n");
        let result = call_tool(&plain, "allowance_status", Value::Null).await;
        assert!(!result.is_error);
        assert_eq!(result.text, "the agent answered 200 OK: alive");
    }

    #[tokio::test]
    async fn allowance_status_shows_the_agents_status_as_readable_json() {
        let status = r#"{"delegate":"dd","allowances":[],"tip_height":12}"#;
        let agent = Fake::answering(StatusCode::OK, status);
        for arguments in [Value::Null, json!({})] {
            let result = call_tool(&agent, "allowance_status", arguments).await;
            assert!(!result.is_error);
            assert_eq!(json_of(&result.text), json_of(status));
            assert!(
                result.text.contains("\n  \"tip_height\": 12"),
                "{}",
                result.text
            );
        }
        assert_eq!(agent.asked(), [Asked::Status, Asked::Status]);
    }

    #[tokio::test]
    async fn allowance_status_refuses_arguments_without_asking_the_agent() {
        let agent = Fake::answering(StatusCode::OK, "{}");
        for arguments in [json!({"allowance": "x"}), json!([]), json!("x"), json!(1)] {
            let result = call_tool(&agent, "allowance_status", arguments).await;
            assert!(result.is_error);
            assert_eq!(result.text, "allowance_status takes no arguments");
        }
        assert!(agent.asked().is_empty());
    }

    #[tokio::test]
    async fn a_tool_that_does_not_exist_is_not_called() {
        let agent = Fake::answering(StatusCode::OK, "{}");
        for name in [
            "delegated_key",
            "reveal_key",
            "reclaim",
            "recover",
            "Pay",
            "",
        ] {
            assert!(call(&agent, name, json!({})).await.is_none(), "{name:?}");
        }
        assert!(agent.asked().is_empty());
    }
}
