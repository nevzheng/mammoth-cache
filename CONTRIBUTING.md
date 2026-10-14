# Contributing to mammoth-cache

Thanks for your interest in mammoth-cache. This guide covers how changes get in.

## Before you start

For anything larger than a small fix, open an issue (or a Discussion, once enabled) first so we can
agree on the direction before you write code.

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

mammoth-cache is pre-alpha. It will build with [Bazel](https://bazel.build/) (bzlmod and
`rules_rust`), and plain Cargo will keep working. Build and test commands will land here with the
first code.

## Documentation

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
