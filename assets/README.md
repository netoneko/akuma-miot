# assets

Akuma's face, vendored.

Local copies rather than a reach across the source tree — the same reasoning
`meow/src/tools/litter/observe.rs` and `sshd/src/protocol.rs` each give for
keeping their own copy. Two sizes, used the way Akuma uses them:

| File | Size | Used for |
|---|---|---|
| `akuma_40.txt` | 16 lines | Banner. One-time splash — a boot, a login, a README header. |
| `akuma_20.txt` | 7 lines | Avatar. Repeated once per message, so it has to stay small next to the actual text. |

`miot-cli`'s log view will want `akuma_20.txt` with one ANSI colour per sender,
exactly as `observe.rs` does it: the colour is what makes a scroll of the whole
litter's back-and-forth readable at a glance.

Sources: `akuma/src/akuma_{20,40}.txt`. There are also `akuma_79.txt` and
`akuma_120.txt` upstream if a wider banner is ever wanted.
