use std::{
    collections::HashSet,
    net::{IpAddr, Ipv6Addr},
    ops::RangeInclusive,
    str::FromStr,
    time::{Duration, SystemTime},
};

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use sqlx::{
    ConnectOptions, Connection, PgConnection, Postgres,
    postgres::{PgConnectOptions, PgPool},
};
use tracing::debug;

use crate::{
    ClientInfo, ConfigCandidateRecord, IpState, OperationRecord, OperationStatus,
    RuntimeReservationRecord, State, Storage,
};

#[derive(Debug)]
pub struct PostgresDb {
    inner: PgPool,
}

impl Clone for PostgresDb {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl PostgresDb {
    /// Connect a pool to `uri` **without** running migrations.
    ///
    /// In the split-service deployment the schema is owned by the run-once
    /// `dora-migrate` job, not by the servers: having every v4/v6/api process
    /// race to migrate the shared database on boot is exactly what we want to
    /// avoid. Services call this; the migrator (and unit tests) call
    /// [`PostgresDb::migrate`] / [`PostgresDb::new`].
    pub async fn connect(uri: impl AsRef<str>) -> Result<Self, sqlx::Error> {
        // log queries at trace level so we don't get a bloated log on `info`
        // (sqlx 0.7+: ConnectOptions setters consume and return Self)
        let opts = PgConnectOptions::from_str(uri.as_ref())?
            .log_statements(tracing::log::LevelFilter::Trace);

        let inner = PgPool::connect_with(opts).await?;
        Ok(Self { inner })
    }

    /// Connect a pool to `uri` and run the embedded migrations against it.
    ///
    /// Kept for in-process users that own their database (e.g. unit tests). The
    /// split services use [`PostgresDb::connect`] instead and rely on the
    /// `dora-migrate` job having already applied the schema.
    pub async fn new(uri: impl AsRef<str>) -> Result<Self, sqlx::Error> {
        let db = Self::connect(uri).await?;
        sqlx::migrate!("../../../migrations").run(&db.inner).await?;
        Ok(db)
    }

    /// Run the embedded migrations against `uri` and return.
    ///
    /// This is the entry point for the run-once `dora-migrate` binary: it
    /// connects, applies any pending migrations, and drops the pool. Idempotent
    /// — re-running against an up-to-date database is a no-op.
    pub async fn migrate(uri: impl AsRef<str>) -> Result<(), sqlx::Error> {
        let db = Self::connect(uri).await?;
        sqlx::migrate!("../../../migrations").run(&db.inner).await?;
        Ok(())
    }

    /// Create an isolated Postgres database for a single test/dev run.
    ///
    /// In-memory SQLite is gone, so each test needs its own database. This
    /// reads an admin/base URL from `DATABASE_URL` (defaulting to the local
    /// dev DB), connects to it, and issues `CREATE DATABASE dora_test_<pid>_<n>`
    /// where `n` comes from a process-wide atomic counter. That yields a unique
    /// database name without pulling in rand/uuid. It then builds a fresh URL
    /// pointing at that database, connects a pool, and runs migrations.
    ///
    /// NOTE: `CREATE DATABASE` cannot run inside a transaction, so it is issued
    /// on a plain pooled connection (sqlx does not wrap bare `execute` in a
    /// transaction).
    #[doc(hidden)]
    pub async fn new_test() -> Result<Self, sqlx::Error> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let base_url = admin_base_url();

        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let db_name = format!("dora_test_{}_{}", std::process::id(), n);

        // create the fresh test database (serialized cluster-wide, see helper)
        create_database(&db_name, false).await?;

        // build a new URL pointing at the fresh database by swapping the path
        // segment (the part after the last '/', before any query string)
        let test_url = swap_db_name(&base_url, &db_name);

        let opts =
            PgConnectOptions::from_str(&test_url)?.log_statements(tracing::log::LevelFilter::Trace);
        let inner = PgPool::connect_with(opts).await?;
        sqlx::migrate!("../../../migrations").run(&inner).await?;
        Ok(Self { inner })
    }
}

/// Create a fresh, empty database named `db_name` for out-of-process integration
/// tests: the dora binary is spawned as a separate process and connects to this
/// database by URL, so a caller cannot use [`PostgresDb::new_test`] (which hands
/// back a live pool). Any pre-existing database of that name is dropped first
/// (`WITH (FORCE)` evicts a lingering connection from a killed prior run) so each
/// test starts clean. Uses the admin/base `DATABASE_URL` (defaulting to the local
/// dev DB). Migrations are run by the spawned dora process on startup, not here.
#[doc(hidden)]
pub async fn create_test_database(db_name: &str) -> Result<(), sqlx::Error> {
    create_database(db_name, true).await
}

/// Create database `db_name` (optionally dropping any prior one first), on a
/// single admin connection holding a cluster-wide advisory lock.
///
/// `CREATE DATABASE` copies `template1`, and two of them running at once can
/// transiently fail with SQLSTATE 55006 ("source database ... is being accessed
/// by other users"). Parallel test processes each provision their own database,
/// so we serialize the operation with `pg_advisory_lock` on a fixed key: Postgres
/// itself queues concurrent callers server-side (no client-side polling). The
/// session lock is released automatically when the connection closes.
///
/// Uses a single [`PgConnection`] (not a pool) so the lock and the `CREATE` run
/// on the same session — a pool could route them to different connections and
/// the lock would not apply. Neither statement may run inside a transaction.
async fn create_database(db_name: &str, drop_first: bool) -> Result<(), sqlx::Error> {
    // arbitrary fixed key shared by all callers; "dora" in hex
    const CREATE_DB_LOCK: i64 = 0x646f_7261;
    let mut admin = PgConnection::connect(&admin_base_url()).await?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(CREATE_DB_LOCK)
        .execute(&mut admin)
        .await?;
    if drop_first {
        sqlx::query(&format!("DROP DATABASE IF EXISTS {db_name} WITH (FORCE)"))
            .execute(&mut admin)
            .await?;
    }
    sqlx::query(&format!("CREATE DATABASE {db_name}"))
        .execute(&mut admin)
        .await?;
    // closing the session releases the advisory lock
    admin.close().await?;
    Ok(())
}

/// Drop a database created by [`create_test_database`]. Best-effort cleanup.
#[doc(hidden)]
pub async fn drop_test_database(db_name: &str) -> Result<(), sqlx::Error> {
    let admin = PgPool::connect(&admin_base_url()).await?;
    sqlx::query(&format!("DROP DATABASE IF EXISTS {db_name} WITH (FORCE)"))
        .execute(&admin)
        .await?;
    admin.close().await;
    Ok(())
}

/// The admin/base connection URL used to create and drop transient test
/// databases, from `DATABASE_URL` or the local dev default.
fn admin_base_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://dora:dora@localhost/dora".to_string())
}

/// Replace the database-name path segment of a Postgres connection URL,
/// preserving any query string.
fn swap_db_name(url: &str, db_name: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (url, None),
    };
    let prefix = match base.rfind('/') {
        Some(idx) => &base[..=idx],
        None => base,
    };
    match query {
        Some(q) => format!("{prefix}{db_name}?{q}"),
        None => format!("{prefix}{db_name}"),
    }
}

#[async_trait]
impl Storage for PostgresDb {
    // TODO: consider alternate error type
    type Error = sqlx::Error;

    /// find the next expired IP in the range, or where client_id matches,
    /// and update it with the new client_id & expiry & state
    /// NOTE: always sets probation = false
    async fn next_expired(
        &self,
        range: RangeInclusive<IpAddr>,
        network: IpAddr,
        id: &[u8],
        expires_at: SystemTime,
        state: Option<IpState>,
    ) -> Result<Option<IpAddr>, Self::Error> {
        match (*range.start(), *range.end(), network) {
            (IpAddr::V4(start), IpAddr::V4(end), IpAddr::V4(_network)) => {
                let start_ip = u32::from(start) as i64;
                let end_ip = u32::from(end) as i64;
                let now = util::systime_epoch(SystemTime::now());
                let (leased, _probate) = state.unwrap_or(IpState::Reserve).into();

                Ok(util::update_next_expired(
                    &self.inner,
                    now,
                    id,
                    start_ip,
                    end_ip,
                    util::systime_epoch(expires_at),
                    leased,
                )
                .await?)
            }
            (IpAddr::V6(start), IpAddr::V6(end), IpAddr::V6(_network)) => {
                let now = util::systime_epoch(SystemTime::now());
                let (leased, _probate) = state.unwrap_or(IpState::Reserve).into();
                Ok(util_v6::update_next_expired(
                    &self.inner,
                    now,
                    id,
                    &util_v6::to_bytes(start),
                    &util_v6::to_bytes(end),
                    util::systime_epoch(expires_at),
                    leased,
                )
                .await?)
            }
            _ => panic!("mixed v4/v6 address families in next_expired"),
        }
    }

