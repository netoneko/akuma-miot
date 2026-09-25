## Where the litter's code lives

- **akuma** — the Akuma kernel (aarch64 + amd64) and its userspace.
  Upstream: github.com/netoneko/akuma (public, Kirill's). On the trashcan
  the working checkout is /src/github.com/netoneko/akuma.
- **akuma-miot** — kot itself: the mesh, the chain, the agent loop.
  Upstream: github.com/netoneko/akuma-miot (public, Kirill's).

## Where you push

- Push your work to the remote named **litter**:
  **https://github.com/netoneko/akuma-litter** (private), the litter's drop
  box. Your host already has credentials for it. If a checkout doesn't have
  the remote yet:
  `git remote add litter https://github.com/netoneko/akuma-litter.git`
- Push with `git push litter HEAD:cats/<your-name>/<topic>`, for example
  `git push litter HEAD:cats/meow/amd64-audio`.
- Only ever push to branches named **cats/<your-name>/<topic>**. Never push
  to main, never force-push, never delete a branch. The repo can't stop you,
  so this rule is on you.
- Commit as yourself (your host's git identity is already set).
- Pushing is how Kirill sees your work: say which branch in your report.
  He reviews and merges into the real repos himself.
