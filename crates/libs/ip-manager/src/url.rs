//! Parsing of the `DATABASE_URL` / `--database-url` value.
//!
//! The URL scheme selects the storage backend:
//!
//! - `postgres://user:pass@host:5432/db` (or `postgresql://`): the PostgreSQL
//!   store shared by the split `dora-v4` / `dora-v6` / `dora-api` services.
//! - `redb:///absolute/path/to/dora.redb`: the embedded single-process store
//!   used by `dora-embedded`. Three slashes: the path must be absolute.
//!
//! Each binary accepts only the scheme it is built for and reports the other
//! with a clear error; this module is feature-independent so every binary can
//! give that error.

use std::{fmt, path::PathBuf, str::FromStr};

use thiserror::Error;

/// A parsed storage URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageUrl {
    /// The original URL, passed verbatim to the Postgres connector.
    Postgres(String),
    /// Absolute path of the redb database file.
    Redb(PathBuf),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StorageUrlError {
    #[error("database URL has no scheme (expected postgres://... or redb:///abs/path)")]
    MissingScheme,
    #[error("unsupported database URL scheme `{0}`; expected `postgres` or `redb`")]
    UnknownScheme(String),
    #[error(
        "redb URL must be an absolute path with three slashes, e.g. \
         redb:///var/lib/dora/dora.redb (got `{0}`)"
    )]
    RedbNotAbsolute(String),
    #[error("redb URL may not contain a query string or fragment")]
    RedbExtraComponents,
    #[error(
        "DATABASE_URL uses the `{found}` scheme but this binary supports {expected} only \
         (use dora-embedded for redb:// stores, dora-v4/v6/api/migrate for postgres://)"
    )]
    WrongBackend {
        expected: &'static str,
        found: &'static str,
    },
}

impl StorageUrl {
    /// The Postgres connection string, or [`StorageUrlError::WrongBackend`].
    pub fn into_postgres(self) -> Result<String, StorageUrlError> {
        match self {
            StorageUrl::Postgres(url) => Ok(url),
            other => Err(StorageUrlError::WrongBackend {
                expected: "postgres",
                found: other.scheme(),
            }),
        }
    }

    /// The redb file path, or [`StorageUrlError::WrongBackend`].
    pub fn into_redb(self) -> Result<PathBuf, StorageUrlError> {
        match self {
            StorageUrl::Redb(path) => Ok(path),
            other => Err(StorageUrlError::WrongBackend {
                expected: "redb",
                found: other.scheme(),
            }),
        }
    }

    /// The scheme name, `"postgres"` or `"redb"`.
    pub fn scheme(&self) -> &'static str {
        match self {
            StorageUrl::Postgres(_) => "postgres",
            StorageUrl::Redb(_) => "redb",
        }
    }

    /// A form safe to log: Postgres credentials are masked, redb paths are
    /// shown as-is.
    pub fn redacted(&self) -> String {
        match self {
            StorageUrl::Postgres(url) => redact_url(url),
            StorageUrl::Redb(path) => format!("redb://{}", path.display()),
        }
    }
}

impl fmt::Display for StorageUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.redacted())
    }
}

impl FromStr for StorageUrl {
    type Err = StorageUrlError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let Some((scheme, rest)) = s.split_once("://") else {
            return Err(StorageUrlError::MissingScheme);
        };
        match scheme.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" => Ok(StorageUrl::Postgres(s.to_owned())),
            "redb" => {
                if rest.contains('?') || rest.contains('#') {
                    return Err(StorageUrlError::RedbExtraComponents);
                }
                // `redb:///abs/path` leaves `rest` as `/abs/path`; anything
                // else (`redb://host/...`, `redb://relative`) is rejected.
                if !rest.starts_with('/') {
                    return Err(StorageUrlError::RedbNotAbsolute(s.to_owned()));
                }
                Ok(StorageUrl::Redb(PathBuf::from(rest)))
            }
            other => Err(StorageUrlError::UnknownScheme(other.to_owned())),
        }
    }
}

