# Contributing

## Rule #1

Nothing from the game, or any other proprietary source, goes into this
repository or a pull request: no assets, shader dumps, decompiled code,
extracted tables or saves. See
[docs/asset-policy.md](docs/asset-policy.md). `cargo xtask install-hooks` installs
the check that runs on every commit.

## Reporting a problem

Use the "Something went wrong" form and attach both logs, as the top of the
[README](README.md) describes. Search the open issues first: if the same
problem is there, add your logs to it instead of opening another.

Questions about running it go to
[Discussions, Q&A](https://github.com/trustno1-coder/VIctory-NVIDIA/discussions/categories/q-a),
ideas to [Ideas](https://github.com/trustno1-coder/VIctory-NVIDIA/discussions/categories/ideas).

## Before you write code

Search the open pull requests for the same files and the same problem. Many
are already there more than once. If one covers it, test it and say so on it
instead of sending another. If you start on an issue, say so on the issue.

## Pull requests

- Based on a recent `main`, one topic each. Follow-up work goes in a new pull
  request.
- The title says what is true after the change, with its area first:
  `GPU: a panicking pipeline build is refused`.
- New behaviour sits behind a `VICTORY_*` switch; the description names it and
  its default.
- The three commands of the CI pass:

  ```
  cargo clippy --workspace --all-targets --locked -- -D warnings
  cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
  cargo test --workspace --locked
  ```

- Say what you ran it on: GPU, driver, Windows, and what you measured.
- Code ported from another fork names it, the pull request or commit, and its
  licence.

Every pull request is read line by line and, where it touches the game, run on
the maintainer's card before it goes in. It goes in as commits rebased onto
`main` that keep you as their author, so the pull request is closed rather than
merged; the closing comment names the commit.

## Testing

Running a pull request on a card the maintainer does not have, and saying what
you saw on it, helps as much as code. Testers are credited in the
[README](README.md#credits).
