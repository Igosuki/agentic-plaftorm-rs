//! Usage: `PLATFORM_JWT_SECRET=... cargo run --example ask -- [url] [question]`.
//! The server requires a bearer token unless it runs with `OPS_REQUIRE_BEARER=0`. `TOKEN` supplies a ready-made one;
//! `AUDIENCE` (default `a2a`) must match the server's `OPS_JWT_AUDIENCE`.

use a2a_rs::domain::{Message, SendCompletion};
use a2a_rs::{JsonRpcClient, Transport};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Value, json};

fn mint(secret: &str, audience: &str) -> Result<String, Box<dyn std::error::Error>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let claims = json!({
        "sub": "alice", "team": "docs-team", "roles": ["reader"],
        "iss": "agentic-platform", "aud": audience, "iat": now, "exp": now + 300
    });
    Ok(encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )?)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let base = args
        .next()
        .unwrap_or_else(|| "http://localhost:8090".into());
    let text = args
        .next()
        .unwrap_or_else(|| "Consumer lag on orders is growing, what do I do?".into());

    // TOKEN wins; otherwise PLATFORM_JWT_SECRET mints a token for alice/docs-team.
    let audience = std::env::var("AUDIENCE").unwrap_or_else(|_| "a2a".into());
    let token = match (std::env::var("TOKEN"), std::env::var("PLATFORM_JWT_SECRET")) {
        (Ok(t), _) => Some(t),
        (_, Ok(s)) => Some(mint(&s, &audience)?),
        _ => {
            eprintln!("no TOKEN or PLATFORM_JWT_SECRET: only works against OPS_REQUIRE_BEARER=0");
            None
        }
    };
    let client = match token {
        Some(t) => JsonRpcClient::try_with_auth(base, t)?,
        None => JsonRpcClient::try_new(base)?,
    };

    let card = client.get_agent_card().await?;
    println!("card: {}", serde_json::to_string(&card)?);

    let msg = Message::user_text(text, uuid::Uuid::new_v4().to_string());
    let task = client
        .send_task_message(None, &msg, None, None, SendCompletion::WhenSettled)
        .await?;
    let v: Value = serde_json::to_value(&task)?;
    println!(
        "state: {}",
        v.pointer("/status/state").unwrap_or(&Value::Null)
    );

    let text: String = v["artifacts"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|a| a["parts"].as_array().into_iter().flatten())
        .filter_map(|p| p["text"].as_str())
        .collect();
    println!("final text: {text}");
    match serde_json::from_str::<Value>(&text) {
        Ok(j) => println!(
            "parsed: runbook_id={} severity={}",
            j["runbook_id"], j["severity"]
        ),
        Err(e) => println!("not JSON: {e}\ntask: {}", serde_json::to_string_pretty(&v)?),
    }
    Ok(())
}
