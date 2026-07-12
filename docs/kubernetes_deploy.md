# Deploying dora on Kubernetes with `kubectl`

A step-by-step guide to running dora on Kubernetes (or K3s) with Cilium, using
the Kustomize manifests in [`deploy/`](../deploy). For a reference of what each
manifest contains, see [`deploy/README.md`](../deploy/README.md).

`kubectl apply -k` runs Kustomize built into `kubectl`, so no Helm or extra
tooling is required.

## Architecture

dora runs as several workloads that share one PostgreSQL database:

| Workload (k8s name) | image | Exposed as |
| --- | --- | --- |
| `usg-dora-v4-server` | `usg-dora-v4` | anycast VIP, UDP/67 |
| `usg-dora-v6-server` | `usg-dora-v6` | anycast VIP, UDP/547 |
| `usg-dora-api` | `usg-dora-api` | site-local IP, TCP/3333 |
| `usg-dora-migrate` | `usg-dora-migrate` | run-once `Job`, no network |
| `usg-dora-db` | PostgreSQL | in-cluster only |

Each service is its **own single-binary image**. They share lease state,
reservations, and config through Postgres (`DATABASE_URL`). The services do not
migrate on startup — the `usg-dora-migrate` `Job` applies the schema once before
the servers roll out. The DHCP servers sit behind a **Cilium anycast VIP**
(advertised via BGP); the management API gets a separate **site-local** IP.

> Kubernetes object names can't contain `_`, so the workloads use hyphens; the
> `usg-dora-v4_server` underscore form is kept as the `dora.io/workload` label.

## Prerequisites

- A Kubernetes or K3s cluster with **Cilium** as the CNI, configured with:
  - `kubeProxyReplacement=true`
  - LB-IPAM enabled (ships with Cilium)
  - the **BGP control plane** enabled (`bgpControlPlane.enabled=true`), ideally
    with an existing peering to your router (dora ships only a
    `CiliumBGPAdvertisement` and plugs into that peering — see Step 5)
  - recommended: `loadBalancer.mode=dsr` for the DHCP Services (so replies keep
    the VIP as their source and relays accept them)
- An upstream router that will peer BGP with the cluster and accept the VIPs.
- `kubectl` v1.27+ (for the Kustomize `replacements` used by the VIP variables).
- The container image `ghcr.io/192d-wing/usg-dora` must be pullable by the nodes
  (it is published by the release workflow). If the GHCR package is private,
  either make it public or add an `imagePullSecret` (see Troubleshooting).

> **K3s:** install k3s with `--disable=servicelb --flannel-backend=none
> --disable-network-policy` and then install Cilium, otherwise k3s's bundled
> Klipper load-balancer fights Cilium for the LoadBalancer IPs. Use the
> `overlays/k3s` overlay (it sets the `local-path` storage class).

## Step 1 — Get the manifests

```sh
git clone https://github.com/192d-Wing/dora
cd dora
```

Everything below edits files under `deploy/`.

## Step 2 — Set the database password

Edit [`deploy/base/db-secret.yaml`](../deploy/base/db-secret.yaml). Replace the
`CHANGE_ME` values and make `DATABASE_URL` match:

```yaml
stringData:
  POSTGRES_USER: dora
  POSTGRES_PASSWORD: "<a strong password>"
  POSTGRES_DB: dora
  DATABASE_URL: postgres://dora:<same password>@usg-dora-db:5432/dora
```

For production, manage this out of band (SealedSecrets / External Secrets / SOPS /
Vault) rather than committing plaintext.

## Step 3 — Set your DHCP config

Edit the `config.yaml` in [`deploy/base/dora-config.yaml`](../deploy/base/dora-config.yaml)
to describe your networks, ranges, and options. Keep `interfaces:` naming an
interface that exists in the pod (default `eth0`); DHCP arrives relayed, and dora
selects the network from the relay, not the receiving NIC.

## Step 4 — Set the VIPs

Edit [`deploy/base/vips.yaml`](../deploy/base/vips.yaml) — this ConfigMap is the
single source of truth for all three addresses:

```yaml
data:
  ipv4_vip: "203.0.113.10"      # DHCPv4 anycast VIP
  ipv6_vip: "2001:db8:a11::10"  # DHCPv6 anycast VIP
  api_vip:  "10.201.0.10"       # management API site-local IP
```

Kustomize `replacements` copy each value into both the Service's requested
address and its Cilium LB-IPAM pool, so you set each VIP in exactly one place.
Make sure the pool CIDRs in
[`deploy/base/cilium-lb-ipam.yaml`](../deploy/base/cilium-lb-ipam.yaml) still
contain your chosen VIPs (the anycast pool holds the v4+v6 VIPs; the site-local
pool holds the API VIP).

## Step 5 — Wire BGP advertisement

dora does **not** set up its own BGP peering — it plugs into the peering your
cluster already has. Cilium's `CiliumBGPPeerConfig` selects which advertisements
to send by label, so all you do is make dora's `CiliumBGPAdvertisement` carry
that label.

Find the label your peer config expects:

```sh
kubectl get ciliumbgppeerconfig -o yaml | grep -A2 advertisements
# e.g.  advertisements:
#         matchLabels:
#           advertise: k3s-pod-cidrs
```

Then set the same value on the `advertise:` label in
[`deploy/base/cilium-bgp.yaml`](../deploy/base/cilium-bgp.yaml) (it defaults to
`k3s-pod-cidrs`). The advertisement selects dora's Services by their
`dora.io/lb-pool` labels, so no ASNs or peer addresses are needed here.

