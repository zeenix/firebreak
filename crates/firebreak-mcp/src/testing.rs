//! Fixtures shared by the unit tests: an agent that records what it is asked.

use std::sync::Mutex;

use bytes::Bytes;
use http::StatusCode;
use serde_json::Value;

use crate::agent::{Agent, Answer, PayRequest};

/// A request that reached the fake agent.
#[derive(Debug, PartialEq)]
pub enum Asked {
    Status,
    /// A payment, as the agent would receive it in JSON.
    Pay(Value),
}

/// An agent that gives every request the same answer, and records what it was asked.
pub struct Fake {
    answer: Result<(StatusCode, String), String>,
    asked: Mutex<Vec<Asked>>,
}

impl Fake {
    /// An agent that answers with `status` and `body`.
    pub fn answering(status: StatusCode, body: &str) -> Fake {
        Fake {
            answer: Ok((status, body.to_owned())),
            asked: Mutex::default(),
        }
    }

    /// An agent that cannot be reached, for `reason`.
    pub fn unreachable(reason: &str) -> Fake {
        Fake {
            answer: Err(reason.to_owned()),
            asked: Mutex::default(),
        }
    }

    /// Every request the agent has been asked, in order.
    pub fn asked(&self) -> Vec<Asked> {
        std::mem::take(&mut *self.asked.lock().expect("not poisoned"))
    }

    fn record(&self, asked: Asked) -> Result<Answer, String> {
        self.asked.lock().expect("not poisoned").push(asked);
        match &self.answer {
            Ok((status, body)) => Ok(Answer {
                status: *status,
                body: Bytes::from(body.clone()),
            }),
            Err(reason) => Err(reason.clone()),
        }
    }
}

impl Agent for Fake {
    async fn status(&self) -> Result<Answer, String> {
        self.record(Asked::Status)
    }

    async fn pay(&self, request: &PayRequest) -> Result<Answer, String> {
        let payment = serde_json::to_value(request).expect("a payment is JSON");
        self.record(Asked::Pay(payment))
    }
}
