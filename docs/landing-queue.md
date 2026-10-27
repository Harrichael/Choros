# Landing queue

Choros tracks a change from "done in my workspace" to "merged on main". You
*land* a workspace's work into a named **queue**. The queue keeps a private
graph of **landing-sets** (one landing-set = one future PR) and a **merge head**
(main plus everything queued) that new work starts from. Only landing-sets
whose dependencies have all merged — the **front** — go to GitHub, as draft
PRs. You press merge; `chorosd` notices, re-syncs everything behind it,
rebuilds the merge head and opens the next PRs.

The shape in one line: **everything behind the front is private, so the engine
may rewrite it freely; everything at the front is public, so the engine only
ever adds to it.** Most rules below fall out of that. One corollary: a set's
place in the graph is settled before its PR opens; a public set is never moved
behind another one.

Without a queue, a repo in a workspace is in **instant mode**: landing merges
to main straight away.

## Vocabulary

| Term | Meaning |
| --- | --- |
| Queue | A named landing pipeline for a trunk, branch-space or workspace, across every repo landed into it. Owns one dependency graph; per repo, a merge head, a sync clone and a resync agent session. |
| Landing-set | One landed batch of commits from one repo. Maps 1:1 to a PR. Branch `<queue>/landing-N` on that repo's origin; N is unique across the queue. |
| Dependency | Within a queue, in the same repo: A's commits sit on top of B's (B is part of A's base). In another repo, or in another queue: ordering only — A's PR does not open until B merges. |
| Front | Every queued, unparked landing-set whose dependencies have all merged. Several at once is normal. |
| Merge head | Per repo: `main` merged with every queued, unparked landing-set in that repo: what new work starts from. Branch `<queue>/merge-head` on that repo's origin. |
| Landing | The folder `<trunk>/landing/<queue>/landing-N/`: a small choros workspace on the landing branch, with a fork of the Claude session(s) that did the work. |
| Sync clone | `<trunk>/landing/<queue>/_sync/<repo>/`: a clone owned by Choros where every engine-side rebase, merge-head build and agent run happens. User workspaces are never rewritten by the daemon. |
| Resync agent | A headless Claude Code session per queue and repo, run in the sync clone, that resolves conflicts when you ask it to. |

## Configuration and scope

A queue is defined at the **trunk** and scoped to the whole trunk (any
workspace may join it), to **one branch-space** (workspaces in it and below
it), or to **one workspace**. It takes every repo: a repo's sync clone and
merge head appear when something first lands in it (or a diffbase PR names
it). A branch-space or workspace has at most one queue scoped to it.
All definitions live in one place, at the trunk, so the daemon has one thing to
scan and branch-spaces can come and go without moving queue state. A queue
scoped to a workspace or branch-space is archived by the daemon once that
folder has been archived and no set is open (to
`<trunk>/.choros-config/archive/landing/<queue>/_queue`).

```
<trunk>/.choros-config/queues/<queue>/
    queue.toml      # settings (hand-editable)
    state.json      # graph + bookkeeping (engine-owned; written via temp file + rename)
    lock            # flock target
```

`queue.toml`:

```toml
scope = "trunk"                     # or { branch = "epic-x" } or { workspace = "epic-x/login" } (paths below the trunk; "root" is read as "trunk")
diffbase = ["Ordo#41", "Lib#12"]    # PRs to build on before they merge (see Diffbase)
[push]
merge_head = true                   # force-push <queue>/merge-head; false keeps it in the sync clone only
private_sets = true                 # push queued (not yet front) landing branches
[pr]
draft = true                        # open front PRs as drafts
[agent]
rotate_after = 10                   # resync-agent runs before its session is rotated
```

Each repo lands on its origin's default branch. Queues made when a queue held
one repo still have `repo` and `main` lines; they say which repo the old sets
are in.

Pushing private branches runs any `on: push` CI on every re-sync; `choros
queue new` warns when a registry repo has such workflows. Merge method,
"delete head branch on merge" and "require branches to be up to date" are read
from GitHub's repo settings, never configured here.

Queue names must not equal an existing branch on a repo's origin (a branch
`ordo` and refs `ordo/landing-1` cannot coexist; checked when the repo first
lands) and must not be `landing` or `none`.

