# Terminal Setup

Agent Core uses the [Kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/) for reliable modifier key detection. Most modern terminals support this protocol, but some require configuration.

## Capability Overrides

Agent Core auto-detects OSC 8 hyperlinks, inline image protocols, and truecolor. If detection fails behind a terminal proxy or multiplexer, use these advanced overrides:

| Capability | Environment variable | JSON setting |
|------------|----------------------|--------------|
| OSC 8 hyperlinks | `AGENT_CORE_HYPERLINKS=1\|0\|auto` | `terminal.hyperlinks: true\|false\|"auto"` |
| Inline images | `AGENT_CORE_IMAGE_PROTOCOL=kitty\|iterm2\|none\|auto` | `terminal.images: "kitty"\|"iterm2"\|false\|"auto"` |
| Truecolor | `AGENT_CORE_TRUE_COLOR=1\|0\|auto` | `terminal.trueColor: true\|false\|"auto"` |

Settings take precedence over environment variables; unset or `auto` preserves detection. Only force capabilities supported by the complete terminal path, since unsupported escape sequences can corrupt rendering.

## Kitty

Works out of the box.

## iTerm2

### Regular TUI mode

Works out of the box.

### Fullscreen TUI mode

Agent Core owns the viewport, so iTerm2 sends mouse-wheel reports instead of scrolling its native scrollback. With iTerm2's default fast-trackpad behavior, those reports can lose most of an accelerated wheel delta, making fullscreen scrolling much slower than regular scrolling.

If fast mouse-wheel gestures move only about one line at a time in fullscreen mode:

1. Open **iTerm2 → Settings → Advanced**.
2. Search for **Trackpad scrolls fast?** and set it to **No**.

