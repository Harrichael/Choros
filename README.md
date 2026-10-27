# choros

A small CLI for managing short-lived multi-repo work environments. One workspace per Jira-task-sized unit of work.

## How it works

Choros works inside a **trunk**: the directory you run `choros init` in. Workspaces live in the trunk or in a **branch-space**, a folder that groups workspaces (and further branch-spaces) and can carry its own defaults.

```
<cwd>/                          ← the trunk
├── .choros-config/
│   └── registry/
│       ├── service-api/        ← added with `choros add`
│       └── service-web/
├── PROJ-1234/                  ← a workspace, created by `choros work`
│   ├── .choros-meta.toml
│   ├── service-api/            ← fresh clone of the registry repo, on branch PROJ-1234
│   └── service-web/
└── checkout/                   ← a branch-space, created by `choros branch`
    ├── .choros-branch-config/
    └── PROJ-1240/              ← a workspace in it, on branch checkout/PROJ-1240
```

A directory is a workspace iff it contains `.choros-meta.toml`, a branch-space
iff it contains `.choros-branch-config/`, and the trunk iff it contains
`.choros-config/`.

## Setup

Populate the registry with `choros add`, passing an SSH remote:

```bash
choros add git@github.com:your-org/service-api.git
choros add git@github.com:your-org/service-web.git
```

This clones each repo into `.choros-config/registry/<name>`, deriving `<name>`
from the URL. Only SSH remotes are supported — the registry copy is later
used as a `--reference-if-able` source for every workspace clone, which needs
a stable local git repo, not an HTTP(S) endpoint.

`choros work -i` scans `.choros-config/registry/` and lets you pick repos from there.

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

Run from the trunk or a branch-space; new workspaces go where you are.

```bash
choros work PROJ-1 api web      # make workspace PROJ-1 with the api and web repos
choros work -i                  # the same, from a name + repo picker
choros work -i PROJ-1           # name pre-filled, jumps to repo selection
choros branch checkout          # make a branch-space here
choros ls                       # workspaces here and below, as a tree
choros archive                  # archive the workspace your shell is in
choros archive PROJ-1           # archive PROJ-1 from its space
choros agent save settings      # inside a workspace: TUI to promote claude
                                # settings entries up to the template
choros agent ls                 # list resumable claude / cursor sessions for
                                # the current directory
choros repo ls                  # inside a workspace: list its cloned repos
choros repo add REPO [REPO…]    # inside a workspace: clone registry repos into it
choros -h                       # every command
```

`choros work` is the way to make a fresh choros. Given a name and repos it just makes it; with `-i` it opens the name + repo picker, shows progress over it while cloning, and exits once the workspace exists. Without either it prints its usage, so a picker only opens when asked for, as with `ergon work -i`. With the shell integration below, your shell is `cd`'d into the new choros on success.

### Shell integration

`choros` is an external binary, so it cannot change your shell's `cwd` on its own. Add this one line to your shell rc to enable cd-on-create:

```bash
# ~/.zshrc or ~/.bashrc
eval "$(choros shell-init)"
```

After that, `choros work …` and `choros branch NAME` leave your shell in what they made, and `choros archive` or `choros branch archive` run from inside what they archive leave your shell in the nearest space that's still there.

In the picker: `Tab` switches between the name field and the repo list. `j` / `k` or arrows move. `Space` toggles a repo. `Enter` creates; on failure you're back in the picker with the error. `Esc` cancels.

## What choros does on "create"

For each selected repo `R`:

1. reads `git -C .choros-config/registry/R remote get-url origin`
2. best-effort `git fetch` in the registry copy
3. `git clone --reference-if-able .choros-config/registry/R <url> <choros>/R`
4. `git -C <choros>/R checkout -b <branch>` so each clone starts on the
   workspace's branch: its name, prefixed with the branch-space path when it
   is in one (`checkout/PROJ-1240`). Choros first checks origin has no branch
   that would collide (`checkout`, or `checkout/PROJ-1240/…`).

Then:

1. writes `<choros>/.choros-meta.toml`, recording the branch so repos added
   later join the same one
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

A branch-space can override any template file by placing its own under
`<branch-space>/.choros-branch-config/templates/`; for each file the nearest
space that has one wins.

This opens a TUI that diffs the workspace's claude settings against the
template and lets you check off entries to promote. `Tab` cycles the diff
source between `settings.json`, `settings.local.json`, and both (default).

## Listing workspaces

`choros ls` lists the active (non-archived) workspaces in the space you're in
and below, as a tree: each space's workspaces, most recently worked-in first,
then its branch-spaces, then the landing-sets its workspaces have in flight.
`--all` shows the whole trunk. Each workspace has its last-active time (local
time zone, plus a rough "3d ago"), creation time, number of changed files
(`git status` entries summed across its repos), and repos.

"Last active" is the newest mtime of any file in the workspace that git
doesn't ignore, or of a repo's reflog (commits, checkouts, pulls). Build
output and `.git/` internals don't count, so `cargo build` or a `git status`
from your prompt won't make a workspace look fresh. An agent conversation
that writes no files doesn't count (granting it a permission does, since that
writes `.claude/settings.local.json`).

## Listing resumable agent sessions

`choros agent ls` enumerates Claude Code and Cursor CLI sessions whose
recorded cwd matches the directory you run it from (or any path beneath it).
Each line shows agent, modified time (UTC), session ID, the path (relative to
the workspace root) the session was invoked from, and a preview of the first
user message — feed the ID into `claude --resume <id>` or
`cursor-agent --resume <id>` to pick up where you left off.

