<div align="center">

# <img src="crates/app/assets/icon.svg" width="48" align="absmiddle" alt="Siesta icon"> Siesta

**A fast, text-first database client. Queries are plain files.**

Postgres · MySQL / MariaDB · Elasticsearch · MongoDB

Write a statement, press <kbd>⌘</kbd> <kbd>Enter</kbd>, read the result. No project files, no cloud, no lock-in.

[![Rust](https://img.shields.io/badge/rust-2024_edition-orange?logo=rust)](https://www.rust-lang.org)
[![GPUI](https://img.shields.io/badge/UI-GPUI-blue)](https://www.gpui.rs)
[![Postgres](https://img.shields.io/badge/Postgres-supported-336791?logo=postgresql&logoColor=white)](docs/connections.md)
[![MySQL / MariaDB](https://img.shields.io/badge/MySQL_%2F_MariaDB-supported-00758F?logo=mysql&logoColor=white)](docs/connections.md)
[![Elasticsearch](https://img.shields.io/badge/Elasticsearch-supported-005571?logo=elasticsearch&logoColor=white)](docs/elasticsearch.md)
[![MongoDB](https://img.shields.io/badge/MongoDB-supported-47A248?logo=mongodb&logoColor=white)](docs/mongodb.md)
[![Platform](https://img.shields.io/badge/platform-macOS-lightgrey?logo=apple)](#install)
[![License: MIT](https://img.shields.io/badge/license-MIT-green)](Cargo.toml)

<img src="docs/screenshot.png" width="900" alt="Siesta: statement tree, .sql editor, connection and database pickers, result panel">

</div>

---

## Why Siesta

- 📄 **Files are the source of truth.** A `.sql` file runs unchanged in `psql` and `mysql`. Commit, diff and review it like code.
- ⚡ **Native and fast.** Rust and [GPUI](https://www.gpui.rs), the UI framework behind Zed. One small binary, no Electron.
- 🛡️ **Safe around production.** A *production* connection asks before every write. A *read-only* one refuses writes.
- 🧰 **App, CLI and AI agents.** The same files run in the app, a terminal, CI or an agent through MCP.
- 🔐 **No plain-text passwords.** Encrypted with a local key, never sent anywhere.

## Quick start

1. Save this as `reports.sql`:

   ```sql
   -- @connection dev-pg

   -- @name users_by_day
   SELECT date_trunc('day', created_at) AS day, count(*)
   FROM users
   GROUP BY 1
   ORDER BY 1 DESC;
   ```

2. Open its folder in Siesta.
3. Add a connection named `dev-pg` with **+ Add connection**.
4. Put the cursor on the statement and press <kbd>⌘</kbd> <kbd>Enter</kbd>.

The same file runs from a terminal:

```sh
siesta run reports.sql users_by_day
```

## Install

Siesta targets **macOS**. You need a recent stable Rust toolchain (edition 2024).

```sh
git clone git@github.com:gandriyko/siesta.git
cd siesta
scripts/bundle-macos.sh --install    # Siesta.app in /Applications
cargo install --path crates/app      # the siesta binary (app + CLI) on PATH
```

## Documentation

| Page | What's inside |
|---|---|
| [Desktop app](docs/desktop-app.md) | Sidebar, running, results, history, shortcuts |
| [Connections](docs/connections.md) | Connection files, keys, `prod` / `read_only`, passwords |
| [Elasticsearch](docs/elasticsearch.md) | `.es` files in the Kibana console format |
| [MongoDB](docs/mongodb.md) | `.mongodb.js` files with mongosh calls |
| [CLI](docs/cli.md) | Commands, flags, output formats, exit codes |
| [AI agents](docs/ai-agents.md) | The agent skill, the MCP server, what an agent may run |
| [Where files live](docs/files.md) | `~/.config/siesta` and `~/.siesta` |
| [Development](docs/development.md) | Crates, tests, build, design |

## License

[MIT](Cargo.toml)
