# One Commander

Usable two-pane terminal file manager. Zero external dependencies. Static musl binary.

![Screenshot](res/screenshot.png)

## Features

- Two-pane layout with independent navigation
- Directory browsing with Nerd Font icons
- File operations: copy, move, rename, delete, mkdir
- Multi-file selection
- Incremental search
- Sortable file list (name / size / extension, ascending / descending)
- Human-readable file sizes
- FTP remote pane support
- rclone remote mount support
- 24-bit truecolor CGA palette
- Mouse support (click, scroll, right-click)
- SGR extended mouse mode

## Keys

| Key | Action |
|-----|--------|
| `Tab` / `Shift+Tab` | Switch active pane |
| `↑` `↓` / `j` `k` | Move cursor |
| `PgUp` `PgDn` | Page up / down |
| `Home` `End` / `gg` `G` | First / last entry |
| `Enter` `→` | Enter directory |
| `←` `Backspace` | Parent directory |
| `Insert` | Toggle selection + move down |
| `Space` | Toggle selection |
| `Ctrl+A` | Select all |
| `Esc` | Clear selection |
| `/` | Incremental search |
| `'` | Go to path |
| `s` | Sort dialog |
| `S` | Sync panes (copy active path to other pane) |
| `R` | Refresh |
| `e` / `F4` | Edit file |
| `r` / `F2` | Rename |
| `dd` / `Delete` / `F8` | Delete |
| `F5` | Copy |
| `F6` | Move |
| `F7` | Mkdir |
| `F3` | View |
| `F9` | Context menu |
| `?` / `F1` | Help |
| `q` / `F10` | Quit |
| `C` | rclone mount |
| `f` | FTP connect |
| Left-click | Switch pane / move cursor |
| Double-click | Enter directory |
| Right-click | Parent directory |
| Scroll | Move cursor ±3 rows |

## Install

```
make install
```

Binary installed to `~/.local/bin/oc`.
