//! Test-only helpers for crates that need a live [`Storage`](crate::Storage)
//! backend in their unit tests (plugins, external-api).
//!
//! `TestStore` is whichever backend the enabled features provide. Today that
//! is the Postgres store; when the embedded `redb` backend lands it takes
//! precedence here (no external service needed), and backend-specific
//! behaviour stays covered by ip-manager's own conformance suite, which runs
//! against every enabled backend.
//!
//! Hidden from docs; not a stable API.

#[cfg(feature = "postgres")]
pub type TestStore = crate::postgres::PostgresDb;

/// The backend's own error type: converts into `anyhow::Error` and
/// `Box<dyn Error>` alike, so `?` works in every test signature.
#[cfg(feature = "postgres")]
pub type TestError = <TestStore as crate::Storage>::Error;

/// A fresh, isolated store per call (a transient Postgres database). Never
/// shared between tests.
#[cfg(feature = "postgres")]
pub async fn new_test_store() -> Result<TestStore, TestError> {
    crate::postgres::PostgresDb::new_test().await
}
