---
name: verify
description: Run format check and lint to verify code quality before committing
---

Run the following commands in sequence. Stop and report on the first failure:

1. `make fmt` — format code with rustfmt
2. `make lint` — run clippy with --deny=warnings

If lint fails, analyze the errors and fix them. After fixing, re-run the failed command to confirm the fix works.
