---
name: finish
description: Finish work on the current branch by committing, rebasing, pushing, creating a PR, and cleaning up the worktree
disable-model-invocation: true
argument-hint: [target-branch]
allowed-tools: Bash
---

Finish the current branch by merging it into `$ARGUMENTS`. Follow these steps exactly:

## Step 1: Identify current branch

Run `git branch --show-current` to get the current working branch name. This is the **work branch**.

If the work branch is `$ARGUMENTS` itself, stop and tell the user they cannot finish into the same branch they are on.

## Step 2: Commit uncommitted changes

Run `git status --short` to check for uncommitted changes.

If there are changes:
- Stage all changes with `git add -A`
- Run `git diff --cached --stat` to summarize what will be committed
- Create a commit with a concise message summarizing the staged changes

If there are no changes, skip this step.

## Step 3: Rebase onto latest target branch

- Fetch latest from remote: `git fetch origin $ARGUMENTS`
- Rebase onto the target: `git rebase origin/$ARGUMENTS`

If the rebase fails due to merge conflicts:
- For each conflicted file, examine the conflict markers and resolve them intelligently by understanding the intent of both sides
- After resolving all conflicts in a file, run `git add <file>`
- Continue the rebase with `git rebase --continue`
- Repeat until the rebase completes
- If a conflict is ambiguous and you cannot confidently resolve it, abort the rebase with `git rebase --abort` and ask the user for guidance

## Step 4: Force push to remote

Run `git push --force-with-lease origin HEAD` to push the rebased branch to the remote.

If the remote branch does not exist yet, use `git push -u origin HEAD` instead.

## Step 5: Create PR

Use `gh pr create` to create a pull request:
- Base branch: `$ARGUMENTS`
- Title: derive from the branch name and commit messages (keep under 70 characters)
- Body: summarize the changes from all commits on this branch (use `git log origin/$ARGUMENTS..HEAD --oneline` to see them)

Use this format:
```
gh pr create --base $ARGUMENTS --title "the title" --body "$(cat <<'EOF'
## Summary
<bullet points summarizing changes>

Generated with Claude Code
EOF
)"
```

If a PR already exists for this branch, skip creation and show the existing PR URL instead.

## Step 6: Clean up worktree and local branch

Determine if the current directory is inside a git worktree by running `git rev-parse --git-common-dir` and comparing it to `git rev-parse --git-dir`.

If it IS a worktree:
- Note the worktree path: run `pwd` to get it
- Tell the user to leave the worktree directory first, then provide ready-to-copy commands:

```
cd <main-repo-path>
git worktree remove <worktree-path>
git branch -D <work-branch>
```

If it is NOT a worktree (regular checkout):
- Check out the target branch: `git checkout $ARGUMENTS`
- Delete the work branch: `git branch -D <work-branch>`

## Step 7: Report to the user

Display a summary:
- Work branch name
- Target branch
- Number of commits included
- PR URL
- Cleanup status (done, or commands provided for worktree case)
