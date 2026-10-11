# Embedded deployment (`dora-embedded`)

`dora-embedded` runs the DHCPv4 server, the DHCPv6 server and the management
API in **one process** on **one [redb](https://github.com/cberner/redb) file**.
It is the dora to run on a router, an appliance or any single host that has no
PostgreSQL. For multi-replica or role-split deployments use the Postgres
services and the [Helm chart](../deploy/README.md) instead; the architecture
comparison is in [architecture.md](./architecture.md#embedded-deployment).

- [Running it](#running-it)
- [The store file](#the-store-file)
- [Durability and flash wear](#durability-and-flash-wear)
- [Backup and restore](#backup-and-restore)
- [Upgrades and the schema version](#upgrades-and-the-schema-version)
- [Resources](#resources)
- [Non-goals](#non-goals)

## Running it

```sh
dora-embedded -c /etc/dora/config.yaml -d redb:///var/lib/dora/dora.redb
```

`dora-embedded` accepts every flag the split services do (`--v4-addr`,
`--v6-addr`, `--external-api`, TLS, `--threads`, logging, …; `--help` lists
them) plus:

| flag / env | default | meaning |
| --- | --- | --- |
| `-d`, `--database-url`, `DATABASE_URL` | `redb:///var/lib/dora/dora.redb` | the store. Must be a `redb:///absolute/path` URL (three slashes). A `postgres://` URL is rejected: that backend belongs to the split services. |
| `--redb-sync`, `DORA_REDB_SYNC` | `immediate` | `immediate` fsyncs every commit; `lazy` flushes periodically (see below) |
| `--redb-flush-ms`, `DORA_REDB_FLUSH_MS` | `1000` | flush period for `--redb-sync lazy` |

There is **no migrate step**: the store is created if missing and its schema is
upgraded on open. The v6 server starts only when the config has a `v6` section.
The management API runs in the same process and its changes (server mode,
runtime reservations, config candidates) take effect immediately in the
datapath — there is no polling interval as in the split deployment.

The container image is `usg-dora-embedded`; see [docker.md](./docker.md#the-embedded-image).

## The store file

- **One writer.** redb takes an exclusive lock on the file. A second
  `dora-embedded` on the same file fails to start with a clear error; so would
  any other tool that opens it for writing while the server runs. Stop the
  server before touching the file.
- **Ownership.** The file is created by the user the server runs as; it needs a
  writable directory. The container entrypoint creates `dora.redb` in the
  mounted data dir and chowns it like the rest of the mount.
- **Contents.** Leases (v4, v6 IA_NA, v6 IA_PD), runtime reservations,
  operations/audit records, config candidates and the server mode — the same
  state the Postgres schema holds. Values are compact binary records; the file
  is not meant to be read by other tools. Use the management API to inspect
  leases.
- **Size.** A lease record is a few dozen bytes; redb's copy-on-write pages
  mean the file grows in 4 KiB steps and reuses freed pages, so even a busy
  router stays in the low megabytes.

## Durability and flash wear

Every DHCP write (offer, ack, renew, release, probation) is one redb write
transaction. With the default **`--redb-sync immediate`** each commit is
fsynced before the reply is sent, so a lease the client was told about is on
disk before the client has it. The costs on flash storage (eMMC, SD):

- an fsync is typically 2–10 ms, which caps the server at roughly 100–400 DORA
  exchanges per second (two to three commits each). Far above what a router
  sees; renewals are one commit.
- each commit rewrites a handful of 4 KiB pages. At a realistic few hundred
  leases a day this is negligible wear; it only matters if the box is a busy
  relay target.

**`--redb-sync lazy`** commits without fsync and issues one durable commit
every `--redb-flush-ms`. This collapses the write cost to one fsync per
interval. The trade-off: after a power loss the server forgets whatever was
committed in the last interval, while those clients still hold their addresses.
When such a client renews, the server has no binding and treats the request as
a new one; a *different* client could meanwhile have been offered the same
address, so a duplicate is possible until one of them backs off. With lease
times of hours and a one-second window this is rare, but it is real. Keep
`immediate` unless flash wear or write latency is a measured problem.

Both modes survive `SIGKILL` and power loss without corrupting the file: redb
commits are atomic, so the store is always at the last completed commit.

## Backup and restore

Stop the server, copy the file, start the server. redb's file format is
self-contained; a copy taken while the server is stopped is a complete backup.
Do not copy the file while the server is running: the copy may be of a state
between two commits.

To restore, stop the server, replace the file, start it again. A file from an
older dora is upgraded in place on open (see below); a file from a newer dora is
refused.

To start fresh (for example after a pool change where you want every binding
forgotten), stop the server and delete the file.

## Upgrades and the schema version

The file carries a schema version. On open, `dora-embedded` applies any
pending upgrades inside a single transaction and records the new version, so an
interrupted upgrade leaves the previous version intact. A file whose version is
newer than the running build is refused with
`database schema version N is newer than this build supports`, so a downgrade
needs the backup taken before the upgrade.

## Resources

- **Threads.** The default tokio worker count is the number of logical CPUs. On
  a 2–4 core router `--threads 2` is plenty; the store runs its blocking I/O on
  tokio's blocking pool and serializes writes, so more workers do not speed up
  leasing.
- **Memory.** The store's page cache is capped at 64 MiB (redb's own default of
  1 GiB is far too much for an appliance); typical steady-state use is well
  below that because the data set is small.
- **Ports.** As the split services: UDP 67 and 547 (privileged), the API on
  3333 by default. Bind the API to a management address with `--external-api`
  and protect it with `DORA_API_TOKEN` and/or TLS as in the other deployments.

## Non-goals

- **Role subsets** (`--role v4` and the like) — the point of the embedded binary
  is one process on one file; a second process could not open the store.
- **Kubernetes / high availability** — the redb store has exactly one writer. For
  replicas and anycast VIPs use the Postgres services and the Helm chart.
- **Reading the file from other programs** while the server runs.
