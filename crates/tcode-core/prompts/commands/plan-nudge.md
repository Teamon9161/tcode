The turn you are in was asked to plan before doing anything (the `/plan` request at its start), and it is about to end without a plan file. That is not allowed to happen silently.

Either:
- produce the plan now with the `progress` tool — `state: "draft"` to draft it, then `state: "active"` to submit it for the user's review; or
- tell the user plainly why you believe no plan is needed, and stop.

Do not start executing the task before a plan is approved. If you already explained why no plan is possible, say so again in one short sentence and stop.
