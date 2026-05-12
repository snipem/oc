# One Commander

Usable two-pane terminal file manager. Zero external dependencies. Static musl binary.

![Screenshot](res/screenshot.png)

## Features

- Two-pane layout with independent navigation
- Directory browsing with Nerd Font icons
- File operations: copy (with progress bar), move, rename, delete, mkdir
- Multi-file selection
- Incremental search with `n`/`N` navigation
- Sortable file list (name / size / extension, ascending / descending)
- Human-readable file sizes
- rclone remote mount support
- 24-bit truecolor CGA palette
- Mouse support (click, scroll, right-click)

## Key Bindings

### Navigation

| Key | Action |
|-----|--------|
| `Tab` / `Shift+Tab` | Switch active pane |
| `↑` `↓` / `j` `k` | Move cursor |
| `PgUp` / `PgDn` | Page up / down |
| `Home` / `End` | First / last entry |
| `gg` / `G` | Top / bottom |
| `/` | Incremental search |
| `n` / `N` | Next / prev match |
| `Enter` / `→` | Enter directory or open file |
| `←` / `Backspace` | Parent directory |

### Selection

| Key | Action |
|-----|--------|
| `Insert` | Toggle selection + move down |
| `Space` | Toggle selection |
| `Ctrl+A` | Select all |
| `Esc` | Deselect all |

### File Operations

| Key | Action |
|-----|--------|
| `F1` / `?` | This help |
| `F2` / `r` | Rename |
| `F3` | View in pager |
| `F4` / `e` | Edit file |
| `F5` | Copy to other pane |
| `F6` | Move to other pane |
| `F7` | Create directory |
| `F8` / `dd` / `Del` | Delete |
| `F9` | Context menu |
| `F10` / `q` | Quit |

### Misc

| Key | Action |
|-----|--------|
| `S` | Sync panes (copy path to other) |
| `'` | Go to path |
| `s` | Sort by name / size / extension |
| `R` | Refresh |
| `C` | rclone remote mount |

### Mouse

| Action | Effect |
|--------|--------|
| Left click | Switch pane / move cursor |
| Double click | Enter dir or open file |
| Right click | Go to parent directory |
| Scroll | Move cursor ±3 rows |

## Install

```
make install
```

Binary installed to `~/.local/bin/oc`.
