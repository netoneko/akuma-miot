# `miot-cli` — requirements

Design of record. Not built yet (Phase 3+). This is the contract the
implementation has to meet, written before any of it exists so the constraints
below are decisions rather than accidents.

---

## 0. The one non-negotiable

**Terminal scrollback must work exactly as it does for any other program.**

Scroll up and you see what happened. Select and copy works. `tmux` copy-mode
works. Piping to a file works. `Ctrl-C` leaves the terminal in a sane state.

That single requirement decides most of the rest, because it rules out the
whole class of design that would otherwise be reached for:

> **`miot-cli` is NOT a full-screen TUI.**
> No alternate screen buffer (`smcup`/`1049h`). No owning the viewport. No
> repainting a scrollable pane we implement ourselves.

Everything the litter says is written to stdout as ordinary lines, in order,
once. The terminal keeps them; we do not. An in-app scrollable history pane is
the thing that breaks native scrollback, so we do not have one — see §4.

---

## 1. Layout

A **line-oriented log that grows upward**, with a small fixed composer pinned
at the bottom. The composer is drawn with relative cursor moves only.

```
  ┌─ terminal scrollback (owned by the terminal, not by us) ────────────┐
  │                                                                     │
  │  14:02  tama   claimed t1.1                                         │
  │  14:04  kuro   audit locking — one lock unheld on the error path     │
  │  14:04  mimi   cleared t1.2                                         │
  │                                                                     │
  │  [obs]  #1180  Happened(Directed{to:mimi, t1, ArtifactNeeded})      │  ← §3
  │                                                                     │
  ├─────────────────────────────────────────────────────────────────────┤
  │  litter ▸ @tama can you re-run the build with -j1                    │  ← §2
  │          and paste just the failing lines_                           │
  └─────────────────────────────────────────────────────────────────────┘
```

The composer occupies N lines at the bottom. To print new output: clear the
composer, write the lines, redraw the composer. Output above never moves and
is never rewritten.

**Not a TTY** (piped, redirected, CI) → no composer, no colour, no cursor
tricks. Just the log, one line at a time.

---

## 2. Input

### Multiline

The composer is a **multiline editor**, not a single-line prompt.

| Key | Does |
|---|---|
| `Enter` | send |
| `Alt-Enter` / `Shift-Enter` | newline within the message |
| `Ctrl-J` | newline (the terminal-independent fallback — many terminals do not distinguish `Shift-Enter`) |
| `Up` / `Down` | move within the message when it is multiline; recall previous input when the cursor is at the first/last line |
| `Ctrl-C` | clear the composer; a second within 1 s exits |
| `Ctrl-D` | exit on an empty composer |

It grows as it needs to and scrolls internally past a cap (say 10 lines) so a
paste of 400 lines does not eat the screen.

### Addressing: the litter by default, participants by tag

**The default target is the whole litter.** Typing prose and pressing Enter
broadcasts. That is the common case and it costs no syntax.

`@name` **tags a participant**. Tagging is addressing:

```
  can everyone look at the lock ordering          → the litter
  @tama re-run the build with -j1                 → tama, and the litter sees it
  @tama @kuro compare your findings               → both, and the litter sees it
```

Rules:

- A tag anywhere in the message tags that participant. Leading position is
  conventional, not required.
- Tagged traffic is **waking** for whoever is tagged (it assembles a turn);
  untagged litter traffic is **non-waking** by default. This is the same
  asymmetry the protocol already enforces — waking four agents per broadcast
  turns one message into four LLM turns.
- `@litter` explicitly targets everyone and wakes nobody in particular.
- An unknown `@name` is a **soft warning printed above the composer, not a
  refusal** — the message still sends. Refusing to send because a name was
  misspelled is worse than sending it.
- Tab-completes from the on-chain roster.
- The composer shows the resolved target on its prompt: `litter ▸`,
  `→ tama ▸`, `→ tama,kuro ▸`.

---

## 3. Protocol observation

A **toggle**, off by default, that prints raw protocol traffic **below the log
and above the composer** as it happens.

```
  miot> /obs              toggle
  miot> /obs on|off
  miot> /obs tasks        only task lifecycle effects
  miot> /obs all          every effect, including non-waking broadcast
```

