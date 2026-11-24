# Siesta: specification of a lightweight SQL client in Rust + GPUI

This file records principles and decisions. It is updated only when a principle or a decision changes — not for every feature or tweak;
behaviour details live in the code, tests, README.md and skills/siesta/SKILL.md.

## Purpose and non-goals

A desktop SQL client (Postgres, MySQL / MariaDB) in Rust + GPUI where queries are plain text in `.sql` files. Not DBeaver with a hundred dialogs, but an editor in the spirit of DBCode for VS Code: write SQL, press Cmd+Enter, see the result table on the right. It is the twin of Respite (same shell, databases instead of HTTP).

- One file holds many statements separated by `;`; Cmd+Enter runs the one under the cursor.
- Native speed: cold start under 300 ms, the first result page shows without noticeable delay beyond the query itself, scrolling does not depend on result size.
- The connection is always visible at the top right (like the environment picker in Respite).

**MongoDB and Elasticsearch are engines of Siesta, not a third product** (decided 2026-11-24). They are used as databases: connections with `prod` / `read_only`, a tree of databases / indices, documents as a table, the JSON popup, history, CLI and MCP — all of which Siesta already has. A third copy of the Respite shell would cost more than an engine switch inside Siesta. Elasticsearch is not left to Respite because Respite has no object tree, no document table and no `prod` guard. SQL-only parts (wrapper paging, transactions, cell editing by primary key) stay SQL-only; each engine gets its own lexer, execution, paging and safety classification. Each engine keeps the rule "a file runs in the native client unchanged", and the file extension picks the language:
  - **MongoDB: `.mongodb.js` in `mongosh` syntax** (`db.users.find({…}).sort({…})`, `db.orders.aggregate([…])`), the same as MongoDB playgrounds in VS Code. A hand-written lexer covers the subset Siesta runs (`db.<collection>.<method>(…)` chains, relaxed JSON, `ObjectId()` / `ISODate()`), not JavaScript.
  - **Elasticsearch: `.es` in Kibana Dev Tools console format** (a `METHOD /path` line, then a JSON body), so a file pastes into Kibana unchanged and covers the whole API, not only what ES SQL can express.
  - A file whose connection is of another engine is a red picker, not a silent run.

**Siesta deliberately is not:**

- a visual query builder, an ER-diagram tool, a schema-diff or migration tool;
- a row insert / delete grid (cell editing exists, see below);
- a client for SQLite, MSSQL, Oracle, ClickHouse or other NoSQL beyond MongoDB and Elasticsearch (considered later);
- a place where project files configure connections or settings.

## Shell shared with Respite

Respite (`/Users/andrew/Projects/respite`) is the reference implementation. Siesta has the same shell: a workspace of files with a sidebar tree, editor with an active statement, result panel on the right, global settings, themes, state restore, `.app` bundle, CLI, agent skill (`skills/siesta/SKILL.md`).

- **The shell is copied from Respite, not shared as a crate.** Two products evolve independently; a shared crate is extracted only when a third place wants the same code. Same pinned `gpui` / `gpui-component` versions, no patches to third-party crates.
- **The editor stays plain text:** no CodeLens, no inline statuses in lines. The only button is the gutter ▶ next to the line number. Status lives in the result header and the sidebar tree.
- **Settings are global only** (`~/.config/siesta/settings.json`, snake_case); there is no per-project layer. The Settings page, `settings.json` and README list the keys; the defaults for shortcuts live in the keymap file, overridable in `keymap.json`.
- **Two folders, one role each, nothing is written to both.** `~/.config/siesta/` holds only what the user writes (settings, keymap, `connections/`, `collections/`) and can live in dotfiles. `~/.siesta/` holds only what Siesta writes itself (state, added-files list, history, schema cache, password key) and never goes to dotfiles: it contains the key, the full text of executed SQL and database structure. `SIESTA_CONFIG_DIR` / `SIESTA_DATA_DIR` override them (tests, sandboxes). Siesta keeps nothing inside projects.
- **The sidebar shows only query files (`.sql`, `.mongodb.js`, `.es`) and folders.** COLLECTIONS is the central folder `~/.config/siesta/collections`; FILES lists query files added from anywhere (added files are only referenced; removing one from the list keeps the file, tab and history). The sidebar file list updates by polling mtimes (files, settings, keymap, connections) once a second.
- **Nothing is ever overwritten.** Import skips existing names, New File only opens an existing file, rename is refused if the target exists (except a case-only rename of the same file), delete goes to the system Trash. A rename is also refused while a transaction is open or a statement runs in an affected tab; after it, tabs, results, history entries and added files follow the new path.
- **Restore:** open tabs, cursors and the active tab are saved in `state.json`. The connection and database of a file are not in state (see directives).
- **`siesta [file[:line]]`** opens a file in the GUI; the same binary is the CLI and the MCP server (`siesta mcp`) over one core.

