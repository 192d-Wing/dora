# Creating a dora container image

The build is defined in [`infra-build/Containerfile`](../infra-build/Containerfile)
(multi-arch: `linux/amd64` and `linux/arm64`). The runtime base is the Iron Bank
hardened UBI 10 image, so you must first authenticate to `registry1.dso.mil`:

```sh
podman login registry1.dso.mil   # or: docker login registry1.dso.mil
```

After checking out the source, build the image **from the repo root** (the build
context needs the whole workspace), pointing at the Containerfile:

```sh
podman build -f infra-build/Containerfile -t dora .
```

With Docker (use buildx for multi-arch):

```sh
docker build -f infra-build/Containerfile -t dora .
docker buildx build --platform linux/amd64,linux/arm64 -f infra-build/Containerfile -t dora .
```

Next, create a `data` directory if it does not exist, and put `config.yaml` in it. This directory is used to read the config. Lease state lives in **PostgreSQL** (not a file), so you also need a reachable Postgres and a `DATABASE_URL`.

```sh
mkdir data
touch data/config.yaml
```

(edit config.yaml)

Then run the image you created with `--net=host`, the data dir volume mounted, and `DATABASE_URL` pointing at your Postgres (dora runs its migrations on startup):

```sh
docker run -it --rm --init --net=host \
  -v "$(pwd)/data":/var/lib/dora \
  -e DATABASE_URL=postgres://user:pass@localhost:5432/dora \
  dora
```

For a quick local Postgres you can run one in a container first, e.g.
`docker run --rm -e POSTGRES_USER=dora -e POSTGRES_PASSWORD=dora -e POSTGRES_DB=dora -p 5432:5432 postgres:16`.