    /// find next available IP in the range and insert an entry for this id
    async fn insert_max_in_range(
        &self,
        range: RangeInclusive<IpAddr>,
        exclusions: &HashSet<IpAddr>,
        network: IpAddr,
        id: &[u8],
        expires_at: SystemTime,
        state: Option<IpState>,
    ) -> Result<Option<IpAddr>, Self::Error> {
        // a different Error type here would let us remove Option
        // Option is currently doing work as the method to say "can't find an IP in the range",
        // this should probably be an error variant
        match (*range.start(), *range.end(), network) {
            (IpAddr::V4(start), IpAddr::V4(end), IpAddr::V4(network)) => {
                let start_ip = u32::from(start) as i64;
                let end_ip = u32::from(end) as i64;
                // allocation needed for future
                let id = id.to_vec();

                debug!("no expired entries, finding start of range");
                // TRANSACTION START
                let mut conn = self.inner.begin().await?;
                // we only use this IP to find what the next available should be
                let ip = match util::max_in_range(&mut *conn, start_ip, end_ip).await? {
                    Some(State::Leased(cur) | State::Reserved(cur) | State::Probated(cur)) => {
                        let start = cur.ip;
                        let end = *range.end();
                        debug!(?start, "get next IP starting from");
                        util::inc_ip(start, end, exclusions)
                    }
                    None => {
                        debug!(start = ?range.start(), "using start of range");
                        // range is empty; use the first non-excluded address
                        util::first_available(*range.start(), *range.end(), exclusions)
                    }
                };
                if let Some(IpAddr::V4(v4_ip)) = ip {
                    util::insert(
                        &mut *conn,
                        u32::from(v4_ip) as i64,
                        u32::from(network) as i64,
                        &id,
                        util::systime_epoch(expires_at),
                        state.map(|s| s.into()),
                    )
                    .await?;
                    // TRANSACTION COMMIT
                    conn.commit().await?;
                    Ok(ip)
                } else {
                    debug!("unable to find start of range");
                    // TRANSACTION ROLLBACK
                    conn.rollback().await?;
                    Ok(None)
                }
            }
            (IpAddr::V6(start), IpAddr::V6(end), IpAddr::V6(network)) => {
                let id = id.to_vec();
                debug!("no expired v6 entries, finding start of range");
                let mut conn = self.inner.begin().await?;
                // find the highest allocated address in the range, then step to the
                // next available one; if the range is empty, use its start
                let ip = match util_v6::max_in_range(
                    &mut *conn,
                    &util_v6::to_bytes(start),
                    &util_v6::to_bytes(end),
                )
                .await?
                {
                    Some(State::Leased(cur) | State::Reserved(cur) | State::Probated(cur)) => {
                        util_v6::inc_ip(cur.ip, IpAddr::V6(end), exclusions)
                    }
                    // range is empty; use the first non-excluded address
                    None => util::first_available(*range.start(), *range.end(), exclusions),
                };
                if let Some(IpAddr::V6(v6_ip)) = ip {
                    util_v6::insert(
                        &mut *conn,
                        &util_v6::to_bytes(v6_ip),
                        &util_v6::to_bytes(network),
                        &id,
                        util::systime_epoch(expires_at),
                        state.map(|s| s.into()),
                    )
                    .await?;
                    conn.commit().await?;
                    Ok(ip)
                } else {
                    debug!("unable to find start of v6 range");
                    conn.rollback().await?;
                    Ok(None)
                }
            }
            _ => panic!("mixed v4/v6 address families in insert_max_in_range"),
        }
    }

    async fn update_expired(
        &self,
        ip: IpAddr,
        state: Option<IpState>,
        id: &[u8],
        expires_at: SystemTime,
    ) -> Result<bool, Self::Error> {
        let (lease, probation) = state.unwrap_or(IpState::Reserve).into();
        match ip {
            IpAddr::V4(ip) => Ok(util::update_expired(
                &self.inner,
                u32::from(ip) as i64,
                id,
                util::systime_epoch(expires_at),
                util::systime_epoch(SystemTime::now()),
                lease,
                probation,
            )
            .await?
            .is_some()),
            IpAddr::V6(ip) => Ok(util_v6::update_expired(
                &self.inner,
                &util_v6::to_bytes(ip),
                id,
                util::systime_epoch(expires_at),
                util::systime_epoch(SystemTime::now()),
                lease,
                probation,
            )
            .await?
            .is_some()),
        }
    }

    async fn update_unexpired(
        &self,
        ip: IpAddr,
        state: IpState,
        id: &[u8],
        expires_at: SystemTime,
        new_id: Option<&[u8]>,
    ) -> Result<Option<IpAddr>, Self::Error> {
        let (lease, probation) = state.into();
        match ip {
            IpAddr::V4(ip) => {
                util::update_unexpired(
                    &self.inner,
                    u32::from(ip) as i64,
                    id,
                    util::systime_epoch(expires_at),
                    util::systime_epoch(SystemTime::now()),
                    lease,
                    probation,
                    new_id,
                )
                .await
            }
            IpAddr::V6(ip) => {
                util_v6::update_unexpired(
                    &self.inner,
                    &util_v6::to_bytes(ip),
                    id,
                    util::systime_epoch(expires_at),
                    util::systime_epoch(SystemTime::now()),
                    lease,
                    probation,
                    new_id,
                )
                .await
            }
        }
    }

    async fn update_ip(
        &self,
        ip: IpAddr,
        state: IpState,
        id: Option<&[u8]>,
        expires_at: SystemTime,
    ) -> Result<Option<State>, Self::Error> {
        let (lease, probation) = state.into();
        match ip {
            IpAddr::V4(ip) => {
                util::update_ip(
                    &self.inner,
                    u32::from(ip) as i64,
                    id,
                    util::systime_epoch(expires_at),
                    lease,
                    probation,
                )
                .await
            }
            IpAddr::V6(ip) => {
                util_v6::update_ip(
                    &self.inner,
                    &util_v6::to_bytes(ip),
                    id,
                    util::systime_epoch(expires_at),
                    lease,
                    probation,
                )
                .await
            }
        }
    }

    async fn insert(
        &self,
        ip: IpAddr,
        network: IpAddr,
        id: &[u8],
        expires_at: SystemTime,
        state: Option<IpState>,
    ) -> Result<(), Self::Error> {
        match (ip, network) {
            (IpAddr::V4(ip), IpAddr::V4(network)) => {
                let ip = u32::from(ip) as i64;
                let network = u32::from(network) as i64;
                let expires_at = util::systime_epoch(expires_at);
                let state = state.map(|s| s.into());
                util::insert(&self.inner, ip, network, id, expires_at, state).await
            }
            (IpAddr::V6(ip), IpAddr::V6(network)) => {
                util_v6::insert(
                    &self.inner,
                    &util_v6::to_bytes(ip),
                    &util_v6::to_bytes(network),
                    id,
                    util::systime_epoch(expires_at),
                    state.map(|s| s.into()),
                )
                .await
            }
            _ => panic!("mixed v4/v6 address families in insert"),
        }
    }

    async fn get(&self, ip: IpAddr) -> Result<Option<State>, Self::Error> {
        match ip {
            IpAddr::V4(ip) => {
                let ip = u32::from(ip) as i64;
                util::find(&self.inner, ip).await
            }
            IpAddr::V6(ip) => util_v6::find(&self.inner, &util_v6::to_bytes(ip)).await,
        }
    }

    async fn get_id(&self, id: &[u8]) -> Result<Option<IpAddr>, Self::Error> {
        util::find_by_id(&self.inner, id, util::systime_epoch(SystemTime::now())).await
    }

