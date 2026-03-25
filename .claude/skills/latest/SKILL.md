---
name: latest
description: Check recent changes and install the latest clust CLI in parallel
allowed-tools: Agent
---

Run both of these agents **in parallel** using the Agent tool (two Agent calls in a single message):

## Agent 1: Check changes

Launch a `clust-cli-installer` agent with this prompt:

> Check what changes have been made to the clust CLI repository at /Users/erikdejager/repos/clust-cli. Run `git log --oneline -10` and `git diff HEAD~1 --stat` to summarize recent changes. Do NOT install anything — only report what changed.

## Agent 2: Install latest

Launch a `clust-cli-installer` agent with this prompt:

> Install the latest version of the clust CLI from the repository at /Users/erikdejager/repos/clust-cli. Follow your standard installation procedure: pull latest, build, install, and verify.

After both agents complete, present a combined summary to the user showing:
1. What changed
2. Installation result
