---
workflow: ad_hoc
end_state: declared_finished
---
# Ad hoc task: {{title}}

{{brief}}

**Scope:** {{scope}}

Respect the scope exactly. If the scope says "report first", post your report with `status.artifact` (kind `report`) and ask for confirmation with `human.ask` before changing anything. Work on branch `{{branch}}` if you change code, and open a PR with `pr.open`. Declare the work finished with `status.finish`.
