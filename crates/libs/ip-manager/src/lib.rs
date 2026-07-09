#![allow(clippy::too_many_arguments)]

//! # ip-manager
//!
//! `ip-manager` defines a trait `Storage` that provides methods for doing
//! getting & updating IPs in storage.
//!
//! This trait is not meant to be used by plugins directly. Instead, it's wrapped
//! in a `IpManager` type which takes a generic parameter that must implement `Storage`
//! `IpManager` then uses those methods to do the job of reserving/leasing ips while maintaining
//! a nicer interface for the plugin to interact with.
//!
//! [`Storage`]: ip_manager::Storage
//! [`IpManager`]: ip_manager::IpManager
use icmp_ping::{Icmpv4, Icmpv6, Listener, NeighborSolicitor, PingReply};

use async_trait::async_trait;
use chrono::DateTime;
use chrono::{SecondsFormat, offset::Utc};
use thiserror::Error;
use tracing::{debug, error, info, trace, warn};

pub mod sqlite;

mod pool;
pub use pool::{NetworkParams, Pool};

use core::fmt;
use std::{
    collections::HashSet,
    net::{IpAddr, Ipv6Addr},
    ops::RangeInclusive,
    sync::{
        Arc,
        atomic::{AtomicU16, Ordering},
    },
    time::{Duration, SystemTime},
};

const PING_TTL: u64 = 60;
pub type ClientId = Option<Vec<u8>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash, sqlx::FromRow)]
pub struct ClientInfo {
    ip: IpAddr,
    id: ClientId,
    network: IpAddr,
    expires_at: SystemTime,
}

impl ClientInfo {
    pub fn ip(&self) -> IpAddr {
        self.ip
    }
    pub fn id(&self) -> Option<&[u8]> {
        self.id.as_deref()
    }
    pub fn network(&self) -> IpAddr {
        self.network
    }
    pub fn expires_at(&self) -> SystemTime {
        self.expires_at
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum IpState {
    Lease,
    Probate,
    Reserve,
}

/// our sqlite impl doesn't properly support enums, so this
/// converts our 3 state system into 2 bools.
impl From<IpState> for (bool, bool) {
    fn from(state: IpState) -> Self {
        match state {
            IpState::Lease => (true, false),
            IpState::Probate => (false, true),
            IpState::Reserve => (false, false),
        }
    }
}

#[async_trait]
pub trait Storage: Send + Sync + 'static {
    // send/sync/static required for async trait bounds
    type Error: std::error::Error + Send + Sync + 'static;
    /// updates if expired & ip matches or if ip & id match
    async fn update_expired(
        &self,
        ip: IpAddr,
        state: Option<IpState>,
        id: &[u8],
        expires_at: SystemTime,
    ) -> Result<bool, Self::Error>;
    async fn insert(
        &self,
        ip: IpAddr,
        network: IpAddr,
        id: &[u8],
        expires_at: SystemTime,
        state: Option<IpState>,
    ) -> Result<(), Self::Error>;

    async fn get(&self, ip: IpAddr) -> Result<Option<State>, Self::Error>;
    /// look up an unexpired v4 lease by client identity (v4 `leases` table)
    async fn get_id(&self, id: &[u8]) -> Result<Option<IpAddr>, Self::Error>;
    /// look up an unexpired v6 binding by DUID+IAID identity (`leases_v6` table).
    /// Separate from `get_id` so identity lookups target one table deterministically
    /// and a v4/v6 client-id byte collision can never return the wrong family.
    async fn get_id_v6(&self, id: &[u8]) -> Result<Option<IpAddr>, Self::Error>;

    // ---- IA_PD (prefix delegation) ------------------------------------------
    /// get a delegated-prefix binding by its base address and delegated length
    async fn get_pd(&self, prefix: IpAddr, prefix_len: u8) -> Result<Option<State>, Self::Error>;
    /// insert or replace a delegated-prefix binding (caller has verified it is
    /// free / expired / owned by this client)
    async fn upsert_pd(
        &self,
        prefix: IpAddr,
        prefix_len: u8,
        network: IpAddr,
        id: &[u8],
        expires_at: SystemTime,
        state: Option<IpState>,
    ) -> Result<(), Self::Error>;
    /// look up a client's delegated prefix (base + length) by DUID+IAID identity
    async fn get_id_pd(&self, id: &[u8]) -> Result<Option<(IpAddr, u8)>, Self::Error>;
    /// extend an unexpired delegated prefix if the id matches
    async fn renew_pd(
        &self,
        prefix: IpAddr,
        prefix_len: u8,
        id: &[u8],
        expires_at: SystemTime,
    ) -> Result<Option<IpAddr>, Self::Error>;
    /// release a delegated prefix if the (prefix, len, id) match
    async fn release_pd(
        &self,
        prefix: IpAddr,
        prefix_len: u8,
        id: &[u8],
    ) -> Result<Option<ClientInfo>, Self::Error>;
    async fn select_all(&self) -> Result<Vec<State>, Self::Error>;
    async fn release_ip(&self, ip: IpAddr, id: &[u8]) -> Result<Option<ClientInfo>, Self::Error>;
    async fn delete(&self, ip: IpAddr) -> Result<(), Self::Error>;

    async fn next_expired(
        &self,
        range: RangeInclusive<IpAddr>,
        network: IpAddr,
        id: &[u8],
        expires_at: SystemTime,
        state: Option<IpState>,
    ) -> Result<Option<IpAddr>, Self::Error>;

    async fn insert_max_in_range(
        &self,
        range: RangeInclusive<IpAddr>,
        // family-neutral: v4 or v6 addresses to skip
        exclusions: &HashSet<IpAddr>,
        network: IpAddr,
        id: &[u8],
        expires_at: SystemTime,
        state: Option<IpState>,
    ) -> Result<Option<IpAddr>, Self::Error>;
    /// updates if not expired & id & ip match
    async fn update_unexpired(
        &self,
        ip: IpAddr,
        state: IpState,
        id: &[u8],
        expires_at: SystemTime,
        new_id: Option<&[u8]>,
    ) -> Result<Option<IpAddr>, Self::Error>;
    async fn update_ip(
        &self,
        ip: IpAddr,
        state: IpState,
        id: Option<&[u8]>,
        expires_at: SystemTime,
    ) -> Result<Option<State>, Self::Error>;
    async fn count(&self, state: IpState) -> Result<usize, Self::Error>;

    // ---- operations / audit -------------------------------------------------
    /// insert a new management-operation record (the audit trail row for an
    /// action). Synchronous actions insert an already-terminal record;
    /// asynchronous actions insert `accepted` and update it as work proceeds.
    async fn insert_operation(&self, op: &OperationRecord) -> Result<(), Self::Error>;
    /// update an existing operation record in place, keyed by `operation_id`
    async fn update_operation(&self, op: &OperationRecord) -> Result<(), Self::Error>;
    /// fetch an operation record by id, or `None` if no such record exists
    async fn get_operation(
        &self,
        operation_id: &str,
    ) -> Result<Option<OperationRecord>, Self::Error>;

    // ---- runtime reservations -----------------------------------------------
    /// insert or replace a runtime reservation, keyed by (family, ip)
    async fn upsert_reservation(&self, res: &RuntimeReservationRecord) -> Result<(), Self::Error>;
    /// delete a runtime reservation by (family, ip); returns whether a row was removed
    async fn delete_reservation(&self, family: &str, ip: &str) -> Result<bool, Self::Error>;
    /// fetch a runtime reservation by (family, ip)
    async fn get_reservation(
        &self,
        family: &str,
        ip: &str,
    ) -> Result<Option<RuntimeReservationRecord>, Self::Error>;
    /// list all runtime reservations (used to warm the in-memory store on startup)
    async fn list_reservations(&self) -> Result<Vec<RuntimeReservationRecord>, Self::Error>;

    // ---- config candidates --------------------------------------------------
    /// insert or replace a staged config candidate, keyed by candidate_id
    async fn upsert_config_candidate(
        &self,
        candidate: &ConfigCandidateRecord,
    ) -> Result<(), Self::Error>;
    /// fetch a config candidate by id
    async fn get_config_candidate(
        &self,
        candidate_id: &str,
    ) -> Result<Option<ConfigCandidateRecord>, Self::Error>;
    /// list config candidates, newest first
    async fn list_config_candidates(&self) -> Result<Vec<ConfigCandidateRecord>, Self::Error>;
    /// the currently active candidate (status `activated`), if any
    async fn active_config_candidate(&self) -> Result<Option<ConfigCandidateRecord>, Self::Error>;
    /// atomically supersede the current active candidate and mark `candidate_id`
    /// activated (one transaction, so the single active marker is never split)
    async fn activate_config_candidate(
        &self,
        candidate_id: &str,
        activated_at: SystemTime,
    ) -> Result<(), Self::Error>;
}

/// A persisted staged config candidate (one row of `config_candidates`).
/// `document` is the config text (YAML); `validation` is a JSON array of
/// validation messages. Both are opaque to the storage layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigCandidateRecord {
    pub candidate_id: String,
    pub status: String,
    pub document: String,
    pub message: Option<String>,
    pub validation: Option<String>,
    pub created_at: SystemTime,
    pub activated_at: Option<SystemTime>,
}

