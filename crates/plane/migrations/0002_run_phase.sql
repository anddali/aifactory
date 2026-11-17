-- Two-phase workflows (ticket build, ad hoc): a run researches and submits a plan ('plan'),
-- and only after a human approves it does it implement ('build'). NULL for single-phase
-- workflows and for runs created before phases existed (they behave as 'build').
ALTER TABLE runs ADD COLUMN phase TEXT;
