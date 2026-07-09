# Deploying dora on Kubernetes (Cilium)

Kustomize manifests to run dora as four workloads against a shared PostgreSQL,
with the DHCP servers behind an **anycast VIP** and the management API on a
separate **site-local** address. Works on both full Kubernetes and K3s via
overlays.

## Architecture

| Workload | k8s name | dora role | Exposure |
| --- | --- | --- | --- |
| `usg-dora-v4_server` | `usg-dora-v4-server` | `--role v4` | anycast VIP, UDP/67 |
| `usg-dora-v6_server` | `usg-dora-v6-server` | `--role v6` | anycast VIP, UDP/547 |
| `usg-dora-api` | `usg-dora-api` | `--role api` | site-local IP, TCP/3333 |
| `usg-dora-db` | `usg-dora-db` | — (PostgreSQL) | in-cluster only |

> **Naming:** Kubernetes object names can't contain `_`, so the workload names
> use hyphens (`usg-dora-v4-server`). The exact underscore form is preserved on
> each workload as the `dora.io/workload` label.

All three dora roles are the **same image** running with a different `--role`
(see the `--role` flag). They share one Postgres (`usg-dora-db`) via
`DATABASE_URL`, which is how separate v4/v6/api pods share lease state, runtime
reservations, operation/audit records, and config candidates. dora runs its
embedded migrations against Postgres on startup.

## Networking (Cilium)

- **Anycast VIP for the DHCP servers.** The v4 and v6 Services are `LoadBalancer`
  type, drawing from the `dora-anycast` Cilium LB-IPAM pool (one v4 VIP, one v6
  VIP — different addresses because different families). The Cilium **BGP control
  plane** advertises those VIPs to your upstream router; with the servers spread
  across nodes (`externalTrafficPolicy: Local` + pod anti-affinity), the router
  ECMP-load-balances to the nearest ready replica — true anycast. Point your
  DHCP relays' `helper-address` / server-address at these VIPs.
- **Site-local IP for the API.** The API Service draws from a separate
  `dora-site-local` pool, so management traffic uses a distinct, internally
  routable address — never the DHCP anycast VIP.

### DHCP-over-L3 caveat

Because clients reach the servers through an L3 anycast VIP, DHCP must arrive
**relayed** (unicast, with `giaddr` / a v6 relay link-address); dora selects the
network from the relay, not the receiving interface. Enable Cilium **DSR** mode
(`loadBalancer.mode=dsr`) for the DHCP Services so replies keep the VIP as their
source address and the relay accepts them.

## Prerequisites

- **Cilium** as the CNI with:
  - `kubeProxyReplacement=true`
  - LB-IPAM enabled (ships with Cilium)
  - **BGP control plane** enabled (`bgpControlPlane.enabled=true`)
  - recommended: `loadBalancer.mode=dsr` for the DHCP Services
- `cilium-bgp.yaml` uses the **BGP Control Plane v2** CRDs (Cilium ≥ 1.16:
  `CiliumBGPClusterConfig` / `CiliumBGPPeerConfig` / `CiliumBGPAdvertisement`).
  On Cilium 1.14–1.15 replace it with a `CiliumBGPPeeringPolicy`.
- An upstream router willing to peer BGP and accept the advertised VIPs.

## What you MUST edit before applying

1. **Image** — defaults to `ghcr.io/192d-wing/usg-dora:latest` (published by the
   `release.yml` workflow). Pin a version or point at your own mirror via
   `deploy/base/kustomization.yaml` `images:`, e.g.
   `kustomize edit set image usg-dora=ghcr.io/192d-wing/usg-dora:v1.2.3`.
2. **DB secret** — `deploy/base/db-secret.yaml` (`POSTGRES_PASSWORD`,
   `DATABASE_URL`). Replace with a real, out-of-band-managed secret.
3. **dora config** — `deploy/base/dora-config.yaml` (`config.yaml`): your
   networks, ranges, and options. `interfaces:` must name an interface present
   in the pod (default `eth0`).
4. **VIPs** — `deploy/base/vips.yaml` (the `dora-vips` ConfigMap): the three
   values `ipv4_vip`, `ipv6_vip`, `api_vip`. This is the single source of truth —
   Kustomize `replacements` copy each into both its Service's requested address
   (`io.cilium/lb-ipam-ips`) and its Cilium LB-IPAM pool block, so you set each
   VIP in exactly one place. To vary per environment, patch this ConfigMap's
   `data` in an overlay.
5. **BGP** — `deploy/base/cilium-bgp.yaml` local/peer ASNs and the peer address.
6. **Storage class** — the overlay patch (`standard` for k8s, `local-path` for
   k3s) to match your cluster.

## Deploy

```sh
# full Kubernetes
kubectl apply -k deploy/overlays/k8s

# K3s
kubectl apply -k deploy/overlays/k3s
```

Preview the rendered manifests without applying:

```sh
kubectl kustomize deploy/overlays/k8s
```

## Securing the API

The API is unauthenticated by default. To require a **bearer token**, create the
`dora-api` secret (the deployment reads it if present):

```sh
kubectl -n dora create secret generic dora-api --from-literal=token="$(openssl rand -hex 32)"
```

For production prefer **mTLS**: mount a server cert/key and a client-CA bundle
and set `--external-api-tls-cert` / `--external-api-tls-key` /
`--external-api-tls-client-ca` (see [`docs/management-api.md`](../docs/management-api.md)).
With a client-CA and no bearer token, mTLS is mandatory.
