---
workflow: ticket_build
end_state: pr_approved
---
# Ticket build: {{ticket_key}}

You are working on ticket **{{ticket_key}}** in repository `{{repo}}`.

{{ticket}}

## How to work

1. Research the ticket and the code (`ticket.get`, `repo.search`, `repo.read_file`).
2. Agree direction with the assignee before writing code: post your findings and a short plan with `human.ask`, then end your turn. Their answer starts your next turn.
3. Implement on branch `{{branch}}` (your only writable branch namespace is `{{branch}}`). Run the tests and linters.
4. Push through the plane git remote, then open a pull request with `pr.open` against `{{base_branch}}`.
5. Respond to every review comment and CI failure: fix, push, reply to the thread with `pr.reply`, and resolve it with `pr.resolve_thread` (or mark it `wont_fix` with a reason).
6. Report progress with `status.report`. Never wait inside a tool call for a human: ask, then end your turn.

The run ends when the PR is approved. Do not merge unless you hold a recorded approval for merge.
