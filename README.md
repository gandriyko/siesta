# Siesta

**A lightweight, text-first SQL client for Postgres and MySQL / MariaDB.** Queries are plain `.sql` files that also run in `psql` and `mysql`.

Built in Rust on [GPUI](https://www.gpui.rs); a sibling of [Respite](../respite), the REST client. The GUI and the CLI share one core. The full design is in [the specification](<Siesta специфікація легкого SQL-клієнта на Rust + GPUI.md>).

<!-- TODO: screenshot -->

## Connections

Connections live in your config dir, one JSON file per connection: `~/.config/siesta/connections/<name>.json` (the file name is the connection name). The desktop app's "New connection" form writes them; you can also write them by hand.

```json
{ "driver": "postgres", "host": "localhost", "port": 5432, "database": "app", "user": "app" }
```

```json
{ "driver": "mysql", "url": "mysql://ro@db.internal:3306/shop", "read_only": true, "prod": true }
```

A `.sql` file picks its connection with `-- @connection dev-pg` on the first line. Values like `${env:SHOP_DB_HOST}` are read from the environment (handy in CI).

`database` is only the connection's default and is optional. A file can use any database the credentials can access: pick it in the second dropdown at the top right (the choice is remembered per file), or write `-- @database app` under `-- @connection` (the CLI takes `-d app`; the flag wins over the directive). `siesta connections databases NAME` lists them.

The password is never written in the file as plain text. Type it in the connection form, or run `siesta connections set-password NAME`: it is stored encrypted as `"password_enc": "v1:…"` (XChaCha20-Poly1305, the connection name is bound to it) with a random key in `~/.config/siesta/key` (mode 0600, created on first use). The key is the secret: keep it out of backups and sync that carry the connection files, and a password has to be set again if the key is lost. This keeps passwords out of screenshots, `grep`, backups and synced connection files without the key; it does not protect against malware running as your user, which can read the key too. The password is decrypted only when a connection opens; without `password_enc` Siesta connects without a password. `"prod": true` marks production: a red banner, and every statement that is not a plain read (DML, DDL, `SET`, …) asks for confirmation (the CLI needs `--yes` for those; `SELECT` / `SHOW` / `EXPLAIN` run freely).

An old `~/.config/siesta/connections.json` (with `connections.private.json`) is split into per-connection files on first start and renamed to `*.migrated`; its literal passwords are encrypted into `password_enc` (old `${keychain…}` markers are dropped with a warning: enter the password again).

## CLI

The desktop app and the CLI ship as one binary.

```text
siesta list          Statements of a file or directory: file, line, name, first line
siesta run           Run statements of a file (all of them in file order when none are named)
siesta exec          Run SQL from an argument or stdin
siesta connections   Connections, secrets masked; `test NAME` opens one, `set-password NAME` stores its password
siesta connections databases NAME   Databases the credentials can access (use with -d / -- @database)
siesta schema        Tables, columns and indexes of a connection; --ddl prints CREATE TABLE
```

Statements are addressed by `-- @name`, `:LINE` or `#N`. Some useful flags:

```sh
siesta run reports.sql users_by_day --output json     # machine-readable output (default when piped)
siesta exec -c dev-pg 'SELECT count(*) FROM users'    # no file needed
siesta exec -c prod-mysql --read-only 'SELECT 1'      # writes are refused client-side (exit 4) and by the server session (a safety default, not a security boundary)
siesta run cleanup.sql --yes                          # non-read statements on a prod connection need --yes
siesta exec -c dev-pg -d app_test 'SELECT current_database()'   # another database on the same server
siesta schema -c dev-pg users                         # columns before writing a query
```

Results are cut at 100 rows (`--max-rows`). `--output` is `table`, `json`, `jsonl` or `csv`; JSON follows the `siesta.run/v1` shape, with JSON / JSONB values nested.

Exit codes: `0` ok, `2` connection error, `3` parse error or unknown connection / statement, `4` refused (confirmation needed, pass `--yes`), `5` SQL error.

## AI agents

[`skills/siesta/SKILL.md`](skills/siesta/SKILL.md) teaches a coding agent (Claude Code, Codex and others that read Agent Skills) to explore a schema and run queries through the CLI. Copy the folder into the agent's skills directory, for example `~/.claude/skills/siesta`.

## Development

```text
crates/
  app/        GPUI desktop app
  cli/        siesta CLI commands
  sql-split/  statement splitter, directives
  db-conn/    connection files, encrypted passwords
  db-exec/    sqlx sessions, paging, schema
  settings/   settings.json
  workspace/  project folder, file index, UI state
```

Integration tests need Docker (Postgres and MariaDB): `scripts/test-db.sh`; `--down` stops the containers.

## License

MIT
