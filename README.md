# lite-term

## Install (macOS, Apple Silicon)

Download `lite-term-<version>-macos-arm64.dmg` from Releases, open it and drag lite-term to Applications.
The app is not notarized, so the first launch is blocked: right-click the app and choose Open, or run
`xattr -dr com.apple.quarantine /Applications/lite-term.app`.

Build it yourself with `packaging/build-app.sh` (needs Rust and the Xcode command line tools).

A small, fast terminal for macOS and Linux. Zero config: no settings file.

    cargo run --release            # login shell
    cargo run --release -- -e cmd  # run a command instead

## What makes it different

- **Command blocks.** With zsh (auto-enabled) or any shell that emits OSC 133, failed commands
  get a faint red tint on their output, and slow or failing commands show `exit N  1.2s` at the
  end of the command line. Cmd+Up / Cmd+Down jump between prompts.
- **Tabs and splits.** Cmd+T opens a tab in the current directory (the tab bar appears with the
  second tab). Cmd+D splits right, Cmd+Shift+D splits down; drag a divider to resize; inactive
  panes are dimmed. Cmd+Shift+Enter zooms one pane.
- **Reflow.** Resizing re-wraps lines (prompts stay intact), keeping the cursor on its text.
- **Ligatures** (`=>`, `!=`, `->`, `<=`, `|>`, ...) with Maple Mono, plus bold-as-bright colours,
  block / underline / bar cursors (blinking when an application asks), double-click to select a
  word, triple-click a line, and the window remembers its size and zoom.
- **Scroll rail.** The right edge shows where you are in the history; failed commands and find
  matches appear as ticks. Click or drag it to jump.
- **Find** (Cmd+F): highlights every match, Enter / Shift+Enter to step, Esc to close.
- **Links:** hold Cmd (Ctrl on Linux) and click a URL.
- **Attention:** the Dock icon bounces when a command that ran 8 s or longer finishes in the
  background.
- **Maple Mono NF** (rounded, with Nerd Font icons and powerline glyphs) is used automatically when installed in `~/Library/Fonts`, `~/.local/share/fonts` or `~/.fonts` (files `MapleMono-NF-{Regular,Bold,Italic,BoldItalic}.ttf`, SIL OFL); otherwise Menlo / DejaVu Sans Mono.
- Correct Thai (tone marks and vowels stack on the base letter), procedural box drawing,
  bold/italic/underline, truecolor, 20k lines of compact scrollback.

## Keys

| Key | Action |
| --- | --- |
| Cmd+C / Cmd+V | Copy selection / paste (Ctrl+Shift on Linux) |
| Cmd+T / Cmd+W | New tab / close pane or tab (Ctrl+Shift+T / W on Linux) |
| Cmd+Shift+[ / ] , Ctrl+Tab | Previous / next tab |
| Cmd+1 … 8, Cmd+9 | Go to tab N / last tab (Alt+N on Linux) |
| Cmd+D / Cmd+Shift+D | Split right / down (Ctrl+Shift+O / E on Linux) |
| Cmd+[ / ] | Previous / next pane |
| Cmd+Option+arrows | Focus the pane in that direction |
| Cmd+Ctrl+arrows / Cmd+Ctrl+= | Move the split / equalize splits |
| Cmd+Shift+Enter | Zoom the focused pane |
| Cmd+K | Clear screen and scrollback |
| Cmd+N | New window |
| Cmd+F | Find |
| Cmd+Up / Down | Previous / next prompt |
| Cmd+Left / Right / Backspace | Start of line / end of line / delete line |
| Cmd+= / Cmd+- / Cmd+0 | Zoom in / out / reset |
| Shift+PageUp / PageDown / Home / End | Scroll history |
| Option+Left / Right | Word left / right |

Mouse-aware programs (vim, tmux, htop) receive the mouse; hold Shift to select text instead.

Debugging: `LITE_TERM_LOG=/tmp/lt.log lite-term` records every byte read from the shell, every key sent and every resize with timestamps, so a display glitch can be replayed exactly.
