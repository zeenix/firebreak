//! Error messages for the person at the dashboard.

use std::error::Error;

/// An error together with the chain of causes behind it, joined by colons.
///
/// Transport errors keep their useful part, such as "Connection refused", in a source rather than
/// in their own message, so the top-level message alone would only say that a request failed.
pub fn explain(error: &(dyn Error + 'static)) -> String {
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
    use std::fmt;

    use super::*;

    #[derive(Debug)]
    struct Failure {
        text: &'static str,
        cause: Option<Box<Failure>>,
    }

    impl fmt::Display for Failure {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.text)
        }
    }

    impl Error for Failure {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.cause
                .as_deref()
                .map(|cause| cause as &(dyn Error + 'static))
        }
    }

    fn failure(text: &'static str, cause: Option<Failure>) -> Failure {
        Failure {
            text,
            cause: cause.map(Box::new),
        }
    }

    #[test]
    fn an_error_is_joined_with_the_causes_behind_it() {
        let refused = failure("Connection refused (os error 111)", None);
        let connect = failure("tcp connect error", Some(refused));
        let request = failure("client error (Connect)", Some(connect));

        assert_eq!(
            explain(&request),
            "client error (Connect): tcp connect error: Connection refused (os error 111)"
        );
    }

    #[test]
    fn a_cause_that_is_already_in_the_message_is_not_said_twice() {
        let inner = failure("timed out", None);
        let outer = failure("request timed out", Some(inner));

        assert_eq!(explain(&outer), "request timed out");
    }
}