    async fn get_id_v6(&self, id: &[u8]) -> Result<Option<IpAddr>, Self::Error> {
        util_v6::find_by_id(&self.inner, id, util::systime_epoch(SystemTime::now())).await
    }

    async fn get_pd(&self, prefix: IpAddr, prefix_len: u8) -> Result<Option<State>, Self::Error> {
        match prefix {
            IpAddr::V6(ip) => {
                util_v6::find_pd(&self.inner, &util_v6::to_bytes(ip), prefix_len as i64).await
            }
            IpAddr::V4(_) => Ok(None),
        }
    }

    async fn upsert_pd(
        &self,
        prefix: IpAddr,
        prefix_len: u8,
        network: IpAddr,
        id: &[u8],
        expires_at: SystemTime,
        state: Option<IpState>,
    ) -> Result<(), Self::Error> {
        match (prefix, network) {
            (IpAddr::V6(ip), IpAddr::V6(net)) => {
                util_v6::upsert_pd(
                    &self.inner,
                    &util_v6::to_bytes(ip),
                    prefix_len as i64,
                    &util_v6::to_bytes(net),
                    id,
                    util::systime_epoch(expires_at),
                    state.map(|s| s.into()),
                )
                .await
            }
            // IA_PD is v6-only; a mismatched family is a no-op (like the other
            // v6 arms returning Ok(None)) rather than a panic
            _ => Ok(()),
        }
    }

    async fn get_id_pd(&self, id: &[u8]) -> Result<Option<(IpAddr, u8)>, Self::Error> {
        util_v6::find_by_id_pd(&self.inner, id, util::systime_epoch(SystemTime::now())).await
    }

    async fn renew_pd(
        &self,
        prefix: IpAddr,
        prefix_len: u8,
        id: &[u8],
        expires_at: SystemTime,
    ) -> Result<Option<IpAddr>, Self::Error> {
        match prefix {
            IpAddr::V6(ip) => {
                util_v6::renew_pd(
                    &self.inner,
                    &util_v6::to_bytes(ip),
                    prefix_len as i64,
                    id,
                    util::systime_epoch(expires_at),
                    util::systime_epoch(SystemTime::now()),
                )
                .await
            }
            IpAddr::V4(_) => Ok(None),
        }
    }

    async fn release_pd(
        &self,
        prefix: IpAddr,
        prefix_len: u8,
        id: &[u8],
    ) -> Result<Option<ClientInfo>, Self::Error> {
        match prefix {
            IpAddr::V6(ip) => {
                util_v6::release_pd(&self.inner, &util_v6::to_bytes(ip), prefix_len as i64, id)
                    .await
            }
            IpAddr::V4(_) => Ok(None),
        }
    }

    async fn release_ip(&self, ip: IpAddr, id: &[u8]) -> Result<Option<ClientInfo>, Self::Error> {
        match ip {
            IpAddr::V4(ip) => {
                let ip = u32::from(ip) as i64;
                util::release_ip(&self.inner, ip, id).await
            }
            IpAddr::V6(ip) => util_v6::release_ip(&self.inner, &util_v6::to_bytes(ip), id).await,
        }
    }

    async fn delete(&self, ip: IpAddr) -> Result<(), Self::Error> {
        match ip {
            IpAddr::V4(ip) => {
                let ip = u32::from(ip) as i64;
                let mut conn = self.inner.begin().await?;
                util::delete(&mut *conn, ip).await?;
                conn.commit().await?;
                Ok(())
            }
            IpAddr::V6(ip) => {
                let mut conn = self.inner.begin().await?;
                util_v6::delete(&mut *conn, &util_v6::to_bytes(ip)).await?;
                conn.commit().await?;
                Ok(())
            }
        }
    }
    async fn count(&self, state: IpState) -> Result<usize, Self::Error> {
        let (lease, probation) = state.into();
        let now = util::systime_epoch(SystemTime::now());
        // count both v4 and v6 bindings
        let v4 = util::count(&self.inner, lease, probation, now).await?;
        let v6 = util_v6::count(&self.inner, lease, probation, now).await?;
        Ok(v4 + v6)
    }

    async fn select_all(&self) -> Result<Vec<State>, Self::Error> {
        let mut all = util::select_all(&self.inner).await?;
        all.extend(util_v6::select_all(&self.inner).await?);
        Ok(all)
    }

    async fn insert_operation(&self, op: &OperationRecord) -> Result<(), Self::Error> {
        let status = op.status.as_str();
        let actor = op.actor.as_deref();
        let input_summary = op.input_summary.as_deref();
        let result = op.result.as_deref();
        let error_code = op.error_code.as_deref();
        let error_message = op.error_message.as_deref();
        let created_at = util::systime_epoch(op.created_at);
        let started_at = op.started_at.map(util::systime_epoch);
        let completed_at = op.completed_at.map(util::systime_epoch);
        sqlx::query!(
            "INSERT INTO operations \
             (operation_id, action, status, actor, input_summary, result, \
              error_code, error_message, created_at, started_at, completed_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
            op.operation_id,
            op.action,
            status,
            actor,
            input_summary,
            result,
            error_code,
            error_message,
            created_at,
            started_at,
            completed_at,
        )
        .execute(&self.inner)
        .await?;
        Ok(())
    }

    async fn update_operation(&self, op: &OperationRecord) -> Result<(), Self::Error> {
        let status = op.status.as_str();
        let actor = op.actor.as_deref();
        let input_summary = op.input_summary.as_deref();
        let result = op.result.as_deref();
        let error_code = op.error_code.as_deref();
        let error_message = op.error_message.as_deref();
        let started_at = op.started_at.map(util::systime_epoch);
        let completed_at = op.completed_at.map(util::systime_epoch);
        sqlx::query!(
            "UPDATE operations SET \
             action = $2, status = $3, actor = $4, input_summary = $5, \
             result = $6, error_code = $7, error_message = $8, \
             started_at = $9, completed_at = $10 \
             WHERE operation_id = $1",
            op.operation_id,
            op.action,
            status,
            actor,
            input_summary,
            result,
            error_code,
            error_message,
            started_at,
            completed_at,
        )
        .execute(&self.inner)
        .await?;
        Ok(())
    }

    async fn get_operation(
        &self,
        operation_id: &str,
    ) -> Result<Option<OperationRecord>, Self::Error> {
        let row = sqlx::query!(
            r#"SELECT
                 operation_id  AS "operation_id!",
                 action        AS "action!",
                 status        AS "status!",
                 actor         AS "actor?",
                 input_summary AS "input_summary?",
                 result        AS "result?",
                 error_code    AS "error_code?",
                 error_message AS "error_message?",
                 created_at    AS "created_at!",
                 started_at    AS "started_at?",
                 completed_at  AS "completed_at?"
               FROM operations WHERE operation_id = $1"#,
            operation_id,
        )
        .fetch_optional(&self.inner)
        .await?;

        let Some(row) = row else { return Ok(None) };
        let status = OperationStatus::from_db_str(&row.status).ok_or_else(|| {
            sqlx::Error::Decode(format!("invalid operation status: {}", row.status).into())
        })?;
        Ok(Some(OperationRecord {
            operation_id: row.operation_id,
            action: row.action,
            status,
            actor: row.actor,
            input_summary: row.input_summary,
            result: row.result,
            error_code: row.error_code,
            error_message: row.error_message,
            created_at: util::to_systime(row.created_at),
            started_at: row.started_at.map(util::to_systime),
            completed_at: row.completed_at.map(util::to_systime),
        }))
    }

    async fn upsert_reservation(&self, res: &RuntimeReservationRecord) -> Result<(), Self::Error> {
        let prefix = res.prefix.as_deref();
        let network = res.network.as_deref();
        let created_at = util::systime_epoch(res.created_at);
        sqlx::query!(
            "INSERT INTO runtime_reservations \
             (family, ip, prefix, network, match_json, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (family, ip) DO UPDATE SET \
             prefix = EXCLUDED.prefix, network = EXCLUDED.network, \
             match_json = EXCLUDED.match_json, created_at = EXCLUDED.created_at",
            res.family,
            res.ip,
            prefix,
            network,
            res.match_json,
            created_at,
        )
        .execute(&self.inner)
        .await?;
        Ok(())
    }

