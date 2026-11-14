# Brief: real connector implementations

Context: `crates/plane` is the control plane of AI Factory (read `REQUIREMENTS.md` sections 7, 8, 10, 11, 15).
Connector roles are traits in `crates/plane/src/connectors/mod.rs` (`TicketSource`, `RepoHost`, `Chat`).
Fakes in `crates/plane/src/connectors/fake.rs` show the expected semantics. A connector is built from
its DB row and decrypted secrets (`Loaded`, with `cfg(name)` and `secret(name)`); field names are in `KINDS`.

Rules for every connector module:

- `pub fn from_loaded(l: &Loaded) -> anyhow::Result<Self>`; use `reqwest` (workspace dep, rustls) with an
  overridable base URL from config (`api_url` / `base_url` / `org_url`), so tests can point at a local mock.
- Implement the trait fully; methods that the remote system cannot support return a clear error.
- Markers: comments carry the given marker so `find_comment` / `find_by_marker` can find an earlier effect
  after a crash (idempotency). Use a hidden HTML comment where the system renders Markdown.
- Webhooks: `pub fn verify_signature(secret: &str, headers: &axum::http::HeaderMap, body: &[u8]) -> bool`
  (constant-time compare; `hmac` 0.13 + `sha2` 0.11 are deps) and
  `pub fn parse_webhook(headers: &axum::http::HeaderMap, body: &[u8]) -> anyhow::Result<Vec<EventType>>`
  returning the role's event type (`TicketEvent`, `RepoEvent`, `ChatEvent`) with a stable `id` for de-duplication.
- Tests (in the same file, `#[cfg(test)]`): unit tests against a local mock HTTP server (an axum router bound
  to `127.0.0.1:0` in the test; `axum` and `tokio` are deps) that check request shape, auth header, pagination
  where relevant, marker search, webhook signature verification (valid and invalid) and webhook parsing from
  realistic sample payloads. These must pass with no network.
- Contract tests against the real service: `#[ignore]` tests that read credentials from environment variables
  (document the names at the top of the file, e.g. `FACTORY_CONTRACT_GITHUB_TOKEN`, `..._REPO`) and return early
  with a message when absent. They must never run by default.
- Check current API documentation before relying on an endpoint, header or field.
- Only edit your own module files, plus the registry lines in `connectors/mod.rs` (the `pub mod` line and the
  match arm in `ticket_source` / `repo_host` / `chat`, which currently `bail!` for your kind). Do not touch any
  other file. Do not commit. Run `cargo test -p factory-plane --lib connectors` and make it pass; make sure
  `cargo build -p factory-plane` has no warnings from your files.