**Which queue a repo lands into.** In order: the queue the workspace bound it
to (`choros repo bind REPO QUEUE|none`, or the `choros work -i` picker); else the
queue scoped to the workspace, or to the nearest branch-space holding it; else
none, which is instant mode. So a queue scoped to a branch-space takes its
workspaces with no binding at all; binding is for trunk queues, and for
opting a repo out. Bindings live in the workspace's `.choros-landing.toml`,
alongside per-repo landing bookkeeping:

```toml
[repo.Ordo]
queue = "ordo"            # or "none"; absent: the scoped queue, if any
landed_through = "<sha>"  # everything up to here has been landed
```

A new workspace binds each repo to the queue it would land into (its
branch-space's default from `choros branch bind`, else the scoped queue), so
it is cloned **from that queue's merge head** (fetched from the sync clone
when the merge head is not pushed), with `landed_through = <merge head>`.
`choros queue new NAME [--workspace WS | --branch PATH]` creates a queue with
the default settings; with `-i` it asks for each one instead (NAME becomes
optional and, when given, the default). `choros queue ls [--all]` lists them.
Existing workspaces have no `landed_through`; it is taken as `merge-base HEAD
origin/main` the first time.

Instant mode settings: workspace meta, falling back to
`<trunk>/.choros-config/instant.toml`:

```toml
[instant]
via = "push"          # or "pr": open a PR and merge it at once (keeps a PR record, runs required checks)
```

## Landing

```
choros land continue [--fork --deps SET,...|none|auto | --fork -i] [--title T] [--body-file F] [--label L]... [--session UUID]...
choros land archive  [--fork --deps SET,...|none|auto | --fork -i] [--title T] [--body-file F] [--label L]... [--session UUID]...
```

**Scope.** Inside a repo: that repo. At the workspace root: every repo with
unlanded commits, each as its own landing-set with its own number. Repos
landed together are not linked; a dependency across repos is added by hand
(`--fork --deps` or `--fork -i`, or the queue TUI). A new set depends on what is queued in
its own repo. Multiple queues are locked one at a time in name order.

**What is landed.** `rev-list --reverse landed_through..HEAD`, after the sync
below. Refused if `landed_through` is not an ancestor of `HEAD` (landed
commits were rewritten), if the tree is dirty, or if the range contains merge
commits.

### Normal land (lands on the merge head)

1. **Sync the workspace first**, in place: if the queue's merge head has moved
   past `base`, rebase `landed_through..HEAD` onto it (this is `choros repo sync`).
   Conflicts stop the land with a message naming the files; the calling
   session (the one that wrote the code) resolves them and runs `land` again.
   This is where land-time conflicts belong.
2. Take the queue lock. If the merge head moved again meanwhile, release and
   repeat step 1.
3. The landing-set's deps are every queued, unparked set in the queue; its
   base is the merge head; its tip is `HEAD`. No replay is needed: the
   workspace already sits on the base.
4. Push `<queue>/landing-N` (if `push.private_sets` or the set is at the
   front), record the set, rebuild the merge head (now a fast-forward to the
   new tip), set `base = landed_through = HEAD`.

### Fork land

