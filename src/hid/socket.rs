//! The loopback WebSocket the OpenMouse web app speaks WebHID over.
//!
//! One socket is one browser tab's HID session: every device it opens is
//! closed when it disconnects. Frames are JSON, one request per frame with a
//! client-chosen `id` echoed in the reply, plus unsolicited `inputreport`
//! events. Byte payloads are plain number arrays, matching what the project's
//! other two HID adapters already exchange with these same drivers.
//!
//! Requests, all of which carry `id` and `type`:
//!
//! ```jsonc
//! { "id": 1, "type": "list", "vendorIds": [1133, 13652] }
//! { "id": 2, "type": "open", "device": "046d:c547:1" }
//! { "id": 3, "type": "close", "device": "046d:c547:1" }
//! { "id": 4, "type": "sendReport", "device": "…", "reportId": 16, "data": [255, 0] }
//! { "id": 5, "type": "sendFeatureReport", "device": "…", "reportId": 5, "data": [] }
//! { "id": 6, "type": "receiveFeatureReport", "device": "…", "reportId": 5 }
//! ```
//!
//! Replies are `{ "id": n, "ok": true, … }` or `{ "id": n, "ok": false, "error": "…" }`.
//! Events are `{ "type": "inputreport", "device": "…", "reportId": n, "data": [] }`.
//!
//! Hot-plug is deliberately not pushed from here: the client re-runs `list`
//! and diffs, which is a few milliseconds of enumeration and keeps connect and
//! disconnect logic in one place, next to the code that turns them into WebHID
//! events.

use std::sync::{Arc, Mutex};

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::unbounded_channel;

use super::{DeviceSummary, HidSession};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Request {
    id: u64,
    #[serde(flatten)]
    command: Command,
}

