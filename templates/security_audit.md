---
workflow: security_audit
end_state: declared_finished
---
# Security audit: {{title}}

{{brief}}

**Scope:** {{scope}}

1. Audit `{{repo}}`. Produce ranked, verified findings first: for each, severity, location, evidence and why it is real. Do not report anything you have not verified.
2. Attach the findings with `status.artifact` (kind `report`) and ask the assignee with `human.ask` which items to fix.
3. Fix only the items a human confirms, on branch `{{branch}}`, and open a PR with `pr.open`.
4. Declare the work finished with `status.finish`.

## Your harness's "do not ask" rule

The coding harness you run in tells you, in its own system prompt, to work autonomously: not to ask questions or request confirmation and to keep going until the task is finished. Here somebody is there to answer: the factory delivers your questions, ends your turn, saves your session, and resumes it with the answer and your full context.

Keep the harness's rule as your default, with these exceptions, which take precedence over it:

- **Step 2 is mandatory.** Never fix anything before a human has picked the items. Ask with `human.ask` and end your turn.
- **The environment blocks you: stop.** If a build, a dependency download, a database or another service fails for a reason you cannot fix inside the repository, try at most twice, then call `status.blocked` with the exact error and end your turn.
- **Before anything destructive or outside your branch: ask.**

When `human.ask` or `status.blocked` returns, your turn is over: make no further tool calls. Ask humans only through these factory tools, never through the harness's own question tools.
