//! Test-only helpers for crates that need a live [`Storage`](crate::Storage)
//! backend in their unit tests (plugins, external-api).
//!
//! `TestStore` is whichever backend the enabled features provide. When both
//! are enabled the embedded `redb` store wins: it needs no external service,
//! and backend-specific behaviour is covered by ip-manager's own conformance
//! suite, which runs against every enabled backend.
//!
//! Hidden from docs; not a stable API.

#[cfg(feature = "redb")]
pub type TestStore = crate::redb::RedbDb;
#[cfg(all(feature = "postgres", not(feature = "redb")))]
pub type TestStore = crate::postgres::PostgresDb;

/// The backend's own error type: converts into `anyhow::Error` and
/// `Box<dyn Error>` alike, so `?` works in every test signature.
#[cfg(any(feature = "postgres", feature = "redb"))]
pub type TestError = <TestStore as crate::Storage>::Error;

/// A fresh, isolated store per call: a temporary redb file, or a transient
/// Postgres database. Never shared between tests.
#[cfg(any(feature = "postgres", feature = "redb"))]
pub async fn new_test_store() -> Result<TestStore, TestError> {
    #[cfg(feature = "redb")]
    {
        crate::redb::RedbDb::new_test()
    }
    #[cfg(all(feature = "postgres", not(feature = "redb")))]
    {
        crate::postgres::PostgresDb::new_test().await
    }
}
