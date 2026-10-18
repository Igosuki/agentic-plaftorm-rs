//! `PLATFORM_JWT_SECRET=... cargo run -p agentic-mcp --example mint_token -- <service | user SUB TEAM ROLES(csv) | wrong-aud>`
use agentic_core::auth::JwtKeys;

fn main() {
    let secret = std::env::var("PLATFORM_JWT_SECRET").expect("PLATFORM_JWT_SECRET");
    let issuer = std::env::var("JWT_ISSUER").unwrap_or_else(|_| "agentic-platform".into());
    let keys = JwtKeys::from_secret(secret.as_bytes());
    let args: Vec<String> = std::env::args().skip(1).collect();
    let token = match args.first().map(String::as_str) {
        Some("service") => keys.issue_service_token(&issuer, 3600),
        Some("user") if args.len() == 4 => {
            let roles: Vec<String> = args[3].split(',').map(str::to_owned).collect();
            let gateway = keys
                .issue_user_token(&args[1], &args[2], &roles, &issuer, 3600)
                .unwrap();
            let parent = keys
                .verify(&gateway, agentic_core::auth::AUD_GATEWAY, &issuer)
                .unwrap();
            keys.issue_delegated_token(&parent, "req-demo", &issuer, 3600)
        }
        Some("wrong-aud") => {
            keys.issue_user_token("alice", "docs-team", &["reader".into()], &issuer, 3600)
        }
        _ => {
            eprintln!("usage: mint_token service | user SUB TEAM ROLES | wrong-aud");
            std::process::exit(2);
        }
    };
    println!("{}", token.expect("issue token"));
}