`--fork` takes `--deps` (`none` = based on main), or `-i` for a picker of
queued sets (and other queues' sets, as ordering-only edges). The workspace's own unmerged earlier sets are always added as deps, so
a fork never loses commits it builds on.

1. Base: no deps → `origin/main`; one dep → its tip; several → a private base
   commit merging their tips.
2. In the sync clone (which first fetches the workspace repo:
   `fetch <workspace path> +<ws branch>:refs/choros/incoming/<ws>`),
   cherry-pick the landed commits onto the base. Conflicts → *Conflicts*.
   A clean cherry-pick that changes less than the commits did (they remove
   lines a set left out of `--deps` added) refuses the land, naming the set.
3. **Test-merge into the merge head before anything is public.** If the fork
   conflicts with what is queued, ask (TTY) or require
   `--on-conflict=merge-head|depend`: resolve in the merge head only (the pair
   is flagged), or make this fork depend on the sets it conflicts with.
4. Push, record, rebuild the merge head. `landed_through = HEAD`; `base` is
   unchanged (the workspace still sits on the merge head).

### Auto land

`--fork --deps auto`, or every plain land into a repo with `deps = "auto"` in
`<trunk>/.choros-config/repos.toml`, lands on as little of the queue as the
commits need:

```toml
[app]
check = "cargo check --workspace"
deps = "auto"
```

1. Start on main plus the workspace's own earlier sets.
2. Replay the commits there. A conflict adds the queued sets that changed the
   conflicting files, and the replay runs again. So does a replay that goes
   through but changes less than the commits did: removing lines a skipped
   set added is "already done" without it, and that part would be lost. The
   skipped sets that changed those files are added.
3. Test-merge the result against every other leaf; a set it conflicts with
   becomes a dependency too, and the replay runs again.
4. Run the repo's `check` in the sync clone on the result. Not conflicting
   isn't the same as not needing: the commits may call what a skipped set
   adds. Auto is refused for a repo with no `check`.

The land reports each dependency and why, and the sets it went ahead of. When
nothing short of the whole queue works (its commits need every set, a
conflict with main itself, or the check fails), it warns why and lands on the
merge head as a normal land does. The check runs under the queue lock, so a
slow one holds up other lands meanwhile.

### Both kinds, after recording

- **Sessions** (see *Landings and sessions*) are forked into the new landing
  folder right away.
- If the set is already at the front (a no-dep fork, or a queue with nothing
  else queued), open its PR now.
- `archive`: archive the workspace once every repo in scope has landed; refuse
  if another repo in the workspace still has unlanded commits outside the
  scope. The shell function `cd`s to the nearest space still there, as for `choros archive`.
  `continue`: keep the workspace and its session for the next change.

**PR text.** The land-the-plane skill passes `--title` and `--body-file` with
the text it proposes at land time; it is stored with the set. A raw-terminal
land stores none: the body is blank and the title falls back to the set's
first commit subject (GitHub requires one).

**Labels.** Each `--label` is kept, in order and without repeats, on every
set the land makes, and shown by `choros status`. Choros never reads them;
they are for other tools to tie their own records to a set. Instant mode
makes no set, so a repo landing that way keeps none and the land warns.

**Instant mode.** Fetch, rebase `landed_through..HEAD` onto `origin/main` in
the workspace (conflicts stop the land as in a normal land), then push to main
or open a PR and merge it with the first method the repo allows. No landing
folder. The land isn't done until the PR merges: one that can't merge yet (a
review due, checks running) is recorded in the workspace, and the next land
finishes it first, retrying it while open, counting it once merged, and
forgetting it if closed, which leaves its commits to land again. A second PR
with the same commits is never opened.

## Diffbase

A queue normally builds on main. Its diffbase lists PRs it builds on before
they reach main, as `repo#N`: any PRs, landed through Choros or not. (A queue
made when queues held one repo also reads `#N` as its repo.) `choros queue base add|rm NAME REF…` (`app#40,41 lib#7`: a bare number is in the repo before it; all or none) edits the list in
`queue.toml` (only that line, under the queue's lock) after checking the PR is
open and its repo is in the registry; `choros queue base NAME` and `choros
status` show it.

- **Built on.** In each repo, its open listed PRs are merged onto main, one at
  a time in list order, into that repo's *diffbase*. It replaces main as the
  start of every set's base in that repo, of its merge head, and so of every
  workspace synced onto it. It is rebuilt only when its inputs change (a PR's
  head moves, one merges, or a set of the queue merges); otherwise it keeps
  the main it was built on, as a set's base does.
- **Nothing opens early.** Front sets open their PRs only once every listed PR
  has merged (asked of GitHub, since a squash merge leaves no trace in git) and
  the set has been replayed onto plain main.
- **Closed PRs are left out.** A listed PR closed without merging is built
  without, as if taken off the list, until it is reopened: its code leaves the
  diffbase, the merge head and new workspaces, sets are replayed off it (one
  that needed it parks like any conflict), and nothing waits for it. It stays
  in `queue.toml`, and `choros status` shows it as left out.
- **Parks.** A listed PR that won't merge onto main and those before it parks
  the whole queue, naming the pair and the files. Nothing else moves while the
  diffbase is parked. `choros queue resolve` resolves the conflict like any
  other: it is recorded, and rerere replays it on every rebuild. That
  resolution exists only to hold the queue together and never ships: when the
  PRs merge, main's own resolution is what the sets are replayed onto.
- **Changes take effect at once.** `queue base add` and `queue base rm` run a
  pass before they return, so the diffbase and merge head are rebuilt right
  away (and a conflict shows, at a terminal resolvable, at once). A hand edit
  of `queue.toml` is read on the daemon's next pass. Workspaces already open
  stay where they are until `choros repo sync`, which their prompt asks for
  with `⇣sync`; new ones start on the new merge head.

## Merge head

Rebuilt from scratch on every land, re-sync and graph edit, in the sync clone:

1. Start at `origin/main`, or the diffbase when there is one.
2. Take the queue's queued, unparked sets that no other such set depends on
   (the leaves); each contains its own ancestors.
3. One leaf: the merge head is that leaf's tip. Several: merge them in
   ascending landing number with `git merge --no-ff`, `rerere` on.
4. Force-push `<queue>/merge-head` if `push.merge_head`.

Merge commits made here belong to the merge head only. Their SHAs are kept in
`state.json` as *recorded resolutions*.

A workspace needs a sync (`⇣sync` in the prompt) when the merge head's
*content* moved past its `base` — compared as (main SHA, set of `(set, tip)`
pairs it contains), not by merge-head SHA, which changes on every rebuild.
`state.json` stores the current composition; the workspace meta stores the one
its `base` had.

## The daemon

`chorosd` is a launchd agent (installed by dev-setup, like Ordo). It finds
trunks in `~/.config/choros/trunks.toml` (written by `choros init`; the older
`roots.toml` is still read). Every 2
minutes, or when `choros queue sync` pokes it (with no daemon running,
`choros queue sync` runs the same pass in-process), for each queue it
**try-locks** (skips a queue whose lock is held) and runs one
**level-triggered** pass: it computes what every set should look like from the
graph, origin and GitHub, and fixes any difference. A crash mid-pass heals on
the next pass.

0. **Recover.** If the sync clone has a rebase, cherry-pick or merge in
   progress, abort it and `reset --hard`. Complete or roll back the journalled
   operation in `state.json`.
1. **Observe.** Fetch. For each open PR: merged → mark merged, keep its final
   tip; closed unmerged → park the set *and its whole subtree* (out of the
   merge head) awaiting a decision; reopened → unpark.
2. **External pushes.** If a set's remote tip has moved past its recorded tip
   (review fixes pushed from its landing, at the front or not), adopt the new
   commits as part of the set.
3. **Desired base** of each queued private set:
   - all deps merged → `origin/main`;
   - some deps merged → a merge of `origin/main` with the unmerged deps'
     tips (a single unmerged dep whose tip already contains `origin/main`
     is used directly);
   - none merged → the deps' tips (merged together when several).

   If the desired base differs from the recorded one, replay the set's own
   commits (`recorded base..tip`, no merges) onto it by cherry-pick, and push
   with `--force-with-lease=<ref>:<recorded tip>`. Squash merges need nothing
   special: the set's own commits are known exactly. A commit that comes out
   empty on the new base is dropped: its change already reached main some
   other way, and its PR would otherwise list it, and GitHub's squash message
   carry its text, for a change the diff doesn't have.
4. **Public sets** are never rebased. They are left alone unless they conflict
   with main or the repo requires branches to be up to date; then `origin/main`
   is *merged into* them (a new commit; review comments stay put).
5. Rebuild the merge head.
6. Open draft PRs for sets that just reached the front (all deps merged,
   cross-queue deps included). A set left with no commits of its own has
   nothing to review: it is marked merged without a PR, and goes the way of
   any merged set.
7. **Landing folders** of private sets that were re-synced are `reset --hard`
   to the new tip when clean; when dirty, status warns. Merged sets' landing
   folders move to `archive/landing/<queue>/landing-N` (from `continue` and
   `archive` alike).

Any conflict parks **only the affected set and its subtree**; the rest of the
queue keeps moving, and new lands are accepted (a land that would depend on a
parked set excludes it, like the merge head does). Operations are journalled in
`state.json` before pushing and committed after. Locks are held only for short
steps, never while waiting on a person.

**Source of truth:** GitHub/origin for PR state and public branch tips;
`state.json` for the graph, bases and the expected tips of private branches. A
failed lease on a private branch means an external push (step 2), not
corruption. `choros queue doctor` reconciles the two by hand.

`gh` runs with the trunk's account (`GH_CONFIG_DIR=<trunk>/.choros-config/gh`
when present). The daemon never runs Claude.

Branch cleanup: GitHub deletes merged PR branches itself when the repo setting
is on. Choros deletes what GitHub never sees as merged: branches of dropped
sets and the merge head of a deleted queue.

## Conflicts

A conflict can arise in a fork land's replay, a daemon re-sync, a
multi-dependency base, the merge head, or merging main into a public set. The
daemon parks; `choros status` shows it; `choros queue resolve <queue>` asks,
every time:

- **a)** Trust the agent: it resolves, Choros checks, then pushes.
- **b)** The agent resolves, the diff is printed, then you choose push or
  intervene.