## `.sql` format and statements

- **Plain SQL, no custom syntax.** A file must run in `psql` / `mysql` unchanged. UTF-8, LF or CRLF.
- **Statements are split by a hand-written token lexer, not a parser,** with dialect rules: quotes and escapes per dialect, `--` / `#` / `/* */` comments, Postgres dollar-quoting, MySQL `DELIMITER` (never sent to the server). The dialect comes from the file's connection (Postgres when none). One lexer serves splitting, classification and every safety check, so they cannot disagree.
- **What is sent** is the text from the first token to the delimiter; leading and trailing comments (with directives) are not sent. The cursor between statements belongs to the previous one. An unclosed string / comment / `$tag$` is a diagnostic, the rest of the file is one statement.
- **Classification by first word:** a *query* (`SELECT`, `TABLE`, `VALUES`, `WITH … SELECT`) is wrapped for paging; a *read* (`SHOW`, `EXPLAIN`, `DESCRIBE`) is not wrapped; everything else is DML / DDL ("other"). `SELECT … INTO`, `FOR UPDATE / SHARE` and `WITH` with DML inside are "other" and not wrapped. **`EXPLAIN ANALYZE <stmt>` is classified by the inner statement** because it really executes it. Plain `EXPLAIN` stays a read.
- **Directives** are comments `-- @word value`: `@connection`, `@database` (file level), `@name`, `@page-size`, `@confirm` (statement level); the list and exact syntax are in the skill file.
  - **`@connection` / `@database` apply only at the top of the file** (before the first statement), so a stray comment deep in a script can never silently switch the server. A directive below, an unknown one or a bad value is a warning; directives inside a statement are ignored.
  - **A file's connection is the `-- @connection` line in the file itself,** so it travels with git, rename and other machines, and the CLI sees the same connection. Picking a connection in the GUI is just a normal undoable edit of that line (the file becomes dirty); nothing is stored separately and the picker never blocks. A name that does not exist shows a red picker, nothing is rewritten behind the user's back.
  - **`@database`** picks a database on the connection's server, same mechanism. Changing the connection removes it (the database belonged to the old server). Effective database: `@database` → connection's `database` → server default (CLI: flag first).

## Connections

