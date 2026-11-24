[← Siesta](../README.md)

# Connections

Each connection is one JSON file in `~/.config/siesta/connections/`, named after the connection. The app's connection form writes it; you can also write it by hand.

```jsonc
// ~/.config/siesta/connections/dev-pg.json
{ "driver": "postgres", "host": "localhost", "port": 5432, "database": "app", "user": "app" }

// ~/.config/siesta/connections/prod-mysql.json
{ "driver": "mysql", "url": "mysql://ro@db.internal:3306/shop", "read_only": true, "prod": true }
```

## Keys

| Key | Meaning |
|---|---|
| `driver` | `postgres`, `mysql` (also MariaDB), `elasticsearch` or `mongodb` |
| `url` | A connection URL; it fills the other keys (no password in it) |
| `host`, `port` | The server |
| `socket` | `true` (find the local Unix socket) or its path; Postgres and MySQL only |
| `database` | The default database; a file or the CLI can pick another |
| `user` | The user name |
| `ssl` | `mode` (`disable`, `prefer`, `require`, `verify-ca`, `verify-full`), `ca`, `cert`, `key` |
| `read_only` | Refuse writes (see below) |
| `prod` | Production: confirm every write (see below) |
| `schema` | Postgres `search_path` |
| `init_sql` | SQL run when a session opens |
| `color` | The colour of the connection's dot and banner |

`${env:SHOP_DB_HOST}` in any value reads the environment, which is handy in CI. There is no `password` key: see [Passwords](#passwords).

Elasticsearch and MongoDB have a few keys of their own: see [Elasticsearch](elasticsearch.md) and [MongoDB](mongodb.md).

## Choosing a connection

- **Per file.** A file runs on the connection its `-- @connection dev-pg` line names. The top-right picker writes that line (as an edit you save or undo), so the choice travels with the file and the CLI sees it too.
- **Per database.** `database` is only the default. The second picker writes `-- @database app_test`. The CLI takes `-d app_test`.

## Safety

| Flag | What it does |
|---|---|
| `"prod": true` | A red banner. Every statement that is not a plain read (DML, DDL, `SET`, …) asks for confirmation. `SELECT`, `SHOW` and `EXPLAIN` run freely. |
| `"read_only": true` | Writes are refused before they reach the server. Postgres and MySQL sessions are also read-only on the server. |

Both are safety defaults, not a security boundary. For real protection, connect with a read-only database user.

An open transaction is asked about before a file or the app closes.

## Passwords

A password is never written in clear.

1. Type it in the connection form, or run `siesta connections set-password NAME`.
2. Siesta stores it as `"password_enc": "v1:…"`, encrypted with XChaCha20-Poly1305 and bound to the connection name.
3. The key is random and lives in `~/.siesta/key` (mode 0600), away from the connection files.

- **The key is the secret.** Connection files can go to a dotfiles repo without it. If the key is lost, set the passwords again.
- **What this protects against:** screenshots, `grep`, backups and synced files without the key. It does not protect against malware running as your user, which can read the key too.
