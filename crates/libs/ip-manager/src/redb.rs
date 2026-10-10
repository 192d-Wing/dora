//! Embedded [`redb`] implementation of [`Storage`] for the single-process
//! `dora-embedded` binary.
//!
//! One process owns the database file (redb takes an exclusive file lock), so
//! the only concurrency is between tokio tasks inside that process. Every
//! mutation is one serialized redb write transaction, which is what gives the
//! allocator its atomicity: where the Postgres backend relies on
//! `UPDATE … WHERE … RETURNING` and `ON CONFLICT`, this backend does the same
//! read-modify-write inside a single write transaction.
//!
//! Layout (all values are `postcard`-encoded records, see the `*Rec` structs):
//!
//! | table                  | key                               | value          |
//! |------------------------|-----------------------------------|----------------|
//! | `meta`                 | `"schema_version"`                | `u32`          |
//! | `leases_v4`            | `u32` host-order address          | [`LeaseRec`]   |
//! | `leases_v6_na`         | `[u8; 16]` IA_NA address          | [`LeaseRec`]   |
//! | `leases_v6_pd`         | `([u8; 16], u8)` prefix + length  | [`LeaseRec`]   |
//! | `operations`           | operation id                      | [`OperationRec`] |
//! | `runtime_reservations` | `(family, ip)`                    | [`ReservationRec`] |
//! | `config_candidates`    | candidate id                      | [`CandidateRec`] |
//! | `server_state`         | `"mode"`                          | mode string    |
//!
//! redb orders integer keys numerically and byte arrays lexicographically, so
//! the range scans the allocator needs (lowest expired address, highest
//! allocated address) are plain key ranges. IA_NA and IA_PD bindings live in
//! separate tables, which replaces the Postgres `prefix_len = 128` filters.
//!
//! redb calls are blocking, so every operation runs on the tokio blocking pool
//! (`spawn_blocking`). Writes additionally take an async mutex first, so at
//! most one write transaction occupies a blocking thread while the rest of the
//! waiters stay cheap futures.