- **Connections live only in the user's config dir** (`~/.config/siesta/connections/<name>.json`, one file per connection), never in the project. A project may be shared or public; credentials and hosts are personal. No project connection files, no folder chain, no `${dotenv:}`, no keychain markers. The file format is strict JSON; its keys are documented in the Connections page / README.
- **Connection names are lowercase ASCII slugs** (`[a-z0-9][a-z0-9_-]*`, max 64) because a name is at once a file name, a `-- @connection` value and a CLI argument. The form normalises input as the user types (transliterating non-ASCII). **Old names migrate automatically** (on start, on a change in `connections/`, on every CLI run): file renamed, password re-encrypted under the new name, `-- @connection` lines rewritten in the collections and added files (open tabs via an undoable edit), one notification. If migration is impossible (read-only dir), the old name still loads with a warning. The same rewrite happens on a rename in the form.
- **Passwords are only stored encrypted in the connection file** (`password_enc`, XChaCha20-Poly1305, AAD = connection name so a copied ciphertext does not decrypt elsewhere), with a random local key in `~/.siesta/key` (0600, created on first need, not next to the connection files so the config dir can go into dotfiles). Protects against leaks as plain text (screenshots, `grep`, backups, synced files without the key); does not protect against malicious code running as the user. The system keychain was rejected: signing dev builds and permission prompts are not worth protection from what is readable by the user anyway. A plain `password` key is ignored with a warning.
- **Decryption is lazy:** only when a session opens. No `password_enc` means a passwordless connection (some local databases need none); a server rejection then says how to set a password. Failing to encrypt on save does not lose the rest of the connection: it saves, the form stays open with the error.
- Values may contain `${env:NAME}` (for CI); other `${…}` is an error. A connection that fails to build goes to errors, a bad file to warnings; the rest still loads.
- **Unix sockets** are an explicit option (`socket`: true = auto-detect usual paths, or a path); an old `host` starting with `/` still works. Postgres without a user takes the OS user, like libpq.
- **The connection form is one simple dialog** (name, driver, URL that is parsed into fields and never written to the file, host / port / database / user, password, SSL, `schema` (Postgres only), `init_sql`, Read only, Production, colour). Hints follow the driver. Test never saves anything; closing the dialog aborts a running Test. The Connections page lists everything flat; its actions never touch any file (a deleted connection leaves red pickers, not rewrites).
- **A connection timeout wraps the whole establishment** (TLS, handshake, init) because sqlx has none; the typical case is the wrong driver on a port where both sides wait silently, so the error names host, port and likely cause.
- **Nothing is selected automatically.** A new file has no connection; a connection created from the form is selected only for the file the form was opened from.
- **The database list is read in the background** (a throwaway session) and cached per connection; only accessible databases are listed, system ones last.

## Sessions and safety

- **One session per (file, connection, database)** while the file is open, so `SET`, temp tables and `BEGIN … COMMIT` work across separate Cmd+Enter runs like in `psql`. Switching the database is a new session (same for both drivers); an open transaction in the old one asks Commit / Rollback / Cancel first. A different file runs in parallel, one session runs strictly in order.
- **Autocommit by default** (open question below). An open transaction is shown in the result header; closing the file, disconnecting and quitting each ask Commit / Rollback / Cancel (one dialog for all files on quit). Transaction state is tracked by us from executed statements, because sqlx does not expose it.
- **`prod` flag (not a name pattern) marks production.** The colour is red by default and a plate marks the editor. On `prod` every statement that is not a read asks for confirmation (the same rule in GUI and CLI); reads run freely. `-- @confirm` always asks; `run.confirm_destructive = always` also asks for `UPDATE` / `DELETE` without `WHERE`, `DROP`, `TRUNCATE`, `ALTER` on other connections. A batch asks once, up front, so it never runs half-confirmed. Re-running the same result (page, sort, filter) does not ask again. Destructiveness is judged on top-level tokens (also through `WITH … DELETE` and `EXPLAIN ANALYZE`), not a parser; `UPDATE` / `DELETE` without `WHERE` also gets a yellow warning in the editor.
- **`read_only` is two layers.** The server session is set read-only, and the client refuses everything that writes before sending (CLI exit code 4, `read_only_refused`) — in `db-exec`, so GUI, CLI and MCP share it. It is a safe default, not a security boundary: a session can switch itself back, and functions that write inside `SELECT` are only stopped by the server. A cheap guard refuses the obvious switches to read-write. For real protection use a database user with read-only privileges. Plain `read_only` shows a neutral badge, not the red `prod` plate.
- **Cancellation** (Esc): sqlx cannot cancel on the server, so the backend id is remembered at open and a separate service connection sends `pg_cancel_backend` / `KILL QUERY`; the session stays alive. The cancel first checks that the id is still ours (Postgres: a unique `application_name` per connection; MySQL: server identity and client host:port), because behind a pooler or load balancer the same id can be another client's statement; unconfirmed, it cancels nothing. Runs queued behind another one are not executed after a cancel or file close ("generations").
- **Lost connection** reopens on the next run. Reads are retried once with a notice that session state is lost; anything else is never retried automatically because it may already have run (the error says "may or may not have run").
- **Multiple result sets** in one statement: only the first with rows is shown, the notice says how many were not.
- **Statement timeout** is applied per dialect on open, on setting change and after a reopen.
- **Colour:** the connection's colour marks the picker dot, the editor frame and the tab of each file, so the active server is hard to confuse.

