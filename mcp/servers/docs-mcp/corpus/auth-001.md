# Authentication
Nimbus Queue supports API tokens and mutual TLS (mTLS). API tokens are created with `nimbusctl token create --name ci --ttl 30d`; the default token lifetime is 90 days and the maximum is 365 days. Send a token in the `Authorization: Bearer <token>` header.
To enable mTLS set `tls.client_auth = "required"` in `nimbus.toml` and point `tls.ca_file` at your CA bundle. The client certificate common name is used as the principal.
Tokens can be revoked immediately with `nimbusctl token revoke <id>`.