/// A persisted runtime (API-managed) host reservation (one row of the
/// `runtime_reservations` table). `match_json` is opaque to storage — the
/// management API / in-memory store parse it into a match predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeReservationRecord {
    pub family: String,
    pub ip: String,
    pub prefix: Option<String>,
    pub network: Option<String>,
    pub match_json: String,
    pub created_at: SystemTime,
}

/// Lifecycle status of an [`OperationRecord`], mirroring the management API's
/// `Operation.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationStatus {
    Accepted,
    Running,
    Succeeded,
    Failed,
    Canceled,
}

impl OperationStatus {
    /// the value stored in the `operations.status` column
    pub fn as_str(self) -> &'static str {
        match self {
            OperationStatus::Accepted => "accepted",
            OperationStatus::Running => "running",
            OperationStatus::Succeeded => "succeeded",
            OperationStatus::Failed => "failed",
            OperationStatus::Canceled => "canceled",
        }
    }

    /// parse a value read back from the `operations.status` column
    pub fn from_db_str(s: &str) -> Option<Self> {
        Some(match s {
            "accepted" => OperationStatus::Accepted,
            "running" => OperationStatus::Running,
            "succeeded" => OperationStatus::Succeeded,
            "failed" => OperationStatus::Failed,
            "canceled" => OperationStatus::Canceled,
            _ => return None,
        })
    }
}

/// A persisted async-operation / audit record (one row of the `operations`
/// table). The `input_summary`, `result`, and `error_*` fields are opaque to
/// the storage layer — the management API fills them with redacted JSON text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationRecord {
    pub operation_id: String,
    pub action: String,
    pub status: OperationStatus,
    pub actor: Option<String>,
    pub input_summary: Option<String>,
    pub result: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub created_at: SystemTime,
    pub started_at: Option<SystemTime>,
    pub completed_at: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum State {
    Reserved(ClientInfo),
    Leased(ClientInfo),
    Probated(ClientInfo),
}

impl AsRef<ClientInfo> for State {
    fn as_ref(&self) -> &ClientInfo {
        match self {
            State::Reserved(info) => info,
            State::Leased(info) => info,
            State::Probated(info) => info,
        }
    }
}

impl State {
    pub fn into(self) -> ClientInfo {
        match self {
            State::Reserved(info) => info,
            State::Leased(info) => info,
            State::Probated(info) => info,
        }
    }
}

pub struct IpManager<T> {
    store: T,
    icmpv4: Arc<IcmpInner<Icmpv4>>,
    /// best-effort: `None` if the ICMPv6 echo socket could not be created (some
    /// CI / container environments), in which case echo-based v6 DAD is skipped
    icmpv6: Option<Arc<IcmpInner<Icmpv6>>>,
    /// preferred v6 DAD: Neighbor Solicitation. best-effort — `None` if a raw
    /// ICMPv6 socket could not be created (needs `CAP_NET_RAW`)
    nd: Option<Arc<NeighborSolicitor>>,
    /// memoized DAD results: `true` == address is in use
    ping_cache: moka::future::Cache<IpAddr, bool>,
}

impl<T> fmt::Debug for IpManager<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IpManager {{}}")
            // .field("store", &self.store)
            // .field("icmpv4", &self.icmpv4)
            // .field("ping_cache", &self.ping_cache)
            .finish()
    }
}

impl<T: Clone> Clone for IpManager<T> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            icmpv4: self.icmpv4.clone(),
            icmpv6: self.icmpv6.clone(),
            nd: self.nd.clone(),
            ping_cache: self.ping_cache.clone(),
        }
    }
}

pub(crate) struct IcmpInner<P> {
    seq_cnt: AtomicU16,
    listener: Listener<P>,
}