## Running queries

- **Run modes:** the statement under the cursor, the exact selection (may be several statements), all statements of the file, repeat last, cancel. Batches show one result tab per statement with rows and a summary; the first error stops the batch (`run.stop_on_error`).
- **Pipeline:** find statement → confirm if needed → take the session (open, `init_sql`) → classify → execute (queries are wrapped for paging, the rest runs as is) → accept result (columns with types, first page, server messages) → record in history. A server error shows in the result panel (SQLSTATE, message, DETAIL, HINT) with the position underlined in the editor.
- **A long query never blocks the UI.** `sqlx` needs tokio: a separate thread with a tokio runtime, the GUI sends tasks over a channel and receives events via `cx.spawn` (the Respite pattern). Paging, sorting and filtering are a new query in the same session while the previous page stays visible.
- **History** (`~/.siesta/history/`, one file per execution, newest first, 500 kept): file, statement text, connection, database, time, duration, row count or error. Result data is never stored (large and sensitive). Only user-initiated runs are recorded, not page / sort / filter / count. The CLI and `siesta mcp` record too (every statement that ran, not one refused before running), each entry marked with its source `gui` / `cli` / `mcp`, so a change an agent made is never invisible; SQL given directly (`exec`, MCP `sql`) has no file. A write failure only warns. **Insert** and **Run again** operate on the file where the statement was recorded, so the statement runs under that file's connection; if it differs from the recorded one, the user is told.

## Result panel

- **Pagination, sorting and filters wrap the statement on the server** (`SELECT * FROM (<query>) AS q … LIMIT n+1 OFFSET m`); the client never holds the whole result. The extra row only tells whether another page exists, avoiding a costly `count(*)` (available on demand). Filter values go only as bind parameters, never concatenated. Sorting covers the whole result, not just the visible page.
- **Cases where wrapping is impossible or unsafe fall back,** visibly: statements that cannot be wrapped (`SHOW`, `EXPLAIN`, `CALL`, DML with `RETURNING`) are streamed and cut at `results.max_rows` with a warning, sorting becomes local (marked "local"), filters are off; duplicate column names (a join with `id` on both sides) do the same; MySQL / MariaDB with a plain page and no top-level `LIMIT` gets `LIMIT n OFFSET m` appended instead of a wrapper, because MariaDB drops a subquery's `ORDER BY` without `LIMIT`.
- **Types:** everything from the database becomes one value enum used by table, CLI and popup; unknown types (PostGIS, enum, `inet`…) come as server text so nothing crashes. Postgres is read over the simple protocol as text and parsed by type name; for filtered pages (bind parameters need the extended protocol) every column is cast to text. MariaDB's `JSON` arrives as `LONGTEXT` with a binary collation; text that parses as a JSON object / array is treated as JSON.
- **Large values:** the row arrives whole, the table paints only a preview prefix (`results.cell_preview_kb`); the popup takes the full value from the already fetched row without another query. JSON cells can show a multi-line preview and a formatted, highlighted tooltip with the whole document.
- **Views of the rows: Table, JSON, Raw** (tabs beside Messages / Query; paging works in all). JSON is the page as an array of objects in the popup's viewer (folding, JSONPath), because nested documents don't fit cells; Raw is the server's JSON, kept for Elasticsearch (the whole response) and MongoDB (documents as extended JSON, with the types JSON loses). Elasticsearch and MongoDB open in JSON, SQL in Table; the pick is remembered per file (in memory).
- **Full value popup** is the main feature for JSON columns: formatted, highlighted, foldable, with a JSONPath filter; other text as is; binary as a hex dump with save-to-file; a row can be shown as a JSON object. A single click only selects (so cells can be copied); double click / Enter / Space opens.
- **Copy and export:** TSV copy, copy as JSON / INSERT; export to CSV / TSV / JSON / Markdown / INSERT (the whole result re-runs without paging and streams to a file) is specified but not implemented yet.
- **Cell editing** is a separate Edit mode on `SELECT`-like results:
  - **The server, not a parser, says where columns come from:** the statement is prepared (not executed) on the file's session and the row description names table and column per field, then primary keys are read from the catalog. So aliases and joins work; expressions and aggregates are read-only.
  - A table is editable only if its whole primary key is in the result; otherwise the strip says why ("can't edit: …") instead of guessing.
  - Changes are never sent at once: they are highlighted, then **Save…** generates readable `UPDATE … WHERE <key> = <old value>` statements, shown in a confirmation (red on `prod`) before running.
  - Without a user transaction the updates run in their own transaction, and each must change exactly one row, otherwise everything is rolled back and nothing is saved. In the user's transaction they just run, the user commits.
  - Pending changes belong to the rows on screen: paging, sort and filter are blocked until saved or discarded; a new run drops them. Edits go to history.
  - An open cell field closes only on Enter / Esc / click elsewhere, so keys never leave the cell by accident. Large values (JSON, memo, blob) are edited in a separate window; JSON is compared by content, so reformatting is not a change.