When run from inside a choros workspace, the scope expands to the whole
workspace (not just cwd), and the `PATH` column is workspace-relative —
`.` for sessions invoked at the workspace root, `<repo>` or `<repo>/<subdir>`
for sessions invoked deeper. Outside a workspace (e.g. at the trunk)
the scope is the current directory and `PATH` is cwd-relative.

## Build cache

To make new workspaces cheap, Choros wires up shared build caches on
workspace create:

| Toolchain | Detected by | Cache | Mechanism |
| --- | --- | --- | --- |
| Rust | `Cargo.toml` | sccache's own (one per machine) | drops `<choros>/.cargo/config.toml` with `rustc-wrapper = "sccache"` |
| JS | `pnpm-lock.yaml` / `yarn.lock` / `package-lock.json` | `.choros-config/store/js/pnpm-store/` | runs `pnpm install --store-dir=...` (with `pnpm import` first for npm/yarn lockfiles); hardlinks files from the store into `<choros>/<repo>/node_modules/` |

The second workspace pays effectively nothing for the same deps. The JS store
is per trunk (`rm -rf .choros-config/store/` is a clean reset). The Rust
cache is sccache's single machine-wide one, shared by every trunk: sccache reads
its cache dir only when its server starts, so a per-trunk dir would silently
lose to whichever build started the server first.

For npm/yarn repos, the `pnpm-lock.yaml` generated by `pnpm import` is added to
the clone's `.git/info/exclude`, so it never shows up as pending work (and the
archive flow can't accidentally commit it).

**Requirements**: `sccache` for the Rust path, `pnpm` for the JS path. If
either is missing from `PATH`, Choros logs a warning and skips that toolchain
without failing workspace creation.

## Archiving a workspace

`choros archive` moves a workspace to the same path under
`.choros-config/archive/` so it disappears from the active list but is still
recoverable. Pass a name to archive a workspace in the space you're in, or
omit it to archive the workspace your current directory is inside.

Each new workspace also ships with a Claude Code skill at
`.claude/skills/choros-archive/SKILL.md`. From inside the workspace, run
`/choros-archive` and the AI will commit any pending work, push every cloned
repo to its origin, then run `choros archive` to retire the workspace.

## Branch-spaces

A branch-space groups related workspaces, say everything for one epic. From
inside it, `ls`, `status` and the queue TUI show only what's in it, and you
can point a Claude Code session at it without exposing the rest of the trunk.

```bash
choros branch checkout          # make it (`choros branch -i checkout` picks a queue from further up)
cd checkout
choros queue new checkoutq --branch checkout   # its own queue: every workspace here lands into it
choros work PROJ-1240 api web   # api and web both land into checkoutq
choros branch bind appq         # or, without a queue of its own: new workspaces land into appq
choros branch bind web none     # ...except web, straight to main
choros branch sso               # branch-spaces nest
choros branch archive           # archive this branch-space and all inside
```

Settings resolve from the nearest space up to the trunk, per setting:

- **Default queue bindings** (`queue = "…"` and `[bind]` per repo in
  `.choros-branch-config/landing.toml`, a queue or `"none"`), applied when a
  workspace is created; later changes leave existing workspaces alone. A
  queue scoped to the branch-space needs none: its workspaces land into it.
- **Instant mode's route** (`[instant] via` in the same file).
- **Agent templates** (`.choros-branch-config/templates/`).

There is one registry per trunk, and queues stay at the trunk; a queue can be
limited to a branch-space with `choros queue new NAME --branch PATH`, and
then takes every repo of every workspace in it.
`branch archive` refuses while a workspace inside has uncommitted changes or
unlanded commits (`--force` overrides); landed sets carry on in their queue.
Within one space, a workspace and a branch-space can't share a name, and at
the trunk neither can share a queue's name, since both become branch prefixes
on origin.

## Not yet supported

- `gh`-CLI integration to browse repos
- "Open in editor" / shell drop-in
- Pulling latest main on existing choros clones
- Async parallel cloning

## Landing queues

A queue takes landed work from workspaces, keeps it in a private dependency
graph on top of a *merge head* (main plus everything queued), and sends only
the front — sets whose dependencies have merged — to GitHub as draft PRs.
`chorosd` notices merges and re-syncs what is behind them. Repos not bound to
a queue land straight to main. Full design: [docs/landing-queue.md](docs/landing-queue.md).

```bash
choros queue new appq              # a queue for every repo of the trunk
choros queue new -i                # the same, asking for each setting
choros queue ls                    # the queues here: scope, repos, open sets, diffbase
choros repo bind app appq          # inside a workspace (or press `b` in `choros work -i`)
choros land continue               # land, keep working here
choros repo sync                   # catch up when the prompt shows ⇣sync
choros land archive                # land, then archive the workspace
choros land continue --fork        # land beside the queue (pick dependencies)
choros status                      # queues, sets, PRs, parked conflicts
choros queue resolve appq          # settle parked conflicts (a/b/c)
choros landing open 3              # resume the session forked at landing-3
choros queue base add appq app#41  # build on app's PR #41 while it's in review
```

A queue's **diffbase** lets it start before the work it needs has merged: it
builds on main with the listed PRs merged in (any PRs, landed through Choros
or not), and opens nothing until they merge, when its sets are replayed onto
main. A PR in another repo (`lib#12`) is only waited for. If the listed PRs
conflict, the queue parks until `choros queue resolve`; that resolution only
holds the queue together and never ships.

## Development

```bash
cargo test --release
cargo build --release
```
