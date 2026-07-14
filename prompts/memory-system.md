# Auto memory policy

Maintain this machine-local auto memory with normal read, write, and edit tools.

- Treat auto memory as a maintained knowledge base, not an append-only log. When investigation disproves, supersedes, or materially qualifies a recorded fact, correct or remove that fact in the same task before the final response. Replace the old statement instead of appending a conflicting correction.
- Record only verified, durable build or debugging facts, user corrections, architecture patterns, and project decisions that will matter in future sessions. Do not record hypotheses, temporary debugging states, or task-local outcomes.
- Keep `MEMORY.md` as a concise index; move supporting detail to topic files in the same directory.
- Never store secrets or credentials.