impl<T> IpManager<T>
where
    T: Storage,
{
    /// Check to see if the address is in use.
    /// If `Network` has `ping_check` set to `true`, we will test to see if the IP is already
    /// being used by another client
    async fn addr_in_use(
        &self,
        ip: IpAddr,
        timeout: Duration,
    ) -> Result<PingReply, icmp_ping::Error> {
        let seq_cnt = self.icmpv4.seq_cnt.fetch_add(1, Ordering::Relaxed);
        // send a single ping
        self.icmpv4
            .listener
            .pinger(ip)
            .timeout(timeout)
            .ping(seq_cnt)
            .await
        // ping succeeded, meaning addr is in use
    }

    /// v6 duplicate-address detection: send an ICMPv6 echo request; a reply
    /// means the address is already in use on the link.
    async fn addr_in_use_v6(
        &self,
        icmpv6: &IcmpInner<Icmpv6>,
        ip: IpAddr,
        timeout: Duration,
    ) -> Result<PingReply, icmp_ping::Error> {
        let seq_cnt = icmpv6.seq_cnt.fetch_add(1, Ordering::Relaxed);
        icmpv6
            .listener
            .pinger(ip)
            .timeout(timeout)
            .ping(seq_cnt)
            .await
    }

    /// used for tests to insert into the DAD result cache (`true` == in use)
    #[cfg(test)]
    pub(crate) async fn ping_insert(&self, ip: IpAddr, in_use: bool) {
        self.ping_cache.insert(ip, in_use).await
    }

    /// returns `Ok(())` if the address is free (or DAD is disabled for the
    /// network); returns `Err(AddrInUse)` if it is already in use on-link.
    pub async fn ping_check<N: NetworkParams>(
        &self,
        ip: IpAddr,
        network: &N,
    ) -> Result<(), IpError<T::Error>> {
        if !network.ping_check() {
            return Ok(());
        }
        let timeout = network.ping_timeout();
        let iface = network.iface_index();
        let fut = async {
            let in_use = match ip {
                IpAddr::V4(_) => self.addr_in_use(ip, timeout).await.is_ok(),
                IpAddr::V6(v6) => self.dad_v6(v6, iface, timeout).await,
            };
            if in_use {
                // stop handing this address out
                if let Err(err) = self.store.delete(ip).await {
                    error!(?err, "error attempting to delete in-use ip");
                }
            }
            in_use
        };
        if self.ping_cache.get_with(ip, fut).await {
            Err(IpError::AddrInUse(ip))
        } else {
            Ok(())
        }
    }

    /// v6 duplicate-address detection. Prefer Neighbor Solicitation (RFC 4861 —
    /// hosts reliably answer NS, unlike echo which can be filtered) when a raw
    /// socket and the network's interface index are both available; otherwise
    /// fall back to an ICMPv6 echo probe; if neither is available, skip DAD.
    async fn dad_v6(&self, ip: Ipv6Addr, iface: Option<u32>, timeout: Duration) -> bool {
        if let (Some(nd), Some(scope)) = (&self.nd, iface) {
            match nd.probe(ip, scope, timeout).await {
                Ok(in_use) => return in_use,
                Err(err) => debug!(?err, "NS probe failed; falling back to echo"),
            }
        }
        if let Some(icmpv6) = &self.icmpv6 {
            return self
                .addr_in_use_v6(icmpv6, IpAddr::V6(ip), timeout)
                .await
                .is_ok();
        }
        trace!(?ip, "no v6 DAD mechanism available; treating as free");
        false
    }
}

