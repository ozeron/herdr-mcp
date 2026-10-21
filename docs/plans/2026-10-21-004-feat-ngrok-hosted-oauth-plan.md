---
title: Ngrok Remote With Hosted OAuth - Plan
type: feat
date: 2026-10-21
artifact_contract: ce-unified-plan/v1
artifact_readiness: implementation-ready
product_contract_source: ce-plan-bootstrap
execution: code
supersedes: 2026-10-21-003-feat-ngrok-oauth-remote-plan.md (U2, U3, U4)
---

# Ngrok Remote With Hosted OAuth - Plan

## Goal Capsule

- **Objective:** ChatGPT reaches herdr-mcp at `https://brainless-insuppressibly-emeline.ngrok-free.dev/mcp`. It gets in only with a token from a hosted OAuth provider, issued after the owner logs in. That replaces the secret-path script and the Node bridge.
- **Authority:** this plan, then `AGENTS.md` and the user's global instructions.
- **Stop conditions:**
  - stop and ask before anything that serves `/mcp` without a verified token;
  - stop before accepting a token whose issuer, audience or owner is unchecked;
  - stop before storing a secret outside the Keychain.
- **Tail ownership:** local commits. The user creates the provider app, connects ChatGPT, and approves the push.

## Status

- Done, from plan 003:
  - U1 `e4f15a9`: `http-serve`, Streamable HTTP on loopback, and the `TokenCheck` layer (`DenyAll` by default).
- Leftover: self-built OAuth dependencies in `Cargo.toml` and `Cargo.lock` (argon2, sha2, base64, getrandom, axum form/json/query, dev opt-levels), with no code. U2 reverts them.
- Live now:
  - the hand-run `~/.local/state/herdr-mcp/ngrok/start-full.sh` (supergateway plus ngrok on the static domain);
  - the only lock is the secret path.

## Key Technical Decisions

- KTD1. **A hosted provider is the authorization server, and herdr-mcp is only a resource server.** No mature Rust OAuth-server library exists, and rmcp's auth is client-side only. That means less security code we own, and the owner gets real 2FA. Self-built (plan 003 U2) stays the fallback if the user rejects a third party.
- KTD2. **The provider must support dynamic client registration and PKCE.** ChatGPT registers itself; candidates are WorkOS AuthKit and Auth0. Check this before U4, not assumed.
- KTD3. **Validate tokens as JWTs against the provider's JWKS**, with the `jsonwebtoken` crate. Check the signature, `iss`, `aud` (the resource URL), `exp` and `nbf`.
  - Fetch the JWKS with reqwest (rustls), and cache it with a TTL.
  - On an unknown `kid`, refetch once.
- KTD4. **Check an owner allowlist on every token** (`sub` or verified `email`). The provider lets anyone it authenticates get a token, so without this check any provider user gets a shell.
- KTD5. **ngrok runs as a child of the same process**, started only after the listener is bound. The port is ours by construction.
- KTD6. **Logout and revoke happen at the provider**: revoke sessions, or rotate its signing keys. Access tokens last minutes, so revoking takes effect quickly. `tunnel ngrok stop` is the local kill switch.

## Implementation Units

### U2. OAuth resource server (JWT check)

- **Goal:** `http-serve --oauth-issuer <url> --oauth-audience <url> --owner <sub|email>...`.
- **What it adds:**
  - serves `/.well-known/oauth-protected-resource` (and `/mcp` suffixed) with `authorization_servers: [issuer]`;
  - guards `/mcp` with a `JwtCheck` that implements `TokenCheck` (made async if needed);
  - with no `--oauth-issuer`, the server stays `DenyAll`.
- **Files:** `Cargo.toml` and `Cargo.lock` (revert the leftover dependencies; add `jsonwebtoken` and reqwest with rustls), `src/http.rs`, `src/oauth.rs` (new), `src/cli.rs`; test: `tests/oauth.rs`.
- **Test scenarios** (a local fake issuer serving JWKS and discovery; the test signs its own tokens):
  - a valid owner token gets `initialize` OK;
  - no token gets `401` with the `WWW-Authenticate` resource-metadata header;
  - rejected with 401: a wrong issuer, a wrong audience, an expired token, a bad signature, `alg: none` or HS256 with the public key, and a valid token for a non-owner;
  - key rotation: an unknown `kid` triggers one refetch, then success;
  - the metadata document names the issuer and the resource;
  - nothing about tokens is logged.

### U3. `herdr-mcp tunnel ngrok` commands

- **Goal:** `setup | start [--full] [--for D] [--session S] | run | stop | status`, plus `doctor` checks.
- **Commands:**
  - `setup` stores the domain, issuer, audience and owner in `~/.config/herdr-mcp/ngrok.toml` (0600), and checks ngrok and its authtoken.
  - `start` writes the LaunchAgent `dev.herdr-mcp.ngrok`; `--full` needs the typed disclosure.
  - `run` binds `http-serve` on a free loopback port, then spawns `ngrok http 127.0.0.1:<port> --url https://<domain>`. If either dies, both stop.
  - `stop` and `status` (mode, URL, expiry, the latest auth failures).
- **Unchanged:** the audit log, `--for`, `herdr_tunnel_stop` and the remote instructions work as on the OpenAI-tunnel path.
- **Also:** plugin actions, and a README section.
- **Files:** `src/tunnel/ngrok.rs` (new), `src/cli.rs`, `src/main.rs`, `herdr-plugin.toml`, `README.md`; tests: `tests/tunnel_ngrok.rs`, `tests/manifest.rs`.
- **Test scenarios** (fake `ngrok` and `launchctl`):
  - setup validation;
  - plist contents and label;
  - `--full` disclosure;
  - `run` binds before spawning ngrok with `--url`;
  - stop and status;
  - doctor reports a missing ngrok or authtoken, and an unreachable issuer.

### U4. Provider setup and live verify (user + agent)

- **Provider:** the user picks it (KTD2); we confirm DCR and PKCE in its docs.
- **The user creates the app:**
  - enables dynamic registration;
  - allows the redirect `https://chatgpt.com/connector/oauth/*`;
  - sets the API audience to `https://<domain>/mcp`;
  - restricts sign-in to their account.
- **The agent then:** runs `tunnel ngrok setup`, then `start --full`; the user adds the plugin in ChatGPT (OAuth) and logs in.
- **Verify:**
  - no token, or a non-owner token: `401`;
  - after login, a read and a run both succeed and are audited;
  - revoking the provider session cuts access within the token lifetime.

### U5. Ship

- The full suite passes; fmt and clippy are clean.
- `git fetch`, reread the diff, push the batched commits, `herdr plugin update`.
- Delete `~/.local/state/herdr-mcp/ngrok/` (the script and saved secret), and stop supergateway.

## Verification Contract

| Gate | Command |
|---|---|
| Format | `cargo fmt --check` |
| Lint | `cargo clippy -j 4 --all-targets -- -D warnings` |
| Tests | targeted `cargo test -j 4 --test oauth --test http --test tunnel_ngrok -- --test-threads 4`; full suite once per unit if `memory_pressure` shows at least 25% free |

## Open Decisions

- Which provider: WorkOS AuthKit, Auth0, or one MedMe already uses. This blocks U4, not U2 or U3.
- An owner check by `sub` (stable) or by email (readable). Default: `sub`, with email shown in status.
