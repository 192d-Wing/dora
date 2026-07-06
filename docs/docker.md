# Creating a dora container image

The build is defined in a [`Containerfile`](../Containerfile) (multi-arch:
`linux/amd64` and `linux/arm64`). The runtime base is the Iron Bank hardened
UBI 10 image, so you must first authenticate to `registry1.dso.mil`:

```sh
podman login registry1.dso.mil   # or: docker login registry1.dso.mil
```

After checking out the source, build the image. Podman finds the `Containerfile`
automatically:

```sh
podman build -t dora .
```

With Docker, point at the file explicitly (and use buildx for multi-arch):

```sh
docker build -f Containerfile -t dora .
docker buildx build --platform linux/amd64,linux/arm64 -f Containerfile -t dora .
```

Next, create a `data` directory if it does not exist, and put `config.yaml` in it. This directory will be used to read the config and to store your leases database file.

```sh
mkdir data
touch data/config.yaml
```

(edit config.yaml)

Then run the image you created with `--net=host` and with the data dir volume mounted:

```sh
docker run -it --rm --init --net=host -v "$(pwd)/data":/var/lib/dora dora
```