- **c)** Intervene: Claude Code opens interactively in the sync clone on the
  resync agent's current session, and you drive.

Without a TTY (from inside a Claude session), `resolve` takes `--agent=trust`
or prints the parked operation and exits non-zero.

**Forks that conflict in the merge head** are settled at land time (see *Fork
land*). Once both are public, the only choices are resolving in the merge head
or closing one PR and re-queueing it as a dependent.

**A closed PR** (parked with its subtree): reopen it; drop it (rebase its
dependents without it, conflicts parked as usual); or close the dependents
too.

**Reusing resolutions.** Every resolving merge commit is recorded. The agent
is handed `git show --remerge-diff <merge>` for any recorded resolution
touching the same files — exactly how it was fixed before. The sync clone also
runs `rerere`, which replays a resolution outright when the same hunks
conflict again (typically: main merged into the second of two conflicting
public forks after the first merges); it is a bonus, not the mechanism.

**Checks after any resolution** (agent or rerere): no unmerged paths, no
conflict markers (`git diff --check`). A resolution that fails them stays
parked.

**The resync agent** runs only from `choros queue resolve`, in your terminal,
in `<trunk>/landing/<queue>/_sync/<repo>/`:

```
claude -p --resume <id> --permission-mode acceptEdits \
  --allowedTools 'Read,Edit,Write,Bash(git status*),Bash(git diff*),Bash(git add*),Bash(git log*),Bash(git show*),Bash(git cherry-pick --continue*),Bash(git rebase --continue*),Bash(git merge --continue*)' \
  --disallowedTools 'Bash(git push*)' --output-format json "<canned prompt>"
```

