//! A connection URL split into the form's fields (a small hand parser: `url` is not a dependency here).

use crate::{Driver, SslMode};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedUrl {
    pub driver: Option<Driver>,
    pub user: String,
    pub password: String,
    pub host: String,
    /// A Unix socket path (`?host=/dir`, `?socket=/file`); `host` is then empty.
    pub socket: String,
    /// `None`: the driver's default.
    pub port: Option<u16>,
    pub database: String,
    pub ssl_mode: Option<SslMode>,
    /// Names of the query parameters that were not used.
    pub ignored: Vec<String>,
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while i < b.len() {
        let hex = (b[i] == b'%' && i + 2 < b.len()).then(|| s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())).flatten();
        match hex {
            Some(v) => (out.push(v), i += 3),
            None => (out.push(b[i]), i += 1),
        };
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parses `postgres://` / `postgresql://` / `mysql://` / `mariadb://` URLs, also as app configs
/// write them: SQLAlchemy's `mysql+pymysql://`, a bare driver name (`pymysql://`, `asyncpg://`)
/// and `jdbc:`. `Err` is a short reason.
pub fn parse_url(url: &str) -> Result<ParsedUrl, String> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://").ok_or("Unsupported scheme")?;
    let scheme = scheme.to_ascii_lowercase();
    let scheme = scheme.strip_prefix("jdbc:").unwrap_or(&scheme);
    // `dialect+driver`: either part names the database.
    let driver = scheme
        .split('+')
        .find_map(|part| match part {
            "postgres" | "postgresql" | "psycopg" | "psycopg2" | "asyncpg" | "pg8000" => Some(Driver::Postgres),
            "mysql" | "mariadb" | "pymysql" | "mysqldb" | "mysqlconnector" | "aiomysql" | "asyncmy" => Some(Driver::MySql),
            _ => None,
        })
        .ok_or("Unsupported scheme")?;
    let rest = rest.split('#').next().unwrap_or_default();
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
    let (userinfo, hostport) = authority.rsplit_once('@').map_or(("", authority), |(u, h)| (u, h));
    let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
    let (host, port) = match hostport.strip_prefix('[') {
        Some(v) => {
            let (h, after) = v.split_once(']').ok_or("Missing ] in the host")?;
            (h, after.strip_prefix(':').or(if after.is_empty() { Some("") } else { None }).ok_or("Invalid host")?)
        }
        None => hostport.split_once(':').unwrap_or((hostport, "")),
    };
    let port = if port.is_empty() { None } else { Some(port.parse::<u16>().map_err(|_| "Invalid port")?) };
    let mut p = ParsedUrl {
        driver: Some(driver),
        user: decode(user),
        password: decode(password),
        host: decode(host),
        port,
        database: decode(path.strip_prefix('/').unwrap_or_default()),
        ..Default::default()
    };
    for (k, v) in query.split('&').filter(|s| !s.is_empty()).map(|s| s.split_once('=').unwrap_or((s, ""))) {
        let v = decode(v);
        // A Unix socket: libpq's `?host=/dir`, MySQL's `?socket=/file`.
        if matches!((driver, k), (Driver::Postgres, "host") | (Driver::MySql, "socket")) && v.starts_with('/') {
            p.socket = v;
            continue;
        }
        let mode = match (driver, k) {
            (Driver::Postgres, "sslmode") => match v.as_str() {
                "disable" => Some(SslMode::Disable),
                "prefer" => Some(SslMode::Prefer),
                "require" => Some(SslMode::Require),
                "verify-ca" => Some(SslMode::VerifyCa),
                "verify-full" => Some(SslMode::VerifyFull),
                _ => None,
            },
            (Driver::MySql, "ssl-mode") => match v.to_ascii_uppercase().as_str() {
                "DISABLED" => Some(SslMode::Disable),
                "PREFERRED" => Some(SslMode::Prefer),
                "REQUIRED" => Some(SslMode::Require),
                "VERIFY_CA" => Some(SslMode::VerifyCa),
                "VERIFY_IDENTITY" => Some(SslMode::VerifyFull),
                _ => None,
            },
            _ => None,
        };
        match mode {
            Some(m) => p.ssl_mode = Some(m),
            None => p.ignored.push(k.into()),
        }
    }
    if p.host.is_empty() && p.socket.is_empty() {
        return Err("Missing host".into());
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    type Cols<S> = (Option<Driver>, S, S, S, Option<u16>, S, Option<SslMode>, Vec<String>);
    type Row = Cols<&'static str>;

    fn ok(url: &str) -> Cols<String> {
        let p = parse_url(url).unwrap();
        (p.driver, p.user, p.password, if p.socket.is_empty() { p.host } else { p.socket }, p.port, p.database, p.ssl_mode, p.ignored)
    }

    fn own(r: Row) -> Cols<String> {
        (r.0, r.1.into(), r.2.into(), r.3.into(), r.4, r.5.into(), r.6, r.7)
    }

    #[test]
    fn table() {
        use Driver::*;
        let none = vec![];
        for (url, want) in [
            ("postgres://u:p@h:5433/db", (Some(Postgres), "u", "p", "h", Some(5433), "db", None, none.clone())),
            ("postgresql://u@h/db", (Some(Postgres), "u", "", "h", None, "db", None, none.clone())),
            ("mysql://u:p@h:3307/db", (Some(MySql), "u", "p", "h", Some(3307), "db", None, none.clone())),
            ("mariadb://h", (Some(MySql), "", "", "h", None, "", None, none.clone())),
            ("postgres://u:p%40%3A%2Fx@h/d%20b", (Some(Postgres), "u", "p@:/x", "h", None, "d b", None, none.clone())),
            ("postgres://u:p@[::1]:5432/db", (Some(Postgres), "u", "p", "::1", Some(5432), "db", None, none.clone())),
            ("postgres://[::1]/db", (Some(Postgres), "", "", "::1", None, "db", None, none.clone())),
            ("postgres://h/db?sslmode=verify-full", (Some(Postgres), "", "", "h", None, "db", Some(SslMode::VerifyFull), none.clone())),
            ("mysql://h/db?ssl-mode=VERIFY_IDENTITY", (Some(MySql), "", "", "h", None, "db", Some(SslMode::VerifyFull), none.clone())),
            ("mysql://h/db?ssl-mode=DISABLED&x=1", (Some(MySql), "", "", "h", None, "db", Some(SslMode::Disable), vec!["x".into()])),
            ("pymysql://u:p@h:3306/db?ssl_ca=config/ca.pem", (Some(MySql), "u", "p", "h", Some(3306), "db", None, vec!["ssl_ca".into()])),
            ("mysql+pymysql://u@h/db", (Some(MySql), "u", "", "h", None, "db", None, none.clone())),
            ("postgresql+asyncpg://u@h/db", (Some(Postgres), "u", "", "h", None, "db", None, none.clone())),
            ("jdbc:mysql://h/db", (Some(MySql), "", "", "h", None, "db", None, none.clone())),
            ("postgres:///db?host=/tmp", (Some(Postgres), "", "", "/tmp", None, "db", None, none.clone())),
            ("postgres://u@%2Fvar%2Frun%2Fpostgresql/db", (Some(Postgres), "u", "", "/var/run/postgresql", None, "db", None, none.clone())),
            ("mysql://u@localhost/db?socket=/tmp/mysql.sock", (Some(MySql), "u", "", "/tmp/mysql.sock", None, "db", None, none.clone())),
            ("postgres://h/db?application_name=a&sslmode=require", (Some(Postgres), "", "", "h", None, "db", Some(SslMode::Require), vec!["application_name".into()])),
        ] {
            assert_eq!(ok(url), own(want), "{url}");
        }
        for (url, err) in [("http://h/db", "Unsupported scheme"), ("h/db", "Unsupported scheme"), ("postgres://", "Missing host"), ("postgres://u:p@/db", "Missing host"), ("postgres://h:x/db", "Invalid port")] {
            assert_eq!(parse_url(url), Err(err.into()), "{url}");
        }
    }
}
