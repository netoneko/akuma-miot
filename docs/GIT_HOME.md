# A git home for the litter — the request, and what's decided

**Status, 2026-09-25 (late): decided and half-built.**

- **The repo:** `github.com/netoneko/akuma-litter`, **private**, created
  2026-09-25. GitHub, not self-hosted: "let's go with stupid github".
- **It is a drop box, not a source of truth.** GitHub's free plan has no
  rulesets or branch protection on a private repo (`403 Upgrade to GitHub Pro
  or make this repository public`), so a cat's write access can force-push or
  delete anything in it, `main` included. Kirill accepted that: nothing there
  is authoritative, he pulls from it into the real repos, and his clones are
  the backup. The ruleset that would apply on Pro is in §2.
- **Auth: fine-grained tokens, not deploy keys.** Kirill creates one per cat,
  repository access `netoneko/akuma-litter` only, permission *Contents: read
  and write*, and saves it on the mac as `~/.akuma/kot/<cat>.github_token`.
  `deploy.py up` installs it as that host's `/root/.git-credentials` (mode
  600, over the ssh channel, never the LAN HTTP `put`) and sets git's
  identity to `<cat> <<cat>@akuma.sh>`. Deploy keys were tried first and
  dropped: Akuma's own `ssh` wants a raw 32-byte key and stops at an
  interactive host-key prompt, where HTTPS git just works on the metal box.
- **Done by the end of 2026-09-25:** tokens for meow, tama, kuro and sora
  (sora's guest has no git, so its token isn't installed); deployed as
  `/root/.git-credentials`; `overlays/deploy/context/projects.md` and
  `00-teahouse.md` live on every home cat via `MIOT_CONTEXT`; meow added the
  `litter` remote itself. The repo is seeded with `akuma`'s `main` and
  `even-more-cats` from the mac, because the first push from the metal box —
  the whole history — died inside git with EBADF (an Akuma bug), and a seeded
  repo makes a cat's push only its own commits.
- **Still to do:** a first successful push from a cat, and the tokens'
  expiry (whatever Kirill chose) remembered before it bites.

The original status, before the decision: requested, not built; §3's
mechanism (`MIOT_CONTEXT`) built and waiting on the repo's address.

## 1. The request, as asked

> set up a git user in a separate container on aws, call it git.akuma.sh,
> auth only with an ssh keys from the cats and me, private repo from where i
> can pull changes if needed and they can push with no security risk
> — unless you can suggest another solution that would be more secure and
> preferable to this

> we will need to somehow update their context so they would know where the
> git source is and what the projects are — maybe we need to tweak the base
> personalities or allow injecting multiple source prompts as system via
> config

What it's for: meow already writes and commits kernel code on the trashcan
(`meow <meow@akuma.sh>`, M0 `d7b47df1`, M1 `c7cca4d2`/`4b97af7e` on its own
`amd64-audio` branch). Today that work lives only in the box's own checkout,
and the only way Kirill sees it is by ssh-ing in. The cats need somewhere to
push that isn't Kirill's GitHub credentials, and Kirill needs somewhere to pull
from.

What "no security risk" has to mean, concretely:

- A cat's key can **push to its own branches and nothing else**: no writing
  `main`, no force-push over another cat's work, no deleting refs.
- A cat's key can do **nothing but git**: no shell, no port forwarding, no
  agent forwarding, no pty.
- A leaked cat key costs that cat's branches, not the repo, the box, or the
  other cats.
- Kirill's key is the only one that can write `main` (or merge into it).

## 2. Where it lives — the choice

Two constraints hold for every option:

- **The cats can't use their kot identity for ssh today.** A cat's account is
  an ed25519 key, but `kot` holds it as a raw 32-byte seed and can't emit an
  OpenSSH private key (`CLAUDE.md`, "Known gaps"). Either each cat gets a
  separate git key (`ssh-keygen` on its host, private half never leaves it),
  or `kot id` learns to write the seed out in OpenSSH format. Then the git
  identity *is* the chain identity, one key per cat and nothing new to manage.
  The second is a small change and the tidier result.
