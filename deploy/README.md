# Deploying dora on Kubernetes (Cilium)

Helm chart to run dora as four workloads against a shared PostgreSQL,
with the DHCP servers behind an **anycast VIP** and the management API on a
separate **site-local** address.

## Quick start

```sh
# Deploy the windep site (default)
helm install dora deploy/chart/

# Deploy with a specific site config
helm install dora deploy/chart/ --set site=windep

# Deploy with an external config file
helm install dora deploy/chart/ --set-file doraConfig=path/to/config.yaml

# K3s: set the storage class
helm install dora deploy/chart/ --set db.storageClass=local-path

# Full K8s: set the storage class
helm install dora deploy/chart/ --set db.storageClass=standard
```

## Site configs

DHCP configs live in `deploy/chart/sites/<site>-config.yaml`. Set `site:` in
`values.yaml` (or `--set site=<name>`) to select which config is loaded into the
`dora-config` ConfigMap. To use a config file outside the chart, pass it with
`--set-file doraConfig=path/to/config.yaml`.

To add a new site, drop a `<site>-config.yaml` into `deploy/chart/sites/`.

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

## Upgrading

```sh
helm upgrade dora deploy/chart/
```

## Uninstalling

```sh
helm uninstall dora
```

> The Postgres PVC is not deleted on uninstall. Remove it manually if needed.
