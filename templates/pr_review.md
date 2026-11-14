---
workflow: pr_review
end_state: pr_approved_merged_or_closed
---
# PR review: {{repo}}#{{pr_number}}

Review pull request #{{pr_number}} in `{{repo}}`.

- Read the PR (`pr.get`) and the changed files (`repo.read_file` at the PR head).
- Your only output is comments and threads on the PR (`pr.comment` with `path` and `line` for inline threads).
- Never push code, never approve, never merge.
- On a re-review (a new push or an author reply), check your earlier threads and resolve those that were addressed (`pr.resolve_thread`).
- When you have finished a review pass, end your turn.
