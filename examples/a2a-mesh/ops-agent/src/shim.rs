//! Wire adapters between adk-server's v1 dialect (a2a-protocol-types 0.5) and a2a-rs 0.10 clients.

use axum::{
    body::{Body, to_bytes},
    extract::Request,
    http::{StatusCode, header::CONTENT_LENGTH},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

const MAX_REQ: usize = 8 << 20;
const MAX_RESP: usize = 32 << 20;

/// Adds the `configuration.acceptedOutputModes` field adk requires but a2a-rs omits.
/// Returns whether the call is `SendMessage`, whose response needs `unwrap_task_result`.
pub fn patch_request(v: &mut Value) -> bool {
    if let Some(c) = v
        .pointer_mut("/params/configuration")
        .and_then(Value::as_object_mut)
    {
        c.entry("acceptedOutputModes").or_insert(json!([]));
    }
    v["method"] == "SendMessage"
}

/// adk returns a bare Task as the SendMessage result; the v1 oneof expects `{"task": ...}`.
pub fn patch_response(v: &mut Value) {
    if let Some(r) = v.get_mut("result").filter(|r| r.get("status").is_some()) {
        *r = json!({"task": r.take()});
    }
}

pub async fn middleware(req: Request, next: Next) -> Response {
    let (mut parts, body) = req.into_parts();
    let Ok(bytes) = to_bytes(body, MAX_REQ).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let mut is_send = false;
    let out = match serde_json::from_slice::<Value>(&bytes) {
        Ok(mut v) => {
            is_send = patch_request(&mut v);
            serde_json::to_vec(&v).unwrap_or_else(|_| bytes.to_vec())
        }
        Err(_) => bytes.to_vec(),
    };
    parts.headers.remove(CONTENT_LENGTH);
    let resp = next.run(Request::from_parts(parts, Body::from(out))).await;
    if !is_send {
        return resp;
    }
    let (mut rp, rb) = resp.into_parts();
    let Ok(rbytes) = to_bytes(rb, MAX_RESP).await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let fixed = match serde_json::from_slice::<Value>(&rbytes) {
        Ok(mut v) => {
            patch_response(&mut v);
            serde_json::to_vec(&v).unwrap_or_else(|_| rbytes.to_vec())
        }
        Err(_) => rbytes.to_vec(),
    };
    rp.headers.remove(CONTENT_LENGTH);
    Response::from_parts(rp, Body::from(fixed))
}

#[cfg(test)]
mod tests {
    use axum::{Router, middleware::from_fn, routing::post};
    use tower::ServiceExt;

    use super::*;

    async fn post_body(body: Vec<u8>) -> StatusCode {
        let app = Router::new()
            .route("/", post(|| async { "ok" }))
            .layer(from_fn(middleware));
        let req = Request::post("/").body(Body::from(body)).unwrap();
        app.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn oversize_request_is_413_not_an_empty_body() {
        assert_eq!(
            post_body(vec![b' '; MAX_REQ + 1]).await,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(post_body(b"{}".to_vec()).await, StatusCode::OK);
    }

    #[test]
    fn injects_accepted_output_modes() {
        let mut v = json!({"method": "SendMessage", "params": {"configuration": {"returnImmediately": false}}});
        assert!(patch_request(&mut v));
        assert_eq!(
            v["params"]["configuration"]["acceptedOutputModes"],
            json!([])
        );
    }

    #[test]
    fn keeps_existing_accepted_output_modes() {
        let mut v = json!({"method": "SendMessage", "params": {"configuration": {"acceptedOutputModes": ["text/plain"]}}});
        patch_request(&mut v);
        assert_eq!(
            v["params"]["configuration"]["acceptedOutputModes"],
            json!(["text/plain"])
        );
    }

    #[test]
    fn no_configuration_untouched() {
        let mut v = json!({"method": "GetTask", "params": {"id": "t"}});
        let before = v.clone();
        assert!(!patch_request(&mut v));
        assert_eq!(v, before);
    }

    #[test]
    fn wraps_bare_task() {
        let mut v = json!({"jsonrpc": "2.0", "id": 1, "result": {"id": "t", "status": {"state": "TASK_STATE_COMPLETED"}}});
        patch_response(&mut v);
        assert_eq!(v["result"]["task"]["id"], "t");
        assert!(v["result"].get("status").is_none());
    }

    #[test]
    fn wrap_is_idempotent() {
        let mut v = json!({"result": {"task": {"id": "t", "status": {}}}});
        let before = v.clone();
        patch_response(&mut v);
        assert_eq!(v, before);
    }

    #[test]
    fn errors_untouched() {
        let mut v = json!({"error": {"code": -32601, "message": "x"}});
        let before = v.clone();
        patch_response(&mut v);
        assert_eq!(v, before);
    }
}
