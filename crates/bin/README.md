# dora

`dora` can be run with both cli options or from environment variables, see help for more info:

```
❯ cargo run -- --help

dora is a DHCP server written from the ground up in Rust

Usage: dora [OPTIONS]

Options:
  -c, --config-path <CONFIG_PATH>      path to dora's config [env: CONFIG_PATH=] [default: /var/lib/dora/config.yaml]
      --v4-addr <V4_ADDR>              the v4 address to listen on [env: V4_ADDR=] [default: 0.0.0.0:67]
      --v6-addr <V6_ADDR>              the v6 address to listen on [env: V6_ADDR=] [default: [::]:547]
      --external-api <EXTERNAL_API>    the management HTTP API address to listen on [env: EXTERNAL_API=] [default: 127.0.0.1:3333]
      --timeout <TIMEOUT>              default timeout, dora will respond within this window or drop [env: TIMEOUT=] [default: 3]
      --max-live-msgs <MAX_LIVE_MSGS>  max live messages before new messages will begin to be dropped [env: MAX_LIVE_MSGS=] [default: 1000]
      --channel-size <CHANNEL_SIZE>    channel size for various mpsc chans [env: CHANNEL_SIZE=] [default: 10000]
      --threads <THREADS>              How many threads are spawned, default is the # of logical CPU cores [env: THREADS=]
      --thread-name <THREAD_NAME>      Worker thread name [env: THREAD_NAME=] [default: dora-dhcp-worker]
      --dora-id <DORA_ID>              ID of this instance [env: DORA_ID=] [default: dora_id]
      --dora-log <DORA_LOG>            set the log level. All valid RUST_LOG arguments are accepted [env: DORA_LOG=] [default: info]
  -d <DATABASE_URL>                    Postgres connection string for the lease/state store, e.g. postgres://user:pass@host:5432/dbname. dora runs the embedded migrations against it on startup [env: DATABASE_URL=] [default: postgres://dora:dora@localhost/dora]
  -h, --help                           Print help
```

## Example

Run on non-standard ports:

```
dora -c /path/to/config.yaml --v4-addr 0.0.0.0:9900
```

is equivalent to:

```
V4_ADDR="0.0.0.0:9900" CONFIG_PATH="/path/to/config.yaml" dora
```

Use `DORA_LOG` to control dora's log level. Takes same arguments as `RUST_LOG`
