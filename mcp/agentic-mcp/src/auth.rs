use agentic_core::auth::{AUD_MCP, JwtKeys};
use fastmcp_rust::prelude::*;
use fastmcp_rust::{AuthProvider, AuthRequest};
use serde_json::json;

/// Verifies `Authorization: Bearer <jwt>` (HS256, aud `mcp`, configured issuer).
pub struct JwtAuthProvider {
    keys: JwtKeys,
    issuer: String,
}

impl JwtAuthProvider {
    pub fn new(keys: JwtKeys, issuer: impl Into<String>) -> Self {
        Self {
            keys,
            issuer: issuer.into(),
        }
    }
}

impl AuthProvider for JwtAuthProvider {
    fn authenticate(&self, _ctx: &McpContext, req: AuthRequest<'_>) -> McpResult<AuthContext> {
        let header = req
            .transport_authorization
            .ok_or_else(|| McpError::invalid_request("missing authorization"))?;
        let token = AccessToken::parse(header)
            .filter(|t| t.scheme.eq_ignore_ascii_case("Bearer"))
            .ok_or_else(|| McpError::invalid_request("malformed authorization"))?;
        let claims = self
            .keys
            .verify(&token.token, AUD_MCP, &self.issuer)
            .map_err(|_| McpError::invalid_request("invalid token"))?;
        let mut auth = AuthContext::with_subject(claims.sub.clone());
        auth.scopes = claims.roles.clone();
        auth.claims = Some(json!({
            "team": claims.team,
            "roles": claims.roles,
            "request_id": claims.request_id,
        }));
        Ok(auth)
    }
}
