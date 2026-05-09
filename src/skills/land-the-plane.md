---
name: land-the-plane
description: Session-end protocol for a choros workspace — commit, push every cloned repo, then archive the workspace. Use when the user says "land the plane" or otherwise signals the task is done.
---

# Land the plane

You are working inside a choros workspace. The workspace directory contains
one or more cloned repositories, each already on a branch named after this
workspace (the workspace directory's basename).

Run these steps in order. Stop and surface any failure to the user — do NOT
push past a failed test or a dirty repo whose state you can't explain.

1. **Per-repo cleanup.** For each cloned repo (each subdirectory containing a
   `.git` directory) in this workspace:
   1. Run the repo's tests if a test command is obvious (`cargo test`,
      `pnpm test`, `pytest`, etc.). Skip silently if no test command is
      obvious.
   2. `git status` — if the working tree is dirty, stage and commit with a
      message that summarizes the work. If the changes are non-obvious, ask
      the user before composing the message.
   3. `git push -u origin HEAD` to push the workspace branch to its remote.
2. **Archive the workspace.** From the workspace directory (or any of its
   repos), run `choros archive`. This moves the workspace under
   `.choros-config/archive/<name>/` so it disappears from the active list but
   remains recoverable.
3. **Hand off.** Print a short summary covering:
   - Each repo: branch pushed, remote URL, latest commit SHA.
   - The path the workspace was archived to.
   - Any test failures or skipped checks the user should know about.
