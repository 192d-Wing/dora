# Deploying dora on Kubernetes (Cilium)

Helm chart to run dora as four workloads against a shared PostgreSQL,
with the DHCP servers behind an **anycast VIP** and the management API on a
separate **site-local** address.

## Quick start

```sh
# Deploy the windep site
helm install dora deploy/chart/ -f deploy/chart/sites/windep/values.yaml

# Deploy with an external config file instead of a bundled site
helm install dora deploy/chart/ --set-file doraConfig=path/to/config.yaml

# Upgrade
helm upgrade dora deploy/chart/ -f deploy/chart/sites/windep/values.yaml
```

## Site configs

Each site is a directory under `deploy/chart/sites/<site>/` containing:

- **`config.yaml`** — the DHCP config (networks, ranges, options)
- **`values.yaml`** — site-specific Helm values (VIPs, BGP label, storage class, DB password)

The site's `config.yaml` is loaded into the `dora-config` ConfigMap via the
`site:` value. The site's `values.yaml` is passed with `-f` to override the
chart defaults.

To add a new site, create a new directory under `deploy/chart/sites/` with both
files. To use a config file outside the chart, pass it with
`--set-file doraConfig=path/to/config.yaml`.

## What you MUST edit

1. **VIPs** — `values.yaml` → `vips.ipv4`, `vips.ipv6`, `vips.api`
2. **DB password** — `values.yaml` → `db.password` (or use an external secret)
3. **DHCP config** — the site config file in `deploy/chart/sites/`
4. **BGP label** — `values.yaml` → `bgp.advertiseLabel` (must match your
   `CiliumBGPPeerConfig`'s `advertisements.matchLabels`)
5. **Storage class** — `values.yaml` → `db.storageClass`
6. **API token** — create the `dora-api` secret:

   ```sh
   kubectl -n dora create secret generic dora-api \
     --from-literal=token="$(openssl rand -hex 32)"
   ```

## Architecture

| Workload | k8s name | Image | Exposure |
| --- | --- | --- | --- |
| DHCPv4 server | `usg-dora-v4-server` | `usg-dora-v4` | anycast VIP, UDP/67 |
| DHCPv6 server | `usg-dora-v6-server` | `usg-dora-v6` | anycast VIP, UDP/547 |
| Management API | `usg-dora-api` | `usg-dora-api` | site-local IP, TCP/3333 |
| DB migrator | `usg-dora-migrate` | `usg-dora-migrate` | run-once Job |
| PostgreSQL | `usg-dora-db` | `postgres:16` | in-cluster only |

## Prerequisites

- **Cilium** as the CNI with `kubeProxyReplacement=true`, LB-IPAM enabled, and
  the BGP control plane enabled (`bgpControlPlane.enabled=true`)
- An existing BGP peering to your upstream router (dora's
  `CiliumBGPAdvertisement` plugs into it)
- Recommended: `loadBalancer.mode=dsr` for the DHCP Services

## Securing the API

The API **fails closed** by default — it will not serve requests without
authentication configured.

Configure a **bearer token** by creating the `dora-api` secret (step 6 above).
For production prefer **mTLS**: mount a server cert/key and a client-CA bundle
and set `--external-api-tls-cert` / `--external-api-tls-key` /
`--external-api-tls-client-ca` (see [`docs/management-api.md`](../docs/management-api.md)).

> With a client-CA configured and no bearer token, mTLS is mandatory — the API
> will reject any request that does not present a valid client certificate.

## Upgrading

```sh
helm upgrade dora deploy/chart/
```

## Uninstalling

```sh
helm uninstall dora
```

> The Postgres PVC is not deleted on uninstall. Remove it manually if needed.
