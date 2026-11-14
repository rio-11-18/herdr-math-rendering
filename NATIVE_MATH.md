# Native math for Herdr

This is an experimental fork of Herdr v0.9.5. It renders LaTeX equations in the
original Codex or Claude terminal pane, alongside normal text. Equations move
with the answer when the agent redraws its screen or the user scrolls. The
implementation keeps equation images in the client and updates their placement
instead of uploading every PNG again on each scroll step.

## Why this is a fork

Herdr plugin v1 can run actions and event hooks and open terminal panes. It
cannot replace the terminal's text cells with typeset images in the same pane
or change the client graphics protocol. This feature therefore changes Herdr's
server and client. Installing this branch with `herdr plugin install` will not
work. This is not an official Herdr release.

## Requirements

- macOS and a terminal with Kitty graphics support (tested with WezTerm).
- Rust, Zig (as required by Herdr's build), Node.js 20 or later, and npm.
- Codex or Claude output using `$...$` for inline equations and display
  equations with `$$` on separate lines.

## Build

From this repository:

```sh
npm ci --prefix math-helper
cargo build --release --locked
```

The build creates `target/release/herdr`. A local installation needs both that
binary and the `math-helper` directory, including its installed npm
dependencies. Set `HERDR_NATIVE_MATH=1` and `HERDR_MATH_HELPER` to the absolute
path of `math-helper/render.mjs` when launching the binary. Both the Herdr
server and a newly opened display client must run this build. Keep a backup of
the existing Herdr executable and configuration before replacing an install.

For Codex, disable its own terminal math formatting in
`~/.codex/config.toml`, then restart Codex once:

```toml
[tui.rendering]
math = false
```

Otherwise Codex may turn LaTeX into character art before Herdr can render it.
Ask agents to write normal LaTeX rather than ASCII or Unicode fraction art.

## Verification

Ask Codex or Claude for a response containing several fractions. Scroll up and
down through the answer. The rendered equations should stay with the text;
they should not reveal raw `\frac` or `$$` after scrolling. Reopen the Herdr
display client after updating the binary: an old client can still connect to
the new server, but it will not gain the reduced image-upload behavior.

## Known limits

- An equation clipped at the edge before its full contents have ever been
  visible may remain text until it becomes fully visible.
- An equation taller than the pane cannot be captured as a whole and may
  remain text at the edge.
- The line currently being streamed by an agent can briefly appear as text.
- Terminal CPU use is still higher than without math because Herdr redraws
  math-bearing panes more often. Further client protocol work is needed to
  remove that cost.

## Validation

The completed local build passed 3,662 Herdr tests. A Codex-driven scroll
reproduction and WezTerm-compatible terminal replay showed no raw LaTeX or
misplaced images after scrolling. In the measured run, PNG uploads during a
scroll fell from 105–154 to 2–4 and median scroll response from 87–90 ms to
32 ms. These are local measurements, not a performance guarantee for other
machines.

The upstream Herdr code remains under [Apache-2.0](LICENSE). The helper's npm
package declares MIT; third-party dependencies retain their own licenses.
