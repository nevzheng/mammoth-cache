# Bazel

> [!NOTE]
> The `/bazel/` routes are served today, off by default. Acceptance tests with a real Bazel client
> are still to come.

mammoth-cache serves Bazel's plain
[HTTP remote cache](https://bazel.build/remote/caching#http-caching), with SHA-256 digests,
Bazel's default. The gRPC cache (REAPI Capabilities, ActionCache, CAS and ByteStream) is in
progress and not on `main` yet; until it lands, `--remote_cache=grpc://` needs
[bazel-remote](https://github.com/buchgr/bazel-remote). Remote execution is out of scope:
mammoth-cache is a build cache, not a remote build system.

## Turn it on

Bazel is off unless the config file asks for it:

```toml
[protocols]
bazel = true
```

Gradle and Maven stay on unless you turn them off too.

## Configure the build

Point Bazel at the node's `/bazel` path, for example in `.bazelrc`:

```text
build --remote_cache=https://cache.example.com/bazel
```

Bazel appends `/ac/<digest>` for action results and `/cas/<digest>` for outputs.

## Credentials

With [authentication](authentication.md) on, put the user and token in the URL, which Bazel sends
as HTTP Basic:

```sh
bazel build //... --remote_cache=https://ci:${MAMMOTH_TOKEN}@cache.example.com/bazel
```

Or send a Bearer token with `--remote_header=Authorization="Bearer ${MAMMOTH_TOKEN}"`, or from a
`--credential_helper`. Reads need `cache:read` and uploads `cache:write`. Keep tokens out of a
checked-in `.bazelrc`; put them in a user `.bazelrc` or pass them on the command line in CI.

## How it differs from Gradle

- **Two stores.** The action cache (`/ac/`) holds action results keyed by the action's digest,
  and a new result replaces the old one. The content-addressed store (`/cas/`) holds outputs keyed
  by their own SHA-256, and never changes.
- **Uploads are verified.** A CAS upload that doesn't hash to its digest gets `400` and is not
  stored. An upload to a digest that is already stored answers `200` and reads and discards the
  body, so the stored blob never changes and the connection stays usable.
- **Oversized stores get `413`.** Bazel warns about a failed upload and keeps using the cache.
- **Size for the CAS.** The node doesn't check that an action result's outputs are still cached.
  On a hit with a missing output, Bazel runs the action again. With `--remote_download_minimal`,
  also set `--experimental_remote_cache_eviction_retries`, and give the cache room so outputs
  aren't evicted long before the results that point at them.
