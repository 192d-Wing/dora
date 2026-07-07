# dora config cli

```
dora-cfg 0.1.0
dora is a DHCP server written from the ground up in Rust

USAGE:
    dora-cfg --path <PATH> --format <FORMAT>

OPTIONS:
    -f, --format <FORMAT>    print the parsed wire format or the dora internal config format
                             [possible values: wire, internal]
    -h, --help               Print help information
    -p, --path <PATH>        path to dora config. We will determine format from extension. If no
                             extension, we will attempt JSON & YAML
    -V, --version            Print version information
```

## JSON schema

`config_schema.json` (repo root) is a JSON Schema for the dora config. Validate
a JSON config against it with:

```sh
dora-cfg --path config.json --schema config_schema.json
```

The schema is kept honest by `tests/schema.rs`, which validates every shipped
sample config (v4 + v6) against it on `cargo test` — so the schema fails CI if it
drifts from the config format.
