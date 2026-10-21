# readme

A terminal file viewer and basic editor: a directory tree on the left, the file
on the right, and the mouse works everywhere. It reads source code, Markdown,
CSV, images, archives and git history, and it was made for a
[herdr](https://herdr.dev) pane next to coding agents, picking up their edits
as they land.

![Markdown rendered, next to the directory tree](docs/screenshots/markdown.png)

## Features

- **Tree**: directories first, folding as you click them. Dotfiles and anything
  `.gitignore` leaves out (`target/`, `node_modules/`) stay hidden until you
  press `.`. Press `/` to filter the whole tree by name as you type. It
  refreshes itself every couple of seconds.
- **Source**: syntax highlighting for the languages [bat](https://github.com/sharkdp/bat)
  knows, in the terminal's own colors, with line numbers, search, and word wrap
  (`w`).
- **Markdown** shows rendered: headings, lists, task lists, tables, quotes,
  links, and code blocks with highlighting. `m` switches to the raw text and
  back, keeping your place.
- **CSV and TSV** show as a table, with aligned columns, numbers lined up on the
  right, and the header row kept on screen as you scroll.
- **Images** (PNG, JPEG, GIF, WebP, BMP, TIFF and more) show in the terminal:
  with real pixels where the terminal has a graphics protocol (Kitty, Sixel,
  iTerm2), and with colored half-blocks anywhere else.
- **Archives**: zip files and tar files (plain, `.gz`, `.bz2`, `.xz`, `.zst`)
  open in the tree like directories, and the files inside them open like any
  other, without unpacking anything.
- **Git history**: in a repository, `H` lists the commits. Open one to see the
  files it changed, each as it was at that commit or, with `d`, as a diff.
- **Editing**: `e` starts editing and `Esc` stops. Selection, undo and redo,
  clipboard, auto-indent, and a prompt before unsaved changes would be lost.
- **Live**: when something else changes the open file, it reloads. If you have
  unsaved changes, it marks the file as changed on disk instead.
- **Clipboard**: selecting text with the mouse copies it, to the clipboard and
  to the primary selection. Uses `wl-copy` on Wayland, `xclip` or `xsel` on X11,
  and OSC 52 otherwise, which also works over ssh.

| | |
|---|---|
| ![Source with highlighting and a search](docs/screenshots/code.png) | ![Editing, with a mouse selection](docs/screenshots/edit.png) |
| Source, with a search | Editing, with a mouse selection |
| ![A CSV file as a table](docs/screenshots/table.png) | ![An image in half-blocks](docs/screenshots/image.png) |
| CSV as a table | An image, in half-blocks |
| ![Git history, with a commit's diff](docs/screenshots/history.png) | ![The tree filtered by name](docs/screenshots/filter.png) |
| A commit's diff | The tree filtered by name |
| ![A file inside a tar.gz](docs/screenshots/archive.png) | |
| Inside a `.tar.gz` | |

## Install

You need Rust 1.90 or newer and a C compiler (for the xz and zstd decoders), plus
`git` for the history view.

```
cargo install --git https://github.com/bhoult/readme
```

Or clone it and run `./install.sh`, which builds a release binary and puts it
in `~/.local/bin/readme`.

## Use

```
readme              # browse the current directory (opens its README if it has one)
readme ~/src/project
readme src/main.rs  # open a file, with its directory in the tree
```

### Keys

| Where | Keys |
|---|---|
| Anywhere | `Tab` switch panes · `Ctrl+S` save · `Ctrl+Q` quit |
| Tree | `↑↓` `j` `k` move · `Enter` open · `←→` `h` `l` fold · `/` filter · `.` hidden files · `r` refresh · `H` git history · `q` quit |
| Filter | type to filter · `↑↓` choose · `Enter` open · `Esc` clear |
| File | `↑↓` `PgUp` `PgDn` `Home` `End` scroll · `←→` sideways · `/` search, `n` `N` next/previous · `w` word wrap · `e` edit · `m` raw/formatted · `y` copy selection · `q` quit |
| Editing | `Esc` stop · `Ctrl+Z` `Ctrl+Y` undo/redo · `Ctrl+C` `Ctrl+X` `Ctrl+V` copy/cut/paste · `Ctrl+A` select all · `Ctrl+F` search · `Alt+Z` word wrap · `Shift`+arrows select · `Ctrl`+arrows by word |
| History | `↑↓` commits · `Enter` a commit's files · `d` diff or file · `a` all files at that commit · `←` `Esc` back · `H` leave |

`Tab` always switches panes, even while editing, so there is no key that
inserts a tab character. `Enter` keeps the line's indentation. With word wrap
on, the arrow keys move by screen row.

### Mouse

- Click a directory or an archive to fold or unfold it; click a file to open
  it; double-click a file to move into it.
- The wheel scrolls whichever pane it's over; `Shift`+wheel scrolls sideways.
- Drag across text to select and copy it. While editing, a click moves the
  cursor and a double-click selects a word; while viewing, a double-click
  starts editing there. Middle-click pastes the primary selection while editing.
- In the history, click a commit or file to show it, double-click to open it,
  and click the title to go back.
- Drag the divider between the panes to resize the tree.

### Images and terminal multiplexers

Outside a multiplexer, readme asks the terminal what it can draw and uses real
graphics if it can (Kitty, Ghostty, WezTerm, Konsole, iTerm2, and Sixel
terminals). Inside herdr, tmux, zellij or screen it draws half-blocks, because
those answer the question themselves even when the terminal around them can't
show the result. herdr, for one, passes Kitty graphics through only to an
outer terminal that supports them. If yours does, set `README_IMAGES=kitty`
(or `sixel`, `iterm2`, `auto`, `halfblocks`).

### Colors

Highlighting uses bat's `ansi` theme by default, which draws with the
terminal's own palette, so it suits light and dark themes alike. Set
`README_THEME` to use another of bat's themes, for example `Nord`,
`Monokai Extended` or `GitHub`.

## Building

```
cargo build --release
cargo test
```

The tests include the whole app driven through ratatui's test backend, mouse
clicks and drags included.

| File | |
|---|---|
| `src/main.rs` | terminal setup and the event loop |
| `src/app.rs` | state, keys, mouse, drawing |
| `src/tree.rs` | the directory tree and its filter |
| `src/buffer.rs` | the text being edited: cursor, selection, undo |
| `src/highlight.rs` | syntect highlighting, cached and computed only as far as shown |
| `src/markdown.rs` | the Markdown renderer |
| `src/table.rs` | CSV and TSV as tables |
| `src/picture.rs` | images |
| `src/archive.rs` | reading zip and tar archives |
| `src/git.rs` | history, through the `git` command |
| `src/text.rs` | column math, wrapping, search, styled line slices |
| `src/clipboard.rs` | copy and paste |

Built with [ratatui](https://ratatui.rs) and
[ratatui-image](https://github.com/ratatui/ratatui-image),
[syntect](https://github.com/trishume/syntect) with
[two-face](https://github.com/CosmicHorrorDev/two-face) for bat's syntaxes,
[pulldown-cmark](https://github.com/pulldown-cmark/pulldown-cmark),
[ignore](https://github.com/BurntSushi/ripgrep/tree/master/crates/ignore),
[csv](https://github.com/BurntSushi/rust-csv), [zip](https://github.com/zip-rs/zip2)
and [tar](https://github.com/alexcrichton/tar-rs).

## License

MIT