## Editor and navigation

- **Highlighting** by `tree-sitter-sql` with the same themes as Respite (Zed-style JSON, light and dark); `-- @…` directives are highlighted apart from ordinary comments.
- **Completion** is a pure function over the statement under the cursor (token scan, not a parser; works even when `FROM` comes after the cursor), fuzzy, keywords / tables / columns / directives by context. **The schema is loaded eagerly in the background as soon as a connection is chosen for a file** (also on restore), cached in memory and on disk per connection (the key includes a fingerprint of the effective connection, so a connection recreated under the same name against another server does not pick up a stale schema), and re-read after a successful schema-changing statement. The old cache stays visible while a new one loads. Schema comes from the chosen database and from `information_schema`, so it shows only objects the user may see.
- **Diagnostics:** server error position, unclosed string / comment, destructive statement without `WHERE`. A full SQL linter is not a goal: the server checks syntax.
- **Formatting** of the statement under the cursor with `sqlformat`.
- **DB tab** of the sidebar: the active connection's schemas → tables / views → columns, indexes; double click inserts `SELECT *` (or adds a column to the select list), Copy DDL, search. **Statement navigation:** statements of a file are listed in the tree (jump, double click = jump and run); search in file and in the whole folder; previous / next statement.
- **Where state lives:** a database / connection switch is always a file edit, never a hidden UI setting.

## CLI and agent

One binary: GUI, CLI and MCP over one core. Commands: `list`, `run`, `exec`, `connections` (list, test, databases, set-password), `schema`, `mcp`; the skill file `skills/siesta/SKILL.md` is the reference and is updated with every CLI change (a test fails if the skill misses a command, long flag, directive or exit code).