use std::{
    borrow::Borrow,
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    ops::RangeInclusive,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use redb::{
    Database, Durability, Key, ReadTransaction, ReadableDatabase, ReadableTable, Table,
    TableDefinition, WriteTransaction,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tracing::{debug, trace, warn};

use crate::{
    ClientInfo, ConfigCandidateRecord, IpState, OperationRecord, OperationStatus,
    RuntimeReservationRecord, State, Storage,
    common::{first_available, inc_ip, into_clientinfo, systime_epoch, to_systime},
};

/// Bump when the encoding of any table changes; add an entry to [`UPGRADES`].
const SCHEMA_VERSION: u32 = 1;

const META: TableDefinition<&str, u32> = TableDefinition::new("meta");
const LEASES_V4: TableDefinition<u32, &[u8]> = TableDefinition::new("leases_v4");
const LEASES_V6: TableDefinition<[u8; 16], &[u8]> = TableDefinition::new("leases_v6_na");
const LEASES_PD: TableDefinition<([u8; 16], u8), &[u8]> = TableDefinition::new("leases_v6_pd");
const OPERATIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("operations");
const RESERVATIONS: TableDefinition<(&str, &str), &[u8]> =
    TableDefinition::new("runtime_reservations");
const CONFIG_CANDIDATES: TableDefinition<&str, &[u8]> = TableDefinition::new("config_candidates");
const SERVER_STATE: TableDefinition<&str, &str> = TableDefinition::new("server_state");

const SCHEMA_VERSION_KEY: &str = "schema_version";
const MODE_KEY: &str = "mode";
const IA_NA_PREFIX_LEN: u8 = 128;

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum RedbError {
    #[error("redb database error")]
    Database(#[from] redb::DatabaseError),
    #[error("redb transaction error")]
    Transaction(#[from] redb::TransactionError),
    #[error("redb table error")]
    Table(#[from] redb::TableError),
    #[error("redb storage error")]
    Storage(#[from] redb::StorageError),
    #[error("redb commit error")]
    Commit(#[from] redb::CommitError),
    #[error("redb durability error")]
    Durability(#[from] redb::SetDurabilityError),
    #[error("failed to encode record")]
    Encode(#[source] postcard::Error),
    #[error("failed to decode record from table `{table}`")]
    Decode {
        table: &'static str,
        #[source]
        source: postcard::Error,
    },
    #[error("unknown operation status {0:?} in table `operations`")]
    InvalidStatus(String),
    #[error("malformed address of {0} bytes in a stored record")]
    InvalidAddress(usize),
    #[error("key already exists: {0}")]
    AlreadyExists(String),
    #[error("database schema version {found} is newer than this build supports ({supported})")]
    SchemaTooNew { found: u32, supported: u32 },
    #[error("storage task panicked or was cancelled")]
    Poisoned(#[from] tokio::task::JoinError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

type Result<T, E = RedbError> = std::result::Result<T, E>;

// ---------------------------------------------------------------------------
// options
// ---------------------------------------------------------------------------

/// How commits reach the disk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SyncMode {
    /// Every commit is fsynced before it returns: a lease is on disk before the
    /// ACK goes out. Costs one fsync per DHCP write (2–3 per DORA exchange).
    #[default]
    Immediate,
    /// Commits are not fsynced; a background tick issues a durable commit every
    /// [`RedbOptions::flush_interval`]. Bounds loss after a crash to that
    /// window, during which a client may hold an address the server forgot.
    Lazy,
}

impl std::str::FromStr for SyncMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "immediate" => Ok(SyncMode::Immediate),
            "lazy" => Ok(SyncMode::Lazy),
            other => Err(format!(
                "unknown sync mode `{other}` (expected immediate|lazy)"
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RedbOptions {
    pub sync: SyncMode,
    /// Flush period for [`SyncMode::Lazy`].
    pub flush_interval: Duration,
    /// redb page cache. redb's own default is 1 GiB, far too much for an
    /// embedded box.
    pub cache_size_bytes: usize,
    /// Enable redb's two-phase commit (slower, survives more exotic storage
    /// failures).
    pub two_phase_commit: bool,
}

impl Default for RedbOptions {
    fn default() -> Self {
        Self {
            sync: SyncMode::Immediate,
            flush_interval: Duration::from_secs(1),
            cache_size_bytes: 64 * 1024 * 1024,
            two_phase_commit: false,
        }
    }
}

// ---------------------------------------------------------------------------
// records
// ---------------------------------------------------------------------------

/// Value of every lease table. The key carries the address (and prefix
/// length); `network` is 4 or 16 bytes depending on the family.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LeaseRec {
    client_id: Option<Vec<u8>>,
    network: Vec<u8>,
    expires_at: i64,
    leased: bool,
    probation: bool,
}

impl LeaseRec {
    fn is_expired(&self, now: i64) -> bool {
        self.expires_at < now
    }
    fn is_live(&self, now: i64) -> bool {
        self.expires_at > now
    }
    fn owned_by(&self, id: &[u8]) -> bool {
        self.client_id.as_deref() == Some(id)
    }
    fn set_state(&mut self, state: Option<IpState>) {
        let (leased, probation) = flags(state);
        self.leased = leased;
        self.probation = probation;
    }
    fn into_state(self, ip: IpAddr) -> Result<State> {
        let network = ip_from_bytes(&self.network)?;
        Ok(into_clientinfo(
            ClientInfo {
                ip,
                id: self.client_id,
                network,
                expires_at: to_systime(self.expires_at),
            },
            self.leased,
            self.probation,
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OperationRec {
    action: String,
    status: String,
    actor: Option<String>,
    input_summary: Option<String>,
    result: Option<String>,
    error_code: Option<String>,
    error_message: Option<String>,
    created_at: i64,
    started_at: Option<i64>,
    completed_at: Option<i64>,
}

impl OperationRec {
    fn from_record(op: &OperationRecord) -> Self {
        Self {
            action: op.action.clone(),
            status: op.status.as_str().to_owned(),
            actor: op.actor.clone(),
            input_summary: op.input_summary.clone(),
            result: op.result.clone(),
            error_code: op.error_code.clone(),
            error_message: op.error_message.clone(),
            created_at: systime_epoch(op.created_at),
            started_at: op.started_at.map(systime_epoch),
            completed_at: op.completed_at.map(systime_epoch),
        }
    }
    fn into_record(self, operation_id: String) -> Result<OperationRecord> {
        let status = OperationStatus::from_db_str(&self.status)
            .ok_or_else(|| RedbError::InvalidStatus(self.status.clone()))?;
        Ok(OperationRecord {
            operation_id,
            action: self.action,
            status,
            actor: self.actor,
            input_summary: self.input_summary,
            result: self.result,
            error_code: self.error_code,
            error_message: self.error_message,
            created_at: to_systime(self.created_at),
            started_at: self.started_at.map(to_systime),
            completed_at: self.completed_at.map(to_systime),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReservationRec {
    prefix: Option<String>,
    network: Option<String>,
    match_json: String,
    options_json: Option<String>,
    class: Option<String>,
    lease_time: Option<i64>,
    created_at: i64,
}

impl ReservationRec {
    fn from_record(r: &RuntimeReservationRecord) -> Self {
        Self {
            prefix: r.prefix.clone(),
            network: r.network.clone(),
            match_json: r.match_json.clone(),
            options_json: r.options_json.clone(),
            class: r.class.clone(),
            lease_time: r.lease_time,
            created_at: systime_epoch(r.created_at),
        }
    }
    fn into_record(self, family: String, ip: String) -> RuntimeReservationRecord {
        RuntimeReservationRecord {
            family,
            ip,
            prefix: self.prefix,
            network: self.network,
            match_json: self.match_json,
            options_json: self.options_json,
            class: self.class,
            lease_time: self.lease_time,
            created_at: to_systime(self.created_at),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CandidateRec {
    status: String,
    document: String,
    message: Option<String>,
    validation: Option<String>,
    created_at: i64,
    activated_at: Option<i64>,
}

impl CandidateRec {
    fn from_record(c: &ConfigCandidateRecord) -> Self {
        Self {
            status: c.status.clone(),
            document: c.document.clone(),
            message: c.message.clone(),
            validation: c.validation.clone(),
            created_at: systime_epoch(c.created_at),
            activated_at: c.activated_at.map(systime_epoch),
        }
    }
    fn into_record(self, candidate_id: String) -> ConfigCandidateRecord {
        ConfigCandidateRecord {
            candidate_id,
            status: self.status,
            document: self.document,
            message: self.message,
            validation: self.validation,
            created_at: to_systime(self.created_at),
            activated_at: self.activated_at.map(to_systime),
        }
    }
}

// ---------------------------------------------------------------------------
// encoding helpers
// ---------------------------------------------------------------------------

fn encode<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    postcard::to_allocvec(v).map_err(RedbError::Encode)
}

fn decode<T: DeserializeOwned>(table: &'static str, bytes: &[u8]) -> Result<T> {
    postcard::from_bytes(bytes).map_err(|source| RedbError::Decode { table, source })
}

/// `(leased, probation)` for a state; `None` means Reserve.
fn flags(state: Option<IpState>) -> (bool, bool) {
    state.unwrap_or(IpState::Reserve).into()
}

fn now_epoch() -> i64 {
    systime_epoch(SystemTime::now())
}

fn ip_to_bytes(ip: IpAddr) -> Vec<u8> {
    match ip {
        IpAddr::V4(a) => a.octets().to_vec(),
        IpAddr::V6(a) => a.octets().to_vec(),
    }
}

fn ip_from_bytes(b: &[u8]) -> Result<IpAddr> {
    match b.len() {
        4 => Ok(IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(b).unwrap()))),
        16 => Ok(IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(b).unwrap()))),
        n => Err(RedbError::InvalidAddress(n)),
    }
}

fn v6_octets(ip: IpAddr) -> Option<[u8; 16]> {
    match ip {
        IpAddr::V6(a) => Some(a.octets()),
        IpAddr::V4(_) => None,
    }
}

/// The two single-address lease tables (v4, v6 IA_NA) share every allocator
/// operation; this trait is what lets those operations be written once.
trait AddrKey: Key + 'static + Sized {
    const TABLE: TableDefinition<'static, Self, &'static [u8]>;
    /// Caller guarantees `ip` is of this family (`Storage` panics on a mixed
    /// family, like the Postgres backend).
    fn key_of(ip: IpAddr) -> Self::SelfType<'static>;
    fn ip_of(key: Self::SelfType<'_>) -> IpAddr;
}

impl AddrKey for u32 {
    const TABLE: TableDefinition<'static, Self, &'static [u8]> = LEASES_V4;
    fn key_of(ip: IpAddr) -> u32 {
        match ip {
            IpAddr::V4(a) => u32::from(a),
            IpAddr::V6(_) => panic!("v6 address in the v4 lease table"),
        }
    }
    fn ip_of(key: u32) -> IpAddr {
        IpAddr::V4(Ipv4Addr::from(key))
    }
}

impl AddrKey for [u8; 16] {
    const TABLE: TableDefinition<'static, Self, &'static [u8]> = LEASES_V6;
    fn key_of(ip: IpAddr) -> [u8; 16] {
        match ip {
            IpAddr::V6(a) => a.octets(),
            IpAddr::V4(_) => panic!("v4 address in the v6 lease table"),
        }
    }
    fn ip_of(key: [u8; 16]) -> IpAddr {
        IpAddr::V6(Ipv6Addr::from(key))
    }
}

/// Run `$body` with `K` bound to the lease-table key type of `$ip`'s family.
macro_rules! by_family {
    ($ip:expr, |$K:ident| $body:expr) => {
        match $ip {
            IpAddr::V4(_) => {
                type $K = u32;
                $body
            }
            IpAddr::V6(_) => {
                type $K = [u8; 16];
                $body
            }
        }
    };
}

fn read_rec<'k, K, T>(table: &T, key: impl Borrow<K::SelfType<'k>>) -> Result<Option<LeaseRec>>
where
    K: Key + 'static,
    T: ReadableTable<K, &'static [u8]>,
{
    match table.get(key)? {
        Some(guard) => Ok(Some(decode("leases", guard.value())?)),
        None => Ok(None),
    }
}

fn write_rec<'k, K: Key + 'static>(
    table: &mut Table<'_, K, &'static [u8]>,
    key: impl Borrow<K::SelfType<'k>>,
    rec: &LeaseRec,
) -> Result<()> {
    let bytes = encode(rec)?;
    table.insert(key, bytes.as_slice())?;
    Ok(())
}

/// Outcome of a write closure: whether to commit (fsync) or abort.
enum Txn<R> {
    Commit(R),
    /// Nothing changed; skip the commit (and its fsync).
    Abort(R),
}

// ---------------------------------------------------------------------------
// the store
// ---------------------------------------------------------------------------

/// An embedded redb-backed [`Storage`]. Cheap to clone (shared handle).
#[derive(Clone, Debug)]
pub struct RedbDb {
    inner: Arc<Inner>,
}

struct Inner {
    // declared before `_tmp` so the file is closed before the directory goes
    db: Database,
    write_lock: tokio::sync::Mutex<()>,
    durability: Durability,
    two_phase_commit: bool,
    path: PathBuf,
    _tmp: Option<tempfile::TempDir>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedbDb")
            .field("path", &self.path)
            .field("durability", &self.durability)
            .finish_non_exhaustive()
    }
}

impl RedbDb {
    /// Open (or create) the database file at `path` and apply any pending
    /// schema upgrades. Blocking: call it during startup (or from
    /// `spawn_blocking`). The parent directory must exist. Fails if another
    /// process holds the file.
    ///
    /// With [`SyncMode::Lazy`] a periodic flusher is spawned onto the current
    /// tokio runtime if there is one.
    pub fn open(path: impl AsRef<Path>, opts: RedbOptions) -> Result<Self> {
        Self::open_inner(path.as_ref().to_path_buf(), opts, None)
    }

    /// Open a store from a `redb:///absolute/path` URL (see
    /// [`crate::url::StorageUrl`]).
    pub fn open_url(url: &str, opts: RedbOptions) -> Result<Self> {
        let parsed: crate::url::StorageUrl = url
            .parse()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{e}")))?;
        let path = parsed
            .into_redb()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{e}")))?;
        Self::open(path, opts)
    }

    /// A fresh store in a temporary directory that lives as long as the store.
    /// Lazy sync so tests do not fsync on every commit.
    #[doc(hidden)]
    pub fn new_test() -> Result<Self> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("dora-test.redb");
        Self::open_inner(
            path,
            RedbOptions {
                sync: SyncMode::Lazy,
                flush_interval: Duration::from_secs(3600),
                ..RedbOptions::default()
            },
            Some(tmp),
        )
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    fn open_inner(
        path: PathBuf,
        opts: RedbOptions,
        tmp: Option<tempfile::TempDir>,
    ) -> Result<Self> {
        debug!(path = %path.display(), ?opts, "opening redb store");
        let db = Database::builder()
            .set_cache_size(opts.cache_size_bytes)
            .create(&path)?;
        {
            let txn = db.begin_write()?;
            migrate(&txn)?;
            txn.commit()?;
        }
        let durability = match opts.sync {
            SyncMode::Immediate => Durability::Immediate,
            SyncMode::Lazy => Durability::None,
        };
        let inner = Arc::new(Inner {
            db,
            write_lock: tokio::sync::Mutex::new(()),
            durability,
            two_phase_commit: opts.two_phase_commit,
            path,
            _tmp: tmp,
        });
        if opts.sync == SyncMode::Lazy {
            spawn_flusher(Arc::downgrade(&inner), opts.flush_interval);
        }
        Ok(Self { inner })
    }

    async fn with_read<R, F>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&ReadTransaction) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let txn = inner.db.begin_read()?;
            f(&txn)
        })
        .await?
    }

    async fn with_write<R, F>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&WriteTransaction) -> Result<Txn<R>> + Send + 'static,
        R: Send + 'static,
    {
        self.with_write_durability(self.inner.durability, f).await
    }

    async fn with_write_durability<R, F>(&self, durability: Durability, f: F) -> Result<R>
    where
        F: FnOnce(&WriteTransaction) -> Result<Txn<R>> + Send + 'static,
        R: Send + 'static,
    {
        // held across the blocking call: at most one write txn in the pool,
        // every other writer waits as a cheap future (FIFO)
        let _serialize = self.inner.write_lock.lock().await;
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut txn = inner.db.begin_write()?;
            txn.set_durability(durability)?;
            txn.set_two_phase_commit(inner.two_phase_commit);
            match f(&txn)? {
                Txn::Commit(r) => {
                    txn.commit()?;
                    Ok(r)
                }
                Txn::Abort(r) => {
                    txn.abort()?;
                    Ok(r)
                }
            }
            // an `Err` from `f` drops the transaction, which aborts it
        })
        .await?
    }
}

/// Periodically issue an empty durable commit so `Durability::None` commits
/// reach the disk. Stops once the store is dropped.
fn spawn_flusher(inner: Weak<Inner>, every: Duration) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        warn!("redb lazy sync requested outside a tokio runtime; commits will not be flushed");
        return;
    };
    handle.spawn(async move {
        let mut ticker = tokio::time::interval(every);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let Some(inner) = inner.upgrade() else { break };
            let db = RedbDb { inner };
            if let Err(err) = db
                .with_write_durability(Durability::Immediate, |_| Ok(Txn::Commit(())))
                .await
            {
                warn!(?err, "redb flush failed");
            }
            trace!("redb flushed");
        }
    });
}

// ---------------------------------------------------------------------------
// schema
// ---------------------------------------------------------------------------

type Upgrade = fn(&WriteTransaction) -> Result<()>;

/// `UPGRADES[n]` migrates a store at schema version `n` to `n + 1`.
const UPGRADES: [Upgrade; SCHEMA_VERSION as usize] = [create_v1];

fn migrate(txn: &WriteTransaction) -> Result<()> {
    let found = txn
        .open_table(META)?
        .get(SCHEMA_VERSION_KEY)?
        .map(|g| g.value())
        .unwrap_or(0);
    if found > SCHEMA_VERSION {
        return Err(RedbError::SchemaTooNew {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    for (from, upgrade) in UPGRADES.iter().enumerate().skip(found as usize) {
        debug!(from, to = from + 1, "upgrading redb schema");
        upgrade(txn)?;
    }
    txn.open_table(META)?
        .insert(SCHEMA_VERSION_KEY, SCHEMA_VERSION)?;
    Ok(())
}

/// 0 → 1: create every table (so reads never hit `TableDoesNotExist`) and seed
/// the server mode, like the Postgres `server_state` migration does.
fn create_v1(txn: &WriteTransaction) -> Result<()> {
    txn.open_table(LEASES_V4)?;
    txn.open_table(LEASES_V6)?;
    txn.open_table(LEASES_PD)?;
    txn.open_table(OPERATIONS)?;
    txn.open_table(RESERVATIONS)?;
    txn.open_table(CONFIG_CANDIDATES)?;
    let mut state = txn.open_table(SERVER_STATE)?;
    if state.get(MODE_KEY)?.is_none() {
        state.insert(MODE_KEY, "normal")?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// family-generic lease operations (v4 and v6 IA_NA)
// ---------------------------------------------------------------------------

fn get_in<K: AddrKey>(txn: &ReadTransaction, ip: IpAddr) -> Result<Option<State>> {
    let table = txn.open_table(K::TABLE)?;
    match read_rec::<K, _>(&table, K::key_of(ip))? {
        Some(rec) => Ok(Some(rec.into_state(ip)?)),
        None => Ok(None),
    }
}

/// First row owned by `id` that is still live, in key order.
fn get_id_in<K: AddrKey>(txn: &ReadTransaction, id: &[u8]) -> Result<Option<IpAddr>> {
    let table = txn.open_table(K::TABLE)?;
    let now = now_epoch();
    for item in table.iter()? {
        let (k, v) = item?;
        let rec: LeaseRec = decode("leases", v.value())?;
        if rec.owned_by(id) && rec.is_live(now) {
            return Ok(Some(K::ip_of(k.value())));
        }
    }
    Ok(None)
}

fn insert_in<K: AddrKey>(
    txn: &WriteTransaction,
    ip: IpAddr,
    network: IpAddr,
    id: &[u8],
    expires_at: SystemTime,
    state: Option<IpState>,
) -> Result<Txn<()>> {
    let mut table = txn.open_table(K::TABLE)?;
    let key = K::key_of(ip);
    if table.get(&key)?.is_some() {
        return Err(RedbError::AlreadyExists(ip.to_string()));
    }
    let (leased, probation) = flags(state);
    let rec = LeaseRec {
        client_id: Some(id.to_vec()),
        network: ip_to_bytes(network),
        expires_at: systime_epoch(expires_at),
        leased,
        probation,
    };
    write_rec(&mut table, key, &rec)?;
    Ok(Txn::Commit(()))
}

/// Take over the row at `ip` if it is expired or already ours; never inserts.
fn update_expired_in<K: AddrKey>(
    txn: &WriteTransaction,
    ip: IpAddr,
    state: Option<IpState>,
    id: &[u8],
    expires_at: SystemTime,
) -> Result<Txn<bool>> {
    let mut table = txn.open_table(K::TABLE)?;
    let key = K::key_of(ip);
    let Some(mut rec) = read_rec::<K, _>(&table, &key)? else {
        return Ok(Txn::Abort(false));
    };
    let now = now_epoch();
    if !(rec.owned_by(id) || rec.is_expired(now)) {
        return Ok(Txn::Abort(false));
    }
    rec.client_id = Some(id.to_vec());
    rec.expires_at = systime_epoch(expires_at);
    rec.set_state(state);
    write_rec(&mut table, key, &rec)?;
    Ok(Txn::Commit(true))
}

/// Update our own live row; `new_id = None` clears the owner (probation).
fn update_unexpired_in<K: AddrKey>(
    txn: &WriteTransaction,
    ip: IpAddr,
    state: IpState,
    id: &[u8],
    expires_at: SystemTime,
    new_id: Option<&[u8]>,
) -> Result<Txn<Option<IpAddr>>> {
    let mut table = txn.open_table(K::TABLE)?;
    let key = K::key_of(ip);
    let Some(mut rec) = read_rec::<K, _>(&table, &key)? else {
        return Ok(Txn::Abort(None));
    };
    let now = now_epoch();
    if !(rec.owned_by(id) && rec.is_live(now)) {
        return Ok(Txn::Abort(None));
    }
    rec.client_id = new_id.map(<[u8]>::to_vec);
    rec.expires_at = systime_epoch(expires_at);
    rec.set_state(Some(state));
    write_rec(&mut table, key, &rec)?;
    Ok(Txn::Commit(Some(ip)))
}

/// Unconditional update of an existing row.
fn update_ip_in<K: AddrKey>(
    txn: &WriteTransaction,
    ip: IpAddr,
    state: IpState,
    id: Option<&[u8]>,
    expires_at: SystemTime,
) -> Result<Txn<Option<State>>> {
    let mut table = txn.open_table(K::TABLE)?;
    let key = K::key_of(ip);
    let Some(mut rec) = read_rec::<K, _>(&table, &key)? else {
        return Ok(Txn::Abort(None));
    };
    rec.client_id = id.map(<[u8]>::to_vec);
    rec.expires_at = systime_epoch(expires_at);
    rec.set_state(Some(state));
    write_rec(&mut table, key, &rec.clone())?;
    Ok(Txn::Commit(Some(rec.into_state(ip)?)))
}

/// Release keeps the row (expired, unowned, reserved) so the allocator can hand
/// it out again: `insert_max_in_range` only ever moves upward from the highest
/// allocated address, so a deleted row would be lost from the pool.
fn release_ip_in<K: AddrKey>(
    txn: &WriteTransaction,
    ip: IpAddr,
    id: &[u8],
) -> Result<Txn<Option<ClientInfo>>> {
    let mut table = txn.open_table(K::TABLE)?;
    let key = K::key_of(ip);
    let Some(mut rec) = read_rec::<K, _>(&table, &key)? else {
        return Ok(Txn::Abort(None));
    };
    if !rec.owned_by(id) {
        return Ok(Txn::Abort(None));
    }
    let prior = rec.clone().into_state(ip)?.into();
    rec.client_id = None;
    rec.expires_at = 0;
    rec.leased = false;
    rec.probation = false;
    write_rec(&mut table, key, &rec)?;
    Ok(Txn::Commit(Some(prior)))
}

fn delete_in<K: AddrKey>(txn: &WriteTransaction, ip: IpAddr) -> Result<Txn<()>> {
    let mut table = txn.open_table(K::TABLE)?;
    table.remove(K::key_of(ip))?;
    Ok(Txn::Commit(()))
}

/// Lowest address in `range` that is expired or already owned by `id`; the
/// id match is range-bounded on purpose so a client's binding in another range
/// is left alone. `probation` is always cleared (Postgres parity); `network`
/// is left untouched.
fn next_expired_in<K: AddrKey>(
    txn: &WriteTransaction,
    range: RangeInclusive<IpAddr>,
    id: &[u8],
    expires_at: SystemTime,
    state: Option<IpState>,
) -> Result<Txn<Option<IpAddr>>> {
    let mut table = txn.open_table(K::TABLE)?;
    let now = now_epoch();
    let start = K::key_of(*range.start());
    let end = K::key_of(*range.end());
    let found = {
        let mut found = None;
        for item in table.range::<K::SelfType<'static>>(start..=end)? {
            let (k, v) = item?;
            let rec: LeaseRec = decode("leases", v.value())?;
            if rec.is_expired(now) || rec.owned_by(id) {
                found = Some((K::ip_of(k.value()), rec));
                break;
            }
        }
        found
    };
    let Some((ip, mut rec)) = found else {
        return Ok(Txn::Abort(None));
    };
    rec.client_id = Some(id.to_vec());
    rec.expires_at = systime_epoch(expires_at);
    rec.leased = flags(state).0;
    rec.probation = false;
    write_rec(&mut table, K::key_of(ip), &rec)?;
    Ok(Txn::Commit(Some(ip)))
}

/// Allocate the address after the highest one in `range` (or the first free
/// one when the range is empty), honouring `exclusions`.
fn insert_max_in_range_in<K: AddrKey>(
    txn: &WriteTransaction,
    range: RangeInclusive<IpAddr>,
    exclusions: &HashSet<IpAddr>,
    network: IpAddr,
    id: &[u8],
    expires_at: SystemTime,
    state: Option<IpState>,
) -> Result<Txn<Option<IpAddr>>> {
    let mut table = txn.open_table(K::TABLE)?;
    let (start, end) = (*range.start(), *range.end());
    let max = {
        let mut iter = table.range::<K::SelfType<'static>>(K::key_of(start)..=K::key_of(end))?;
        match iter.next_back() {
            Some(item) => Some(K::ip_of(item?.0.value())),
            None => None,
        }
    };
    let candidate = match max {
        Some(max) => inc_ip(max, end, exclusions),
        None => first_available(start, end, exclusions),
    };
    let Some(ip) = candidate else {
        return Ok(Txn::Abort(None));
    };
    let key = K::key_of(ip);
    if table.get(&key)?.is_some() {
        // cannot happen under the serialized write lock; kept as a guard
        return Err(RedbError::AlreadyExists(ip.to_string()));
    }
    let (leased, probation) = flags(state);
    let rec = LeaseRec {
        client_id: Some(id.to_vec()),
        network: ip_to_bytes(network),
        expires_at: systime_epoch(expires_at),
        leased,
        probation,
    };
    write_rec(&mut table, key, &rec)?;
    Ok(Txn::Commit(Some(ip)))
}

fn select_all_in<K: AddrKey>(txn: &ReadTransaction, out: &mut Vec<State>) -> Result<()> {
    let table = txn.open_table(K::TABLE)?;
    for item in table.iter()? {
        let (k, v) = item?;
        let rec: LeaseRec = decode("leases", v.value())?;
        out.push(rec.into_state(K::ip_of(k.value()))?);
    }
    Ok(())
}

fn count_in<K: AddrKey>(txn: &ReadTransaction, want: (bool, bool), now: i64) -> Result<usize> {
    let table = txn.open_table(K::TABLE)?;
    let mut n = 0;
    for item in table.iter()? {
        let (_, v) = item?;
        let rec: LeaseRec = decode("leases", v.value())?;
        if (rec.leased, rec.probation) == want && rec.is_live(now) {
            n += 1;
        }
    }
    Ok(n)
}

// ---------------------------------------------------------------------------
// prefix-delegation helpers (IA_PD table, or the IA_NA table for /128)
// ---------------------------------------------------------------------------

/// Where a `(prefix, prefix_len)` binding lives: Postgres keeps IA_NA and
/// IA_PD in one table keyed by `(addr, prefix_len)`, so a `/128` "prefix"
/// addresses the IA_NA row. Mirror that.
enum PdSlot {
    Na([u8; 16]),
    Pd(([u8; 16], u8)),
}

fn pd_slot(prefix: IpAddr, prefix_len: u8) -> Option<PdSlot> {
    let octets = v6_octets(prefix)?;
    Some(if prefix_len == IA_NA_PREFIX_LEN {
        PdSlot::Na(octets)
    } else {
        PdSlot::Pd((octets, prefix_len))
    })
}

fn pd_read(txn: &impl PdTables, slot: &PdSlot) -> Result<Option<LeaseRec>> {
    match slot {
        PdSlot::Na(k) => txn.read_na(k),
        PdSlot::Pd(k) => txn.read_pd(k),
    }
}

/// Read access to the two v6 tables from either transaction kind.
trait PdTables {
    fn read_na(&self, key: &[u8; 16]) -> Result<Option<LeaseRec>>;
    fn read_pd(&self, key: &([u8; 16], u8)) -> Result<Option<LeaseRec>>;
}

impl PdTables for ReadTransaction {
    fn read_na(&self, key: &[u8; 16]) -> Result<Option<LeaseRec>> {
        read_rec::<[u8; 16], _>(&self.open_table(LEASES_V6)?, key)
    }
    fn read_pd(&self, key: &([u8; 16], u8)) -> Result<Option<LeaseRec>> {
        read_rec::<([u8; 16], u8), _>(&self.open_table(LEASES_PD)?, key)
    }
}

impl PdTables for WriteTransaction {
    fn read_na(&self, key: &[u8; 16]) -> Result<Option<LeaseRec>> {
        read_rec::<[u8; 16], _>(&self.open_table(LEASES_V6)?, key)
    }
    fn read_pd(&self, key: &([u8; 16], u8)) -> Result<Option<LeaseRec>> {
        read_rec::<([u8; 16], u8), _>(&self.open_table(LEASES_PD)?, key)
    }
}

fn pd_write(txn: &WriteTransaction, slot: &PdSlot, rec: &LeaseRec) -> Result<()> {
    match slot {
        PdSlot::Na(k) => write_rec(&mut txn.open_table(LEASES_V6)?, k, rec),
        PdSlot::Pd(k) => write_rec(&mut txn.open_table(LEASES_PD)?, k, rec),
    }
}

fn pd_remove(txn: &WriteTransaction, slot: &PdSlot) -> Result<()> {
    match slot {
        PdSlot::Na(k) => {
            txn.open_table(LEASES_V6)?.remove(k)?;
        }
        PdSlot::Pd(k) => {
            txn.open_table(LEASES_PD)?.remove(k)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Storage impl
// ---------------------------------------------------------------------------

#[async_trait]
impl Storage for RedbDb {
    type Error = RedbError;

    async fn update_expired(
        &self,
        ip: IpAddr,
        state: Option<IpState>,
        id: &[u8],
        expires_at: SystemTime,
    ) -> Result<bool> {
        let id = id.to_vec();
        self.with_write(move |txn| {
            by_family!(ip, |K| update_expired_in::<K>(
                txn, ip, state, &id, expires_at
            ))
        })
        .await
    }

    async fn insert(
        &self,
        ip: IpAddr,
        network: IpAddr,
        id: &[u8],
        expires_at: SystemTime,
        state: Option<IpState>,
    ) -> Result<()> {
        assert_eq!(
            ip.is_ipv4(),
            network.is_ipv4(),
            "insert: address and network must be the same family"
        );
        let id = id.to_vec();
        self.with_write(move |txn| {
            by_family!(ip, |K| insert_in::<K>(
                txn, ip, network, &id, expires_at, state
            ))
        })
        .await
    }

    async fn get(&self, ip: IpAddr) -> Result<Option<State>> {
        self.with_read(move |txn| by_family!(ip, |K| get_in::<K>(txn, ip)))
            .await
    }

    async fn get_id(&self, id: &[u8]) -> Result<Option<IpAddr>> {
        let id = id.to_vec();
        self.with_read(move |txn| get_id_in::<u32>(txn, &id)).await
    }

    async fn get_id_v6(&self, id: &[u8]) -> Result<Option<IpAddr>> {
        let id = id.to_vec();
        self.with_read(move |txn| get_id_in::<[u8; 16]>(txn, &id))
            .await
    }

    async fn get_pd(&self, prefix: IpAddr, prefix_len: u8) -> Result<Option<State>> {
        let Some(slot) = pd_slot(prefix, prefix_len) else {
            return Ok(None);
        };
        self.with_read(move |txn| match pd_read(txn, &slot)? {
            Some(rec) => Ok(Some(rec.into_state(prefix)?)),
            None => Ok(None),
        })
        .await
    }

    async fn upsert_pd(
        &self,
        prefix: IpAddr,
        prefix_len: u8,
        network: IpAddr,
        id: &[u8],
        expires_at: SystemTime,
        state: Option<IpState>,
    ) -> Result<()> {
        let (Some(slot), Some(_)) = (pd_slot(prefix, prefix_len), v6_octets(network)) else {
            return Ok(());
        };
        let id = id.to_vec();
        self.with_write(move |txn| {
            let (leased, probation) = flags(state);
            let rec = LeaseRec {
                client_id: Some(id),
                network: ip_to_bytes(network),
                expires_at: systime_epoch(expires_at),
                leased,
                probation,
            };
            pd_write(txn, &slot, &rec)?;
            Ok(Txn::Commit(()))
        })
        .await
    }

    async fn claim_pd(
        &self,
        prefix: IpAddr,
        prefix_len: u8,
        network: IpAddr,
        id: &[u8],
        expires_at: SystemTime,
        state: Option<IpState>,
    ) -> Result<bool> {
        let (Some(slot), Some(_)) = (pd_slot(prefix, prefix_len), v6_octets(network)) else {
            return Ok(false);
        };
        let id = id.to_vec();
        self.with_write(move |txn| {
            let now = now_epoch();
            if let Some(existing) = pd_read(txn, &slot)?
                && !(existing.is_expired(now) || existing.owned_by(&id))
            {
                return Ok(Txn::Abort(false));
            }
            let (leased, probation) = flags(state);
            let rec = LeaseRec {
                client_id: Some(id),
                network: ip_to_bytes(network),
                expires_at: systime_epoch(expires_at),
                leased,
                probation,
            };
            pd_write(txn, &slot, &rec)?;
            Ok(Txn::Commit(true))
        })
        .await
    }

    async fn get_id_pd(&self, id: &[u8]) -> Result<Option<(IpAddr, u8)>> {
        let id = id.to_vec();
        self.with_read(move |txn| {
            let table = txn.open_table(LEASES_PD)?;
            let now = now_epoch();
            for item in table.iter()? {
                let (k, v) = item?;
                let rec: LeaseRec = decode("leases_v6_pd", v.value())?;
                if rec.owned_by(&id) && rec.is_live(now) {
                    let (octets, len) = k.value();
                    return Ok(Some((IpAddr::V6(Ipv6Addr::from(octets)), len)));
                }
            }
            Ok(None)
        })
        .await
    }

    async fn renew_pd(
        &self,
        prefix: IpAddr,
        prefix_len: u8,
        id: &[u8],
        expires_at: SystemTime,
    ) -> Result<Option<IpAddr>> {
        let Some(slot) = pd_slot(prefix, prefix_len) else {
            return Ok(None);
        };
        let id = id.to_vec();
        self.with_write(move |txn| {
            let now = now_epoch();
            let Some(mut rec) = pd_read(txn, &slot)? else {
                return Ok(Txn::Abort(None));
            };
            if !(rec.owned_by(&id) && rec.is_live(now)) {
                return Ok(Txn::Abort(None));
            }
            rec.leased = true;
            rec.probation = false;
            rec.expires_at = systime_epoch(expires_at);
            pd_write(txn, &slot, &rec)?;
            Ok(Txn::Commit(Some(prefix)))
        })
        .await
    }

    async fn release_pd(
        &self,
        prefix: IpAddr,
        prefix_len: u8,
        id: &[u8],
    ) -> Result<Option<ClientInfo>> {
        let Some(slot) = pd_slot(prefix, prefix_len) else {
            return Ok(None);
        };
        let id = id.to_vec();
        self.with_write(move |txn| {
            let Some(rec) = pd_read(txn, &slot)? else {
                return Ok(Txn::Abort(None));
            };
            if !rec.owned_by(&id) {
                return Ok(Txn::Abort(None));
            }
            // unlike release_ip, a released delegation is deleted (Postgres parity)
            pd_remove(txn, &slot)?;
            Ok(Txn::Commit(Some(rec.into_state(prefix)?.into())))
        })
        .await
    }

    async fn select_all(&self) -> Result<Vec<State>> {
        self.with_read(|txn| {
            let mut out = Vec::new();
            select_all_in::<u32>(txn, &mut out)?;
            select_all_in::<[u8; 16]>(txn, &mut out)?;
            let pd = txn.open_table(LEASES_PD)?;
            for item in pd.iter()? {
                let (k, v) = item?;
                let rec: LeaseRec = decode("leases_v6_pd", v.value())?;
                out.push(rec.into_state(IpAddr::V6(Ipv6Addr::from(k.value().0)))?);
            }
            Ok(out)
        })
        .await
    }

    async fn release_ip(&self, ip: IpAddr, id: &[u8]) -> Result<Option<ClientInfo>> {
        let id = id.to_vec();
        self.with_write(move |txn| by_family!(ip, |K| release_ip_in::<K>(txn, ip, &id)))
            .await
    }

    async fn delete(&self, ip: IpAddr) -> Result<()> {
        self.with_write(move |txn| by_family!(ip, |K| delete_in::<K>(txn, ip)))
            .await
    }

    async fn next_expired(
        &self,
        range: RangeInclusive<IpAddr>,
        _network: IpAddr,
        id: &[u8],
        expires_at: SystemTime,
        state: Option<IpState>,
    ) -> Result<Option<IpAddr>> {
        assert_eq!(
            range.start().is_ipv4(),
            range.end().is_ipv4(),
            "next_expired: range bounds must be the same family"
        );
        let id = id.to_vec();
        self.with_write(move |txn| {
            let start = *range.start();
            by_family!(start, |K| next_expired_in::<K>(
                txn, range, &id, expires_at, state
            ))
        })
        .await
    }

    async fn insert_max_in_range(
        &self,
        range: RangeInclusive<IpAddr>,
        exclusions: &HashSet<IpAddr>,
        network: IpAddr,
        id: &[u8],
        expires_at: SystemTime,
        state: Option<IpState>,
    ) -> Result<Option<IpAddr>> {
        assert!(
            range.start().is_ipv4() == range.end().is_ipv4()
                && range.start().is_ipv4() == network.is_ipv4(),
            "insert_max_in_range: range and network must be the same family"
        );
        let id = id.to_vec();
        let exclusions = exclusions.clone();
        self.with_write(move |txn| {
            let start = *range.start();
            by_family!(start, |K| insert_max_in_range_in::<K>(
                txn,
                range,
                &exclusions,
                network,
                &id,
                expires_at,
                state
            ))
        })
        .await
    }

    async fn update_unexpired(
        &self,
        ip: IpAddr,
        state: IpState,
        id: &[u8],
        expires_at: SystemTime,
        new_id: Option<&[u8]>,
    ) -> Result<Option<IpAddr>> {
        let id = id.to_vec();
        let new_id = new_id.map(<[u8]>::to_vec);
        self.with_write(move |txn| {
            by_family!(ip, |K| update_unexpired_in::<K>(
                txn,
                ip,
                state,
                &id,
                expires_at,
                new_id.as_deref()
            ))
        })
        .await
    }

    async fn update_ip(
        &self,
        ip: IpAddr,
        state: IpState,
        id: Option<&[u8]>,
        expires_at: SystemTime,
    ) -> Result<Option<State>> {
        let id = id.map(<[u8]>::to_vec);
        self.with_write(move |txn| {
            by_family!(ip, |K| update_ip_in::<K>(
                txn,
                ip,
                state,
                id.as_deref(),
                expires_at
            ))
        })
        .await
    }

    async fn count(&self, state: IpState) -> Result<usize> {
        let want: (bool, bool) = state.into();
        self.with_read(move |txn| {
            let now = now_epoch();
            let mut n = count_in::<u32>(txn, want, now)?;
            n += count_in::<[u8; 16]>(txn, want, now)?;
            let pd = txn.open_table(LEASES_PD)?;
            for item in pd.iter()? {
                let (_, v) = item?;
                let rec: LeaseRec = decode("leases_v6_pd", v.value())?;
                if (rec.leased, rec.probation) == want && rec.is_live(now) {
                    n += 1;
                }
            }
            Ok(n)
        })
        .await
    }

    // ----- operations -------------------------------------------------------

    async fn insert_operation(&self, op: &OperationRecord) -> Result<()> {
        let id = op.operation_id.clone();
        let rec = OperationRec::from_record(op);
        self.with_write(move |txn| {
            let mut table = txn.open_table(OPERATIONS)?;
            if table.get(id.as_str())?.is_some() {
                return Err(RedbError::AlreadyExists(id));
            }
            table.insert(id.as_str(), encode(&rec)?.as_slice())?;
            Ok(Txn::Commit(()))
        })
        .await
    }

    async fn update_operation(&self, op: &OperationRecord) -> Result<()> {
        let id = op.operation_id.clone();
        let mut rec = OperationRec::from_record(op);
        self.with_write(move |txn| {
            let mut table = txn.open_table(OPERATIONS)?;
            let existing: Option<OperationRec> = match table.get(id.as_str())? {
                Some(g) => Some(decode("operations", g.value())?),
                None => None,
            };
            let Some(existing) = existing else {
                return Ok(Txn::Abort(()));
            };
            // an UPDATE never rewrites created_at
            rec.created_at = existing.created_at;
            table.insert(id.as_str(), encode(&rec)?.as_slice())?;
            Ok(Txn::Commit(()))
        })
        .await
    }

    async fn get_operation(&self, operation_id: &str) -> Result<Option<OperationRecord>> {
        let id = operation_id.to_owned();
        self.with_read(move |txn| {
            let table = txn.open_table(OPERATIONS)?;
            match table.get(id.as_str())? {
                Some(g) => {
                    let rec: OperationRec = decode("operations", g.value())?;
                    Ok(Some(rec.into_record(id)?))
                }
                None => Ok(None),
            }
        })
        .await
    }

    // ----- runtime reservations ---------------------------------------------

    async fn upsert_reservation(&self, res: &RuntimeReservationRecord) -> Result<()> {
        let (family, ip) = (res.family.clone(), res.ip.clone());
        let rec = ReservationRec::from_record(res);
        self.with_write(move |txn| {
            txn.open_table(RESERVATIONS)?
                .insert((family.as_str(), ip.as_str()), encode(&rec)?.as_slice())?;
            Ok(Txn::Commit(()))
        })
        .await
    }

    async fn delete_reservation(&self, family: &str, ip: &str) -> Result<bool> {
        let (family, ip) = (family.to_owned(), ip.to_owned());
        self.with_write(move |txn| {
            let removed = txn
                .open_table(RESERVATIONS)?
                .remove((family.as_str(), ip.as_str()))?
                .is_some();
            Ok(if removed {
                Txn::Commit(true)
            } else {
                Txn::Abort(false)
            })
        })
        .await
    }

    async fn get_reservation(
        &self,
        family: &str,
        ip: &str,
    ) -> Result<Option<RuntimeReservationRecord>> {
        let (family, ip) = (family.to_owned(), ip.to_owned());
        self.with_read(move |txn| {
            let table = txn.open_table(RESERVATIONS)?;
            match table.get((family.as_str(), ip.as_str()))? {
                Some(g) => {
                    let rec: ReservationRec = decode("runtime_reservations", g.value())?;
                    Ok(Some(rec.into_record(family, ip)))
                }
                None => Ok(None),
            }
        })
        .await
    }

    async fn list_reservations(&self) -> Result<Vec<RuntimeReservationRecord>> {
        self.with_read(|txn| {
            let table = txn.open_table(RESERVATIONS)?;
            let mut out = Vec::new();
            // tuple keys iterate in (family, ip) order == `ORDER BY family, ip`
            for item in table.iter()? {
                let (k, v) = item?;
                let (family, ip) = k.value();
                let rec: ReservationRec = decode("runtime_reservations", v.value())?;
                out.push(rec.into_record(family.to_owned(), ip.to_owned()));
            }
            Ok(out)
        })
        .await
    }

    // ----- config candidates ------------------------------------------------

    async fn upsert_config_candidate(&self, candidate: &ConfigCandidateRecord) -> Result<()> {
        let id = candidate.candidate_id.clone();
        let rec = CandidateRec::from_record(candidate);
        self.with_write(move |txn| {
            txn.open_table(CONFIG_CANDIDATES)?
                .insert(id.as_str(), encode(&rec)?.as_slice())?;
            Ok(Txn::Commit(()))
        })
        .await
    }

    async fn get_config_candidate(
        &self,
        candidate_id: &str,
    ) -> Result<Option<ConfigCandidateRecord>> {
        let id = candidate_id.to_owned();
        self.with_read(move |txn| {
            let table = txn.open_table(CONFIG_CANDIDATES)?;
            match table.get(id.as_str())? {
                Some(g) => {
                    let rec: CandidateRec = decode("config_candidates", g.value())?;
                    Ok(Some(rec.into_record(id)))
                }
                None => Ok(None),
            }
        })
        .await
    }

    async fn list_config_candidates(&self) -> Result<Vec<ConfigCandidateRecord>> {
        self.with_read(|txn| {
            let mut out = all_candidates(txn)?;
            // `ORDER BY created_at DESC`, with the id as a deterministic tie-break
            out.sort_by(|a, b| {
                b.created_at
                    .cmp(&a.created_at)
                    .then_with(|| b.candidate_id.cmp(&a.candidate_id))
            });
            Ok(out)
        })
        .await
    }

    async fn active_config_candidate(&self) -> Result<Option<ConfigCandidateRecord>> {
        self.with_read(|txn| {
            Ok(all_candidates(txn)?
                .into_iter()
                .filter(|c| c.status == "activated")
                .max_by_key(|c| c.activated_at))
        })
        .await
    }

    async fn activate_config_candidate(
        &self,
        candidate_id: &str,
        activated_at: SystemTime,
    ) -> Result<()> {
        let id = candidate_id.to_owned();
        self.with_write(move |txn| {
            let mut table = txn.open_table(CONFIG_CANDIDATES)?;
            // 1. every currently-activated candidate becomes superseded
            let activated: Vec<(String, CandidateRec)> = {
                let mut v = Vec::new();
                for item in table.iter()? {
                    let (k, val) = item?;
                    let rec: CandidateRec = decode("config_candidates", val.value())?;
                    if rec.status == "activated" {
                        v.push((k.value().to_owned(), rec));
                    }
                }
                v
            };
            for (k, mut rec) in activated {
                rec.status = "superseded".to_owned();
                table.insert(k.as_str(), encode(&rec)?.as_slice())?;
            }
            // 2. the target (if it exists) becomes activated. An unknown id
            //    leaves no active candidate, as in Postgres.
            let target: Option<CandidateRec> = match table.get(id.as_str())? {
                Some(g) => Some(decode("config_candidates", g.value())?),
                None => None,
            };
            if let Some(mut rec) = target {
                rec.status = "activated".to_owned();
                rec.activated_at = Some(systime_epoch(activated_at));
                table.insert(id.as_str(), encode(&rec)?.as_slice())?;
            }
            Ok(Txn::Commit(()))
        })
        .await
    }

    // ----- server mode -------------------------------------------------------

    async fn get_server_mode(&self) -> Result<Option<String>> {
        self.with_read(|txn| {
            let table = txn.open_table(SERVER_STATE)?;
            Ok(table.get(MODE_KEY)?.map(|g| g.value().to_owned()))
        })
        .await
    }

    async fn set_server_mode(&self, mode: &str) -> Result<()> {
        let mode = mode.to_owned();
        self.with_write(move |txn| {
            txn.open_table(SERVER_STATE)?
                .insert(MODE_KEY, mode.as_str())?;
            Ok(Txn::Commit(()))
        })
        .await
    }
}

fn all_candidates(txn: &ReadTransaction) -> Result<Vec<ConfigCandidateRecord>> {
    let table = txn.open_table(CONFIG_CANDIDATES)?;
    let mut out = Vec::new();
    for item in table.iter()? {
        let (k, v) = item?;
        let rec: CandidateRec = decode("config_candidates", v.value())?;
        out.push(rec.into_record(k.value().to_owned()));
    }
    Ok(out)
}

#[cfg(test)]
mod conformance {
    storage_conformance_tests!(async { crate::redb::RedbDb::new_test() });
}

#[cfg(test)]
mod redb_tests {
    use std::{
        collections::HashSet,
        net::{IpAddr, Ipv4Addr},
        sync::Arc,
        time::{Duration, SystemTime},
    };

    use super::*;
    use crate::IpState;

    #[tokio::test]
    async fn reopen_preserves_leases() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("persist.redb");
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7));
        let net = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0));
        let exp = SystemTime::now() + Duration::from_secs(600);
        {
            let db = RedbDb::open(&path, RedbOptions::default())?;
            db.insert(ip, net, &[1, 2, 3], exp, Some(IpState::Lease))
                .await?;
            db.set_server_mode("drain").await?;
        }
        let db = RedbDb::open(&path, RedbOptions::default())?;
        let all = db.select_all().await?;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].as_ref().ip(), ip);
        assert_eq!(all[0].as_ref().id(), Some(&[1u8, 2, 3][..]));
        assert_eq!(db.get_server_mode().await?, Some("drain".to_owned()));
        Ok(())
    }

    #[tokio::test]
    async fn schema_too_new_is_refused() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("future.redb");
        {
            let db = Database::create(&path)?;
            let txn = db.begin_write()?;
            txn.open_table(META)?
                .insert(SCHEMA_VERSION_KEY, SCHEMA_VERSION + 1)?;
            txn.commit()?;
        }
        match RedbDb::open(&path, RedbOptions::default()) {
            Err(RedbError::SchemaTooNew { found, supported }) => {
                assert_eq!(found, SCHEMA_VERSION + 1);
                assert_eq!(supported, SCHEMA_VERSION);
            }
            other => panic!("expected SchemaTooNew, got {other:?}"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn second_opener_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("locked.redb");
        let _first = RedbDb::open(&path, RedbOptions::default())?;
        assert!(
            RedbDb::open(&path, RedbOptions::default()).is_err(),
            "redb must refuse a second writer on the same file"
        );
        Ok(())
    }

    #[tokio::test]
    async fn open_url_requires_redb_scheme() {
        assert!(RedbDb::open_url("postgres://x@y/z", RedbOptions::default()).is_err());
        assert!(RedbDb::open_url("redb://relative.redb", RedbOptions::default()).is_err());
    }

    /// Writes are serialized by the store, so a storm of concurrent allocations
    /// never collides and never needs the retry the Postgres backend relies on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_allocations_are_unique_without_retries()
    -> Result<(), Box<dyn std::error::Error>> {
        let db = Arc::new(RedbDb::new_test()?);
        let range =
            IpAddr::V4(Ipv4Addr::new(192, 168, 9, 1))..=IpAddr::V4(Ipv4Addr::new(192, 168, 9, 254));
        let net = IpAddr::V4(Ipv4Addr::new(192, 168, 9, 0));
        let exp = SystemTime::now() + Duration::from_secs(60);
        let excl = HashSet::new();
        let mut handles = Vec::new();
        for i in 0u16..64 {
            let (db, range, excl) = (Arc::clone(&db), range.clone(), excl.clone());
            handles.push(tokio::spawn(async move {
                let id = i.to_be_bytes();
                if let Some(ip) = db
                    .next_expired(range.clone(), net, &id, exp, Some(IpState::Reserve))
                    .await?
                {
                    return Ok::<_, RedbError>(ip);
                }
                Ok(db
                    .insert_max_in_range(range, &excl, net, &id, exp, Some(IpState::Reserve))
                    .await?
                    .expect("range not exhausted"))
            }));
        }
        let mut seen = HashSet::new();
        for h in handles {
            let ip = h.await??;
            assert!(seen.insert(ip), "duplicate allocation {ip}");
        }
        assert_eq!(seen.len(), 64);
        assert_eq!(db.count(IpState::Reserve).await?, 64);
        Ok(())
    }
}
