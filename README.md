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

Then writes `<choros>/.choros-meta.toml` and drops a `land-the-plane` skill at `<choros>/.claude/skills/land-the-plane/SKILL.md`.

## Land the plane

Each new workspace ships with a Claude Code skill that runs a session-end
protocol: commit any pending work, push every cloned repo to its origin, and
then `choros archive` the workspace. Tell your AI assistant "land the plane"
when you're done with the task.

`choros archive` moves the workspace under `.choros-config/archive/<name>/`,
so it disappears from the active list but is still recoverable. Pass a name
explicitly to archive from the project root, or omit it to archive the
workspace your current directory is inside.

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
