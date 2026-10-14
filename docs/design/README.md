# Design docs

Design docs and decision records explain how mammoth-cache works and why it was built that way.
Write one for any change that affects the protocol, storage format, configuration or operations, or
that is hard to reverse.

## Naming

Files are numbered in order: `NNNN-short-title.md`, for example `0001-storage-layout.md`. Copy
[0000-template.md](0000-template.md) to start. The template follows the structure of Google's
[design docs](https://www.industrialempathy.com/posts/design-docs-at-google/).

## Status

Each doc has a status:

| Status     | Meaning                                         |
| ---------- | ----------------------------------------------- |
| Draft      | Under discussion; may change freely.            |
| Accepted   | Agreed and being, or already, implemented.      |
| Superseded | Replaced by a later doc, which it links to.     |

## Proposing a design

1. For big ideas, start a thread in
   [GitHub Discussions](https://github.com/nevzheng/mammoth-cache/discussions) first.
2. Open a pull request adding the doc with status Draft.
3. Once reviewers agree, set the status to Accepted and merge.
