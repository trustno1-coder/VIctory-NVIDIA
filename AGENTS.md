# Guide for coding agents

Read this before changing anything here. [CONTRIBUTING.md](CONTRIBUTING.md) has the rules for every contributor;
this file adds what an agent tends to get wrong, and
[docs/agent_collaboration.md](docs/agent_collaboration.md) how agents of different contributors work together.
These rules are for contributors' agents; the maintainer's own workflow is theirs.

## Hard rules

- **Nothing from the game, or any other proprietary source**: no assets, executables, shader dumps or disassembly,
  extracted tables, saves, SDK files or headers under someone else's licence, in commits, pull requests or test
  fixtures. See
  [docs/asset-policy.md](docs/asset-policy.md); `cargo xtask policy` is authoritative and the hook from
  `cargo xtask install-hooks` runs it on every commit.
- **New behaviour behind a switch.** New behaviour that may not suit every card sits behind a `VICTORY_*` switch;
  the pull request names it and its default. A bug fix does not need one.
- **Your human decides what is published.** An agent does not open, comment on or update a pull request, an issue
  or a discussion without its human's approval.

## Before you start

- Search the open pull requests for the same files and the same problem
  (`gh pr list -R trustno1-coder/VIctory-NVIDIA --state open --search "<words>"`). There are often several already.
  If one covers it, test it and report on it instead of writing it again.
- Search the open issues. If you take one on, say so on it in one line: who, which branch. If someone already
  said so, stop and tell your human instead of writing it a second time.
- One topic per pull request, on a recent `main`. Put `Depends on: #N` for a pull request yours builds on.

## Before you guess, look

- How the runtime is built: [docs/architecture.md](docs/architecture.md). What was measured, and on which card:
  [docs/findings.md](docs/findings.md) and [docs/bug-journal.md](docs/bug-journal.md). Quote the file a claim
  comes from, or call it a hypothesis.
- A figure without the machine (GPU, video memory, RAM) and the build it came from cannot be compared.

## Test without the game first

None of these needs the game files; run them before a pull request, as the CI does:

```
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
```

A run of the game is the human's: an agent runs it only when its human prepared it, and reports it as a `RUN` block
([docs/agent_collaboration.md](docs/agent_collaboration.md#three-blocks)).