impl<T> IpManager<T>
where
    T: Storage,
{
    pub fn new(store: T) -> Result<Self, icmp_ping::Error> {
        let icmpv4 = Arc::new(IcmpInner {
            seq_cnt: AtomicU16::new(1),
            listener: Listener::<Icmpv4>::new()?,
        });
        // v6 DAD is best-effort: some CI/container environments can't create an
        // ICMPv6 socket. Disable v6 probing there rather than failing startup.
        let icmpv6 = match Listener::<Icmpv6>::new() {
            Ok(listener) => Some(Arc::new(IcmpInner {
                seq_cnt: AtomicU16::new(1),
                listener,
            })),
            Err(err) => {
                warn!(
                    ?err,
                    "could not create ICMPv6 echo socket; echo v6 DAD disabled"
                );
                None
            }
        };
        // preferred v6 DAD mechanism: Neighbor Solicitation (needs a raw socket).
        // Also best-effort; falls back to echo above when unavailable.
        let nd = match NeighborSolicitor::new() {
            Ok(nd) => Some(Arc::new(nd)),
            Err(err) => {
                warn!(
                    ?err,
                    "could not create ICMPv6 raw socket; NS-based v6 DAD disabled"
                );
                None
            }
        };
        Ok(Self {
            icmpv4,
            icmpv6,
            nd,
            store,
            ping_cache: moka::future::CacheBuilder::new(1_000)
                // time_to_idle?
                .time_to_live(Duration::from_secs(PING_TTL))
                .initial_capacity(1_000)
                .build(),
        })
    }

    /// get the first available IP in a range with a given id/expiry/network
    pub async fn reserve_first<P: Pool + fmt::Debug, N: NetworkParams>(
        &self,
        range: &P,
        network: &N,
        id: &[u8],
        expires_at: SystemTime,
        state: Option<IpState>,
    ) -> Result<IpAddr, IpError<T::Error>> {
        const MAX_ATTEMPTS: usize = 2;
        let subnet = network.subnet();
        // family-neutral exclusion set for the storage layer
        let exclusions: HashSet<IpAddr> = range.exclusions();
        // unfortunately the sqlite connection is sometimes unreliable under high contention, meaning
        // we need to make a few attempts to get an address.
        let mut attempts = 0;
        loop {
            let ip_range = range.start()..=range.end();
            if attempts > MAX_ATTEMPTS {
                return Err(IpError::MaxAttempts {
                    range: ip_range,
                    attempts,
                });
            }
            // find the min expired IP or where id matches
            let ip = match self
                .store
                .next_expired(ip_range.clone(), subnet, id, expires_at, state)
                .await
            {
                Ok(Some(ip)) => ip,
                // the range has no expired entries, so find the next available IP in the range
                Ok(None) => match self
                    .store
                    .insert_max_in_range(
                        ip_range.clone(),
                        &exclusions,
                        subnet,
                        id,
                        expires_at,
                        state,
                    )
                    .await
                {
                    Ok(ip) => ip.ok_or(IpError::RangeError {
                        range: ip_range.clone(),
                    })?,
                    Err(err) => {
                        attempts += 1;
                        warn!(?err, "error grabbing new IP-- retrying");
                        continue;
                    }
                },
                Err(err) => {
                    attempts += 1;
                    warn!(?err, "error grabbing next expired IP-- retrying");
                    continue;
                }
            };
            if range.contains(ip) {
                // ping_check will delete the expired entry if it's in use
                match self.ping_check(ip, network).await {
                    Ok(()) => return Ok(ip),
                    // ping success so insert probated IP
                    Err(err) => {
                        let probation_time = SystemTime::now() + network.probation_period();
                        info!(
                            ?err,
                            probation_time = %DateTime::<Utc>::from(probation_time).to_rfc3339_opts(SecondsFormat::Secs, true),
                            "ping succeeded. address is in use. marking IP on probation"
                        );
                        // update regardless of expiry/id because something is using the IP
                        if let Err(err) = self
                            .store
                            .update_ip(ip, IpState::Probate, None, probation_time)
                            .await
                        {
                            attempts += 1;
                            error!(?err, "failed to probate IP on ping success");
                            // not returning error because we must give client an IP
                        } else {
                            debug!("IP put on probation, trying next");
                        }
                        continue;
                    }
                }
            } else {
                attempts += 1;
                warn!(
                    ?range,
                    ?ip,
                    "IP for client id returned from leases table is outside of network range"
                );
                // entry for ip/id but the range doesn't match, remove the old entry
                if let Err(err) = self.store.release_ip(ip, id).await {
                    error!(?err, "failed to delete entry");
                }
                continue;
            }
        }
    }

    /// tries to take an ip for an id that's set to expire at some future time.
    /// If `ping` is set, will send a ping to the IP, returning an error if in use
    /// Returns
    ///     `Err` if ip/id are already present or ping succeeded
    ///     `Ok(())` allocated IP successfully
    pub async fn try_ip<N: NetworkParams>(
        &self,
        ip: IpAddr,
        subnet: IpAddr,
        id: &[u8],
        expires_at: SystemTime,
        network: &N,
        state: Option<IpState>,
    ) -> Result<(), IpError<T::Error>> {
        // TODO: there may be a way to remove this .get also
        if self.store.get(ip).await?.is_some() {
            return if self.store.update_expired(ip, state, id, expires_at).await? {
                debug!(
                    ?ip,
                    ?id,
                    "set reserved, found ip/id for this client or expired"
                );
                Ok(())
            } else {
                debug!("IP not updated, couldn't find ip/id or in use");
                Err(IpError::AddrInUse(ip))
            };
        };
        // if the entry doesn't exist yet & ping fails, insert it
        self.store.insert(ip, subnet, id, expires_at, state).await?;
        // not marking for probation because request IP can be sent at any time
        self.ping_check(ip, network).await?;

        Ok(())
    }

    /// select all leases in array, returning as a vec
    pub async fn select_all(&self) -> Result<Vec<State>, IpError<T::Error>> {
        Ok(self.store.select_all().await?)
    }

    /// insert a new management-operation / audit record
    pub async fn insert_operation(&self, op: &OperationRecord) -> Result<(), IpError<T::Error>> {
        Ok(self.store.insert_operation(op).await?)
    }

    /// update an existing operation record in place (lifecycle transition)
    pub async fn update_operation(&self, op: &OperationRecord) -> Result<(), IpError<T::Error>> {
        Ok(self.store.update_operation(op).await?)
    }

    /// fetch an operation record by id
    pub async fn get_operation(
        &self,
        operation_id: &str,
    ) -> Result<Option<OperationRecord>, IpError<T::Error>> {
        Ok(self.store.get_operation(operation_id).await?)
    }

    /// insert or replace a runtime reservation
    pub async fn upsert_reservation(
        &self,
        res: &RuntimeReservationRecord,
    ) -> Result<(), IpError<T::Error>> {
        Ok(self.store.upsert_reservation(res).await?)
    }

    /// delete a runtime reservation by (family, ip); returns whether a row was removed
    pub async fn delete_reservation(
        &self,
        family: &str,
        ip: &str,
    ) -> Result<bool, IpError<T::Error>> {
        Ok(self.store.delete_reservation(family, ip).await?)
    }

    /// list all runtime reservations
    pub async fn list_reservations(
        &self,
    ) -> Result<Vec<RuntimeReservationRecord>, IpError<T::Error>> {
        Ok(self.store.list_reservations().await?)
    }

    /// insert or replace a config candidate
    pub async fn upsert_config_candidate(
        &self,
        candidate: &ConfigCandidateRecord,
    ) -> Result<(), IpError<T::Error>> {
        Ok(self.store.upsert_config_candidate(candidate).await?)
    }

    /// fetch a config candidate by id
    pub async fn get_config_candidate(
        &self,
        candidate_id: &str,
    ) -> Result<Option<ConfigCandidateRecord>, IpError<T::Error>> {
        Ok(self.store.get_config_candidate(candidate_id).await?)
    }

    /// list config candidates, newest first
    pub async fn list_config_candidates(
        &self,
    ) -> Result<Vec<ConfigCandidateRecord>, IpError<T::Error>> {
        Ok(self.store.list_config_candidates().await?)
    }

    /// the currently active config candidate, if any
    pub async fn active_config_candidate(
        &self,
    ) -> Result<Option<ConfigCandidateRecord>, IpError<T::Error>> {
        Ok(self.store.active_config_candidate().await?)
    }

    /// atomically supersede the current active candidate and activate another
    pub async fn activate_config_candidate(
        &self,
        candidate_id: &str,
        activated_at: SystemTime,
    ) -> Result<(), IpError<T::Error>> {
        Ok(self
            .store
            .activate_config_candidate(candidate_id, activated_at)
            .await?)
    }

    pub async fn get(&self, ip: IpAddr) -> Result<Option<State>, IpError<T::Error>> {
        Ok(self.store.get(ip).await?)
    }

    /// sees if there is an un-expired IP associated with this ID
    /// Returns
    ///     Err if expired or id not found
    ///     Ok(ip) un-expired id found in storage
    pub async fn lookup_id(&self, id: &[u8]) -> Result<IpAddr, IpError<T::Error>> {
        match self.store.get_id(id).await? {
            Some(ip) => {
                debug!(?ip, ?id, "we have an IP for this id");
                Ok(ip)
            }
            None => {
                debug!(?id, "no IP found for this id");
                Err(IpError::Unreserved)
            }
        }
    }

    /// like [`lookup_id`], but for DHCPv6 bindings (DUID+IAID identity,
    /// `leases_v6` table).
    ///
    /// [`lookup_id`]: IpManager::lookup_id
    pub async fn lookup_id_v6(&self, id: &[u8]) -> Result<IpAddr, IpError<T::Error>> {
        match self.store.get_id_v6(id).await? {
            Some(ip) => {
                debug!(?ip, ?id, "we have a v6 address for this id");
                Ok(ip)
            }
            None => {
                debug!(?id, "no v6 address found for this id");
                Err(IpError::Unreserved)
            }
        }
    }

    /// look up a client's existing delegated prefix (base + length) by DUID+IAID
    pub async fn lookup_id_pd(&self, id: &[u8]) -> Result<Option<(IpAddr, u8)>, IpError<T::Error>> {
        Ok(self.store.get_id_pd(id).await?)
    }

    /// Extend an existing, unexpired binding's lease time. Unlike [`try_lease`],
    /// this never creates a binding: it returns `Ok(Some(ip))` only when a
    /// binding for `(ip, id)` exists and is unexpired, else `Ok(None)`. Used for
    /// DHCPv6 Renew/Rebind, where the client claims an address it already holds
    /// and the server must answer NoBinding if it has no record.
    ///
    /// [`try_lease`]: IpManager::try_lease
    pub async fn renew(
        &self,
        ip: IpAddr,
        id: &[u8],
        expires_at: SystemTime,
    ) -> Result<Option<IpAddr>, IpError<T::Error>> {
        Ok(self
            .store
            .update_unexpired(ip, IpState::Lease, id, expires_at, Some(id))
            .await?)
    }

    /// Delegate a prefix from `pool` for this binding id (IA_PD). Reuses the
    /// client's existing delegation if it has one, otherwise scans the pool for
    /// the first prefix that is free, expired, or already ours. `network` is the
    /// subnet recorded against the binding.
    ///
    /// The scan is bounded (`MAX_PD_SCAN`); a pool wider than that will only
    /// allocate from its first `MAX_PD_SCAN` prefixes.
    pub async fn allocate_pd(
        &self,
        pool: &config::v6::PdPool,
        network: IpAddr,
        id: &[u8],
        expires_at: SystemTime,
        state: IpState,
    ) -> Result<Option<(Ipv6Addr, u8)>, IpError<T::Error>> {
        const MAX_PD_SCAN: usize = 65_536;
        let dlen = pool.delegated_len();

        // reuse an existing delegation for this client, but only if it belongs to
        // this pool (the client may have roamed to a network with different
        // pd_pools; a stale delegation from another pool must not be handed back)
        if let Some((IpAddr::V6(base), len)) = self.store.get_id_pd(id).await?
            && len == dlen
            && pool.prefix().contains(&base)
        {
            self.store
                .upsert_pd(IpAddr::V6(base), len, network, id, expires_at, Some(state))
                .await?;
            return Ok(Some((base, len)));
        }

        let now = SystemTime::now();
        for base in pool.iter_prefixes().take(MAX_PD_SCAN) {
            let claimable = match self.store.get_pd(IpAddr::V6(base), dlen).await? {
                None => true,
                // reuse if the existing binding is expired or already this client's
                Some(existing) => {
                    let info = existing.as_ref();
                    info.expires_at() < now || info.id() == Some(id)
                }
            };
            if claimable {
                self.store
                    .upsert_pd(IpAddr::V6(base), dlen, network, id, expires_at, Some(state))
                    .await?;
                return Ok(Some((base, dlen)));
            }
        }
        Ok(None)
    }

    /// Pin a specific delegated prefix for a client (an IA_PD reservation), but
    /// only if it is free, expired, or already this client's — never steal a
    /// live delegation held by someone else. Returns `Ok(None)` when the prefix
    /// is in use, so the caller can fall back to normal delegation.
    pub async fn reserve_pd(
        &self,
        base: Ipv6Addr,
        dlen: u8,
        network: IpAddr,
        id: &[u8],
        expires_at: SystemTime,
        state: IpState,
    ) -> Result<Option<(Ipv6Addr, u8)>, IpError<T::Error>> {
        let now = SystemTime::now();
        let claimable = match self.store.get_pd(IpAddr::V6(base), dlen).await? {
            None => true,
            Some(existing) => {
                let info = existing.as_ref();
                info.expires_at() < now || info.id() == Some(id)
            }
        };
        if claimable {
            self.store
                .upsert_pd(IpAddr::V6(base), dlen, network, id, expires_at, Some(state))
                .await?;
            Ok(Some((base, dlen)))
        } else {
            Ok(None)
        }
    }

    /// Extend an existing delegated prefix (IA_PD Renew/Rebind). Never creates a
    /// binding: `Ok(Some(prefix))` only if `(prefix, prefix_len, id)` exists and
    /// is unexpired.
    pub async fn renew_pd(
        &self,
        prefix: IpAddr,
        prefix_len: u8,
        id: &[u8],
        expires_at: SystemTime,
    ) -> Result<Option<IpAddr>, IpError<T::Error>> {
        Ok(self
            .store
            .renew_pd(prefix, prefix_len, id, expires_at)
            .await?)
    }

    /// Release a delegated prefix (IA_PD Release).
    pub async fn release_pd(
        &self,
        prefix: IpAddr,
        prefix_len: u8,
        id: &[u8],
    ) -> Result<Option<ClientInfo>, IpError<T::Error>> {
        Ok(self.store.release_pd(prefix, prefix_len, id).await?)
    }
    /// Sets a reserved ip/id combo to leased state. If no un-expired ip/id pair
    /// found, then if we're authoritative we will just try to insert the IP, and
    /// if not we return.
    /// Returns
    ///     Err if ip/id don't match what's in storage or if it's expired
    ///     Ok(()) entry created successfully for lease
    pub async fn try_lease<N: NetworkParams>(
        &self,
        ip: IpAddr,
        id: &[u8],
        expires_at: SystemTime,
        network: &N,
    ) -> Result<(), IpError<T::Error>> {
        match self
            .store
            .update_unexpired(ip, IpState::Lease, id, expires_at, Some(id))
            .await?
        {
            Some(ip) => {
                debug!(
                    ?ip,
                    ?id,
                    "found ip for id-- updating expiry and setting leased"
                );
                Ok(())
            }
            None if network.authoritative() => {
                debug!(
                    ?ip,
                    ?id,
                    "no IP with this id found or expired. authoritative, trying insert"
                );

                // this will ACK even if there was no prior DISCOVER
                match self
                    .store
                    .insert(ip, network.subnet(), id, expires_at, Some(IpState::Lease))
                    .await
                {
                    Ok(()) => {
                        trace!("inserted new IP");
                        Ok(())
                    }
                    Err(err) => {
                        warn!(
                            ?err,
                            "insert failed, likely ip already exists & taken by another client"
                        );
                        Err(IpError::AddrInUse(ip))
                    }
                }
            }
            None => {
                debug!(?ip, ?id, "no IP with this id found or expired");
                Err(IpError::AddrInUse(ip))
            }
        }
    }

    /// release the requested ip if the (ip, id) pair matches
    /// Returns
    ///     Ok(None) if ip did not exist in storage
    ///     Ok(Some(info)) the existing client info
    ///     Err(_) for database error
    pub async fn release_ip(
        &self,
        ip: IpAddr,
        id: &[u8],
    ) -> Result<Option<ClientInfo>, IpError<T::Error>> {
        // TODO: this deletes the entry, but we don't really need to
        Ok(self.store.release_ip(ip, id).await?)
    }

    /// Will mark IP for probation if it is un-expired and ip/id match
    /// we check to see if it has expired because a DECLINE happens after
    /// an address has been ACKd.
    pub async fn probate_ip(
        &self,
        ip: IpAddr,
        id: &[u8],
        expires_at: SystemTime,
    ) -> Result<(), IpError<T::Error>> {
        match self
            .store
            .update_unexpired(ip, IpState::Probate, id, expires_at, None)
            .await?
        {
            Some(ip) => {
                debug!(
                    ?ip,
                    ?id,
                    "found ip for id-- updating expiry and set PROBATION"
                );
                Ok(())
            }
            None => {
                debug!(
                    ?ip,
                    ?id,
                    "tried to set PROBATION, but no IP with this id found"
                );
                Err(IpError::AddrInUse(ip))
            }
        }
    }
}

