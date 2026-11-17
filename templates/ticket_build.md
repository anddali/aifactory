---
workflow: ticket_build
end_state: pr_approved
phases: [plan, build]
---
# Ticket build: {{ticket_key}} (phase 1 of 2: plan)

You are working on ticket **{{ticket_key}}** in repository `{{repo}}`.

{{ticket}}

This run has two phases. You are in the **plan phase**: research the work and write a plan. A human reviews it, and only if they approve does the build phase start, with its own instructions. Until then nothing can be pushed to `{{branch}}` and no pull request can be opened.

## What to do now

1. Research the ticket and the code (`ticket.get`, `repo.search`, `repo.read_file`, and reading files in your checkout). You may run read-only commands and the existing tests to learn the baseline. Do not implement the change.
2. Submit your plan with `plan.submit`. Keep it short and concrete, with these sections:
   - **Understanding**: what the ticket asks for, in your words, and anything unclear.
   - **Approach**: the design, and why this one over the alternatives worth mentioning.
   - **Changes**: the files and modules you will add or change.
   - **Tests**: how you will verify it.
   - **Environment**: anything the runner needs that you found missing (services, credentials, network access).
   - **Risks and questions**: what could go wrong, and questions whose answers would change the plan.
   - **Size**: small, medium or large, and whether the ticket should be split.
   Prefer the smallest design that does what the ticket asks.
3. After `plan.submit` your turn is over. The reviewer approves the plan, asks for changes (revise it and submit it again), or decides not to build.

Do not hand implementation to sub-agents in this phase.

## Your harness's "do not ask" rule

The coding harness you run in tells you, in its own system prompt, to work autonomously: not to ask questions or request confirmation, and to keep going until the task is finished. That rule assumes nobody is there to answer. Here somebody is: the factory delivers your plan or question, ends your turn, saves your session, and resumes this same session with the reply and your full context. These instructions take precedence over that rule in these cases:

- **The plan review is mandatory.** Submitting the plan and ending your turn is the task of this phase, however small the ticket looks.
- **When the environment blocks you, stop.** If something you need fails for a reason you cannot fix inside the repository (network, DNS, missing credentials, missing services or tools), try at most twice, then call `status.blocked` with the exact error. Do not retry variations of the same command.

When `plan.submit`, `human.ask` or `status.blocked` returns, your turn is over: make no further tool calls. Ask humans only through these factory tools, never through the harness's own question tools.

<!-- build phase -->
# Ticket build: {{ticket_key}} (phase 2 of 2: build)

Your plan was approved. Build it in repository `{{repo}}`.

1. Implement on branch `{{branch}}` (your only writable branch namespace is `{{branch}}`). Run the tests and linters.
2. Push through the plane git remote, then open a pull request with `pr.open` against `{{base_branch}}`.
3. Respond to every review comment and CI failure: fix, push, reply to the thread with `pr.reply`, and resolve it with `pr.resolve_thread` (or mark it `wont_fix` with a reason).
4. Report progress with `status.report`.

Follow the approved plan. If it turns out to need a material change (a different design, a much larger scope), ask with `human.ask` before going further; otherwise make reasonable assumptions and state them in `status.report`. If the environment blocks you, try at most twice, then call `status.blocked` with the exact error. When `human.ask` or `status.blocked` returns, your turn is over.

The run ends when the PR is approved. Do not merge unless you hold a recorded approval for merge.
