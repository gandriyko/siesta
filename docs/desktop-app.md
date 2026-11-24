[← Siesta](../README.md)

# Desktop app

## The window

| Part | What it does |
|---|---|
| **Sidebar → Files** | Your query files: **COLLECTIONS** (the central folder) and **FILES** (files added from anywhere) |
| **Sidebar → DB** | The connection's schemas, tables, columns and indexes |
| **Editor** | One tab per file; the statement under the cursor is highlighted, a ▶ next to its line runs it |
| **Pickers (top right)** | The file's connection and database |
| **Result panel** | The last result of each statement: Table, JSON, Raw, Messages, Query |

Open tabs and cursors come back on the next launch.

## Files

- **COLLECTIONS** is the folder `~/.config/siesta/collections`, with subfolders. Its header buttons import files (copied in), make a new file or a new folder. The trash button moves a file or folder to the Trash.
- **FILES** lists files that stay where they are, for example in a repository. Add them with `+`, **File → Add Files…** (<kbd>⌘</kbd> <kbd>O</kbd>) or by dropping them on the window. **×** takes one off the list; the file stays.
- `siesta report.sql:12` opens a file at a line and adds it to FILES.
- Siesta lists `.sql`, `.es` ([Elasticsearch](elasticsearch.md)) and `.mongodb.js` ([MongoDB](mongodb.md)) files.

## Running

- <kbd>⌘</kbd> <kbd>Enter</kbd> runs the statement under the cursor, or the selection.
- A file runs on the connection its `-- @connection NAME` line names. The connection picker writes that line for you. See [Connections](connections.md).
- Errors from the server are underlined at their position.

## Results

- **Paging, sorting and filters** run on the server, for any query. Filter syntax: `text` (contains), `=42`, `!=0`, `>10`, `<=2026-01-01`, `null`, `!null`, `a,b,c` (one of).
- **Count all** counts the rows of the whole query.
- **JSON** shows the page as foldable JSON with a JSONPath filter (`$[*].user.address.city`).
- **Raw** shows the server's own JSON (Elasticsearch and MongoDB only).
- **Cell values:** JSON / JSONB is shown as JSON, with a preview on hover; a double click opens the value with search.
- **Copy** the page as TSV, JSON or `INSERT`.
- **Edit** changes cells of a Postgres / MySQL table and writes `UPDATE … WHERE <primary key>` after you confirm.

## History

Every run is stored: statement, connection, time, duration, rows or error. Never the data. Runs from the app, the CLI (`cli`) and AI agents (`mcp`) are all there. <kbd>⌘</kbd> <kbd>Y</kbd> opens the list; you can insert or rerun any entry.

## Shortcuts

| Shortcut | Action |
|---|---|
| <kbd>⌘</kbd> <kbd>Enter</kbd> | Run the statement under the cursor (or the selection) |
| <kbd>⌘</kbd> <kbd>⇧</kbd> <kbd>Enter</kbd> | Run every statement in the file |
| <kbd>⌘</kbd> <kbd>.</kbd> | Cancel the running statement |
| <kbd>⌘</kbd> <kbd>⌥</kbd> <kbd>L</kbd> | Run the last statement again |
| <kbd>⌘</kbd> <kbd>⌥</kbd> <kbd>E</kbd> | Pick the connection |
| <kbd>⌘</kbd> <kbd>Y</kbd> | History |
| <kbd>⌘</kbd> <kbd>P</kbd> / <kbd>⌘</kbd> <kbd>⇧</kbd> <kbd>O</kbd> | Go to file / statement |
| <kbd>⌘</kbd> <kbd>⇧</kbd> <kbd>P</kbd> | Command palette |
| <kbd>⌥</kbd> <kbd>↑</kbd> / <kbd>⌥</kbd> <kbd>↓</kbd> | Previous / next statement |
| <kbd>⌘</kbd> <kbd>J</kbd> | Toggle the result panel |
| <kbd>⌘</kbd> <kbd>⌥</kbd> <kbd>F</kbd> | Column filters |
| <kbd>⌘</kbd> <kbd>[</kbd> / <kbd>⌘</kbd> <kbd>]</kbd> | Previous / next page of results |
| <kbd>⌘</kbd> <kbd>B</kbd> | Toggle the sidebar |
| <kbd>⌘</kbd> <kbd>,</kbd> | Settings |

Change them in `~/.config/siesta/keymap.json`. VS Code and JetBrains keymaps are built in.