- **Connections come from the user config dir, not the current directory.** Selection: `-c` > `-- @connection` in the file > error. Database: flag > `-- @database` > connection's `database` > server default.
- **Output is JSON when piped, a table in a terminal** (`json`, `jsonl`, `csv` on request); JSON values stay nested JSON, not strings; versioned `schema` field in output. stdout stays clean data; notices go to stderr.
- **Safety is the same as in the GUI** (`prod` confirmation, `read_only` refusal), checked for the whole batch before anything runs, never half-way. Without a terminal the CLI never asks: it exits 4 unless `--yes`, except that on `prod` `--yes` does not count without a terminal (`prod_needs_terminal`): a non-read statement on production is confirmed only by a person at a prompt, so an agent's shell or a script cannot unlock it with a flag. `--read-only` forces the server-side guard.
- **Exit codes:** 0 success; 2 connection error (connect, auth, lost); 3 parse / unknown connection, statement or table / no connection / bad arguments; 4 refusal (confirmation needed, `read_only`, `prod_refused`); 5 SQL error on the server. With several statements the first error stops the run and sets the code (also when `stop_on_error` is off).
- **MCP (`siesta mcp`, stdio)** has tools `connections`, `schema`, `list_statements`, `run_sql`, each a CLI call with JSON output. **The agent can never confirm:** `run_sql` never passes `--yes` and stdin is closed. On `read_only` writes are refused; on `prod` an agent may run only reads and `UPDATE` with a top-level `WHERE`, everything else is `prod_refused` before execution; anything needing confirmation is `confirmation_required`. **An agent's reads on `prod` run in a server-side read-only session** (switched back only for the allowed `UPDATE … WHERE`, re-applied after a reconnect): the text check can't see a function that writes inside a `SELECT` (`SELECT purge()`, `setval`), the server can. MySQL executable comments (`/*! … */`) are refused under `read_only` and for agents on `prod`, because the lexer skips what the server runs. The CLI detects MCP through `SIESTA_MCP`, set by `siesta mcp` for its child commands. These are rules for agents separate from the human GUI / CLI rules, and still rest on `read_only`, not on a promise that parsing is perfect.

## Architecture

A Cargo workspace; the core does not depend on the GUI.

- `sql-split` — lexer, statements with positions, dialects, directives, classification, safety token checks. No dependencies on the rest.
- `db-conn` — connection files, `${env:}`, encrypted passwords, secret masking.
- `db-exec` — sessions, execution, page / sort / filter wrapper, cancellation, type conversion, history, schema cache and source-column discovery. `sqlx` 0.9 (Postgres, MySQL, rustls; no `uuid` / `rust_decimal`: Postgres is read as text) and `tokio`.
- `workspace` — central folder scan (`.gitignore` aware), added files, saved UI state. No GUI.
- `settings` — global settings, config / data dir helpers; a separate crate because both `app` and `cli` need it.
- `app` — GPUI window, sidebar, editor, result panel, popups (`gpui`, `gpui-component`).
- `cli` — commands, output, exit codes (`clap`).

Data flow: `app` / `cli` → `sql-split` (what to run) → `db-conn` (where) → `db-exec` (run, return values) → table / popup / JSON. State in the GUI is held in GPUI entities: workspace, buffer per file, session store, result store (result per file + statement text: changed text is a new statement; closing a file forgets its results and sessions), schema cache shared by all editors.

**Driver: `sqlx`.** One API for Postgres and MySQL / MariaDB, pure Rust, rustls. Its missing cancellation is covered by `pg_cancel_backend` / `KILL QUERY` from a separate connection.

**Elasticsearch: `reqwest` inside the same `Session`,** not a separate executor, so sessions, history, cancel, paging and the result panel need no second path; `.es` requests are a `sql-split` dialect for the same reason (one lexer for editor, CLI and safety). Responses become the same `Outcome` tables. Two deliberate gaps: `read_only` is only the client check (the server has no read-only session), and cancel drops the request on the client while the server may finish it (server-side `_tasks/_cancel` when long searches need it).

**MongoDB: the official `mongodb` driver inside the same `Session`,** for the same reasons; `.mongodb.js` is a `sql-split` dialect. A statement is **one mongosh call**, not JavaScript: Siesta parses `db.<collection>.<method>(…)` with its cursor chain and the literals (relaxed JSON, `ObjectId`, `ISODate`, regexes) into extended JSON, and the executor builds a server command from that same parse (`find`, `aggregate`, `update`, `findAndModify`…), so what the safety checks classify is exactly what runs. Variables, functions and loops are refused, not half-run: a JS engine would let a statement do anything the checks can't see. Options never replace what the statement built (a `pipeline` option can't swap the checked pipeline); any `$out` / `$merge` key makes an aggregation a write. Paging appends `skip` / `limit` (or stages); `.limit()` / `.skip()` run as written. Same two gaps as Elasticsearch: `read_only` is the client check, cancel is client-side. An agent on `prod` may run reads and `updateOne` / `updateMany` with a filter and update operators only (no upsert, replacement or pipeline): the counterpart of `UPDATE … WHERE`. A `mongodb+srv://` URL, several hosts or URL options stay in the connection's `url` (without the password); the fields hold only what a plain URL has.

