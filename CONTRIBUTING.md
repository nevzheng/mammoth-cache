# Contributing to mammoth-cache

Thanks for your interest in mammoth-cache. This guide covers how changes get in.

## Before you start

Questions and bigger proposals belong in
[GitHub Discussions](https://github.com/nevzheng/mammoth-cache/discussions). For anything larger
than a small fix, start a Discussion or open an issue first so we can agree on the direction before
you write code.

## Workflow

1. Fork the repository and create a branch from `main`.
2. Make your change, with tests and docs where they apply.
3. Sign off every commit (see [Developer Certificate of Origin](#developer-certificate-of-origin)).
4. Open a pull request against `main` and fill in the template.

Pull requests are squash-merged, so the PR title becomes the commit message on `main`. See
[Commit and PR title conventions](#commit-and-pr-title-conventions).

## Commit and PR title conventions

We follow [Conventional Commits 1.0.0](https://www.conventionalcommits.org/en/v1.0.0/). Because
pull requests are squash-merged, the PR title is the commit that lands on `main`, so the PR title
is what CI checks. Commits inside your branch can be named however you like.

Format: `<type>(<optional scope>): <subject>`, for example
`feat(storage): add disk eviction by size`.

| Type       | Use for                                                   |
| ---------- | --------------------------------------------------------- |
| `feat`     | A new feature                                             |
| `fix`      | A bug fix                                                 |
| `docs`     | Documentation only                                        |
| `style`    | Formatting, no code meaning change                        |
| `refactor` | A code change that neither fixes a bug nor adds a feature |
| `perf`     | A performance improvement                                 |
| `test`     | Adding or fixing tests                                    |
| `build`    | Build system or dependencies                              |
| `ci`       | CI configuration                                          |
| `chore`    | Other maintenance                                         |
| `revert`   | Reverting a previous commit                               |

Mark a breaking change with `!` after the type or scope (`feat(api)!: drop v1 endpoints`), or with
a `BREAKING CHANGE:` footer in the commit body.

## Code review

Review comments follow [Conventional Comments](https://conventionalcomments.org/):
`<label> [decorations]: <subject>`. Labels:

- `praise`: something done well.
- `nitpick`: a trivial, preference-based request.
- `suggestion`: a proposed improvement.
- `issue`: a problem that needs addressing.
- `todo`: a small, necessary change.
- `question`: something the reviewer wants clarified.
- `thought`: an idea that came up while reviewing.
- `chore`: a process task, such as updating a changelog.
- `note`: something the reader should know.

Add `(blocking)` or `(non-blocking)` to say whether the comment must be resolved before merging.
For example:

```text
suggestion (non-blocking): Extract the eviction loop into its own function so it can be tested.
```

## Developer Certificate of Origin

Every commit must carry a `Signed-off-by` line. Add it with `git commit -s`. By signing off you
certify the [Developer Certificate of Origin](https://developercertificate.org/): that you wrote the
change, or otherwise have the right to submit it under the project's open-source licenses.

Forgot to sign off? Run `git commit --amend -s` for the last commit, or `git rebase --signoff main`
for a branch, then force-push.

## Building and testing

mammoth-cache is pre-alpha. It builds with [Bazel](https://bazel.build/) (bzlmod and
[`rules_rust`](https://bazelbuild.github.io/rules_rust/)) and with Cargo. Bazel is the build of
record: CI builds, lints and tests with it. Cargo stays supported for rust-analyzer, quick
iteration and feature builds; CI uses it only for what Bazel doesn't cover (other allocators, the
MSRV check, cargo-deny and the REAPI code generator).

Install [Bazelisk](https://github.com/bazelbuild/bazelisk) as `bazel`; it reads `.bazelversion` and
fetches the pinned Bazel. Bazel downloads the Rust toolchain itself, so no `rustup` is needed for a
Bazel build.

```sh
bazel build //...
bazel test //...
bazel run //crates/mammoth-cache:mammoth   # listens on 0.0.0.0:5071; pass -- --config <file>
bazel build --config=lint //...            # clippy and rustfmt over every Rust target
bazel coverage //crates/...                # line coverage; scripts/coverage-summary.py reads it
```

With Cargo, the toolchain is pinned in `rust-toolchain.toml`, and the minimum supported Rust version
is the `rust-version` in `Cargo.toml`.

```sh
cargo build
cargo run --bin mammoth   # listens on 0.0.0.0:5071, memory only; see docs/user/configuration.md
```

### How the two builds stay in step

- **Dependencies:** `Cargo.toml` and `Cargo.lock` are the source of truth. Add a crate with
  `cargo add`; Bazel's crate_universe reads both files on the next build. No BUILD file changes are
  needed unless you add a new crate to the workspace.
- **Lints:** the `[workspace.lints]` table in `Cargo.toml` applies to Bazel's rustc and clippy too,
  and `clippy.toml` and `rustfmt.toml` are shared.
- **Rust version:** `MODULE.bazel` repeats the version in `rust-toolchain.toml`. Bump both together;
  `scripts/check-toolchain-sync.sh` fails CI when they differ.
- **New source files** and integration tests under `tests/` are picked up by the globs in each
  crate's `BUILD.bazel`.

Put machine-specific Bazel settings, such as a disk cache or registry mirror, in a git-ignored
`user.bazelrc` at the repository root:

```sh
echo 'common --disk_cache=~/.cache/bazel-disk' >> user.bazelrc
```

To share a cache between machines, point Bazel at a mammoth node with `protocols.bazel` on (see
[docs/user/bazel.md](docs/user/bazel.md)):

```sh
echo 'build --remote_cache=http://localhost:5071/bazel' >> user.bazelrc
```

Each test file under `tests/` is its own Bazel test target, so they already run in parallel and
cache separately; none is long enough to be worth sharding.

A container image is built from the `Dockerfile`. Only releases publish images, to
`ghcr.io/nevzheng/mammoth-cache` (see [docs/design/0009-releases.md](docs/design/0009-releases.md));
to run something newer than the latest release, build it yourself:

```sh
docker build -t mammoth-cache:dev .
docker run --rm -p 5071:5071 mammoth-cache:dev
```

## Local checks

There are no Git hooks: CI runs the checks below on pull requests, with warnings treated as
errors, and skips the ones the changed files can't affect. Run the relevant ones before pushing:

```sh
bazel test --config=ci --config=lint //...   # build, test, clippy, rustfmt, ruff, pyrefly
bazel run @buildifier_prebuilt//:buildifier -- -mode=check -lint=warn -r "$PWD"
scripts/check-toolchain-sync.sh
uv lock --check --directory tests           # tests/uv.lock matches tests/pyproject.toml
cargo clippy --workspace --all-targets --locked --no-default-features --features jemalloc
cargo nextest run --workspace --locked --no-default-features --features jemalloc
cargo +1.91 check --workspace --locked   # MSRV
cargo deny check                         # licenses, advisories, sources
rumdl check .
actionlint                               # workflow files
```

## Documentation

Project docs live in [docs/](docs/README.md) and are published with
[mdBook](https://rust-lang.github.io/mdBook/) to <https://nevzheng.github.io/mammoth-cache/>.
Preview them with `scripts/docs.sh serve`; colors and logos come from
[branding/](branding/README.md). Changes that affect the protocol, storage, configuration or
operations need a [design doc](docs/design/README.md).

Markdown is linted with [rumdl](https://rumdl.dev/) using the repository's `.rumdl.toml`. Before
pushing doc changes, run:

```sh
rumdl fmt .
rumdl check .
```

CI runs the same check on every pull request.

## Licensing

mammoth-cache is dual-licensed under [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE). Unless you
explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by
you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional
terms or conditions.

## Code of conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md). By participating, you agree to
uphold it.
