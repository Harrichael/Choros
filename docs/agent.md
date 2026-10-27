# Working in a choros space

choros keeps work on many repos apart, in workspaces, and lands it through
queues that turn each landing into a PR. Run `choros here` first: it says
what the folder you're in is, which queue a land from it goes into, whether
each repo has a check, and where the queue's state lives.

## Folders

- **trunk**: the folder `choros init` ran in. `.choros-config/` in it holds
  choros's own files.
- **branch-space**: a folder in the trunk that groups workspaces and further
  branch-spaces. Queues and settings set on one apply to everything in it.
- **workspace**: made by `choros work NAME REPO…` in a trunk or branch-space.
  Each repo in it is its own clone, on its own branch.
- **landing folder**: `<trunk>/landing/<queue>/landing-N`, one per landed
  set, checked out on the set's branch.

## One workspace per PR

Each land of a repo makes one landing-set, and each set becomes one PR. So
give each change its own workspace: `choros work NAME REPO…` from the space,
commit there, land it, and archive it once landed.

Landing again from the same workspace makes another set; it does not change
the first. To change a set already landed (review fixes, say), commit in its
landing folder and `git push`: commits pushed to its branch join the set.

`choros land archive` and `choros archive` move the workspace folder into
`.choros-config/archive/`. Whatever runs inside it, your own session
included, loses its working directory. Prefer `choros land continue`, then
`choros archive NAME` from the space.

## Landing

    choros land continue|archive [flags]

From inside a repo it lands that repo; from the workspace root, each repo as
its own set. A repo lands into its queue, or straight to main without one.
`continue` keeps the workspace; `archive` archives it after.

    --title T, --body-file F    the PR's title and body
    --fork --deps SET,…|none    land beside the queue, on only these sets
                                (3, landing-3, otherq/3); none: on main alone
    --fork --deps auto          land on only what the commits need (below)
    --on-conflict depend|merge-head
                                when a fork clashes with a set it left out:
                                land behind that set, or stay beside it and
                                resolve the clash in the merge head
    --session ID                fork this Claude Code session into the
                                landing folder (repeatable); by default, the
                                session running the land

Without `--fork`, a land goes on top of everything queued in its repo.

## --deps auto

Needs a check for the repo in `repos.toml`. The land starts on main and the
workspace's own earlier sets, and takes in a queued set only when the commits
conflict with it or change what it wrote. Then it runs the check on the
result. If the check fails without the sets it left out, the land goes on top
of everything queued instead, and says so in one line naming those sets and
where the check's full output is. The check is not run again on top of the
queue, so a failure means either the commits need one of those sets or they
are broken; run the check yourself on top of the queue to tell which.

## repos.toml

`<trunk>/.choros-config/repos.toml`, one table per repo, keyed by its name in
the trunk's registry:

    [app]
    check = "cargo check --workspace"   # a shell command, run at the repo root
    deps = "auto"                       # every plain land uses --deps auto

`deps = "auto"` needs a `check`.

## Queue state

`<trunk>/.choros-config/queues/<queue>/`: `queue.toml` holds its settings,
`state.json` its sets. Read them with `choros status` (everything from here)
or `choros queue NAME`, not by hand. A land runs the queue's pass itself; a
parked set (a conflict to resolve) shows as `! parked` in `choros status`.
