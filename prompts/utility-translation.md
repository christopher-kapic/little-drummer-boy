# Utility-model translation (round-trip)

## Goal

Let a user work in their own language while the coding model works in a
different one: use the **utility model** to translate the user's prompt
into the model's language on the way in, and translate the agent's
response back into the user's language on the way out.

## Current behavior

- Utility model: `utility_model: Option<String>` in
  `src/config/extended.rs`; one-shot calls via
  `Model::text_completion()` (`src/engine/model.rs`). Degrades
  gracefully when unset (see `src/auto_title.rs`).
- `/settings` rows: `src/tui/settings/ui_page.rs`, persisted to
  `extended-config.json`.

## Desired behavior

- Two languages configured explicitly in `/settings`: the **user's
  language** and the **model's language** (persisted in the extended
  config).
- **Round-trip:**
  - **Inbound** — translate the user's prompt from the user's language
    into the model's language before it reaches the main agent.
  - **Outbound** — translate the agent's final user-facing response from
    the model's language back into the user's language.
- **Skip translation entirely** when the two languages match, or when
  the feature/utility model is unavailable.
- Translation is a history-free utility-model call (one for inbound, one
  for outbound).

## Edge cases & decisions (settled)

- **Preserve code verbatim.** This is a coding harness: the translation
  prompt must instruct the utility model to translate only natural-
  language prose and leave code blocks, inline code, file paths,
  identifiers, commands, and CLI flags untouched. Mis-translating these
  would corrupt the agent's input/output.
- **No streaming translation.** Translate the *complete* assembled
  response, not streamed chunks. (Streaming-aware translation is out of
  scope; accept that translated output appears after the response
  completes.)
- **Utility model unset/unavailable or error:** degrade — pass text
  through untranslated rather than blocking the turn.
- **Ordering vs other utility features** (so they compose): the
  inbound prompt-injection scan
  (`prompts/utility-prompt-injection-detection.md`) sees the **raw**
  user text first; translation happens after that scan and before the
  prompt is handed to the main agent (and before outbound redaction).
  Outbound translation runs on the agent's final text before it is
  shown to the user.

## Expected UX / acceptance

- With user language = Spanish, model language = English: the user types
  in Spanish, the model reasons/acts in English, and the user reads the
  response in Spanish. Code snippets in the response stay in their
  original form.
- With both languages equal, or the utility model unset, text flows
  through unchanged.

## Constraints

- Implement without incurring tech debt: no shortcuts, no
  TODO-for-later, no half-finished paths.
- For any new package, use the latest stable release unless this prompt
  says otherwise, and verify correct API/dependency usage with
  `kcl ask <package> "<question>"` before wiring it in.
- Honor token economy (GOALS §10): one-sentence tool descriptions,
  noun-phrase parameter descriptions, base system prompt ≤ ~400 tokens.
