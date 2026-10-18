use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

pub const AUD_GATEWAY: &str = "gateway";
pub const AUD_MCP: &str = "mcp";
pub const AUD_A2A: &str = "a2a";
pub const ROLE_PLATFORM: &str = "platform";
pub const ROLE_ADMIN: &str = "admin";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub team: String,
    pub roles: Vec<String>,
    pub iss: String,
    pub aud: String,
    pub iat: i64,
    pub exp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

impl Claims {
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }
}

/// Roles a token delegated to a peer may carry: `caller` ∩ `allowed` when set, else `caller` minus `admin` and `platform`.
pub fn downscope_roles(caller: &[String], allowed: Option<&[String]>) -> Vec<String> {
    caller
        .iter()
        .filter(|r| match allowed {
            Some(allowed) => allowed.contains(r),
            None => !matches!(r.as_str(), ROLE_ADMIN | ROLE_PLATFORM),
        })
        .cloned()
        .collect()
}

#[derive(Clone)]
pub struct JwtKeys {
    enc: EncodingKey,
    dec: DecodingKey,
}

impl std::fmt::Debug for JwtKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JwtKeys(..)")
    }
}

pub const MIN_SECRET_LEN: usize = 32;
/// The value shipped in older `.env.example` files; publicly known, so never acceptable.
pub const PLACEHOLDER_SECRET: &str = "change-me-dev-secret-at-least-32-bytes-long";

pub fn check_secret(var_name: &str, secret: &str) -> Result<()> {
    let reason = if secret.len() < MIN_SECRET_LEN {
        "must be at least 32 bytes"
    } else if secret == PLACEHOLDER_SECRET {
        "is the public placeholder value"
    } else {
        return Ok(());
    };
    Err(Error::WeakSecret {
        var: var_name.to_owned(),
        reason,
    })
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl JwtKeys {
    pub fn from_secret(secret: &[u8]) -> Self {
        Self {
            enc: EncodingKey::from_secret(secret),
            dec: DecodingKey::from_secret(secret),
        }
    }

    pub fn from_env(var_name: &str) -> Result<Self> {
        let secret = std::env::var(var_name).map_err(|_| Error::MissingEnv(var_name.to_owned()))?;
        check_secret(var_name, &secret)?;
        Ok(Self::from_secret(secret.as_bytes()))
    }

    pub fn issue(&self, claims: &Claims) -> Result<String> {
        Ok(jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            claims,
            &self.enc,
        )?)
    }

    pub fn issue_user_token(
        &self,
        sub: &str,
        team: &str,
        roles: &[String],
        issuer: &str,
        ttl_secs: i64,
    ) -> Result<String> {
        self.issue(&claims(
            sub,
            team,
            roles.to_vec(),
            issuer,
            AUD_GATEWAY,
            ttl_secs,
            None,
        ))
    }

    /// Re-audiences `parent`'s identity for MCP servers, bound to `request_id`.
    pub fn issue_delegated_token(
        &self,
        parent: &Claims,
        request_id: &str,
        issuer: &str,
        ttl_secs: i64,
    ) -> Result<String> {
        self.issue_delegated_token_for(parent, AUD_MCP, request_id, issuer, ttl_secs)
    }

    /// Like `issue_delegated_token` for an arbitrary audience (e.g. a remote agent).
    pub fn issue_delegated_token_for(
        &self,
        parent: &Claims,
        audience: &str,
        request_id: &str,
        issuer: &str,
        ttl_secs: i64,
    ) -> Result<String> {
        self.issue(&claims(
            &parent.sub,
            &parent.team,
            parent.roles.clone(),
            issuer,
            audience,
            ttl_secs,
            Some(request_id.to_owned()),
        ))
    }

    pub fn issue_service_token(&self, issuer: &str, ttl_secs: i64) -> Result<String> {
        self.issue(&claims(
            ROLE_PLATFORM,
            ROLE_PLATFORM,
            vec![ROLE_PLATFORM.to_owned()],
            issuer,
            AUD_MCP,
            ttl_secs,
            None,
        ))
    }

    /// Expiry is enforced with no leeway.
    pub fn verify(&self, token: &str, expected_aud: &str, issuer: &str) -> Result<Claims> {
        let mut v = Validation::new(Algorithm::HS256);
        v.leeway = 0;
        v.set_audience(&[expected_aud]);
        v.set_issuer(&[issuer]);
        v.set_required_spec_claims(&["exp", "aud", "iss", "sub"]);
        Ok(jsonwebtoken::decode::<Claims>(token, &self.dec, &v)?.claims)
    }
}

