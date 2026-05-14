# choros

A small TUI for managing short-lived multi-repo work environments. One choros per Jira-task-sized unit of work.

## How it works

Choros is project-scoped to the directory it's launched in. Run it from the directory where you want your choros to live.

```
<cwd>/
├── .choros-config/
│   └── registry/
│       ├── service-api/        ← clone these yourself (MVP)
│       └── service-web/
├── PROJ-1234/                  ← a choros, created by choros
│   ├── .choros-meta.toml
│   ├── service-api/            ← fresh clone of the registry repo
│   └── service-web/
└── PROJ-1235/
    └── ...
```

A directory is recognized as a choros iff it contains `.choros-meta.toml`.

## Setup

For now you populate the registry yourself:

```bash
mkdir -p .choros-config/registry
git clone git@github.com:your-org/service-api.git .choros-config/registry/service-api
git clone git@github.com:your-org/service-web.git .choros-config/registry/service-web
```

The TUI scans `.choros-config/registry/` on launch and lets you pick repos from there.

## Install

```bash
./install.sh                                       # ~/.local/bin/choros
INSTALL_DIR=/usr/local/bin sudo -E ./install.sh    # system-wide
```

Or with cargo directly:

```bash
cargo install --path .
```

## Usage

Run from inside the directory you want choros to live in.

```bash
choros                          # full TUI (manage existing + create new)
choros work                     # fast-create: just name + repos, then exit
choros work PROJ-1              # name pre-filled, jumps to repo selection
choros work PROJ-1 api web      # fully non-interactive (no TUI)
choros archive                  # archive the workspace your shell is in
choros archive PROJ-1           # archive PROJ-1 from the project root
choros agent save settings      # inside a workspace: TUI to promote claude
                                # settings entries up to the template
```

`choros work` is the quick path for "I want a fresh choros right now". It skips the main screen and drops you straight into the name + repo picker. With the shell integration below, your shell is `cd`'d into the new choros on success.

### Shell integration

`choros` is an external binary, so it cannot change your shell's `cwd` on its own. Add this one line to your shell rc to enable cd-on-create:

```bash
# ~/.zshrc or ~/.bashrc
eval "$(choros shell-init)"
```

After that, `choros work …` is silent on success and your shell ends up in the new choros dir.

Keys (main screen):

| key | action |
| --- | --- |
| `n` | new choros |
| `d` | delete the selected choros (with confirm) |
| `Enter` | show choros details |
| `j` / `k` / arrows | move selection |
| `r` | re-scan |
| `q` | quit |

In the new-choros modal: `Tab` switches between the name field and the repo list. `Space` toggles a repo. `Enter` creates. `Esc` cancels.

## What choros does on "create"

For each selected repo `R`:

1. reads `git -C .choros-config/registry/R remote get-url origin`
2. best-effort `git fetch` in the registry copy
3. `git clone --reference-if-able .choros-config/registry/R <url> <choros>/R`
4. `git -C <choros>/R checkout -b <choros-name>` so each clone starts on a workspace-named branch

Then:

1. writes `<choros>/.choros-meta.toml`
2. drops a `choros-archive` skill at `<choros>/.claude/skills/choros-archive/SKILL.md`
3. copies any per-agent baselines from `.choros-config/templates/` (currently
   `.claude/settings.json`; cursor `.cursor/cli.json` is opt-in — see
   *Templates* below)
4. detects Rust / JS toolchains in the cloned repos and wires up a shared
   build cache under `.choros-config/store/` — see *Build cache* below

## Templates

`choros init` seeds `.choros-config/templates/.claude/settings.json` with a
minimal baseline. Edit this file once and every future workspace inherits it
(workspace creation copies it to `<choros>/.claude/settings.json`).

Claude writes interactive "always allow" grants to
`<choros>/.claude/settings.local.json`, not the template. To promote those new
grants up to the template so future workspaces inherit them:

```bash
cd <choros>
choros agent save settings
```

This opens a TUI that diffs the workspace's claude settings against the
template and lets you check off entries to promote. `Tab` cycles the diff
source between `settings.json`, `settings.local.json`, and both (default).

## Build cache

To make new workspaces cheap, Choros wires up a shared, per-project build
cache at `<root>/.choros-config/store/` on workspace create:

| Toolchain | Detected by | Cache | Mechanism |
| --- | --- | --- | --- |
| Rust | `Cargo.toml` | `.choros-config/store/rust/sccache/` | drops `<choros>/.cargo/config.toml` with `rustc-wrapper = "sccache"` and `[env] SCCACHE_DIR = ...` |
| JS | `pnpm-lock.yaml` / `yarn.lock` / `package-lock.json` | `.choros-config/store/js/pnpm-store/` | runs `pnpm install --store-dir=...` (with `pnpm import` first for npm/yarn lockfiles); hardlinks files from the store into `<choros>/<repo>/node_modules/` |

The cache is shared across every workspace under the same Choros root, so the
second workspace pays effectively nothing for the same deps. The cache is *not*
shared across different Choros roots — keeps Choros state self-contained and
makes `rm -rf .choros-config/store/` a clean reset.

**Requirements**: `sccache` for the Rust path, `pnpm` for the JS path. If
either is missing from `PATH`, Choros logs a warning and skips that toolchain
without failing workspace creation.

**Sccache server gotcha**: sccache runs as a background daemon and inherits
the `SCCACHE_DIR` from whichever process first starts it. If you have a
sccache server already running from another context (e.g. you ran
`sccache --show-stats` outside a Choros workspace), it'll keep using its
original `SCCACHE_DIR` instead of the workspace's. Fix with
`sccache --stop-server`; the next cargo build inside a Choros workspace will
start a fresh server with the right env.

## Archiving a workspace

`choros archive` moves a workspace under `.choros-config/archive/<name>/` so
it disappears from the active list but is still recoverable. Pass a name
explicitly to archive from the project root, or omit it to archive the
workspace your current directory is inside.

Each new workspace also ships with a Claude Code skill at
`.claude/skills/choros-archive/SKILL.md`. From inside the workspace, run
`/choros-archive` and the AI will commit any pending work, push every cloned
repo to its origin, then run `choros archive` to retire the workspace.

## Not yet supported

- Adding/removing registry repos from inside the TUI
- `gh`-CLI integration to browse repos
- "Open in editor" / shell drop-in
- Pulling latest main on existing choros clones
- Async parallel cloning
- Dirty-state checks on delete (delete is `rm -rf` — choros are ephemeral by design)

## Development

```bash
cargo test --release
cargo build --release
```
