# hollerith

A small TUI for managing short-lived multi-repo workspaces. One workspace per Jira-task-sized unit of work.

## How it works

Hollerith is project-scoped to the directory it's launched in. Run it from the directory where you want your workspaces to live.

```
<cwd>/
├── .ws-config/
│   └── registry/
│       ├── service-api/        ← clone these yourself (MVP)
│       └── service-web/
├── PROJ-1234/                  ← workspace, created by hollerith
│   ├── .ws-meta.toml
│   ├── service-api/            ← fresh clone of the registry repo
│   └── service-web/
└── PROJ-1235/
    └── ...
```

A directory is recognized as a workspace iff it contains `.ws-meta.toml`.

## Setup

For now you populate the registry yourself:

```bash
mkdir -p .ws-config/registry
git clone git@github.com:your-org/service-api.git .ws-config/registry/service-api
git clone git@github.com:your-org/service-web.git .ws-config/registry/service-web
```

The TUI scans `.ws-config/registry/` on launch and lets you pick repos from there.

## Usage

```bash
cargo run --release
```

Keys (main screen):

| key | action |
| --- | --- |
| `n` | new workspace |
| `d` | delete the selected workspace (with confirm) |
| `Enter` | show workspace details |
| `j` / `k` / arrows | move selection |
| `r` | re-scan |
| `q` | quit |

In the new-workspace modal: `Tab` switches between the name field and the repo list. `Space` toggles a repo. `Enter` creates. `Esc` cancels.

## What hollerith does on "create workspace"

For each selected repo `R`:

1. reads `git -C .ws-config/registry/R remote get-url origin`
2. best-effort `git fetch` in the registry copy
3. `git clone --reference-if-able .ws-config/registry/R <url> <workspace>/R`

Then writes `<workspace>/.ws-meta.toml`.

## Not yet supported

- Adding/removing registry repos from inside the TUI
- `gh`-CLI integration to browse repos
- "Open in editor" / shell drop-in
- Pulling latest main on existing workspace clones
- Async parallel cloning
- Dirty-state checks on delete (delete is `rm -rf` — workspaces are ephemeral by design)

## Development

```bash
cargo test
cargo build --release
```
