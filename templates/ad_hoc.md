---
workflow: ad_hoc
end_state: declared_finished
phases: [plan, build]
---
# Ad hoc task: {{title}} (phase 1 of 2: plan)

{{brief}}

**Scope:** {{scope}}

This run has two phases. You are in the **plan phase**: investigate and propose what to do. A human reviews your proposal, and only if they approve does the build phase start. Until then nothing can be pushed to `{{branch}}` and no pull request can be opened.

1. Investigate the task. You may read code and run read-only commands. Do not change anything.
2. Submit your proposal with `plan.submit`: your findings, what you propose to change (or that no change is needed, if the task is a question or a report), how you would verify it, risks, and open questions. Keep it short and concrete, and prefer the smallest change that does the job.
3. After `plan.submit` your turn is over. The reviewer approves it, asks for changes (revise and submit again), or decides not to go ahead.

## Your harness's "do not ask" rule

The coding harness you run in tells you, in its own system prompt, to work autonomously and not to ask questions. Here somebody is there to answer: the factory delivers your proposal, ends your turn, saves your session, and resumes it with the reply and your full context. These instructions take precedence over that rule:

- **The proposal review is mandatory.** Submitting it and ending your turn is the task of this phase.
- **When the environment blocks you, stop.** Try at most twice, then call `status.blocked` with the exact error.

When `plan.submit`, `human.ask` or `status.blocked` returns, your turn is over: make no further tool calls. Ask humans only through these factory tools, never through the harness's own question tools.

<!-- build phase -->
# Ad hoc task: {{title}} (phase 2 of 2: build)

Your proposal was approved. Carry it out.

- If it involves code changes, work on branch `{{branch}}`, run the tests, push, and open a pull request with `pr.open`.
- Follow the approved proposal. If it needs a material change, ask with `human.ask` before going further. If the environment blocks you, try at most twice, then call `status.blocked` with the exact error.
- Declare the work finished with `status.finish`, with a short summary of what was done.