    async fn delete_reservation(&self, family: &str, ip: &str) -> Result<bool, Self::Error> {
        let result = sqlx::query!(
            "DELETE FROM runtime_reservations WHERE family = $1 AND ip = $2",
            family,
            ip
        )
        .execute(&self.inner)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn get_reservation(
        &self,
        family: &str,
        ip: &str,
    ) -> Result<Option<RuntimeReservationRecord>, Self::Error> {
        let row = sqlx::query!(
            r#"SELECT family AS "family!", ip AS "ip!", prefix AS "prefix?",
                      network AS "network?", match_json AS "match_json!",
                      created_at AS "created_at!"
               FROM runtime_reservations WHERE family = $1 AND ip = $2"#,
            family,
            ip,
        )
        .fetch_optional(&self.inner)
        .await?;
        Ok(row.map(|r| RuntimeReservationRecord {
            family: r.family,
            ip: r.ip,
            prefix: r.prefix,
            network: r.network,
            match_json: r.match_json,
            created_at: util::to_systime(r.created_at),
        }))
    }

    async fn list_reservations(&self) -> Result<Vec<RuntimeReservationRecord>, Self::Error> {
        let rows = sqlx::query!(
            r#"SELECT family AS "family!", ip AS "ip!", prefix AS "prefix?",
                      network AS "network?", match_json AS "match_json!",
                      created_at AS "created_at!"
               FROM runtime_reservations ORDER BY family, ip"#
        )
        .fetch_all(&self.inner)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| RuntimeReservationRecord {
                family: r.family,
                ip: r.ip,
                prefix: r.prefix,
                network: r.network,
                match_json: r.match_json,
                created_at: util::to_systime(r.created_at),
            })
            .collect())
    }

    async fn upsert_config_candidate(
        &self,
        candidate: &ConfigCandidateRecord,
    ) -> Result<(), Self::Error> {
        let message = candidate.message.as_deref();
        let validation = candidate.validation.as_deref();
        let created_at = util::systime_epoch(candidate.created_at);
        let activated_at = candidate.activated_at.map(util::systime_epoch);
        sqlx::query!(
            "INSERT INTO config_candidates \
             (candidate_id, status, document, message, validation, created_at, activated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (candidate_id) DO UPDATE SET \
             status = EXCLUDED.status, document = EXCLUDED.document, \
             message = EXCLUDED.message, validation = EXCLUDED.validation, \
             created_at = EXCLUDED.created_at, activated_at = EXCLUDED.activated_at",
            candidate.candidate_id,
            candidate.status,
            candidate.document,
            message,
            validation,
            created_at,
            activated_at,
        )
        .execute(&self.inner)
        .await?;
        Ok(())
    }

    async fn get_config_candidate(
        &self,
        candidate_id: &str,
    ) -> Result<Option<ConfigCandidateRecord>, Self::Error> {
        let row = sqlx::query!(
            r#"SELECT candidate_id AS "candidate_id!", status AS "status!",
                      document AS "document!", message AS "message?",
                      validation AS "validation?", created_at AS "created_at!",
                      activated_at AS "activated_at?"
               FROM config_candidates WHERE candidate_id = $1"#,
            candidate_id,
        )
        .fetch_optional(&self.inner)
        .await?;
        Ok(row.map(|r| ConfigCandidateRecord {
            candidate_id: r.candidate_id,
            status: r.status,
            document: r.document,
            message: r.message,
            validation: r.validation,
            created_at: util::to_systime(r.created_at),
            activated_at: r.activated_at.map(util::to_systime),
        }))
    }

    async fn list_config_candidates(&self) -> Result<Vec<ConfigCandidateRecord>, Self::Error> {
        let rows = sqlx::query!(
            r#"SELECT candidate_id AS "candidate_id!", status AS "status!",
                      document AS "document!", message AS "message?",
                      validation AS "validation?", created_at AS "created_at!",
                      activated_at AS "activated_at?"
               FROM config_candidates ORDER BY created_at DESC"#
        )
        .fetch_all(&self.inner)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| ConfigCandidateRecord {
                candidate_id: r.candidate_id,
                status: r.status,
                document: r.document,
                message: r.message,
                validation: r.validation,
                created_at: util::to_systime(r.created_at),
                activated_at: r.activated_at.map(util::to_systime),
            })
            .collect())
    }

    async fn active_config_candidate(&self) -> Result<Option<ConfigCandidateRecord>, Self::Error> {
        let row = sqlx::query!(
            r#"SELECT candidate_id AS "candidate_id!", status AS "status!",
                      document AS "document!", message AS "message?",
                      validation AS "validation?", created_at AS "created_at!",
                      activated_at AS "activated_at?"
               FROM config_candidates WHERE status = 'activated'
               ORDER BY activated_at DESC LIMIT 1"#
        )
        .fetch_optional(&self.inner)
        .await?;
        Ok(row.map(|r| ConfigCandidateRecord {
            candidate_id: r.candidate_id,
            status: r.status,
            document: r.document,
            message: r.message,
            validation: r.validation,
            created_at: util::to_systime(r.created_at),
            activated_at: r.activated_at.map(util::to_systime),
        }))
    }

    async fn activate_config_candidate(
        &self,
        candidate_id: &str,
        activated_at: SystemTime,
    ) -> Result<(), Self::Error> {
        let ts = util::systime_epoch(activated_at);
        let mut tx = self.inner.begin().await?;
        sqlx::query!(
            "UPDATE config_candidates SET status = 'superseded' WHERE status = 'activated'"
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE config_candidates SET status = 'activated', activated_at = $2 \
             WHERE candidate_id = $1",
            candidate_id,
            ts,
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    // Runtime-checked queries (not the `query!` macro) so the new `server_state`
    // table needs no regeneration of the committed sqlx offline cache.
    async fn get_server_mode(&self) -> Result<Option<String>, Self::Error> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT mode FROM server_state WHERE id = TRUE")
                .fetch_optional(&self.inner)
                .await?;
        Ok(row.map(|(mode,)| mode))
    }

    async fn set_server_mode(&self, mode: &str) -> Result<(), Self::Error> {
        sqlx::query(
            "INSERT INTO server_state (id, mode, updated_at) VALUES (TRUE, $1, now()) \
             ON CONFLICT (id) DO UPDATE SET mode = EXCLUDED.mode, updated_at = now()",
        )
        .bind(mode)
        .execute(&self.inner)
        .await?;
        Ok(())
    }
}

mod util {
    use std::net::Ipv4Addr;

    use crate::State;

    use super::*;
    pub fn systime_epoch(time: SystemTime) -> i64 {
        // / get secs as i64 (for use in sqlite) from epoch to `time`
        time.duration_since(SystemTime::UNIX_EPOCH)
            .expect("failed to get system time")
            .as_secs() as i64
    }

    pub fn to_systime(time: i64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(time as u64)
    }

    pub async fn delete<'a, E>(conn: E, ip: i64) -> Result<(), sqlx::Error>
    where
        E: sqlx::Executor<'a, Database = Postgres>,
    {
        sqlx::query!("DELETE FROM leases WHERE ip = $1", ip)
            .execute(conn)
            .await?;
        Ok(())
    }

    pub async fn release_ip(
        conn: &PgPool,
        ip: i64,
        id: &[u8],
    ) -> Result<Option<ClientInfo>, sqlx::Error> {
        let mut trans = conn.begin().await?;
        let cur = sqlx::query!(
            "SELECT * FROM leases WHERE ip = $1 AND client_id = $2",
            ip,
            id
        )
        .fetch_optional(&mut *trans)
        .await?
        .map(|cur| ClientInfo {
            ip: IpAddr::V4(Ipv4Addr::from(cur.ip as u32)),
            id: cur.client_id.map(|v| v.to_vec()),
            network: IpAddr::V4(Ipv4Addr::from(cur.network as u32)),
            expires_at: to_systime(cur.expires_at),
        });
        // only remove the binding if the (ip, id) pair actually matched, so a
        // client cannot release an address leased to someone else
        if cur.is_some() {
            util::delete(&mut *trans, ip).await?;
        }
        trans.commit().await?;
        // instead of deleting:
        // sqlx::query!(
        //     "UPDATE leases SET leased = false WHERE ip = $1 AND client_id = $2",
        //     ip,
        //     id
        // )
        // .fetch_optional(conn)
        // .await?;
        Ok(cur)
    }

