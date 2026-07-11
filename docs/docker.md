# Creating dora container images

dora is split into one image per service — `dora-v4`, `dora-v6`, `dora-api`, and
the run-once `dora-migrate` — each built from the same
[`infra-build/Containerfile`](../infra-build/Containerfile) (multi-arch:
`linux/amd64` and `linux/arm64`) by passing the `SERVICE` build-arg. The runtime
base is the Iron Bank hardened UBI 10 image, so you must first authenticate to
`registry1.dso.mil`:

```sh
podman login registry1.dso.mil   # or: docker login registry1.dso.mil
```

After checking out the source, build an image **from the repo root** (the build
context needs the whole workspace), selecting the service with `--build-arg SERVICE=...`:

```sh
podman build -f infra-build/Containerfile --build-arg SERVICE=v4      -t usg-dora-v4 .
podman build -f infra-build/Containerfile --build-arg SERVICE=v6      -t usg-dora-v6 .
podman build -f infra-build/Containerfile --build-arg SERVICE=api     -t usg-dora-api .
podman build -f infra-build/Containerfile --build-arg SERVICE=migrate -t usg-dora-migrate .
```

With Docker (use buildx for multi-arch):

```sh
docker build -f infra-build/Containerfile --build-arg SERVICE=v4 -t usg-dora-v4 .
docker buildx build --platform linux/amd64,linux/arm64 -f infra-build/Containerfile --build-arg SERVICE=v4 -t usg-dora-v4 .
```

Next, create a `data` directory if it does not exist, and put `config.yaml` in it. This directory is used to read the config. Lease state lives in **PostgreSQL** (not a file), so you also need a reachable Postgres and a `DATABASE_URL`.

```sh
mkdir data
touch data/config.yaml
```

(edit config.yaml)

The services no longer migrate on startup, so first apply the schema once with
the migrate image, then run the servers with `--net=host`, the data dir volume
mounted, and `DATABASE_URL` pointing at your Postgres:

```sh
# once: create/upgrade the schema
docker run -it --rm --init \
  -e DATABASE_URL=postgres://user:pass@localhost:5432/dora \
  usg-dora-migrate --dora-log info

# then a server (v4 shown; usg-dora-v6 / usg-dora-api run the same way)
docker run -it --rm --init --net=host \
  -v "$(pwd)/data":/var/lib/dora \
  -e DATABASE_URL=postgres://user:pass@localhost:5432/dora \
  usg-dora-v4 -c /var/lib/dora/config.yaml
```

For a quick local Postgres you can run one in a container first, e.g.
`docker run --rm -e POSTGRES_USER=dora -e POSTGRES_PASSWORD=dora -e POSTGRES_DB=dora -p 5432:5432 postgres:16`.
