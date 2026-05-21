# mixer — provider auth notes

Findings from `mixer-rs/` (sibling project, vendored at `./mixer-rs/`):
how it authenticates to each LLM provider it supports.

Skipped: `glm` / z.ai. Mixer uses an unofficial dashboard-internal
endpoint there and the README explicitly warns it "may break without
notice." Not in scope for cockpit.

Cross-references: see `mixer-rs/src/providers/<id>.rs` for the source,
`mixer-rs/src/auth/device_flow.rs` for the shared RFC 8628 helper, and
`mixer-rs/src/providers/common/oauth_refresh.rs` for the refresh-and-
expiry helpers.

---

## Storage shape

- Credentials live one-file-per-provider at
  `~/.config/mixer/credentials/<provider>.json`, mode `0600` on Unix.
- Each blob is an opaque `serde_json::Value` — every provider owns its
  own shape. The mixer core doesn't care; the provider impl serializes
  what it needs.
- Only credential **names** (env-var names for API keys) ever land in
  `config.json`. Tokens, refresh tokens, and API keys are written to
  the per-provider credentials file, never to the main config.

`cockpit` should follow the same split: per-provider credentials file
under `~/.config/cockpit/credentials/<id>.json`, opaque blobs, 0600,
and env-var *names* (not values) in the main config.

---

## Auth kinds in use

Three shapes cover every supported provider:

### 1. OAuth 2.0 device authorization grant (RFC 8628)

- **kimi-code** (Moonshot's Kimi Code subscription).
  Vanilla RFC 8628 against `auth.kimi.com`:
  - `POST /api/oauth/device_authorization` → user code + verification URI.
  - Print code + URL to stderr, try to open the browser.
  - Poll `POST /api/oauth/token` with `grant_type=urn:ietf:params:oauth:grant-type:device_code`.
  - Persist `{access_token, refresh_token, expires_at}`.
  - Refresh is a plain `POST /api/oauth/token` with `grant_type=refresh_token` — no PKCE.

  cockpit can reuse this exactly as-is; it's RFC-clean.

### 2. OAuth 2.0 with PKCE + device flow hybrid (codex / ChatGPT)

- **codex** (ChatGPT Plus/Pro).
  Looks like RFC 8628 on the surface but is **not** — OpenAI's variant:
  - `POST /api/accounts/deviceauth/usercode` → user code (custom path, not RFC).
  - `POST /api/accounts/deviceauth/token` returns an `authorization_code` + PKCE `code_verifier`, **not** an access token.
  - Finish with a standard `grant_type=authorization_code` exchange at `/oauth/token` (same `auth.openai.com`).
  - Decode the resulting `id_token` (JWT) to pull the `chatgpt_account_id` claim, which is required as a header on subsequent Responses-API calls.
  - Persist `{access_token, refresh_token, id_token, chatgpt_account_id, expires_at}`.

  The codex flow needs a separate codepath; the same `device_flow.rs`
  helper does **not** work unmodified. Cross-referenced upstream in
  `codex-rs/login/src/device_code_auth.rs` and opencode's
  `plugin/codex.ts`.

### 3. Static API key

- **minimax** — Minimax dashboard API key, `Authorization: Bearer ...`.
- **opencode** — opencode subscription API key.
- **kimi-api** — Moonshot pay-per-token API key (separate from kimi-code's OAuth path; backup when the subscription quota is drained).

  All three are the same shape: prompt for the key once on `mixer auth
  login <id>`, store as `{"api_key": "..."}` in the credentials file,
  done. No refresh, no expiry tracking. The provider config can also
  point at an env var (`api_key_env`) that takes precedence over the
  stored credential — useful for CI / one-shot overrides.

### 4. No auth (self-hosted)

- **ollama** — no credential. Just a base URL.
  Disabled by default in mixer; `max_concurrent_requests: 2` out of
  the box for GPU-constrained hosts.

  cockpit's equivalent should be the same: zero-config, no credential
  file at all, opt-in by enabling in config.

---

## Freshness / refresh policy

Both OAuth providers use the shared `oauth_refresh` helper:

- `oauth_freshness(blob, now)` returns `Valid` / `ExpiredRefreshable` / `ExpiredDead`.
- `EXPIRY_THRESHOLD_SECS` margin so we refresh slightly before the wire deadline.
- `provider_refresh_lock(provider_id)` serializes concurrent refreshes for a single provider — important when multiple in-flight requests notice expiry at once.

cockpit's daemon (`GOALS.md` §8) is single-process, so the same
in-memory mutex pattern applies: one refresh in flight per provider,
the rest wait on it.

---

## Implications for cockpit

- **Three auth backends to ship, not seven.** API key, RFC-8628 device
  flow, and the codex PKCE-hybrid flow cover every supported provider.
- The codex flow is the awkward one; budget a separate module for it
  and don't try to fold it into the shared `device_flow.rs`.
- Credential storage = per-provider opaque JSON at 0600. The main
  config never holds raw secrets — only env-var names. Already
  consistent with `GOALS.md` §11 (redaction policy) which assumes
  secret *values* never live in config files.
- Per-provider refresh lock belongs in the daemon, not the provider
  impl. The daemon already has the single-process authority needed for
  this (cf. file-lock manager in `miscellaneous.md` §7).