    /// Inserts ip/network/client_id/expires_at into db.
    /// If state is Some, we will insert the leased/probation state too.
    /// if None then we use the default column type
    pub async fn insert<'a, E>(
        conn: E,
        ip: i64,
        network: i64,
        client_id: &[u8],
        expires_at: i64,
        state: Option<(bool, bool)>,
    ) -> Result<(), sqlx::Error>
    where
        E: sqlx::Executor<'a, Database = Postgres>,
    {
        match state {
            Some((leased, probation)) => {
                sqlx::query!(
                    r#"INSERT INTO leases
                    (ip, client_id, expires_at, network, leased, probation)
                VALUES
                    ($1, $2, $3, $4, $5, $6)"#,
                    ip,
                    client_id,
                    expires_at,
                    network,
                    leased,
                    probation
                )
                .execute(conn)
                .await?;
            }
            None => {
                sqlx::query!(
                "INSERT INTO leases (ip, client_id, expires_at, network) VALUES ($1, $2, $3, $4)",
                ip,
                client_id,
                expires_at,
                network,
            )
                .execute(conn)
                .await?;
            }
        }
        Ok(())
    }

    pub async fn find(pool: &PgPool, ip: i64) -> Result<Option<State>, sqlx::Error> {
        Ok(sqlx::query!("SELECT * FROM leases WHERE ip = $1", ip)
            .fetch_optional(pool)
            .await?
            .map(|cur| {
                let info = ClientInfo {
                    ip: IpAddr::V4(Ipv4Addr::from(cur.ip as u32)),
                    id: cur.client_id.map(|v| v.to_vec()),
                    network: IpAddr::V4(Ipv4Addr::from(cur.network as u32)),
                    expires_at: to_systime(cur.expires_at),
                };
                into_clientinfo(info, cur.leased, cur.probation)
            }))
    }

    /// select all values in leases table and return them
    pub async fn select_all(pool: &PgPool) -> Result<Vec<State>, sqlx::Error> {
        Ok(sqlx::query!("SELECT * FROM leases")
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|cur| {
                let info = ClientInfo {
                    ip: IpAddr::V4(Ipv4Addr::from(cur.ip as u32)),
                    id: cur.client_id.map(|v| v.to_vec()),
                    network: IpAddr::V4(Ipv4Addr::from(cur.network as u32)),
                    expires_at: to_systime(cur.expires_at),
                };
                into_clientinfo(info, cur.leased, cur.probation)
            })
            .collect())
    }

    /// return a count of all rows where leased & probation & un-expired
    pub async fn count(
        pool: &PgPool,
        leased: bool,
        probation: bool,
        expires_at: i64,
    ) -> Result<usize, sqlx::Error> {
        Ok(sqlx::query_scalar!(
            r#"SELECT COUNT(ip) as "count_ip!" FROM leases WHERE leased = $1 AND probation = $2 AND expires_at > $3"#,
            leased,
            probation,
            expires_at
        )
        .fetch_one(pool)
        .await? as usize)
    }

    /// return the info for this client_id and if it's un-expired
    pub async fn find_by_id(
        pool: &PgPool,
        id: &[u8],
        now: i64,
    ) -> Result<Option<IpAddr>, sqlx::Error> {
        Ok(sqlx::query!(
            "SELECT ip
            FROM
                leases
            WHERE
                client_id = $1 AND expires_at > $2
            LIMIT 1",
            id,
            now
        )
        .fetch_optional(pool)
        .await?
        .map(|cur| IpAddr::V4(Ipv4Addr::from(cur.ip as u32))))
    }

    /// returns the first expired IP in a range, or where the id matches
    /// expires_at can refer to IPs under probation
    pub async fn update_next_expired<'a, E>(
        conn: E,
        // select
        now: i64,
        id: &[u8],
        start_ip: i64,
        end_ip: i64,
        // update
        expires_at: i64,
        leased: bool,
    ) -> Result<Option<IpAddr>, sqlx::Error>
    where
        E: sqlx::Executor<'a, Database = Postgres>,
    {
        // leased = false -> we got a discover but not yet ACK'd
        // leased = true -> we have ACK'd
        Ok(sqlx::query!(
            r#"
            UPDATE leases
            SET
                client_id = $4, leased = $5, expires_at = $6, probation = FALSE
            WHERE ip in
               (
                   SELECT ip
                    FROM leases
                    WHERE
                        ((expires_at < $1) AND (ip >= $2 AND ip <= $3)) OR (client_id = $4)
                    ORDER BY ip LIMIT 1
                )
            RETURNING ip
            "#,
            now,
            start_ip,
            end_ip,
            id,
            leased,
            expires_at,
        )
        .fetch_optional(conn)
        .await?
        .map(|cur| IpAddr::V4(Ipv4Addr::from(cur.ip as u32))))
    }

    /// updates an entry if the ip & id match and not expired
    pub async fn update_unexpired<'a, E>(
        conn: E,
        ip: i64,
        client_id: &[u8],
        expires_at: i64,
        now: i64,
        leased: bool,
        probation: bool,
        new_id: Option<&[u8]>,
    ) -> Result<Option<IpAddr>, sqlx::Error>
    where
        E: sqlx::Executor<'a, Database = Postgres>,
    {
        Ok(sqlx::query!(
            r#"
            UPDATE leases
            SET
                leased = $4, expires_at = $5, probation = $6, client_id = $7
            WHERE ip in
               (
                    SELECT ip
                    FROM leases
                    WHERE
                        ((expires_at > $1) AND (client_id = $2) AND (ip = $3))
                    ORDER BY ip LIMIT 1
                )
            RETURNING ip
            "#,
            now,
            client_id,
            ip,
            leased,
            expires_at,
            probation,
            new_id
        )
        .fetch_optional(conn)
        .await?
        .map(|cur| IpAddr::V4(Ipv4Addr::from(cur.ip as u32))))
    }

    /// updates an entry if the ip & id match
    /// or if the entry is expired and the ip matches
    pub async fn update_expired<'a, E>(
        conn: E,
        ip: i64,
        client_id: &[u8],
        expires_at: i64,
        now: i64,
        leased: bool,
        probation: bool,
    ) -> Result<Option<IpAddr>, sqlx::Error>
    where
        E: sqlx::Executor<'a, Database = Postgres>,
    {
        Ok(sqlx::query!(
            r#"
            UPDATE leases
            SET
                client_id = $2, leased = $4, expires_at = $5, probation = $6
            WHERE ip in
               (
                    SELECT ip
                    FROM leases
                    WHERE
                        ((client_id = $2 AND ip = $3)
                            OR (expires_at < $1 AND ip = $3))
                    ORDER BY ip LIMIT 1
                )
            RETURNING ip
            "#,
            now,
            client_id,
            ip,
            leased,
            expires_at,
            probation
        )
        .fetch_optional(conn)
        .await?
        .map(|cur| IpAddr::V4(Ipv4Addr::from(cur.ip as u32))))
    }

    /// get the max IP in a given range
    pub async fn max_in_range<'a, E>(
        conn: E,
        start_ip: i64,
        end_ip: i64,
    ) -> Result<Option<State>, sqlx::Error>
    where
        E: sqlx::Executor<'a, Database = Postgres>,
    {
        Ok(sqlx::query!(
            r#"
            SELECT
                *
            FROM
                leases
            WHERE
                ip >= $1 AND ip <= $2
            ORDER BY
                ip DESC
            LIMIT 1
            "#,
            start_ip,
            end_ip
        )
        .fetch_optional(conn)
        .await?
        .map(|cur| {
            let info = ClientInfo {
                ip: IpAddr::V4(Ipv4Addr::from(cur.ip as u32)),
                id: cur.client_id.map(|v| v.to_vec()),
                network: IpAddr::V4(Ipv4Addr::from(cur.network as u32)),
                expires_at: to_systime(cur.expires_at),
            };
            into_clientinfo(info, cur.leased, cur.probation)
        }))
    }

    /// the first address in `[start, end]` that is not excluded. Used when a
    /// range is empty so the excluded start of a range is not handed out.
    pub fn first_available(
        start: IpAddr,
        end: IpAddr,
        exclusions: &HashSet<IpAddr>,
    ) -> Option<IpAddr> {
        match (start, end) {
            (IpAddr::V4(start), IpAddr::V4(end)) => ipnet::Ipv4AddrRange::new(start, end)
                .map(IpAddr::V4)
                .find(|ip| !exclusions.contains(ip)),
            (IpAddr::V6(start), IpAddr::V6(end)) => ipnet::Ipv6AddrRange::new(start, end)
                .map(IpAddr::V6)
                .find(|ip| !exclusions.contains(ip)),
            _ => None,
        }
    }

    /// get the next IP between start and end, skipping any exclusions.
    /// `start` is the current max allocated address, so `nth(1)` returns the
    /// next address after it (both v4 and v6).
    pub fn inc_ip(start: IpAddr, end: IpAddr, exclusions: &HashSet<IpAddr>) -> Option<IpAddr> {
        match (start, end) {
            (IpAddr::V4(ip), IpAddr::V4(end)) => ipnet::Ipv4AddrRange::new(ip, end)
                .map(IpAddr::V4)
                .filter(|ip| !exclusions.contains(ip))
                .nth(1),
            (IpAddr::V6(ip), IpAddr::V6(end)) => ipnet::Ipv6AddrRange::new(ip, end)
                .map(IpAddr::V6)
                .filter(|ip| !exclusions.contains(ip))
                .nth(1),
            _ => None,
        }
    }
    pub(super) fn into_clientinfo(info: ClientInfo, leased: bool, probation: bool) -> State {
        if leased {
            State::Leased(info)
        } else if probation {
            State::Probated(info)
        } else {
            State::Reserved(info)
        }
    }

    pub async fn update_ip<'a, E>(
        conn: E,
        ip: i64,
        client_id: Option<&[u8]>,
        expires_at: i64,
        leased: bool,
        probation: bool,
    ) -> Result<Option<State>, sqlx::Error>
    where
        E: sqlx::Executor<'a, Database = Postgres>,
    {
        Ok(sqlx::query!(
            r#"
            UPDATE leases
            SET
                client_id = $2, expires_at = $3, leased = $4, probation = $5
            WHERE
                ip = $1
            RETURNING *
            "#,
            ip,
            client_id,
            expires_at,
            leased,
            probation
        )
        .fetch_optional(conn)
        .await?
        .map(|cur| {
            let info = ClientInfo {
                ip: IpAddr::V4(Ipv4Addr::from(cur.ip as u32)),
                id: cur.client_id.map(|v| v.to_vec()),
                network: IpAddr::V4(Ipv4Addr::from(cur.network as u32)),
                expires_at: to_systime(cur.expires_at),
            };
            into_clientinfo(info, cur.leased, cur.probation)
        }))
    }
}