/// Mask the password in `scheme://user:password@host/...`. A pure string
/// operation on the userinfo part; URLs without credentials are returned
/// unchanged, and it never fails.
pub fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    // the authority ends at the first '/', '?' or '#'; the userinfo is the part
    // of the authority before the last '@' (passwords may contain '@').
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let Some((userinfo, hostport)) = authority.rsplit_once('@') else {
        return url.to_owned();
    };
    let user = userinfo.split_once(':').map_or(userinfo, |(u, _)| u);
    let masked = if userinfo.contains(':') {
        format!("{user}:***")
    } else {
        user.to_owned()
    };
    format!("{scheme}://{masked}@{hostport}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_postgres_schemes() {
        for url in [
            "postgres://dora:dora@localhost/dora",
            "postgresql://dora:dora@localhost:5432/dora?sslmode=require",
            "POSTGRES://x@y/z",
        ] {
            assert_eq!(url.parse(), Ok(StorageUrl::Postgres(url.to_owned())));
        }
    }

    #[test]
    fn parses_absolute_redb_path() {
        assert_eq!(
            "redb:///var/lib/dora/dora.redb".parse(),
            Ok(StorageUrl::Redb(PathBuf::from("/var/lib/dora/dora.redb")))
        );
    }

    #[test]
    fn rejects_relative_or_host_redb() {
        assert!(matches!(
            "redb://dora.redb".parse::<StorageUrl>(),
            Err(StorageUrlError::RedbNotAbsolute(_))
        ));
        assert!(matches!(
            "redb://localhost/var/lib/dora.redb".parse::<StorageUrl>(),
            Err(StorageUrlError::RedbNotAbsolute(_))
        ));
    }

    #[test]
    fn rejects_redb_query_and_fragment() {
        assert_eq!(
            "redb:///a/b.redb?sync=lazy".parse::<StorageUrl>(),
            Err(StorageUrlError::RedbExtraComponents)
        );
        assert_eq!(
            "redb:///a/b.redb#x".parse::<StorageUrl>(),
            Err(StorageUrlError::RedbExtraComponents)
        );
    }

    #[test]
    fn rejects_unknown_or_missing_scheme() {
        assert_eq!(
            "sqlite:///a.db".parse::<StorageUrl>(),
            Err(StorageUrlError::UnknownScheme("sqlite".into()))
        );
        assert_eq!(
            "/var/lib/dora/dora.redb".parse::<StorageUrl>(),
            Err(StorageUrlError::MissingScheme)
        );
    }

    #[test]
    fn redacts_password_only() {
        assert_eq!(
            redact_url("postgres://dora:s3cret@db:5432/dora?sslmode=require"),
            "postgres://dora:***@db:5432/dora?sslmode=require"
        );
        assert_eq!(
            redact_url("postgres://dora:p@ss@db/dora"),
            "postgres://dora:***@db/dora"
        );
        assert_eq!(
            redact_url("postgres://dora@db/dora"),
            "postgres://dora@db/dora"
        );
        assert_eq!(redact_url("postgres://db/dora"), "postgres://db/dora");
        assert_eq!(redact_url("not a url"), "not a url");
    }

    #[test]
    fn backend_selection() {
        let pg: StorageUrl = "postgres://u:p@h/d".parse().unwrap();
        assert_eq!(pg.clone().into_postgres().unwrap(), "postgres://u:p@h/d");
        assert_eq!(
            pg.into_redb(),
            Err(StorageUrlError::WrongBackend {
                expected: "redb",
                found: "postgres"
            })
        );
        let rb: StorageUrl = "redb:///tmp/x.redb".parse().unwrap();
        assert_eq!(
            rb.clone().into_redb().unwrap(),
            PathBuf::from("/tmp/x.redb")
        );
        assert_eq!(
            rb.into_postgres(),
            Err(StorageUrlError::WrongBackend {
                expected: "postgres",
                found: "redb"
            })
        );
    }

    #[test]
    fn display_is_redacted() {
        let url: StorageUrl = "postgres://u:p@h/d".parse().unwrap();
        assert_eq!(url.to_string(), "postgres://u:***@h/d");
        assert_eq!(url.scheme(), "postgres");
        let url: StorageUrl = "redb:///tmp/x.redb".parse().unwrap();
        assert_eq!(url.to_string(), "redb:///tmp/x.redb");
        assert_eq!(url.scheme(), "redb");
    }
}