- Off by default because the interesting output is what the cats say, not the
  bookkeeping underneath it.
- When on, each line is prefixed `[obs]` and dimmed, so it is visually
  subordinate and greppable.
- It is **printed, not paged** — observation lines go into scrollback like
  everything else. No separate pane.
- Shows: block height, the `Effect` variant, the task it concerns, and who it
  was addressed to. That is `Effect`'s own vocabulary, unmodified — the CLI
  does not invent a second spelling of it.

---

## 4. Message history

**Available, but it must never compete with scrollback.**

- On start, the CLI prints a **replay** of recent litter traffic as ordinary
  lines, so scrollback has context. Bounded (`--history N`, default small) so
  a fresh start is not a wall of text.
- `/history [n]` prints the last `n` into scrollback on demand.
- `/history --task t42` prints one task's traffic.
- There is **no in-app scrollable history viewer.** If one is ever wanted it
  ships **disabled by default** and behind an explicit flag, because the moment
  it takes the viewport it has broken §0.
- For real archaeology, `miot log` (§5) is a non-interactive dump that pipes to
  `less`, `grep`, or a file. That is the right tool, and it is the terminal's
  job, not ours.

---

## 5. Commands

Simple. Flat. No nested menus. Slash commands inside the interactive session,
subcommands outside it — and they are the same verbs.

### Interactive (`miot`)

| | |
|---|---|
| `/task <text>` | open a parent task (operator only) |
| `/plan` | show the current plan |
| `/tasks` | one line per live task: id, status, assignee, lease |
| `/artifact <id>` | print a closed parent's report |
| `/peers` | the roster, with who is leader |
| `/obs [on\|off\|tasks\|all]` | §3 |
| `/history [n]` | §4 |
| `/quit` | leave |

Anything not starting with `/` is a message (§2).

### Non-interactive (`miot <cmd>`)

The operator's hands, and what scripts and the failure drills use. One-shot:
sign one extrinsic or read state, print, exit.

```
  miot run --as tama              run an agent loop (this is the cat)
  miot task open "debate & report"
  miot task list
  miot artifact t42               → markdown on stdout, pipe it anywhere
  miot peers
  miot log [--task t42] [--follow]
```

`miot run` is the only long-lived one, and it is **not interactive** — it emits
events, it does not draw a composer. An agent has no keyboard.

---

## 6. Visual style

Copied from Akuma. `assets/` holds the vendored art.

- **Banner**: `akuma_40.txt` on start, once. Not repeated.
- **Per-sender avatars**: `akuma_20.txt` with **one ANSI colour per sender**,
  reused every time that sender speaks — exactly what
  `meow/src/tools/litter/observe.rs` does, and for the stated reason: it is
  what makes a scroll of the whole litter's back-and-forth readable at a
  glance. Bright variants first; the art is mostly `%#*+` glyphs and reads
  badly dim on a dark terminal.
- Avatars are opt-out (`--no-avatars`) because they cost vertical space, and
  the compact form is a coloured name.
- Colour follows `NO_COLOR` and disables itself when stdout is not a TTY.
- No box-drawing around the log. Boxes and scrollback are enemies.

---

## 7. What it must not do

- Enter the alternate screen.
- Redraw anything above the composer.
- Hold the terminal on `Ctrl-C` or leave it without a cursor.
- Invent a second vocabulary for `Effect` — the observation view and the event
  log use the protocol's own names. Two spellings of one thing is the drift
  this project exists to avoid.
- Block the agent loop on a human. `miot run` and `miot` are separate
  processes with separate accounts; the CLI is a client of the chain like
  anything else.

---

## 8. Open

- **Streaming a turn.** A model's output arrives token by token over minutes.
  Printing it live is nice but it interleaves badly with other cats' traffic
  arriving mid-stream. Options: buffer the turn and print it whole, or print a
  `tama is thinking…` line that is replaced on completion (a rewrite, which
  §0 resists). Leaning toward buffering with a spinner on the composer line.
- **Tagging semantics on chain.** Is `@tama` a field on the message record, or
  is it parsed out of the body by each agent? A field is checkable and cannot
  be misspelled into invisibility; the body is what the model actually writes.
  Probably both: the CLI parses and sets the field, the agent reads the field.
