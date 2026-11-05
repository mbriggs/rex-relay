//! The prototype host daemon and its client: `client.perform` between two rex-relay processes,
//! as docs/streaming.md §3 Option A proposes. The only action is `relay.ping`.

use std::time::Duration;

use anyhow::Result;
use rexrs::api::client::{Principal, ReplyError};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::rex::{self, Rex};

/// The label the host daemon connects with, and the one `debug perform` looks for by default.
pub const HOST_LABEL: &str = "rex-relay-host";
/// Echoes its args and the caller's identity.
pub const PING: &str = "relay.ping";
/// The longest wait the server honours.
pub const MAX_PERFORM_TIMEOUT: Duration = Duration::from_secs(30);

/// A `client_perform_requested` event: another client asks this one to perform an action.
#[derive(Debug, Deserialize)]
pub struct PerformRequest {
    pub call_id: String,
    pub action: String,
    #[serde(default)]
    pub args: Option<Value>,
    pub requester: Requester,
}

/// The client that sent a request, as the server identified it.
#[derive(Debug, Deserialize)]
pub struct Requester {
    pub client_id: String,
    pub principal: Principal,
}

/// The answer to a request: the action's result, or why it wasn't performed.
pub fn answer(request: &PerformRequest) -> Result<Value, ReplyError> {
    match request.action.as_str() {
        PING => Ok(json!({
            "args": request.args.clone().unwrap_or_else(|| json!({})),
            "caller": {
                "client_id": request.requester.client_id,
                "principal": request.requester.principal,
            },
        })),
        other => Err(ReplyError::new(
            "not_supported",
            format!("rex-relay host has no action {other:?}"),
        )),
    }
}

/// Answers `client_perform_requested` events, one at a time, until the connection fails.
/// Other events are skipped. A reply the server rejects, such as one to a request that timed
/// out while this daemon was busy or stopped, is noted and skipped.
pub fn serve(rex: &mut Rex) -> Result<()> {
    loop {
        let Some(event) = rex.next_event(Duration::from_secs(60))? else {
            continue;
        };
        if event.name != "client_perform_requested" {
            continue;
        }
        let request: PerformRequest = event.parse()?;
        let reply = answer(&request);
        eprintln!(
            "rex-relay host: {} from {} ({}): {}",
            request.action,
            request.requester.client_id,
            request.requester.principal.kind,
            if reply.is_ok() { "ok" } else { "refused" }
        );
        if let Err(error) = rex.reply(&request.call_id, reply) {
            if rex::is_connection_error(&error) {
                return Err(error);
            }
            eprintln!("rex-relay host: replying to {}: {error:#}", request.call_id);
        }
    }
}

/// Parses `--timeout` for `debug perform`, refusing a wait the server would cut short.
pub fn parse_perform_timeout(text: &str) -> Result<Duration, String> {
    let timeout = humantime::parse_duration(text).map_err(|e| e.to_string())?;
    if timeout.is_zero() {
        return Err("the timeout must be positive".into());
    }
    if timeout > MAX_PERFORM_TIMEOUT {
        return Err(format!(
            "Rex waits at most {}s for a client's answer",
            MAX_PERFORM_TIMEOUT.as_secs()
        ));
    }
    Ok(timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(action: &str, args: Option<Value>) -> PerformRequest {
        let mut event = json!({
            "call_id": "7",
            "action": action,
            "origin": "api",
            "requester": {
                "client_id": "client:caller",
                "info": {"name": "rex-relay", "kind": "cli"},
                "principal": {"kind": "tailscale", "transport": "tailscale",
                              "display": "someone", "tags": ["tag:relay"]},
            },
        });
        if let Some(args) = args {
            event["args"] = args;
        }
        serde_json::from_value(event).unwrap()
    }

    #[test]
    fn requests_decode_from_the_event() {
        let r = request(PING, Some(json!({"n": 1})));
        assert_eq!(r.call_id, "7");
        assert_eq!(r.requester.client_id, "client:caller");
        assert_eq!(r.requester.principal.kind, "tailscale");
        assert_eq!(r.requester.principal.tags, ["tag:relay"]);
        assert_eq!(r.args, Some(json!({"n": 1})));
        assert_eq!(request(PING, None).args, None);
    }

    #[test]
    fn ping_echoes_args_and_caller() {
        let reply = answer(&request(PING, Some(json!({"n": 1})))).unwrap();
        assert_eq!(reply["args"], json!({"n": 1}));
        assert_eq!(reply["caller"]["client_id"], "client:caller");
        assert_eq!(reply["caller"]["principal"]["kind"], "tailscale");
        assert_eq!(answer(&request(PING, None)).unwrap()["args"], json!({}));
    }

    #[test]
    fn other_actions_are_not_supported() {
        let error = answer(&request("relay.events", None)).unwrap_err();
        assert_eq!(error.code, "not_supported");
    }

    #[test]
    fn timeouts_stay_within_what_rex_honours() {
        assert_eq!(parse_perform_timeout("30s"), Ok(MAX_PERFORM_TIMEOUT));
        assert!(parse_perform_timeout("31s").is_err());
        assert!(parse_perform_timeout("0s").is_err());
    }
}