Choros mints the session id with `--session-id` on first use, records it in
`state.json`, and resumes it on later runs. After `agent.rotate_after` runs it
starts a new session, seeded with a handoff summary; the queue's long-term
notes live in a `CLAUDE.md` in the sync clone. The canned prompt names the
operation, conflicted files, both sides' commits and any recorded resolution.
Choros, not the agent, pushes. Git in the sync clone runs with
`core.hooksPath=/dev/null` and `commit.gpgSign=false`. For now there is no
test gate (a configurable check command is a later addition).

## Landings and sessions

`<trunk>/landing/<queue>/landing-N/` is a choros workspace: a
`.choros-meta.toml` with `parent = "<source workspace>"` and
`landing = { queue, n }`, a reference clone of the repo on `<queue>/landing-N`,
the usual agent provisioning, and a `CLAUDE.md` saying the branch may be
updated by Choros (pull before working; push review fixes as new commits).
Choros commands find the trunk by walking up to `.choros-config`, so they work
inside a landing. `landing` is a reserved workspace name.

**Session fork, at land time.** Claude Code (verified on 2.1.307) forks a
session into another directory without touching its files: from the landing
folder,

```
claude -p --resume <source-id> --fork-session --session-id <fork-id> -n "<queue> landing-N" "<handoff note>"
```

finds the source session, writes the fork under the landing folder with full
history and the id we chose. Forking at land time (one cheap turn) freezes the
context at the moment of landing — a lazy fork after `land continue` would
also capture the next task — and the handoff note closes the half-finished
`land` tool call the source session is in. `choros landing open N` then runs
`claude --resume <fork-id>` there.