This is an iTerm2-wide workaround and may also change native trackpad scrolling. The underlying behavior is tracked in [iTerm2 issue 9619](https://gitlab.com/gnachman/iterm2/-/work_items/9619).

## Apple Terminal

Agent Core enables enhanced key reporting when available. If Terminal.app still sends plain Return for `Shift+Enter`, agent-core uses a local macOS modifier fallback to treat that Return as `Shift+Enter`.

This fallback only works when agent-core runs on the same Mac as Terminal.app. It cannot detect the local keyboard over remote SSH.

## Ghostty

Add to your Ghostty config (`~/Library/Application Support/com.mitchellh.ghostty/config` on macOS, `~/.config/ghostty/config` on Linux):

```
keybind = alt+backspace=text:\x1b\x7f
```

Older Claude Code versions may have added this Ghostty mapping:

```
keybind = shift+enter=text:\n
```

That mapping sends a raw linefeed byte. Inside agent-core, that is indistinguishable from `Ctrl+J`, so tmux and agent-core no longer see a real `shift+enter` key event.

If Claude Code 2.x or newer is the only reason you added that mapping, you can remove it, unless you want to use Claude Code in tmux, where it still requires that Ghostty mapping.

Agent Core binds `Ctrl+J` as a default newline alias, so `Shift+Enter` keeps working in tmux via that remap without extra agent-core configuration.

### Fullscreen TUI mode

In fullscreen mode, links remain clickable, but Ghostty does not show its hover underline or lower-left URL preview while agent-core captures mouse input. Hold `Shift+Command` on macOS or `Shift+Ctrl` on Linux to use Ghostty's native link handling.

## WezTerm

WezTerm usually works out of the box for `Shift+Enter` via xterm modifyOtherKeys. To use the Kitty keyboard protocol explicitly, create `~/.wezterm.lua`:

```lua
local wezterm = require 'wezterm'
local config = wezterm.config_builder()
config.enable_kitty_keyboard = true
return config
```

On macOS, WezTerm binds `Option+Enter` to fullscreen by default. To use `Option+Enter` for agent-core follow-up queueing, add this key override:

```lua
local wezterm = require 'wezterm'
local config = wezterm.config_builder()
config.keys = {
  {
    key = 'Enter',
    mods = 'ALT',
    action = wezterm.action.SendString('\x1b[13;3u'),
  },
}
return config
```

If you already have a `config.keys` table, add the entry to it.

On WSL, WezTerm may require a visible hardware cursor for IME candidate window positioning. If CJK IME candidates do not follow the text cursor, set `AGENT_CORE_HARDWARE_CURSOR=1` before running agent-core or set `showHardwareCursor` to `true` in settings.

## Alacritty

Alacritty usually works out of the box for `Shift+Enter`. On macOS, `Option+Enter` may arrive as plain `Enter`. To use `Option+Enter` for agent-core follow-up queueing, add to `~/.config/alacritty/alacritty.toml`:

```toml
[[keyboard.bindings]]
key = "Enter"
mods = "Alt"
chars = "\u001b[13;3u"
```

Restart Alacritty after changing the config.

## VS Code (Integrated Terminal)

VS Code 1.109.5 and newer enable Kitty keyboard protocol in the integrated terminal by default, so `Shift+Enter` should work out of the box.

VS Code versions older than 1.109.5 need an explicit terminal keybinding for `Shift+Enter`.

`keybindings.json` locations:
- macOS: `~/Library/Application Support/Code/User/keybindings.json`
- Linux: `~/.config/Code/User/keybindings.json`
- Windows: `%APPDATA%\\Code\\User\\keybindings.json`

Add to `keybindings.json`:

```json
{
  "key": "shift+enter",
  "command": "workbench.action.terminal.sendSequence",
  "args": { "text": "\u001b[13;2u" },
  "when": "terminalFocus"
}
```

## Zed (Integrated Terminal)

Add these key bindings to your Zed `keymap.json`:

```json
{
  "context": "Terminal",
  "bindings": {
    "shift-enter": ["terminal::SendText", "\u001b[13;2u"],
    "ctrl--": ["terminal::SendText", "\u001b[45;5u"],
    "ctrl-alt-]": ["terminal::SendText", "\u001b[93;7u"]
  }
}
```

## Windows Terminal

Agent Core uses Windows-style keybindings when running natively on Windows or in WSL:

- `Alt+V` pastes an image or clipboard text.
- `Ctrl+F` searches the transcript in fullscreen mode, and `Ctrl+Up`/`Ctrl+Down` jump between marked messages.
- `Alt+P` cycles to the previous model.
- `Ctrl+Z` undoes editing on native Windows; WSL uses `Alt+Z` so `Ctrl+Z` can suspend agent-core.
- `Ctrl+Q` queues a follow-up message and `Alt+Q` restores queued messages.

Add to `settings.json` (Ctrl+Shift+, or Settings → Open JSON file) to forward `Shift+Enter` for inserting a new line:

```json
{
  "actions": [
    {
      "command": { "action": "sendInput", "input": "\u001b[13;2u" },
      "keys": "shift+enter"
    }
  ]
}
```

Windows Terminal binds `Alt+Enter` to fullscreen by default. To use it instead of agent-core's `Ctrl+Q` default for follow-up queueing, configure Windows Terminal to send the key and bind `app.message.followUp` to `alt+enter` in agent-core.

If you already have an `actions` array, add the object to it. Fully close and reopen Windows Terminal after changing its settings.

## xfce4-terminal, terminator

These terminals have limited escape sequence support. Modified Enter keys like `Ctrl+Enter` and `Shift+Enter` cannot be distinguished from plain `Enter`, preventing custom keybindings such as `submit: ["ctrl+enter"]` from working.

For the best experience, use a terminal that supports the Kitty keyboard protocol:
- [Kitty](https://sw.kovidgoyal.net/kitty/)
- [Ghostty](https://ghostty.org/)
- [WezTerm](https://wezfurlong.org/wezterm/)
- [iTerm2](https://iterm2.com/)
- [Alacritty](https://github.com/alacritty/alacritty) (requires compilation with Kitty protocol support)

## IntelliJ IDEA (Integrated Terminal)

The built-in terminal has limited escape sequence support. Shift+Enter cannot be distinguished from Enter in IntelliJ's terminal.

If you want the hardware cursor visible, set `AGENT_CORE_HARDWARE_CURSOR=1` before running agent-core (disabled by default for compatibility).

Consider using a dedicated terminal emulator for the best experience.

## tmux

Agent Core works inside tmux, but tmux strips modifier information from certain keys by default. Without configuration, `Shift+Enter` and `Ctrl+Enter` are usually indistinguishable from plain `Enter`.

### Recommended configuration

Add to `~/.tmux.conf`:

```tmux
set -g extended-keys on
set -g extended-keys-format csi-u
```

Then restart tmux fully:

```bash
tmux kill-server
tmux
```

Agent Core requests extended key reporting automatically when Kitty keyboard protocol is not available. With `extended-keys-format csi-u`, tmux forwards modified keys in CSI-u format, which is the most reliable configuration. The `extended-keys-format` option requires tmux 3.5 or later.

### Why `csi-u` is recommended

With only:

```tmux
set -g extended-keys on
```

tmux defaults to `extended-keys-format xterm`. When an application requests extended key reporting, modified keys are forwarded in xterm `modifyOtherKeys` format such as:

- `Ctrl+C` → `\x1b[27;5;99~`
- `Ctrl+D` → `\x1b[27;5;100~`
- `Ctrl+Enter` → `\x1b[27;5;13~`

With `extended-keys-format csi-u`, the same keys are forwarded as:

- `Ctrl+C` → `\x1b[99;5u`
- `Ctrl+D` → `\x1b[100;5u`
- `Ctrl+Enter` → `\x1b[13;5u`

Agent Core supports both formats, but `csi-u` is the recommended tmux setup.

### What this fixes

Without tmux extended keys, modified Enter keys collapse to legacy sequences:

| Key | Without extkeys | With `csi-u` |
|-----|-----------------|--------------|
| Enter | `\r` | `\r` |
| Shift+Enter | `\r` | `\x1b[13;2u` |
| Ctrl+Enter | `\r` | `\x1b[13;5u` |
| Alt/Option+Enter | `\x1b\r` | `\x1b[13;3u` |

This affects the default keybindings (`Enter` to submit, `Shift+Enter` for newline) and any custom keybindings using modified Enter.

### Requirements

- tmux 3.5 or later for `extended-keys-format csi-u` (run `tmux -V` to check)
- A terminal emulator that supports extended keys (Ghostty, Kitty, iTerm2, WezTerm, Windows Terminal)

With tmux 3.2 through 3.4, omit `extended-keys-format csi-u`; Agent Core still supports tmux's default xterm `modifyOtherKeys` format.

## Windows (shell setup)

Agent Core uses Git Bash by default on Windows. Checked locations (in order):

1. Custom path from `~/.agent-core/agent/settings.json`
2. Git Bash (`C:\Program Files\Git\bin\bash.exe`)
3. `bash.exe` on PATH (Cygwin, MSYS2, WSL)

For most users, [Git for Windows](https://git-scm.com/download/win) is sufficient.

### PowerShell tool

The optional `powershell` tool runs commands through `pwsh.exe` when available, otherwise Windows PowerShell. It starts PowerShell with `-NoProfile -NonInteractive -ExecutionPolicy Bypass`. Administrator-enforced execution policies can still take precedence.

Use `defaultTools` to replace the model-facing `bash` tool:

```json
{
  "defaultTools": ["read", "powershell", "edit", "write"]
}
```

Or enable both while comparing behavior:

```json
{
  "defaultTools": ["read", "bash", "powershell", "edit", "write"]
}
```

The `!` and `!!` editor commands still use Bash.

### Custom bash path

```json
{
  "shellPath": "C:\\cygwin64\\bin\\bash.exe"
}
```

## Shell aliases

Agent Core runs bash in non-interactive mode (`bash -c`), which doesn't expand aliases by default.

To enable your shell aliases, add to `~/.agent-core/agent/settings.json`:

```json
{
  "shellCommandPrefix": "shopt -s expand_aliases\neval \"$(grep '^alias ' ~/.zshrc)\""
}
```

Adjust the path (`~/.zshrc`, `~/.bashrc`, etc.) to match your shell config.
