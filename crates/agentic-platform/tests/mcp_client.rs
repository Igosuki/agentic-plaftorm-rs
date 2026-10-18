use agentic_platform::mcp::{McpClient, McpError, ToolOutcome};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, header, header_exists, method, path},
};

async fn server_with(resp: ResponseTemplate) -> (MockServer, McpClient) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(header("mcp-protocol-version", "2026-07-28"))
        .and(header("mcp-method", "tools/call"))
        .and(header("mcp-name", "add"))
        .and(header("authorization", "Bearer tok"))
        .and(header_exists("accept"))
        .and(body_partial_json(json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": {
                "name": "add",
                "arguments": {"a": 2, "b": 3},
                "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}
            }
        })))
        .respond_with(resp)
        .mount(&server)
        .await;
    let client = McpClient::new(format!("{}/mcp", server.uri()));
    (server, client)
}

async fn call(client: &McpClient) -> Result<ToolOutcome, McpError> {
    client
        .call_tool("tok", "add", json!({"a": 2, "b": 3}))
        .await
}

fn json_ok(body: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

#[tokio::test]
async fn json_response() {
    let (_s, c) = server_with(json_ok(json!({
        "jsonrpc": "2.0", "id": 1,
        "result": {"content": [{"type": "text", "text": "5"}], "structuredContent": {"sum": 5}, "resultType": "complete"}
    })))
    .await;
    assert_eq!(
        call(&c).await.unwrap(),
        ToolOutcome::Ok {
            text: "5".into(),
            structured: Some(json!({"sum": 5}))
        }
    );
}

#[tokio::test]
async fn sse_response() {
    let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"5\"}]}}\n\n";
    let (_s, c) =
        server_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")).await;
    assert_eq!(
        call(&c).await.unwrap(),
        ToolOutcome::Ok {
            text: "5".into(),
            structured: None
        }
    );
}

#[tokio::test]
async fn denied_via_rpc_error_code() {
    let (_s, c) = server_with(json_ok(json!({
        "jsonrpc": "2.0", "id": 1, "error": {"code": -32003, "message": "nope"}
    })))
    .await;
    assert!(matches!(
        call(&c).await.unwrap(),
        ToolOutcome::Denied { .. }
    ));
}

#[tokio::test]
async fn denied_via_message_prefix() {
    let (_s, c) = server_with(json_ok(json!({
        "jsonrpc": "2.0", "id": 1, "error": {"code": -32603, "message": "policy_denied: role reader may not call docs.add"}
    })))
    .await;
    match call(&c).await.unwrap() {
        ToolOutcome::Denied { reason } => assert!(reason.contains("reader")),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn denied_via_result_meta() {
    let (_s, c) = server_with(json_ok(json!({
        "jsonrpc": "2.0", "id": 1,
        "result": {"isError": true, "content": [{"type": "text", "text": "not allowed"}], "_meta": {"agentic/denied": true}}
    })))
    .await;
    assert!(matches!(
        call(&c).await.unwrap(),
        ToolOutcome::Denied { .. }
    ));
}

#[tokio::test]
async fn denied_via_http_403() {
    let (_s, c) = server_with(ResponseTemplate::new(403).set_body_string("forbidden")).await;
    assert!(matches!(
        call(&c).await.unwrap(),
        ToolOutcome::Denied { .. }
    ));
}

#[tokio::test]
async fn tool_error_is_not_denial() {
    let (_s, c) = server_with(json_ok(json!({
        "jsonrpc": "2.0", "id": 1,
        "result": {"isError": true, "content": [{"type": "text", "text": "division by zero"}]}
    })))
    .await;
    assert_eq!(
        call(&c).await.unwrap(),
        ToolOutcome::Error {
            text: "division by zero".into()
        }
    );
}

#[tokio::test]
async fn http_401_is_unauthorized() {
    let (_s, c) = server_with(ResponseTemplate::new(401)).await;
    assert!(matches!(call(&c).await, Err(McpError::Unauthorized)));
}

#[tokio::test]
async fn http_500_is_status_error() {
    let (_s, c) = server_with(ResponseTemplate::new(500)).await;
    assert!(matches!(call(&c).await, Err(McpError::Status(s)) if s.as_u16() == 500));
}

#[tokio::test]
async fn list_tools_json_and_sse() {
    let tools = json!({"tools": [
        {"name": "add", "description": "Add", "inputSchema": {"type": "object", "properties": {"a": {"type": "integer"}}}},
        {"name": "ping"}
    ]});
    for template in [
        json_ok(json!({"jsonrpc": "2.0", "id": 1, "result": tools})),
        ResponseTemplate::new(200).set_body_raw(
            format!(
                "data: {}\n\n",
                json!({"jsonrpc": "2.0", "id": 1, "result": tools})
            ),
            "text/event-stream",
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("mcp-method", "tools/list"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(template)
            .mount(&server)
            .await;
        let got = McpClient::new(format!("{}/mcp", server.uri()))
            .list_tools("tok")
            .await
            .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "add");
        assert_eq!(got[0].description, "Add");
        assert_eq!(got[1].input_schema["type"], "object");
    }
}

#[tokio::test]
async fn list_tools_follows_cursor() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"params": {"cursor": "p2"}})))
        .respond_with(json_ok(
            json!({"jsonrpc": "2.0", "id": 2, "result": {"tools": [{"name": "b"}]}}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(json_ok(json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": [{"name": "a"}], "nextCursor": "p2"}})))
        .mount(&server)
        .await;
    let got = McpClient::new(format!("{}/mcp", server.uri()))
        .list_tools("t")
        .await
        .unwrap();
    assert_eq!(
        got.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        ["a", "b"]
    );
}

#[tokio::test]
async fn list_tools_401() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let r = McpClient::new(format!("{}/mcp", server.uri()))
        .list_tools("bad")
        .await;
    assert!(matches!(r, Err(McpError::Unauthorized)));
}

#[tokio::test]
async fn reply_with_foreign_id_is_rejected_in_json_and_sse() {
    let reply =
        json!({"jsonrpc": "2.0", "id": 99, "result": {"content": [{"type": "text", "text": "5"}]}});
    for template in [
        json_ok(reply.clone()),
        ResponseTemplate::new(200).set_body_raw(format!("data: {reply}\n\n"), "text/event-stream"),
    ] {
        let (_s, c) = server_with(template).await;
        assert!(matches!(call(&c).await, Err(McpError::Protocol(_))));
    }
}