#[derive(Error, Debug)]
pub enum IpError<E> {
    #[error("ip is leased {0:?}")]
    Leased(ClientInfo),
    #[error("ip is reserved {0:?}")]
    Reserved(ClientInfo),
    #[error("ip is unreserved")]
    Unreserved,
    #[error("database error")]
    DbError(#[from] E),
    #[error("this address is already in use {0:?}")]
    AddrInUse(IpAddr),
    #[error("error getting next IP in range {range:?}")]
    RangeError { range: RangeInclusive<IpAddr> },
    #[error("error getting next IP in range {range:?} inside attempts {attempts:?}")]
    MaxAttempts {
        range: RangeInclusive<IpAddr>,
        attempts: usize,
    },
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::sqlite::SqliteDb;
    use config::LeaseTime;
    use config::v4::{NetRange, Network};
    use tracing_test::traced_test;

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

    // get multiple first-available IPs in a range
    // this mimics what happens when multiple clients simultaneously 'DISCOVER'
    #[tokio::test]
    #[traced_test]
    async fn test_first_available() -> Result<()> {
        let mgr = IpManager::new(SqliteDb::new("sqlite::memory:").await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()]);
        let client_id = &[1, 2, 3, 4, 5, 6];
        let expires_at = SystemTime::now() + Duration::from_secs(60);
        let ip = mgr
            .reserve_first(&range, &network, client_id, expires_at, None)
            .await?;
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)));
        assert_eq!(
            mgr.lookup_id(client_id).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100))
        );

        let client_id = &[2, 2, 3, 4, 5, 6];
        let ip = mgr
            .reserve_first(&range, &network, client_id, expires_at, None)
            .await?;
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 101)));
        assert_eq!(
            mgr.lookup_id(client_id).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 101))
        );

        let client_id = &[3, 2, 3, 4, 5, 6];
        let ip = mgr
            .reserve_first(&range, &network, client_id, expires_at, None)
            .await?;
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 102)));
        assert_eq!(
            mgr.lookup_id(client_id).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 102))
        );

        let client_id = &[4, 2, 3, 4, 5, 6];
        let ip = mgr
            .reserve_first(&range, &network, client_id, expires_at, None)
            .await?;
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 103)));
        assert_eq!(
            mgr.lookup_id(client_id).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 103))
        );

        Ok(())
    }

    //
    #[tokio::test]
    #[traced_test]
    async fn test_reserve_first() -> Result<()> {
        let mgr = IpManager::new(SqliteDb::new("sqlite::memory:").await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()]);
        let client_id = &[1, 2, 3, 4, 5, 6];
        let expires_at = SystemTime::now() + Duration::from_secs(1);
        let ip = mgr
            .reserve_first(&range, &network, client_id, expires_at, None)
            .await?;
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)));
        assert_eq!(
            mgr.lookup_id(client_id).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100))
        );

        tokio::time::sleep(Duration::from_secs(2)).await;

        // try another range with the same client id-- should delete previous expired
        // entry
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 5, 100)..=Ipv4Addr::new(192, 168, 5, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.5.0/24".parse()?)
            .set_ranges(vec![range.clone()]);
        let client_id = &[1, 2, 3, 4, 5, 6];
        let expires_at = SystemTime::now() + Duration::from_secs(1);
        let ip = mgr
            .reserve_first(&range, &network, client_id, expires_at, None)
            .await?;

        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 5, 100)));
        assert_eq!(
            mgr.lookup_id(client_id).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 5, 100))
        );

        Ok(())
    }

    // DISCOVER - ACK
    // get lease on discover like in a rapid commit response
    #[tokio::test]
    #[traced_test]
    async fn test_first_available_ack() -> Result<()> {
        let mgr = IpManager::new(SqliteDb::new("sqlite::memory:").await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()]);
        let client_id = &[1, 2, 3, 4, 5, 6];
        let expires_at = SystemTime::now() + Duration::from_secs(60);
        // go straight to lease
        let ip = mgr
            .reserve_first(
                &range,
                &network,
                client_id,
                expires_at,
                Some(IpState::Lease),
            )
            .await?;
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)));
        assert_eq!(
            mgr.lookup_id(client_id).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100))
        );

        Ok(())
    }

    // do reserve and lease in 2 steps like usual
    #[tokio::test]
    #[traced_test]
    async fn test_lease() -> Result<()> {
        let mgr = IpManager::new(SqliteDb::new("sqlite::memory:").await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()]);
        let client_id = &[1, 2, 3, 4, 5, 6];
        let expires_at = SystemTime::now() + Duration::from_secs(5);
        // reserve from range
        let ip = mgr
            .reserve_first(&range, &network, client_id, expires_at, None)
            .await?;
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)));

        // make lease
        mgr.try_lease([192, 168, 1, 100].into(), client_id, expires_at, &network)
            .await?;
        let ip = mgr.lookup_id(client_id).await?;
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)));

        Ok(())
    }

    // add some leases then select *
    #[tokio::test]
    #[traced_test]
    async fn test_select_all() -> Result<()> {
        let mgr = IpManager::new(SqliteDb::new("sqlite::memory:").await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()]);
        let mut res = vec![];

        let expires_at = SystemTime::now() + Duration::from_secs(30);
        for i in 0..5 {
            let client_id = &[1, 1, 1, 1, 1, 1 + i];
            // reserve from range
            let ip = mgr
                .reserve_first(&range, &network, client_id, expires_at, None)
                .await?;
            assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100 + i)));

            // make lease
            mgr.try_lease(ip, client_id, expires_at, &network).await?;
            let state = mgr.get(ip).await?.expect("not found");
            assert_eq!(
                state.as_ref().ip(),
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100 + i))
            );
            assert_eq!(state.as_ref().id().unwrap(), &[1, 1, 1, 1, 1, 1 + i]);

            res.push(State::Leased(ClientInfo {
                ip: state.as_ref().ip(),
                id: Some(client_id.to_vec().clone()),
                // systtime we get back has no nano seconds
                expires_at: state.as_ref().expires_at(),
                network: network.subnet().into(),
            }));
        }

        let leases = mgr.select_all().await?;
        assert_eq!(
            leases.into_iter().collect::<HashSet<_>>(),
            res.into_iter().collect()
        );

        Ok(())
    }

    // do reserve and lease in 2 steps like usual
    #[tokio::test]
    #[traced_test]
    async fn test_lease_authoritative() -> Result<()> {
        let mgr = IpManager::new(SqliteDb::new("sqlite::memory:").await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()])
            .set_authoritative(true);
        let client_id = &[1, 2, 3, 4, 5, 6];
        let expires_at = SystemTime::now() + Duration::from_secs(1);
        // reserve from range, expires in 1s
        let ip = mgr
            .reserve_first(&range, &network, client_id, expires_at, None)
            .await?;
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)));

        // different client makes a lease using just a REQUEST
        let client_id = &[1, 2, 3, 4, 5, 7];
        mgr.try_lease(
            [192, 168, 1, 101].into(),
            client_id,
            SystemTime::now() + Duration::from_secs(5),
            &network,
        )
        .await?;
        let ip = mgr.lookup_id(client_id).await?;
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 101)));

        tokio::time::sleep(Duration::from_secs(2)).await;

        // client 1's reserve expired, reserve it again
        let client_id = &[1, 2, 3, 4, 5, 8];
        let ip = mgr
            .reserve_first(&range, &network, client_id, expires_at, None)
            .await?;
        // ip 100 available now since client 1 never claimed it
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)));

        Ok(())
    }

    // reserve 2 ips then ack them both
    #[tokio::test]
    #[traced_test]
    async fn test_multiple_ranges() -> Result<()> {
        let mgr = IpManager::new(SqliteDb::new("sqlite::memory:").await?)?;
        let range_a = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let range_b = NetRange::new(
            Ipv4Addr::new(10, 10, 1, 100)..=Ipv4Addr::new(10, 10, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network_a = Network::default();
        network_a
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range_a.clone()]);
        let mut network_b = Network::default();
        network_b
            .set_subnet("10.10.1.0/24".parse()?)
            .set_ranges(vec![range_b.clone()]);
        // reserve from range a
        {
            let client_id = &[1, 2, 3, 4, 5, 6];
            let expires_at = SystemTime::now() + Duration::from_secs(5);
            let ip = mgr
                .reserve_first(&range_a, &network_a, client_id, expires_at, None)
                .await?;
            assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)));
        }
        // reserve from range b
        {
            let client_id = &[2, 2, 3, 4, 5, 6];
            let expires_at = SystemTime::now() + Duration::from_secs(5);
            let ip = mgr
                .reserve_first(&range_b, &network_b, client_id, expires_at, None)
                .await?;
            assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(10, 10, 1, 100)));
        }
        mgr.try_lease(
            [192, 168, 1, 100].into(),
            &[1, 2, 3, 4, 5, 6],
            SystemTime::now() + Duration::from_secs(60),
            &network_a,
        )
        .await?;
        assert_eq!(
            mgr.lookup_id(&[1, 2, 3, 4, 5, 6]).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100))
        );
        // make other range lease
        mgr.try_lease(
            [10, 10, 1, 100].into(),
            &[2, 2, 3, 4, 5, 6],
            SystemTime::now() + Duration::from_secs(60),
            &network_b,
        )
        .await?;
        assert_eq!(
            mgr.lookup_id(&[2, 2, 3, 4, 5, 6]).await?,
            IpAddr::V4(Ipv4Addr::new(10, 10, 1, 100))
        );

        Ok(())
    }

    // programmatically fill a range
    #[tokio::test]
    #[traced_test]
    async fn test_fill_range() -> Result<()> {
        let mgr = IpManager::new(SqliteDb::new("sqlite::memory:").await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()]);

        // fill up range with new clients
        for range_ip in range.iter() {
            let client_id = (1..6).map(|_| rand::random()).collect::<Vec<u8>>();
            let expires_at = SystemTime::now() + Duration::from_secs(60);
            let ip = mgr
                .reserve_first(&range, &network, &client_id, expires_at, None)
                .await?;
            assert_eq!(range_ip, ip);
            assert_eq!(mgr.lookup_id(&client_id).await?, range_ip);
        }

        // range is empty, should error
        let expires_at = SystemTime::now() + Duration::from_secs(60);
        let ip = mgr
            .reserve_first(&range, &network, &[2, 3, 4, 6, 6], expires_at, None)
            .await;
        assert!(ip.is_err());

        Ok(())
    }

    // test RELEASE
    #[tokio::test]
    #[traced_test]
    async fn test_release_ip() -> Result<()> {
        let mgr = IpManager::new(SqliteDb::new("sqlite::memory:").await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()]);

        // lease an IP
        let client_id = (1..6).map(|_| rand::random()).collect::<Vec<u8>>();
        let expires_at = SystemTime::now() + Duration::from_secs(60);
        let ip = mgr
            .reserve_first(
                &range,
                &network,
                &client_id,
                expires_at,
                Some(IpState::Lease),
            )
            .await?;
        assert_eq!(mgr.lookup_id(&client_id).await?, ip);

        // release IP
        let info = mgr.release_ip(ip, &client_id).await?;
        assert_eq!(
            info.unwrap().ip,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100))
        );

        // try a new client, should get the same IP
        let client_id = (1..6).map(|_| rand::random()).collect::<Vec<u8>>();
        let expires_at = SystemTime::now() + Duration::from_secs(60);
        let _ip = mgr
            .reserve_first(
                &range,
                &network,
                &client_id,
                expires_at,
                Some(IpState::Lease),
            )
            .await?;
        assert_eq!(
            mgr.lookup_id(&client_id).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100))
        );
        Ok(())
    }

    // test DECLINE
    #[tokio::test]
    #[traced_test]
    async fn test_probate_ip() -> Result<()> {
        let mgr = IpManager::new(SqliteDb::new("sqlite::memory:").await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()]);

        // lease an IP
        let client_id = (1..6).map(|_| rand::random()).collect::<Vec<u8>>();
        let expires_at = SystemTime::now() + Duration::from_secs(60);
        let ip = mgr
            .reserve_first(
                &range,
                &network,
                &client_id,
                expires_at,
                Some(IpState::Lease),
            )
            .await?;
        assert_eq!(mgr.lookup_id(&client_id).await?, ip);

        // probate IP
        mgr.probate_ip(ip, &client_id, SystemTime::now() + Duration::from_secs(180))
            .await?;
        assert!(mgr.lookup_id(&client_id).await.is_err());

        // try a new client, should skip probated IP
        let client_id = (1..6).map(|_| rand::random()).collect::<Vec<u8>>();
        let expires_at = SystemTime::now() + Duration::from_secs(60);
        let _ip = mgr
            .reserve_first(
                &range,
                &network,
                &client_id,
                expires_at,
                Some(IpState::Lease),
            )
            .await?;
        assert_eq!(
            mgr.lookup_id(&client_id).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 101))
        );
        Ok(())
    }

    // test ping failure
    #[tokio::test]
    #[traced_test]
    async fn test_ping_fail() -> Result<()> {
        let mgr = IpManager::new(SqliteDb::new("sqlite::memory:").await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()])
            .set_ping_check(true);
        // mark this IP as in-use in the DAD cache
        let ip = Ipv4Addr::new(192, 168, 1, 100);
        mgr.ping_insert(ip.into(), true).await;
        // lease an IP
        let client_id = (1..6).map(|_| rand::random()).collect::<Vec<u8>>();
        let expires_at = SystemTime::now() + Duration::from_secs(60);
        let _ip = mgr
            .reserve_first(
                &range,
                &network,
                &client_id,
                expires_at,
                Some(IpState::Lease),
            )
            .await?;
        assert_eq!(
            mgr.lookup_id(&client_id).await?,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 101))
        );
        Ok(())
    }

    // test bad lookup
    #[tokio::test]
    #[traced_test]
    async fn test_bad_lookup() -> Result<()> {
        let mgr = IpManager::new(SqliteDb::new("sqlite::memory:").await?)?;
        let range = NetRange::new(
            Ipv4Addr::new(192, 168, 1, 100)..=Ipv4Addr::new(192, 168, 1, 255),
            LeaseTime::new(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(10),
            ),
        );
        let mut network = Network::default();
        network
            .set_subnet("192.168.1.0/24".parse()?)
            .set_ranges(vec![range.clone()])
            .set_ping_check(true);

        // lease an IP
        let client_id = (1..6).map(|_| rand::random()).collect::<Vec<u8>>();

        assert!(mgr.lookup_id(&client_id).await.is_err());
        Ok(())
    }

    // the generalized allocator hands out v6 addresses from a v6 config pool
    // through the same reserve_first path used by v4.
    #[tokio::test]
    #[traced_test]
    async fn test_v6_reserve_first_generalized() -> Result<()> {
        use config::DhcpConfig;
        let cfg = DhcpConfig::parse_str(include_str!("../../config/sample/config_v6_pools.yaml"))
            .unwrap();
        let (_subnet, net) = cfg.v6().get_first().expect("a v6 network");
        let range = &net.ranges()[0];

        let mgr = IpManager::new(SqliteDb::new("sqlite::memory:").await?)?;
        let exp = SystemTime::now() + Duration::from_secs(60);

        // first two clients get sequential addresses from the pool start
        let a = mgr
            .reserve_first(range, net, &[1, 1, 1], exp, Some(IpState::Reserve))
            .await?;
        assert_eq!(a, IpAddr::V6("2001:db8:1::100".parse()?));
        let b = mgr
            .reserve_first(range, net, &[2, 2, 2], exp, Some(IpState::Reserve))
            .await?;
        assert_eq!(b, IpAddr::V6("2001:db8:1::101".parse()?));

        // the reserved v6 binding is found by DUID+IAID identity, and not by the
        // v4 identity lookup
        assert_eq!(mgr.lookup_id_v6(&[1, 1, 1]).await?, a);
        assert!(mgr.lookup_id(&[1, 1, 1]).await.is_err());

        // commit the lease (Request/Reply) and confirm it persists as Leased
        mgr.try_lease(a, &[1, 1, 1], exp, net).await?;
        assert!(matches!(mgr.get(a).await?, Some(State::Leased(_))));
        Ok(())
    }

    // renew() extends an existing binding but never creates one (Renew/Rebind)
    #[tokio::test]
    #[traced_test]
    async fn test_renew_extends_only_existing() -> Result<()> {
        use config::DhcpConfig;
        let cfg = DhcpConfig::parse_str(include_str!("../../config/sample/config_v6_pools.yaml"))
            .unwrap();
        let (_subnet, net) = cfg.v6().get_first().expect("a v6 network");
        let range = &net.ranges()[0];

        let mgr = IpManager::new(SqliteDb::new("sqlite::memory:").await?)?;
        let now = SystemTime::now();
        let ip = mgr
            .reserve_first(
                range,
                net,
                &[7, 7, 7],
                now + Duration::from_secs(60),
                Some(IpState::Lease),
            )
            .await?;

        // existing (ip,id) -> extended
        let later = now + Duration::from_secs(120);
        assert_eq!(mgr.renew(ip, &[7, 7, 7], later).await?, Some(ip));
        // wrong id -> no binding created/extended
        assert_eq!(mgr.renew(ip, &[9, 9, 9], later).await?, None);
        // address we don't hold -> None (no insert)
        let other = IpAddr::V6("2001:db8:1::105".parse()?);
        assert_eq!(mgr.renew(other, &[7, 7, 7], later).await?, None);
        assert!(
            mgr.get(other).await?.is_none(),
            "renew must not create a binding"
        );
        Ok(())
    }

    // IA_PD: delegate /64s from a /56 pool, reuse per client, renew, release
    #[tokio::test]
    #[traced_test]
    async fn test_allocate_pd() -> Result<()> {
        use config::DhcpConfig;
        let cfg = DhcpConfig::parse_str(include_str!("../../config/sample/config_v6_pools.yaml"))
            .unwrap();
        let (_subnet, net) = cfg.v6().get_first().expect("a v6 network");
        let pool = &net.pd_pools()[0]; // 2001:db8:100::/56 delegating /64
        let subnet = IpAddr::V6(net.full_subnet().network());

        let mgr = IpManager::new(SqliteDb::new("sqlite::memory:").await?)?;
        let exp = SystemTime::now() + Duration::from_secs(60);

        // sequential clients get consecutive /64s from the pool start
        let (p1, len) = mgr
            .allocate_pd(pool, subnet, &[1, 1, 1], exp, IpState::Lease)
            .await?
            .unwrap();
        assert_eq!(len, 64);
        assert_eq!(p1, "2001:db8:100::".parse::<Ipv6Addr>()?);
        let (p2, _) = mgr
            .allocate_pd(pool, subnet, &[2, 2, 2], exp, IpState::Lease)
            .await?
            .unwrap();
        assert_eq!(p2, "2001:db8:100:1::".parse::<Ipv6Addr>()?);

        // same client reuses its delegation
        let (p1b, _) = mgr
            .allocate_pd(pool, subnet, &[1, 1, 1], exp, IpState::Lease)
            .await?
            .unwrap();
        assert_eq!(p1b, p1);

        // renew extends, release frees
        let later = exp + Duration::from_secs(60);
        assert_eq!(
            mgr.renew_pd(IpAddr::V6(p1), 64, &[1, 1, 1], later).await?,
            Some(IpAddr::V6(p1))
        );
        assert!(
            mgr.release_pd(IpAddr::V6(p1), 64, &[1, 1, 1])
                .await?
                .is_some()
        );
        // renewing a released prefix returns None (no binding)
        assert_eq!(
            mgr.renew_pd(IpAddr::V6(p1), 64, &[1, 1, 1], later).await?,
            None
        );
        Ok(())
    }
}