> **No BGP peering yet?** If `kubectl get ciliumbgpclusterconfig` is empty, apply
> [`deploy/examples/cilium-bgp-peer.example.yaml`](../deploy/examples/cilium-bgp-peer.example.yaml)
> first (edit its ASNs and peer address), then continue.
>
> The Cilium CRDs here are `cilium.io/v2` (current Cilium). On older Cilium that
> still serves `cilium.io/v2alpha1`, adjust the `apiVersion` accordingly.

## Step 6 — (optional) Set the image and storage class

- **Images** default to `ghcr.io/192d-wing/usg-dora-{v4,v6,api,migrate}:latest`.
  To pin a version or use your own mirror, set each image:

  ```sh
  cd deploy/overlays/k8s   # or overlays/k3s
  kustomize edit set image usg-dora-v4=ghcr.io/192d-wing/usg-dora-v4:v0.1.0
  kustomize edit set image usg-dora-v6=ghcr.io/192d-wing/usg-dora-v6:v0.1.0
  kustomize edit set image usg-dora-api=ghcr.io/192d-wing/usg-dora-api:v0.1.0
  kustomize edit set image usg-dora-migrate=ghcr.io/192d-wing/usg-dora-migrate:v0.1.0
  cd -
  ```

- **Storage class** for the Postgres volume is set by the overlay
  (`standard` for k8s, `local-path` for k3s). Edit the overlay's
  `kustomization.yaml` patch to match your cluster.

## Step 7 — Preview, then apply

Render the manifests to review them first:

```sh
kubectl kustomize deploy/overlays/k8s      # or overlays/k3s
```

Then apply:

```sh
kubectl apply -k deploy/overlays/k8s       # full Kubernetes
# or
kubectl apply -k deploy/overlays/k3s       # K3s
```

## Step 8 — Verify

```sh
# workloads and services in the dora namespace
kubectl -n dora get pods,svc

# the LoadBalancer Services should show EXTERNAL-IP = your VIPs
kubectl -n dora get svc usg-dora-v4-server usg-dora-v6-server usg-dora-api

# Cilium assigned the VIPs from the pools
kubectl get ciliumloadbalancerippool

# BGP session is established and advertising the VIPs
cilium bgp peers
cilium bgp routes advertised ipv4 unicast
```

Expected: the four pods are `Running`, the three LoadBalancer Services have your
VIPs as `EXTERNAL-IP`, and the BGP session to your router is `established`.

## Step 9 — Point relays and clients at the VIPs

- Configure your DHCP relays (`ip helper-address` / DHCPv6 relay destination) to
  forward to the v4 and v6 anycast VIPs.
- Reach the management API at the site-local VIP on port 3333, e.g.
  `curl http://<api_vip>:3333/health`. The OpenAPI docs are at
  `http://<api_vip>:3333/docs`.

## Using the management API

The API fails closed by default. Configure a **bearer token** by creating the
`dora-api` secret before (or after) applying — the deployment reads it if present:

```sh
kubectl -n dora create secret generic dora-api \
  --from-literal=token="$(openssl rand -hex 32)"
kubectl -n dora rollout restart deploy/usg-dora-api
```

For production prefer **mTLS**: mount a server cert/key and a client-CA and set
`--external-api-tls-cert` / `--external-api-tls-key` / `--external-api-tls-client-ca`
(see [management-api.md](./management-api.md)). With a client-CA and no bearer
token, mTLS is mandatory.

## Upgrading

Pin a new image tag and re-apply, or roll the deployments:

```sh
cd deploy/overlays/k8s
kustomize edit set image usg-dora=ghcr.io/192d-wing/usg-dora:v0.2.0
cd -
kubectl apply -k deploy/overlays/k8s
```

New pods run the embedded migrations on startup, so schema changes apply
automatically. Config changes: edit `dora-config.yaml`, `kubectl apply -k`, then
`kubectl -n dora rollout restart deploy/usg-dora-v4-server deploy/usg-dora-v6-server`
(and `deploy/usg-dora-api`) to pick up the new config.

## Uninstalling

```sh
kubectl delete -k deploy/overlays/k8s
```

This removes the workloads, Services, and Cilium pool/BGP resources. The Postgres
`PersistentVolumeClaim` may be retained depending on your storage class's reclaim
policy; delete it explicitly to wipe lease data:

```sh
kubectl -n dora delete pvc -l app.kubernetes.io/name=usg-dora-db
```

## Troubleshooting

- **Pods stuck `ImagePullBackOff`** — the GHCR package is private. Make it public
  in the repo/org package settings, or create a pull secret and reference it:

  ```sh
  kubectl -n dora create secret docker-registry ghcr \
    --docker-server=ghcr.io --docker-username=<user> --docker-password=<token>
  ```

  then add `imagePullSecrets: [{name: ghcr}]` to the pod specs (via a patch).

- **LoadBalancer Service stuck `<pending>`** — Cilium LB-IPAM isn't assigning an
  IP. Check the VIP falls inside its pool's CIDR
  (`kubectl get ciliumloadbalancerippool -o yaml`) and that LB-IPAM is enabled.

- **VIP assigned but not reachable** — BGP isn't advertising it. Check
  `cilium bgp peers` (session `established`?) and that your router accepts the
  route. Confirm `externalTrafficPolicy: Local` nodes actually run a ready pod.

- **dora `CrashLoopBackOff`, logs show a database error** — check `DATABASE_URL`
  in the `dora-db` secret matches the Postgres credentials and that
  `usg-dora-db` is `Running` and `Ready`.

- **dora logs "unable to find interface ..."** — the config's `interfaces:` names
  an interface not present in the pod. Use `eth0` (the default pod interface).

- **DHCP replies not reaching relays** — enable Cilium DSR
  (`loadBalancer.mode=dsr`) so replies keep the VIP as their source address.