**Which sessions:** `--session` if given; else `CLAUDE_CODE_SESSION_ID` (set
by Claude Code in its Bash tool's environment); else, from a raw terminal, the
Claude Code sessions whose working directory is inside the land's scope and
that were modified since the previous land, discovered the way `choros agent
ls` does. The most recent is the primary fork `landing open` resumes;
`landing open N --session ID` picks another.

Claude Code deletes sessions inactive for `cleanupPeriodDays` (default 30);
dev-setup's Claude settings should raise it so landing forks survive long
reviews.

## Status

`choros status`: inside a workspace, the queues it is bound to; in a
branch-space, the queues scoped to it or below and those its workspaces are
bound to; at the trunk (or with `--all`), every queue there; outside any
trunk, every queue of every registered trunk. Per queue: each set's state (queued / front with PR link / merged / parked
and why), deps, flagged conflicting forks, dirty landing folders, and
workspaces that need a sync.

`choros status --json` is the same report for tools, merged sets included:
`{"queues": [{trunk, name, repos, sets, needs_sync, diffbase,
diffbase_park}]}`. Other tools read each set's `n`, `repo`, `state`
(`queued`, `front`, `merged`, `closed`), `title`, `pr` (`{number, url}`),
`sessions` (`[{source, fork}]`) and `labels`, so those names stay fixed.
Dropped sets are left out.

`choros status --prompt` prints one short token (`⇣sync`, `!resolve`, or
nothing) from local files only — no git, no network, no lock — for the bash
prompt and Gnomon.

`choros repo sync` (in a workspace or repo): rebase unlanded commits onto the
current merge head, in place, and update `base`. Explicit only.

## The queue TUI and recovery

`choros queue -i [NAME]`: the graph as a list of landing-sets with deps, state and
PR. Edit deps of *private* sets (add, remove, cross-queue ordering edges),
change a workspace's binding, drop a set. Edits are validated (acyclic; no
same-repo cross-queue edges; public sets cannot gain deps) and trigger a
re-sync and merge-head rebuild, conflicts parked as usual.

`choros queue drop <set>` removes a queued set (its dependents re-sync without
it). Its commits go back to the workspace that landed them: otherwise that
workspace's next sync would take them out, as no longer in the merge head.
The next `choros repo sync` or land replays them onto the merge head, under
any work done since, as unlanded. The set's tip stays in the sync clone under
`refs/choros/returned/<n>`.

Landing again from a workspace always makes a new set. Changes to a set still
in the queue go through its landing folder, and the land says so when the
workspace has one open. `choros queue doctor` reconciles `state.json` with origin and GitHub.

## Code layout

Choros becomes a cargo workspace:

- `crates/choros-core` (lib): today's domain — workspaces, registry, git,
  caches, agent provisioning. Extracted in a move-only commit first.
- `crates/choros-land` (lib): the engine — queue config and state, the graph,
  land, sync, merge-head build, parking and resolution, PR lifecycle. Talks to
  GitHub through a small `Forge` trait (create PR, view PR, read repo settings,
  merge PR) and to Claude through a small `Resolver` trait (resolve a parked
  operation; fork a session); real impls shell out to `gh` and `claude`.
- `crates/choros` (bin): the CLI and TUIs.
- `crates/chorosd` (bin): the daemon loop.

**Tests** use real git in temp dirs (a bare repo as origin), a fake `Forge`
holding PR state in memory, and a fake `Resolver` applying scripted
resolutions. Stories, not mechanisms: land two sets and merge the front;
squash-merge then re-sync a chain; a set with two deps after one is
squash-merged keeps the other's and main's content; review commits on the
front re-sync its dependents; conflicting forks settled at land time; a
recorded resolution handed to the resolver; a public set behind main left
alone until it conflicts, then merged into; a closed PR parks its subtree and
leaves the merge head; a crash mid-pass heals on the next pass; two lands
serialize on the lock; a workspace that merged main in lands only its own
commits.

## Decided elsewhere, later

- A configurable test gate on re-sync and dependency edits.
- An automated review process of your own.
- A standing "trust the agent" policy that would let the daemon resolve
  unattended (it would then have to run Claude under launchd).
- A custom land-the-plane skill (drives `choros land` with `--title`,
  `--body-file`; the session id comes from the environment).

## Implementation notes

Where the build departs from the plan above, and why:

- **Per-workspace landing data lives in `.choros-landing.toml`**, not in
  `.choros-meta.toml`. The meta file is choros-core's; keeping landing out of
  it means core knows nothing about queues, and only the engine reads or
  writes the landing data.
