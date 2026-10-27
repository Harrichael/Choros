# Landing work with choros

Run `choros here` first: it says what this folder is, which queue each repo
lands into, and whether the repo has a check.

In a branch-space, `choros work NAME REPO…` makes a workspace in it; you can
keep several. In a workspace, each repo is its own clone on its own branch.
Commit there, then land:

    choros land continue [flags]

Inside a repo it lands that repo; at the workspace root, each repo with
unlanded commits. Each land makes a new landing-set, and so a new PR, from
the commits since the last land. Keep working and land again as you go.

    (no --fork)                 on top of everything queued in the repo
    --fork --deps SET,…|none    beside the queue, on only these sets (3,
                                landing-3, otherq/3); none: on main. This
                                workspace's own earlier sets are always kept
    --fork --deps auto          on only what the commits need (below)
    --on-conflict depend|merge-head
                                when a fork clashes with a set it left out:
                                land behind it, or resolve beside it
    --title T, --body-file F    the PR's title and body. The body tells the
                                reviewer what changed and why, in the repo's
                                PR template; leave out landing-sets and
                                dependencies. Landing without one shows the
                                template to fill
    --label L                   text kept on the set for other tools
                                (repeatable); choros never reads it
    --session ID                the session to fork into the landing folder;
                                by default, the one running the land

`choros land archive` lands and then moves the workspace into
`.choros-config/archive/`. Don't run it, or `choros archive`, on the
workspace your own session runs in.

## --deps auto

Needs a `check` for the repo. If the check fails without the sets it left
out, the land goes on top of everything queued instead, and names those sets
and a log of the check's output. The check isn't rerun there, so the
commits may need those sets or may be broken: run it yourself to tell.

A check goes in `<trunk>/.choros-config/repos.toml`:

    [app]
    check = "cargo check --workspace"   # run at the repo root
    deps = "auto"                       # optional: plain lands use auto

## After landing

`choros status` shows each set; `! parked` means a conflict to resolve.
To change an open set, such as for review fixes, work in its landing folder,
`<trunk>/landing/<queue>/landing-N`: commit and `git push`, and the commits
join the set (a rewrite doesn't). `choros landing open N` resumes the
session forked there.
