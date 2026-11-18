# Siesta

Lightweight desktop SQL client (Postgres, MySQL / MariaDB) on Rust + GPUI + gpui-component.
The spec is `Siesta специфікація легкого SQL-клієнта на Rust + GPUI.md` (written in English): it records
principles and decisions with their reasons. Change it **only when a principle or a decision changes**
(or a new one is made) — not for every feature, tweak, key, setting or message. Behaviour details live in
the code, the tests, `README.md` and `skills/siesta/SKILL.md`; don't copy them into the spec.

## Git

Work, commit and push straight on `main`. Never create branches (no feature branches, no PRs)
unless asked.

## Sibling project: Respite

`/Users/andrew/Projects/respite` is Siesta's sibling: a text-based REST client with the same shell
(workspace folder, sidebar file tree, editor with an active block, result panel on the right, settings,
themes, state restore, `.app` bundle, CLI, agent skill). Treat it as the reference implementation.

- Before building any piece Respite already has (window/sidebar/tabs, editor setup, highlighting,
  settings, themes, tokio-runtime ↔ GPUI bridge, CLI output, bundling scripts), read how Respite does
  it and follow the same approach. Re-check it whenever you start a new area — Respite keeps changing.
- Copy code from Respite and adapt it; don't depend on Respite crates and don't edit Respite's files.
  Respite is read-only from this project.
- Respite's `CLAUDE.md` rules apply here too: same pinned `gpui-pre` / `gpui-component` versions,
  never patch third-party crates, icons via `IconName` or a copied Lucide SVG, icon size from the UI font,
  snake_case in `settings.json`, the agent skill (`skills/siesta/SKILL.md`) updated with every CLI change.
- The editor stays plain text: no CodeLens or inline statuses in editor lines. The only button is
  Respite's gutter ▶ next to the line number (`editor.run_button`), never in the text.
- UI text is English.