/// DHCPv6 storage helpers. Mirror the v4 `util` queries but operate on the
/// `leases_v6` table where the address is a 16-byte BLOB. IA_NA addresses use
/// `prefix_len = 128`; IA_PD prefixes (a later phase) will use other lengths.
/// Because IPv6 octets are big-endian, SQLite's bytewise BLOB comparison
/// matches numeric address ordering, so range queries and ORDER BY work directly.
mod util_v6 {
    use super::util::{into_clientinfo, to_systime};
    use super::*;
    use crate::{ClientInfo, State};

    /// encode an IPv6 address to its 16-byte big-endian BLOB form.
    /// Returns a fixed array (no heap allocation); `&bytes` coerces to `&[u8]`
    /// for the sqlx bindings.
    pub fn to_bytes(ip: Ipv6Addr) -> [u8; 16] {
        ip.octets()
    }

    /// decode a 16-byte BLOB back into an `IpAddr::V6`. Every writer stores
    /// exactly 16 bytes, so a wrong width signals corruption and must fail loudly
    /// rather than silently decode to a bogus address.
    pub fn from_bytes(b: &[u8]) -> IpAddr {
        let octets: [u8; 16] = b
            .try_into()
            .expect("leases_v6 address BLOB must be exactly 16 bytes");
        IpAddr::V6(Ipv6Addr::from(octets))
    }

    // stepping and tri-state mapping are family-neutral; reuse the shared helpers
    pub use super::util::inc_ip;

    fn to_state(
        addr: &[u8],
        client_id: Option<Vec<u8>>,
        network: &[u8],
        expires_at: i64,
        leased: bool,
        probation: bool,
    ) -> State {
        let info = ClientInfo {
            ip: from_bytes(addr),
            id: client_id,
            network: from_bytes(network),
            expires_at: to_systime(expires_at),
        };
        into_clientinfo(info, leased, probation)
    }