- **`choros queue sync` runs the pass in-process** rather than poking the
  daemon over a socket. The queue lock makes the two equivalent, and it
  works whether or not `chorosd` is running. The daemon's interval is
  `CHOROSD_INTERVAL_SECS` (default 120); `chorosd --once` runs one tick.
- **The resync agent keeps its notes in the sync clone's `CLAUDE.md`**,
  listed in `.git/info/exclude` so it can never be staged into a resolution
  (and skipped when the repo tracks its own). Rotation starts a fresh session
  that reads those notes; old session ids stay in `state.json`.
- **Bases always include main.** A set's `BaseSpec` records the main commit it
  was built on and keeps it until a dependency merges past it, so the merge
  head and every set's base are built by the same function (start at main,
  merge in leaves, fast-forward where possible) and a fresh land never
  re-syncs on the next pass.
- **Parks retry when their inputs change.** Each park records what conflicted
  (`on`); a pass that finds different inputs (a dependency moved, main
  moved) simply tries again, so a conflict that went away clears itself.
- **A workspace rebase remembers where it was headed** (`pending` in
  `.choros-landing.toml`). A sync or fork land that stops on a conflict can be
  finished by hand (`git rebase --continue`) and the land re-run; landing
  resumes from the commit the rebase was headed for. A fork moved onto the
  sets it clashes with also remembers them (`fork_deps`) for the re-run.
- **Pushes are journalled** (`pending` on the set): written before a
  rewritten tip is pushed, cleared after. A crash in between, or a refused
  push, resolves to whichever tip origin actually has. A refused push holds
  back that set only.
- **A merge commit pushed onto a private set parks it** (`pushed-merge`); its
  commits could no longer be replayed one by one.
- **A clash in the merge head parks one set, not the queue.** The leaf that
  was knowingly landed beside the other's work (else the newer) is parked out
  of the merge head with what builds on it; everyone else keeps landing. It is
  retried when the merge head it would join changes, or by `choros queue
  resolve`.
- **Opening a PR is idempotent**: an open PR for the branch is adopted rather
  than a second one attempted, so a pass that died after opening one heals.
- **The engine does not read the environment**: the CLI passes
  `CLAUDE_CODE_SESSION_ID` in as `LandRequest::env_session`.
- **State stays in JSON for now.** SQLite would not remove the queue lock,
  which guards the sync clone's one git worktree; and the crash windows that
  matter (GitHub against local state, the workspace file against the queue
  file) span stores no database can make atomic, so write-then-reconcile and
  idempotent lookups stay either way. Its real wins are an event history and
  queries across queues; switch when a caller wants those (a timeline,
  stats). Until then an append-only `events.jsonl` would give the history.
  The state already loads and saves behind one module, so the swap is
  surface, not shape.
- **Driving a conflict yourself (`c`) still holds the queue's lock** while
  Claude Code is open. Other lands wait, and say they are waiting. Releasing
  the lock mid-conflict needs a git worktree per parked conflict (the daemon's
  recovery would otherwise wipe a half-done resolution); not yet.
- **`choros archive` and `choros repo add` still assume top-level
  workspaces**; inside a landing folder use the landing commands.
  `find_workspace` (core) is what the engine uses to find the trunk from
  anywhere, landings included.

## Decision log (the user's words, from the design grill)

Round 1:
1. multiple things can be at the front and yes its when their dependencies are merged.
2. I could be starting multiple spaces off of the merge head in parallel, and for those to have no dependencies. But I also want the ability to manage the diffbases of things in the queue so I can introduce a fork and I have some kind of automated check tell me the tests still pass or something - might require an AI to fix conflicts or something cool
3. I would think pushed to origin as branches. branch names should be something like `<choros-workspace-name>/landing-1`
4. Merge head gets rebuilt every land. [...] I would like a custom land the plane skill.
5. The queue will be per repo by default but I can manually edit and add a cross repo dependency
6. Add new commits for sure, our queue graph needs a concept like landing-set that translates 1 to 1 with a pr
7. Different repos are different. We will encounter merge commit and squash both
8. I press merge
9. I want it to be draft PR but I will want this configurable. Later I will even want my own sort of review process automated
10. I will want to be able to configure landing to instant merge, so yeah default mode is no queue and landing will merge
11. Re-synced automatically
12. I want choros to manage a claude code resync agent for me for a given merge queue. It will track the claude session uuid for it and such and give it to it. Upon a sync error, I will get an option to a) trust the agent and push, b) have the agent do it, print git diff, choose to push or intervene, c) intervene with claude code opened right away and I drive
13. (landing contents) Option A: a landing is a small choros workspace. When I land a changeset, I may land archive or land continue, but I will be explicit, so those are two commands.
- Also: only the things at the front of the queue go to GitHub for review. I want a merge head. Build our own engine.

