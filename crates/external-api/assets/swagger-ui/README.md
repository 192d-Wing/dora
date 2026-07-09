# Vendored Swagger UI

`swagger-ui-bundle.js` and `swagger-ui.css` are the [Swagger UI][swagger-ui]
distribution, **vendored on purpose** so `GET /docs` renders offline — dora runs
in a hardened, often air-gapped container with no access to a CDN. They are
embedded into the binary via `include_str!` and served by public handlers in
`../../src/lib.rs`. `index.html` is dora's own shell that boots Swagger UI
against `GET /openapi.json`.

- **Version:** swagger-ui-dist@5.17.14
- **License:** Apache-2.0 (© SmartBear Software)

## Updating

Pull a pinned version from the `swagger-ui-dist` npm package and re-vendor the
two dist files (do not hand-edit them):

```sh
VER=5.17.14
base="https://cdn.jsdelivr.net/npm/swagger-ui-dist@${VER}"
curl -fsSL "$base/swagger-ui-bundle.js" -o swagger-ui-bundle.js
curl -fsSL "$base/swagger-ui.css"       -o swagger-ui.css
```

Then bump the version above and run the `external-api` tests.

[swagger-ui]: https://github.com/swagger-api/swagger-ui