    pub async fn insert<'a, E>(
        conn: E,
        addr: &[u8],
        network: &[u8],
        client_id: &[u8],
        expires_at: i64,
        state: Option<(bool, bool)>,
    ) -> Result<(), sqlx::Error>
    where
        E: sqlx::Executor<'a, Database = Postgres>,
    {
        let (leased, probation) = state.unwrap_or((false, false));
        // prefix_len 128: IA_NA is always a full /128 (IA_PD prefixes: later phase)
        sqlx::query!(
            r#"INSERT INTO leases_v6
                (addr, prefix_len, client_id, expires_at, network, leased, probation)
               VALUES ($1, 128, $2, $3, $4, $5, $6)"#,
            addr,
            client_id,
            expires_at,
            network,
            leased,
            probation
        )
        .execute(conn)
        .await?;
        Ok(())
    }

    pub async fn find(pool: &PgPool, addr: &[u8]) -> Result<Option<State>, sqlx::Error> {
        Ok(sqlx::query!(
            "SELECT * FROM leases_v6 WHERE addr = $1 AND prefix_len = 128",
            addr
        )
        .fetch_optional(pool)
        .await?
        .map(|cur| {
            to_state(
                &cur.addr,
                cur.client_id,
                &cur.network,
                cur.expires_at,
                cur.leased,
                cur.probation,
            )
        }))
    }

    pub async fn find_by_id(
        pool: &PgPool,
        id: &[u8],
        now: i64,
    ) -> Result<Option<IpAddr>, sqlx::Error> {
        Ok(sqlx::query!(
            "SELECT addr FROM leases_v6
             WHERE client_id = $1 AND expires_at > $2 AND prefix_len = 128
             LIMIT 1",
            id,
            now
        )
        .fetch_optional(pool)
        .await?
        .map(|cur| from_bytes(&cur.addr)))
    }

    pub async fn delete<'a, E>(conn: E, addr: &[u8]) -> Result<(), sqlx::Error>
    where
        E: sqlx::Executor<'a, Database = Postgres>,
    {
        sqlx::query!(
            "DELETE FROM leases_v6 WHERE addr = $1 AND prefix_len = 128",
            addr
        )
        .execute(conn)
        .await?;
        Ok(())
    }

    pub async fn release_ip(
        pool: &PgPool,
        addr: &[u8],
        id: &[u8],
    ) -> Result<Option<ClientInfo>, sqlx::Error> {
        let mut trans = pool.begin().await?;
        let cur = sqlx::query!(
            "SELECT * FROM leases_v6 WHERE addr = $1 AND client_id = $2 AND prefix_len = 128",
            addr,
            id
        )
        .fetch_optional(&mut *trans)
        .await?
        .map(|cur| ClientInfo {
            ip: from_bytes(&cur.addr),
            id: cur.client_id,
            network: from_bytes(&cur.network),
            expires_at: to_systime(cur.expires_at),
        });
        // only remove the binding if the (addr, id) pair actually matched, so a
        // client cannot release an address leased to another client
        if cur.is_some() {
            delete(&mut *trans, addr).await?;
        }
        trans.commit().await?;
        Ok(cur)
    }

    pub async fn max_in_range<'a, E>(
        conn: E,
        start: &[u8],
        end: &[u8],
    ) -> Result<Option<State>, sqlx::Error>
    where
        E: sqlx::Executor<'a, Database = Postgres>,
    {
        Ok(sqlx::query!(
            r#"SELECT * FROM leases_v6
               WHERE prefix_len = 128 AND addr >= $1 AND addr <= $2
               ORDER BY addr DESC LIMIT 1"#,
            start,
            end
        )
        .fetch_optional(conn)
        .await?
        .map(|cur| {
            to_state(
                &cur.addr,
                cur.client_id,
                &cur.network,
                cur.expires_at,
                cur.leased,
                cur.probation,
            )
        }))
    }

    /// returns the first expired address in a range, or where the id matches
    pub async fn update_next_expired<'a, E>(
        conn: E,
        now: i64,
        id: &[u8],
        start: &[u8],
        end: &[u8],
        expires_at: i64,
        leased: bool,
    ) -> Result<Option<IpAddr>, sqlx::Error>
    where
        E: sqlx::Executor<'a, Database = Postgres>,
    {
        Ok(sqlx::query!(
            r#"
            UPDATE leases_v6
            SET client_id = $4, leased = $5, expires_at = $6, probation = FALSE
            WHERE prefix_len = 128 AND addr IN
               (
                   SELECT addr FROM leases_v6
                   WHERE prefix_len = 128
                     AND (((expires_at < $1) AND (addr >= $2 AND addr <= $3)) OR (client_id = $4))
                   ORDER BY addr LIMIT 1
               )
            RETURNING addr
            "#,
            now,
            start,
            end,
            id,
            leased,
            expires_at,
        )
        .fetch_optional(conn)
        .await?
        .map(|cur| from_bytes(&cur.addr)))
    }

    /// updates an entry if the addr & id match and not expired
    #[allow(clippy::too_many_arguments)]
    pub async fn update_unexpired<'a, E>(
        conn: E,
        addr: &[u8],
        client_id: &[u8],
        expires_at: i64,
        now: i64,
        leased: bool,
        probation: bool,
        new_id: Option<&[u8]>,
    ) -> Result<Option<IpAddr>, sqlx::Error>
    where
        E: sqlx::Executor<'a, Database = Postgres>,
    {
        Ok(sqlx::query!(
            r#"
            UPDATE leases_v6
            SET leased = $4, expires_at = $5, probation = $6, client_id = $7
            WHERE prefix_len = 128 AND addr IN
               (
                    SELECT addr FROM leases_v6
                    WHERE prefix_len = 128
                      AND ((expires_at > $1) AND (client_id = $2) AND (addr = $3))
                    ORDER BY addr LIMIT 1
               )
            RETURNING addr
            "#,
            now,
            client_id,
            addr,
            leased,
            expires_at,
            probation,
            new_id
        )
        .fetch_optional(conn)
        .await?
        .map(|cur| from_bytes(&cur.addr)))
    }

    /// updates an entry if the addr & id match, or if expired and addr matches
    pub async fn update_expired<'a, E>(
        conn: E,
        addr: &[u8],
        client_id: &[u8],
        expires_at: i64,
        now: i64,
        leased: bool,
        probation: bool,
    ) -> Result<Option<IpAddr>, sqlx::Error>
    where
        E: sqlx::Executor<'a, Database = Postgres>,
    {
        Ok(sqlx::query!(
            r#"
            UPDATE leases_v6
            SET client_id = $2, leased = $4, expires_at = $5, probation = $6
            WHERE prefix_len = 128 AND addr IN
               (
                    SELECT addr FROM leases_v6
                    WHERE prefix_len = 128
                      AND ((client_id = $2 AND addr = $3) OR (expires_at < $1 AND addr = $3))
                    ORDER BY addr LIMIT 1
               )
            RETURNING addr
            "#,
            now,
            client_id,
            addr,
            leased,
            expires_at,
            probation
        )
        .fetch_optional(conn)
        .await?
        .map(|cur| from_bytes(&cur.addr)))
    }

    pub async fn update_ip<'a, E>(
        conn: E,
        addr: &[u8],
        client_id: Option<&[u8]>,
        expires_at: i64,
        leased: bool,
        probation: bool,
    ) -> Result<Option<State>, sqlx::Error>
    where
        E: sqlx::Executor<'a, Database = Postgres>,
    {
        Ok(sqlx::query!(
            r#"
            UPDATE leases_v6
            SET client_id = $2, expires_at = $3, leased = $4, probation = $5
            WHERE addr = $1 AND prefix_len = 128
            RETURNING *
            "#,
            addr,
            client_id,
            expires_at,
            leased,
            probation
        )
        .fetch_optional(conn)
        .await?
        .map(|cur| {
            to_state(
                &cur.addr,
                cur.client_id,
                &cur.network,
                cur.expires_at,
                cur.leased,
                cur.probation,
            )
        }))
    }

    // ---- IA_PD (prefix delegation) ------------------------------------------
    // Delegated prefixes live in the same table, keyed by (addr = prefix base,
    // prefix_len = delegated length != 128). These mirror the IA_NA helpers but
    // take the prefix length as a parameter.

    /// find a delegated-prefix binding by its base and length.
    pub async fn find_pd(
        pool: &PgPool,
        addr: &[u8],
        prefix_len: i64,
    ) -> Result<Option<State>, sqlx::Error> {
        Ok(sqlx::query!(
            "SELECT * FROM leases_v6 WHERE addr = $1 AND prefix_len = $2",
            addr,
            prefix_len
        )
        .fetch_optional(pool)
        .await?
        .map(|cur| {
            to_state(
                &cur.addr,
                cur.client_id,
                &cur.network,
                cur.expires_at,
                cur.leased,
                cur.probation,
            )
        }))
    }

    /// insert or replace a delegated-prefix binding. The caller must have already
    /// checked the prefix is free / expired / owned by this client.
    pub async fn upsert_pd(
        pool: &PgPool,
        addr: &[u8],
        prefix_len: i64,
        network: &[u8],
        client_id: &[u8],
        expires_at: i64,
        state: Option<(bool, bool)>,
    ) -> Result<(), sqlx::Error> {
        let (leased, probation) = state.unwrap_or((false, false));
        sqlx::query!(
            r#"INSERT INTO leases_v6
                (addr, prefix_len, client_id, expires_at, network, leased, probation)
               VALUES ($1, $2, $3, $4, $5, $6, $7)
               ON CONFLICT (addr, prefix_len) DO UPDATE SET
                 client_id = EXCLUDED.client_id, expires_at = EXCLUDED.expires_at,
                 network = EXCLUDED.network, leased = EXCLUDED.leased,
                 probation = EXCLUDED.probation"#,
            addr,
            prefix_len,
            client_id,
            expires_at,
            network,
            leased,
            probation
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    /// look up a client's delegated prefix (base + length) by identity. Never
    /// returns IA_NA rows (prefix_len 128).
    pub async fn find_by_id_pd(
        pool: &PgPool,
        id: &[u8],
        now: i64,
    ) -> Result<Option<(IpAddr, u8)>, sqlx::Error> {
        Ok(sqlx::query!(
            "SELECT addr, prefix_len FROM leases_v6
             WHERE client_id = $1 AND expires_at > $2 AND prefix_len != 128
             LIMIT 1",
            id,
            now
        )
        .fetch_optional(pool)
        .await?
        .map(|cur| (from_bytes(&cur.addr), cur.prefix_len as u8)))
    }

    /// extend an existing, unexpired delegated prefix if the id matches.
    pub async fn renew_pd(
        pool: &PgPool,
        addr: &[u8],
        prefix_len: i64,
        client_id: &[u8],
        expires_at: i64,
        now: i64,
    ) -> Result<Option<IpAddr>, sqlx::Error> {
        Ok(sqlx::query!(
            r#"UPDATE leases_v6 SET leased = TRUE, expires_at = $4, probation = FALSE
               WHERE addr = $1 AND prefix_len = $2 AND client_id = $3 AND expires_at > $5
               RETURNING addr"#,
            addr,
            prefix_len,
            client_id,
            expires_at,
            now
        )
        .fetch_optional(pool)
        .await?
        .map(|cur| from_bytes(&cur.addr)))
    }

    /// release a delegated prefix if the (addr, len, id) all match.
    pub async fn release_pd(
        pool: &PgPool,
        addr: &[u8],
        prefix_len: i64,
        id: &[u8],
    ) -> Result<Option<ClientInfo>, sqlx::Error> {
        let mut trans = pool.begin().await?;
        let cur = sqlx::query!(
            "SELECT * FROM leases_v6 WHERE addr = $1 AND prefix_len = $2 AND client_id = $3",
            addr,
            prefix_len,
            id
        )
        .fetch_optional(&mut *trans)
        .await?
        .map(|cur| ClientInfo {
            ip: from_bytes(&cur.addr),
            id: cur.client_id,
            network: from_bytes(&cur.network),
            expires_at: to_systime(cur.expires_at),
        });
        if cur.is_some() {
            sqlx::query!(
                "DELETE FROM leases_v6 WHERE addr = $1 AND prefix_len = $2",
                addr,
                prefix_len
            )
            .execute(&mut *trans)
            .await?;
        }
        trans.commit().await?;
        Ok(cur)
    }

    /// count leases_v6 rows in the given state that are un-expired
    pub async fn count(
        pool: &PgPool,
        leased: bool,
        probation: bool,
        now: i64,
    ) -> Result<usize, sqlx::Error> {
        Ok(sqlx::query_scalar!(
            r#"SELECT COUNT(addr) as "count_addr!" FROM leases_v6 WHERE leased = $1 AND probation = $2 AND expires_at > $3"#,
            leased,
            probation,
            now
        )
        .fetch_one(pool)
        .await? as usize)
    }

    /// all leases_v6 bindings (IA_NA addresses and IA_PD prefixes)
    pub async fn select_all(pool: &PgPool) -> Result<Vec<State>, sqlx::Error> {
        Ok(sqlx::query!("SELECT * FROM leases_v6")
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|cur| {
                to_state(
                    &cur.addr,
                    cur.client_id,
                    &cur.network,
                    cur.expires_at,
                    cur.leased,
                    cur.probation,
                )
            })
            .collect())
    }
}

