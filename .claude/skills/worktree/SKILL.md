---
name: worktree
description: Create a git worktree on a new branch off the current branch for parallel development
disable-model-invocation: true
argument-hint: [branch-name]
allowed-tools: Bash
---

Create a git worktree for the branch named: $ARGUMENTS

Follow these steps exactly:

## Step 1: Identify current branch

Run `git branch --show-current` and note the current branch name. This is the base branch.

## Step 2: Create a new local branch (without checking out)

Run `git branch $ARGUMENTS` to create a new branch named `$ARGUMENTS` off the current HEAD. Do NOT use `git checkout` or `git switch` — the user should stay on their current branch.

If the branch already exists, stop and inform the user.

## Step 3: Create the worktree

Run `git worktree add ../$ARGUMENTS $ARGUMENTS` to create a worktree in a sibling directory using the new branch.

## Step 4: Verify the worktree

Run `git worktree list` to confirm the worktree was created successfully.

## Step 5: Report to the user

Display a summary:
- Base branch (the branch the user was on)
- New branch name
- Worktree path (absolute)

Then show a ready-to-copy terminal command:

```
cd <absolute-worktree-path>
```