- **meow's box has no ssh client worth trusting yet.** The trashcan's userland
  is busybox plus what's been staged; `git push` over ssh needs `ssh` on the
  box. Over HTTPS it needs only git and a token. Check before choosing a
  transport for meow.

| | GitHub private repo + deploy keys | Self-hosted `git.akuma.sh` (nspawn + `git-shell`) | Forgejo/Gitea on AWS |
|---|---|---|---|
| new internet-facing service to run | none | one sshd, in a container | a web app + sshd |
| per-cat key, repo-scoped | yes, deploy key per cat (write) | yes, `authorized_keys` line per cat | yes |
| confine a cat to `cats/<name>/*` | a ruleset (branch protection by pattern) | an `update` hook, ~30 lines | built-in branch protection |
| no shell / no forwarding | GitHub's side | `restrict` + `git-shell` | its side |
| who holds the data | GitHub | the AWS box, beside yuki/shiro | the AWS box |
| Kirill pulls with | his normal GitHub access | `git@git.akuma.sh:…` | either |
| cost of getting it wrong | a key or a rule | an exposed sshd | a larger exposed app |

**Recommendation: GitHub private repo, a deploy key per cat, a ruleset.**
It meets every line of §1 with no new service to keep patched: a deploy key is
scoped to one repo, and a ruleset can require `main` to be written only by
Kirill and let each key create and update only its own branch pattern. The
cost is that the litter's work lives on a third party. If that's the
objection, the self-hosted column is the one to build, and its design is:

- a dedicated `systemd-nspawn` container (`kotctl`'s pattern, but its own),
  nothing in it but `git`, `openssh-server` and the bare repos;
- one Unix user `git`, shell `git-shell`, `authorized_keys` lines of the form
  `restrict,command="git-shell -c \"$SSH_ORIGINAL_COMMAND\"",environment="CAT=meow" ssh-ed25519 …`
  (`restrict` is no pty, no forwarding, no agent, no X11, no rc);
- an `update` hook that reads `$CAT` and refuses any ref outside
  `refs/heads/cats/$CAT/`, any non-fast-forward, and any delete, except for
  Kirill's key, which may write `main`;
- `git.akuma.sh` in Route 53, port 22 on the container (the host's own sshd
  stays where it is), nginx not involved;
- backed up by the same snapshot the box's other state is (none yet — note it).

**Open:** which column. Everything below is independent of it.

## 3. Telling the cats — `MIOT_CONTEXT`

The personas are character, not facts: `crates/kot/personas/<cat>.md` says
who a cat is and how it talks. Where the litter's code lives, what the
projects are and what's allowed changes on its own schedule, and it's the
same for every cat. So it belongs in a file every cat reads, not pasted into
seven personas.

**Built, 2026-09-25:** `kot run --context <paths>` / `MIOT_CONTEXT`, a
comma-separated list of files or directories (every `*.md` in a directory,
sorted by name). Each is appended to the system prompt after the persona,
under a `## <file name>` heading, before the shared rules. Read once at start,
so changing one is a restart. A path that doesn't exist is reported at start
and skipped, not fatal. `kot chat` takes the same flag.

**To do, once §2 is decided:**

- `overlays/deploy/context/` in this repo, holding at least `projects.md`:
  the repos, what each is, where its source is (`git.akuma.sh:…` or the
  GitHub URL), the branch rule (`cats/<name>/…`, never `main`), and who
  reviews. One source of truth, shipped to every host.
- `deploy.py up` copies that directory to `/root/kot/context/` and sets
  `MIOT_CONTEXT=/root/kot/context`; `kotctl sync` does the same into each
  container (`/kot/context`).
- Each cat's git key (§2) installed on its host, and the remote configured in
  its working checkout, so "push your branch" is one command it already knows
  where to send.