#[cfg(test)]
mod v6_tests {
    use std::collections::HashSet;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::time::{Duration, SystemTime};

    use super::PostgresDb;
    use crate::{IpState, State, Storage};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn v6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse::<Ipv6Addr>().unwrap())
    }

    /// mimics IpManager::reserve_first's two-step: try next_expired (or id
    /// match), else insert the next available address in the range.
    async fn alloc(
        db: &PostgresDb,
        range: &std::ops::RangeInclusive<IpAddr>,
        excl: &HashSet<IpAddr>,
        network: IpAddr,
        id: &[u8],
        expires_at: SystemTime,
    ) -> Result<IpAddr, sqlx::Error> {
        if let Some(ip) = db
            .next_expired(
                range.clone(),
                network,
                id,
                expires_at,
                Some(IpState::Reserve),
            )
            .await?
        {
            return Ok(ip);
        }
        Ok(db
            .insert_max_in_range(
                range.clone(),
                excl,
                network,
                id,
                expires_at,
                Some(IpState::Reserve),
            )
            .await?
            .expect("range should have an available address"))
    }

    #[tokio::test]
    async fn v6_insert_get_id_release() -> TestResult {
        let db = PostgresDb::new_test().await?;
        let addr = v6("2001:db8:1::100");
        let net = v6("2001:db8:1::");
        let id: &[u8] = &[0xaa, 0xbb, 0xcc];
        let exp = SystemTime::now() + Duration::from_secs(60);

        db.insert(addr, net, id, exp, Some(IpState::Lease)).await?;

        match db.get(addr).await? {
            Some(State::Leased(info)) => {
                assert_eq!(info.ip(), addr);
                assert_eq!(info.id(), Some(id));
                assert_eq!(info.network(), net);
            }
            other => panic!("expected Leased, got {other:?}"),
        }
        assert_eq!(db.get_id_v6(id).await?, Some(addr));

        // a release carrying the wrong id must NOT delete another client's lease
        let wrong = db.release_ip(addr, &[0xde, 0xad]).await?;
        assert!(wrong.is_none(), "wrong-id release returns no info");
        assert!(
            db.get(addr).await?.is_some(),
            "lease must survive wrong-id release"
        );

        let released = db.release_ip(addr, id).await?;
        assert!(released.is_some(), "release should return prior info");
        assert!(db.get(addr).await?.is_none(), "entry should be gone");
        Ok(())
    }

    #[tokio::test]
    async fn v6_next_available_sequential_honors_exclusions() -> TestResult {
        let db = PostgresDb::new_test().await?;
        let net = v6("2001:db8:1::");
        let range = v6("2001:db8:1::100")..=v6("2001:db8:1::110");
        let exp = SystemTime::now() + Duration::from_secs(60);
        let mut excl = HashSet::new();
        excl.insert(v6("2001:db8:1::101"));

        // empty range -> start of range
        assert_eq!(
            alloc(&db, &range, &excl, net, &[1], exp).await?,
            v6("2001:db8:1::100")
        );
        // ::101 excluded -> ::102
        assert_eq!(
            alloc(&db, &range, &excl, net, &[2], exp).await?,
            v6("2001:db8:1::102")
        );
        // then ::103
        assert_eq!(
            alloc(&db, &range, &excl, net, &[3], exp).await?,
            v6("2001:db8:1::103")
        );

        // same id returns its existing address (idempotent via next_expired id-match)
        assert_eq!(
            alloc(&db, &range, &excl, net, &[2], exp).await?,
            v6("2001:db8:1::102")
        );
        Ok(())
    }

    #[tokio::test]
    async fn v6_empty_range_skips_excluded_start() -> TestResult {
        // regression: an empty range whose first address is excluded must not
        // hand out that excluded address (which would then be rejected and loop).
        let db = PostgresDb::new_test().await?;
        let net = v6("2001:db8:1::");
        let range = v6("2001:db8:1::100")..=v6("2001:db8:1::110");
        let exp = SystemTime::now() + Duration::from_secs(60);
        let mut excl = HashSet::new();
        excl.insert(v6("2001:db8:1::100")); // exclude the start

        assert_eq!(
            alloc(&db, &range, &excl, net, &[1], exp).await?,
            v6("2001:db8:1::101")
        );
        Ok(())
    }

    #[tokio::test]
    async fn v6_reserve_then_lease_via_update_unexpired() -> TestResult {
        let db = PostgresDb::new_test().await?;
        let addr = v6("2001:db8:1::100");
        let net = v6("2001:db8:1::");
        let id: &[u8] = &[9, 9, 9];
        let exp = SystemTime::now() + Duration::from_secs(60);

        // reserve (offer), then transition to lease (request/reply)
        db.insert(addr, net, id, exp, Some(IpState::Reserve))
            .await?;
        let leased = db
            .update_unexpired(addr, IpState::Lease, id, exp, Some(id))
            .await?;
        assert_eq!(leased, Some(addr));
        assert!(matches!(db.get(addr).await?, Some(State::Leased(_))));
        Ok(())
    }

    #[tokio::test]
    async fn v4_and_v6_coexist_in_separate_tables() -> TestResult {
        let db = PostgresDb::new_test().await?;
        let v4 = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50));
        let v4net = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 0));
        let a6 = v6("2001:db8:1::100");
        let n6 = v6("2001:db8:1::");
        let exp = SystemTime::now() + Duration::from_secs(60);

        db.insert(v4, v4net, &[1, 2, 3], exp, Some(IpState::Lease))
            .await?;
        db.insert(a6, n6, &[4, 5, 6], exp, Some(IpState::Lease))
            .await?;

        assert!(matches!(db.get(v4).await?, Some(State::Leased(i)) if i.ip() == v4));
        assert!(matches!(db.get(a6).await?, Some(State::Leased(i)) if i.ip() == a6));
        assert_eq!(db.get_id_v6(&[4, 5, 6]).await?, Some(a6));
        assert_eq!(db.get_id(&[1, 2, 3]).await?, Some(v4));
        // family lookups do not cross tables: v6 id absent from v4 table and vice versa
        assert_eq!(db.get_id(&[4, 5, 6]).await?, None);
        assert_eq!(db.get_id_v6(&[1, 2, 3]).await?, None);

        // select_all and count(Lease) include both families
        assert_eq!(db.select_all().await?.len(), 2);
        assert_eq!(db.count(IpState::Lease).await?, 2);
        Ok(())
    }
}
