-- Plan-level model override (prompt `plan-duplication-and-model-override.md`).
-- A plan may pin an optional model that overrides every agent's frontmatter
-- model during that plan's execution (resolution precedence: plan-level model
-- → agent frontmatter `model` → session model). Stored in the canonical
-- `provider/model` slash form. NULL means "no plan-level model" — execution
-- behaves exactly as before. The duplicate-plan flow seeds this from `--model`.
ALTER TABLE plans ADD COLUMN model TEXT;