#[derive(Debug, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum Command {
    List {
        vendor_ids: Vec<u16>,
    },
    Open {
        device: String,
    },
    Close {
        device: String,
    },
    SendReport {
        device: String,
        report_id: u8,
        data: Vec<u8>,
    },
    SendFeatureReport {
        device: String,
        report_id: u8,
        data: Vec<u8>,
    },
    ReceiveFeatureReport {
        device: String,
        report_id: u8,
    },
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Reply {
    id: u64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    devices: Option<Vec<DeviceSummary>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Vec<u8>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InputReportEvent {
    #[serde(rename = "type")]
    kind: &'static str,
    device: String,
    report_id: u8,
    data: Vec<u8>,
}

/// Upgrades a handshake from an allowed origin.
///
/// A WebSocket handshake is not subject to CORS — the browser sends it
/// regardless of what the server's CORS layer says — so the allowlist that
/// protects the rest of the API has to be applied here by hand. Without this
/// check any page the user visits could enumerate and write to their mouse.
pub async fn upgrade(
    upgrade: WebSocketUpgrade,
    headers: HeaderMap,
    origins: Arc<Vec<String>>,
) -> Response {
    if !origin_allowed(&headers, &origins) {
        let origin = headers
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        tracing::warn!(%origin, "refused a HID socket from an origin that is not allowed");
        return (
            StatusCode::FORBIDDEN,
            "This origin is not allowed to use OpenMouse Bridge.",
        )
            .into_response();
    }
    upgrade.on_upgrade(serve)
}

/// Whichever of the two sides of the session produced work first.
enum Next {
    Frame(Option<Result<Message, axum::Error>>),
    Report(Option<super::InputReport>),
}

/// A handshake with no `Origin` header did not come from a browser, and a
/// handshake with an unlisted one came from a page that must not reach the
/// user's hardware. Both are refused: this is the only thing standing between
/// any website the user visits and their mouse.
fn origin_allowed(headers: &HeaderMap, origins: &[String]) -> bool {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    origins.iter().any(|allowed| allowed == origin)
}

async fn serve(mut socket: WebSocket) {
    let (reports, mut incoming_reports) = unbounded_channel();

    let session = match tokio::task::spawn_blocking(move || HidSession::new(reports)).await {
        Ok(Ok(session)) => Arc::new(Mutex::new(session)),
        Ok(Err(error)) => {
            tracing::error!(%error, "could not start a HID session");
            return;
        }
        Err(error) => {
            tracing::error!(%error, "the HID session task failed");
            return;
        }
    };

    loop {
        // The socket is only borrowed while the select is pending, so replies
        // below can use it again once a branch has resolved.
        let next = tokio::select! {
            frame = socket.recv() => Next::Frame(frame),
            report = incoming_reports.recv() => Next::Report(report),
        };

        let outgoing = match next {
            Next::Frame(Some(Ok(Message::Text(text)))) => execute(&session, text.as_str()).await,
            // Ping and pong are answered by axum; binary is not part of this
            // protocol.
            Next::Frame(Some(Ok(Message::Binary(_) | Message::Ping(_) | Message::Pong(_)))) => {
                continue;
            }
            Next::Frame(_) => break,
            Next::Report(Some(report)) => {
                let event = InputReportEvent {
                    kind: "inputreport",
                    device: report.key,
                    report_id: report.report_id,
                    data: report.data,
                };
                match serde_json::to_string(&event) {
                    Ok(encoded) => encoded,
                    Err(_) => continue,
                }
            }
            Next::Report(None) => break,
        };

        if socket.send(Message::Text(outgoing.into())).await.is_err() {
            break;
        }
    }
    // Dropping the session closes every device this tab opened and stops its
    // reader threads.
}

async fn execute(session: &Arc<Mutex<HidSession>>, text: &str) -> String {
    let request: Request = match serde_json::from_str(text) {
        Ok(request) => request,
        Err(error) => {
            return encode(Reply {
                id: 0,
                ok: false,
                error: Some(error.to_string()),
                ..Reply::default()
            });
        }
    };

    let id = request.id;
    let session = session.clone();
    let outcome = tokio::task::spawn_blocking(move || run(&session, request.command)).await;

    match outcome {
        Ok(Ok(mut reply)) => {
            reply.id = id;
            reply.ok = true;
            encode(reply)
        }
        Ok(Err(error)) => encode(Reply {
            id,
            ok: false,
            error: Some(format!("{error:#}")),
            ..Reply::default()
        }),
        Err(error) => encode(Reply {
            id,
            ok: false,
            error: Some(error.to_string()),
            ..Reply::default()
        }),
    }
}

fn run(session: &Mutex<HidSession>, command: Command) -> anyhow::Result<Reply> {
    let mut session = session.lock().unwrap();
    match command {
        Command::List { vendor_ids } => Ok(Reply {
            devices: Some(session.list(&vendor_ids)?),
            ..Reply::default()
        }),
        Command::Open { device } => {
            session.open(&device)?;
            Ok(Reply::default())
        }
        Command::Close { device } => {
            session.close(&device);
            Ok(Reply::default())
        }
        Command::SendReport {
            device,
            report_id,
            data,
        } => {
            session.send_report(&device, report_id, &data)?;
            Ok(Reply::default())
        }
        Command::SendFeatureReport {
            device,
            report_id,
            data,
        } => {
            session.send_feature_report(&device, report_id, &data)?;
            Ok(Reply::default())
        }
        Command::ReceiveFeatureReport { device, report_id } => Ok(Reply {
            data: Some(session.receive_feature_report(&device, report_id)?),
            ..Reply::default()
        }),
    }
}

fn encode(reply: Reply) -> String {
    serde_json::to_string(&reply).unwrap_or_else(|_| {
        r#"{"id":0,"ok":false,"error":"Bridge could not encode its reply."}"#.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_send_report_request() {
        let request: Request = serde_json::from_str(
            r#"{"id":4,"type":"sendReport","device":"046d:c547:1","reportId":16,"data":[255,0]}"#,
        )
        .expect("the frame should parse");

        assert_eq!(request.id, 4);
        match request.command {
            Command::SendReport {
                device,
                report_id,
                data,
            } => {
                assert_eq!(device, "046d:c547:1");
                assert_eq!(report_id, 16);
                assert_eq!(data, vec![255, 0]);
            }
            other => panic!("parsed the wrong command: {other:?}"),
        }
    }

    #[test]
    fn only_listed_origins_may_open_a_hid_socket() {
        let origins = vec!["https://openmouse.app".to_string()];
        let with = |origin: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::ORIGIN, origin.parse().unwrap());
            headers
        };

        assert!(origin_allowed(&with("https://openmouse.app"), &origins));
        assert!(!origin_allowed(
            &with("https://openmouse.app.evil.test"),
            &origins
        ));
        assert!(!origin_allowed(&with("http://openmouse.app"), &origins));
        // No Origin at all: not a browser, and not something to serve.
        assert!(!origin_allowed(&HeaderMap::new(), &origins));
    }

    #[test]
    fn a_failed_reply_carries_the_reason() {
        let encoded = encode(Reply {
            id: 7,
            ok: false,
            error: Some("no interface answered".into()),
            ..Reply::default()
        });

        assert_eq!(
            encoded,
            r#"{"id":7,"ok":false,"error":"no interface answered"}"#
        );
    }
}
