use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{StatusCode, header::AUTHORIZATION},
    middleware::Next,
    response::{IntoResponse, Response},
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Claims {
    pub sub: String,
    #[serde(default)]
    pub team: String,
}

pub struct BearerAuth {
    key: DecodingKey,
    validation: Validation,
}

impl BearerAuth {
    pub fn new(secret: &[u8], audience: &str, issuer: &str) -> Self {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_audience(&[audience]);
        validation.set_issuer(&[issuer]);
        validation.set_required_spec_claims(&["exp", "sub", "aud", "iss"]);
        Self {
            key: DecodingKey::from_secret(secret),
            validation,
        }
    }

    pub fn verify_header(&self, header: Option<&str>) -> Result<Claims, String> {
        let token = header
            .and_then(|h| h.strip_prefix("Bearer "))
            .ok_or("missing bearer token")?;
        decode::<Claims>(token, &self.key, &self.validation)
            .map(|d| d.claims)
            .map_err(|e| e.to_string())
    }
}

pub const DEFAULT_AUDIENCE: &str = "a2a";
const ISSUER: &str = "agentic-platform";

/// Bearer auth is on unless `OPS_REQUIRE_BEARER=0`; an enabled layer needs `PLATFORM_JWT_SECRET`.
pub fn from_env(var: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Option<BearerAuth>> {
    if var("OPS_REQUIRE_BEARER").as_deref() == Some("0") {
        tracing::warn!("OPS_REQUIRE_BEARER=0: A2A requests are accepted without authentication");
        return Ok(None);
    }
    let Some(secret) = var("PLATFORM_JWT_SECRET") else {
        anyhow::bail!(
            "bearer auth is required: set PLATFORM_JWT_SECRET, or OPS_REQUIRE_BEARER=0 to disable it"
        );
    };
    let aud = var("OPS_JWT_AUDIENCE").unwrap_or_else(|| DEFAULT_AUDIENCE.into());
    Ok(Some(BearerAuth::new(secret.as_bytes(), &aud, ISSUER)))
}

pub async fn middleware(State(auth): State<Arc<BearerAuth>>, req: Request, next: Next) -> Response {
    let header = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    match auth.verify_header(header) {
        Ok(c) => {
            tracing::info!(sub = %c.sub, team = %c.team, "authenticated caller");
            next.run(req).await
        }
        Err(e) => {
            tracing::warn!(error = %e, "rejected request");
            (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::json;

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

    fn token(secret: &[u8], aud: &str, iss: &str, exp: i64) -> String {
        let claims =
            json!({"sub": "alice", "team": "docs-team", "aud": aud, "iss": iss, "exp": exp});
        encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(secret),
        )
        .unwrap()
    }

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn bearer_is_on_by_default_and_needs_a_secret() {
        assert!(from_env(&lookup(&[])).is_err());
        let a = from_env(&lookup(&[(
            "PLATFORM_JWT_SECRET",
            "0123456789abcdef0123456789abcdef",
        )]))
        .unwrap()
        .expect("auth enabled by default");
        let t = token(SECRET, "a2a", "agentic-platform", far());
        assert!(a.verify_header(Some(&format!("Bearer {t}"))).is_ok());
        assert!(a.verify_header(None).is_err());
    }

    #[test]
    fn explicit_zero_disables_bearer_even_without_a_secret() {
        assert!(
            from_env(&lookup(&[("OPS_REQUIRE_BEARER", "0")]))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn audience_is_configurable() {
        let a = from_env(&lookup(&[
            ("PLATFORM_JWT_SECRET", "0123456789abcdef0123456789abcdef"),
            ("OPS_JWT_AUDIENCE", "a2a:ops"),
        ]))
        .unwrap()
        .unwrap();
        for (aud, ok) in [("a2a:ops", true), ("a2a", false), ("a2a:other", false)] {
            let t = token(SECRET, aud, "agentic-platform", far());
            assert_eq!(
                a.verify_header(Some(&format!("Bearer {t}"))).is_ok(),
                ok,
                "{aud}"
            );
        }
    }

    fn far() -> i64 {
        4_102_444_800
    }

    fn auth() -> BearerAuth {
        BearerAuth::new(SECRET, "a2a", "agentic-platform")
    }

    #[test]
    fn accepts_valid_token() {
        let t = token(SECRET, "a2a", "agentic-platform", far());
        let c = auth().verify_header(Some(&format!("Bearer {t}"))).unwrap();
        assert_eq!((c.sub.as_str(), c.team.as_str()), ("alice", "docs-team"));
    }

    #[test]
    fn rejects_missing_header() {
        assert!(auth().verify_header(None).is_err());
        assert!(auth().verify_header(Some("Basic abc")).is_err());
    }

    #[test]
    fn rejects_bad_secret_aud_iss_and_expiry() {
        let a = auth();
        for t in [
            token(
                b"another-secret-another-secret-123",
                "a2a",
                "agentic-platform",
                far(),
            ),
            token(SECRET, "other", "agentic-platform", far()),
            token(SECRET, "a2a", "evil", far()),
            token(SECRET, "a2a", "agentic-platform", 1_000_000),
        ] {
            assert!(a.verify_header(Some(&format!("Bearer {t}"))).is_err());
        }
    }
}