### Testing

- `sql-split`: table of cases with `assert_eq!` (strings, comments, `$$`, `DELIMITER`, CRLF), no snapshot libraries (as in Respite).
- `db-exec` and `cli`: unit tests of pure parts (wrapper, filter syntax, type parsing, statement addressing, output formats, exit codes, safety checks) plus `#[ignore]` integration tests against Postgres 17 and MariaDB 11 in Docker (`scripts/test-db.sh`; URLs in `SIESTA_TEST_PG_URL` / `SIESTA_TEST_MYSQL_URL`). MySQL 8 is not tested in CI (the difference is only `JSON`).
- Tests use temporary `SIESTA_CONFIG_DIR` / `SIESTA_DATA_DIR` with their own key and never touch the real `~/.config/siesta` or `~/.siesta`.
- GUI: `#[gpui::test]` for navigation, table and popup.

## Roadmap

**Phase 1 — MVP (done except export):** statement splitting, connections with `read_only` / `prod` and the form, execution on `sqlx` with sessions and cancel, GUI shell from Respite, result table (types, paging, sort, column filters, copy), full-value popup, DB tab and completion, CLI and skill, history. Open: result export.

**Phase 2:** MCP server (done); passwords encrypted with a local key (done). Open: SSH tunnel, streamed export of large results and `EXPLAIN` as a tree, query parameters (`:name` / `{{var}}` with a prompt).

**Phase 3:** cell editing with `UPDATE` by primary key (done). Open: SQLite, ClickHouse, MSSQL.

**Phase 4 (done):** MongoDB and Elasticsearch engines: connections, files, execution with paging, safety classification, DB tab, CLI / MCP. Open: completion of collection / index and field names.

## Decisions and open questions

- **Connections are files in the config dir, passwords only encrypted in them** (see Connections).
- **Pagination, sorting and filters on the server** through a wrapper; the client never holds the whole result.
- **Session per file + connection + database**, so `SET` and transactions behave like in a console client.
- **The editor is clean**, as in Respite: no statuses in lines, the only button is the gutter ▶.
- **Result is shown per statement under the cursor;** a fresh run starts at page one without sort and filters; filters apply on Enter (a query per keystroke would be wasteful); a batch shows a tab strip and a new single run drops it.
- **The shell is copied from Respite** instead of a shared crate (for now).
- **The agent skill and the CLI change together.**
- **License: MIT.**
- **Name: Siesta.** A pair to Respite (both about rest). Binary `siesta`; the name is taken on crates.io, so crates are prefixed (`siesta-cli`, `siesta-settings`, …), as in Respite; Homebrew is free.

**Still open**

1. **Wrapper for MySQL 5.7 / old MariaDB:** a subquery in `FROM` needs an alias and not every `SELECT … FOR UPDATE` wraps; such statements run unwrapped.
2. **Default transactions:** autocommit (like `psql`) or an explicit mode with a Commit button (like DataGrip)?
3. **Server messages:** Postgres `NOTICE` is unreachable through sqlx (it only logs), so `notices` is empty for Postgres; MySQL warnings are read with `SHOW WARNINGS` only after non-read statements. Wanting `RAISE NOTICE` is a separate decision (a `tracing` interceptor or another driver).
4. **MariaDB `JSON`** is detected by heuristic (text that is a JSON object / array); knowing the type from `information_schema` (`longtext` + `json_valid` CHECK) would be more exact, but the heuristic is enough.
5. **Shared shell with Respite:** if both products live long, when to extract shared crates (workspace, themes, sidebar)?