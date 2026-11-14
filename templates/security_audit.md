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