Round 2:
1. I want a chorosd daemon to help manage choros now
2. choros status should tell me. Outside of a choros workspace it will tell me about every merge queue, within a workspace, only about merge queues managed there.
3. dependency graph lives only on this mac
4. [merge head] force pushed by default, but that will be a merge queue setting, so we can also hide it
5. when you land its a merge to the current merge queue head so all commits and such
6. it has to merge to the current merge queue head which means we wont naturally get forks unless we try. If we do land continue/archive fork then because of the fork it presents a tui option list of which should be dependencies
7. live workspaces need to be synced explicitly. We can show a status line in bash and Gnomon that choros needs resync
8. our merge queue needs a name: `<merge-queue>/landing-N` ... its possible merge queues cross workspaces
9. rebuilt is easiest and best. I am fine if the commit history for the pr I send out is mostly lost. If I update mid pr review that is different
11. [dependency editing] a tui
12. [test gate] can be configured in the future, ignore for now, trust the resync agent on merge conflicts and otherwise yolo it
13. landing queues need names, so `landing/<queue_name>/_sync/<repo>`
14. [a/b/c] asked on every error
15. [PR content] just merge what commits we had
16. [merge method] read from github's setting for the repo
17. [instant merge: direct push or PR then merge] support both
18. If I am in a repo when I call it, it applies to that repo, if in the workspace root, then all repos.
19. [session file layout] if we can avoid depending on it, lets try → verified avoidable
20. [which session] The one I call land from, or if just raw terminal, then every claude code session in the scope landed
21. landing archive goes archive automatically and landing continue lets you keep editing
22. Crate but not binary - chorosd would be its own binary

Round 3:
- Queues can be configured by workspace or by choros root of multiple workspaces.
- The land the plane skill should record the proposed PR text at time of land; otherwise it should be blank.
1. [conflicting forks in the merge head] ask each time (resolve in merge head, or make it a dependency) — but if we fixed the merge conflict for the merge head, we have a record of how to fix it later.
2. [public PR behind main] leave it alone unless it conflicts or the repo requires up to date; then merge main in.
3. [fork with no deps] starts from main, otherwise its not really a fork; it fast-tracks changes straight to main.
4. [concurrent lands] a lock.

Review (Opus 5.5) adopted as defaults, user may veto:
- A normal land syncs the workspace onto the merge head first, so land-time conflicts are resolved in place by the session that wrote the code (a land is an explicit act, consistent with "synced explicitly").
- The fork-or-dependency question is asked at land time, before a PR opens; public sets never move behind others.
- A closed PR parks its subtree out of the merge head.
- Raw-terminal session selection: sessions in scope modified since the previous land.
- A workspace-scoped queue replaces the root queue for that repo; cross-queue edges are cross-repo ordering only. (Superseded by round 4: one queue takes every repo.)
- Lands into a queue with parked sets are accepted.
- The daemon never runs Claude; the agent runs from `choros queue resolve`.
- Landing folders from `land continue` are archived on merge too.
- Sessions are forked eagerly at land time.

Round 4 (multi-repo queues):
- "the queue should be for my whole branch, it should handle repo segmentation by default internally by itself and not be scope per repo. By branch, I mean branch space"
- "if I want to define a dependency across repos I should be able to. You don't need a lane to accomplish this, youjust need that by default the deps for a new landing set are the latest landing sets in the same repo"
- "If they are different landing sets, then give them different numbers. for now, choros land at workspace level will just make a landing set in each repo as a convenience wrapper type thing"
- [how workspaces find their queue] the scope decides: a queue scoped to a branch-space takes every workspace under it; binding is only needed for a trunk-wide queue ("I think C is the best no nonsense change for right now").
