use serde_json::{Value, json};

pub const PROTOCOL_VERSION: &str = "2026-07-28";

pub const HEADER_PROTOCOL_VERSION: &str = "MCP-Protocol-Version";
pub const HEADER_METHOD: &str = "Mcp-Method";
pub const HEADER_NAME: &str = "Mcp-Name";
pub const ACCEPT: &str = "application/json, text/event-stream";

pub const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";

pub const METHOD_TOOLS_LIST: &str = "tools/list";
pub const METHOD_TOOLS_CALL: &str = "tools/call";

pub const POLICY_DENIED_CODE: i64 = -32003;
pub const POLICY_DENIED_PREFIX: &str = "policy_denied:";
pub const DENIED_META_KEY: &str = "agentic/denied";

pub fn request_meta() -> Value {
    json!({
        META_PROTOCOL_VERSION: PROTOCOL_VERSION,
        META_CLIENT_CAPABILITIES: {},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_shape() {
        let m = request_meta();
        assert_eq!(m[META_PROTOCOL_VERSION], "2026-07-28");
        assert_eq!(m[META_CLIENT_CAPABILITIES], json!({}));
    }
}