fn claims(
    sub: &str,
    team: &str,
    roles: Vec<String>,
    iss: &str,
    aud: &str,
    ttl_secs: i64,
    request_id: Option<String>,
) -> Claims {
    let iat = now();
    Claims {
        sub: sub.to_owned(),
        team: team.to_owned(),
        roles,
        iss: iss.to_owned(),
        aud: aud.to_owned(),
        iat,
        exp: iat + ttl_secs,
        request_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISS: &str = "agentic-platform";

    fn keys() -> JwtKeys {
        JwtKeys::from_secret(b"test-secret-test-secret-test-secret")
    }

    fn user() -> String {
        keys()
            .issue_user_token("alice", "docs-team", &["reader".into()], ISS, 60)
            .unwrap()
    }

    #[test]
    fn round_trip() {
        let c = keys().verify(&user(), AUD_GATEWAY, ISS).unwrap();
        assert_eq!(c.sub, "alice");
        assert_eq!(c.team, "docs-team");
        assert_eq!(c.roles, ["reader"]);
        assert_eq!(c.aud, AUD_GATEWAY);
        assert!(c.request_id.is_none());
        assert!(c.has_role("reader"));
    }

    #[test]
    fn wrong_audience() {
        assert!(keys().verify(&user(), AUD_MCP, ISS).is_err());
    }

    #[test]
    fn wrong_issuer() {
        assert!(keys().verify(&user(), AUD_GATEWAY, "other").is_err());
    }

    #[test]
    fn wrong_secret() {
        let other = JwtKeys::from_secret(b"another-secret-another-secret-xx");
        assert!(other.verify(&user(), AUD_GATEWAY, ISS).is_err());
    }

    #[test]
    fn expired() {
        let t = keys().issue_user_token("a", "t", &[], ISS, -10).unwrap();
        assert!(keys().verify(&t, AUD_GATEWAY, ISS).is_err());
    }

    #[test]
    fn delegated_carries_identity_and_request_id() {
        let parent = keys().verify(&user(), AUD_GATEWAY, ISS).unwrap();
        let t = keys()
            .issue_delegated_token(&parent, "req-1", ISS, 60)
            .unwrap();
        let c = keys().verify(&t, AUD_MCP, ISS).unwrap();
        assert_eq!(c.sub, "alice");
        assert_eq!(c.roles, ["reader"]);
        assert_eq!(c.request_id.as_deref(), Some("req-1"));
        assert!(keys().verify(&t, AUD_GATEWAY, ISS).is_err());
    }

    #[test]
    fn downscoping_intersects_or_strips_privileged_roles() {
        let roles = |r: &[&str]| r.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let caller = roles(&["reader", "admin", "platform", "engineer"]);
        assert_eq!(
            downscope_roles(&caller, None),
            roles(&["reader", "engineer"])
        );
        assert_eq!(
            downscope_roles(&caller, Some(&roles(&["engineer", "admin", "other"]))),
            roles(&["admin", "engineer"])
        );
        assert!(downscope_roles(&caller, Some(&[])).is_empty());
    }

    #[test]
    fn service_token() {
        let t = keys().issue_service_token(ISS, 60).unwrap();
        let c = keys().verify(&t, AUD_MCP, ISS).unwrap();
        assert_eq!(c.sub, ROLE_PLATFORM);
        assert_eq!(c.roles, [ROLE_PLATFORM]);
    }

    #[test]
    fn check_secret_rejects_short_and_placeholder() {
        assert!(check_secret("X", "short").is_err());
        assert!(check_secret("X", &"a".repeat(31)).is_err());
        assert!(check_secret("X", PLACEHOLDER_SECRET).is_err());
        assert!(check_secret("X", &"a".repeat(32)).is_ok());
        let msg = check_secret("X", "short").unwrap_err().to_string();
        assert!(
            msg.contains("32 bytes") && msg.contains("openssl rand"),
            "{msg}"
        );
    }

    #[test]
    fn from_env_missing() {
        assert!(matches!(
            JwtKeys::from_env("AGENTIC_CORE_SURELY_UNSET_VAR"),
            Err(Error::MissingEnv(_))
        ));
    }
}
