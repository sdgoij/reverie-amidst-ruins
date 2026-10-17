Reverie Amidst Ruins
====================

A simple 2D platformer game to be played directly in the terminal.

[![Watch the gameplay demo](https://reverie.ysap.sh/reverie-demo.jpg)](https://reverie.ysap.sh)

How To Play
-----------

Make sure you have [Rust](https://rust-lang.org) installed to compile the source
code, then clone this repo and run it with:

    git clone https://github.com/bahamas10/reverie-amidst-ruins.git
    cd reverie-amidst-ruins
    cargo run --release

On Linux you may need to install:

    sudo apt-get install libasound2-dev libudev-dev pkg-config

Controls
--------

Movement - Keyboard
- `wasd` / `arrows` -  Move player
- `space bar` - Jump
- `enter` - Confirm
- `r` - Restart level (counts as a death)

Movement - Gamepad
- `d-pad` - Move player
- `south button` (`A` / `Cross`) - Jump / confirm
- `start` - Confirm
- `west button` (`X` / `Square`) - Restart level (counts as a death)

Utility
- `num 1` - Toggle post-processing effects
- `num 2` - Save screenshot to current directory
- `num 3` - Randomize level color palette
- `num 4` - Toggle debug statistics
- `num 8` - Lower master volume by 5%
- `num 9` - Raise master volume by 5%
- `m` - Mute audio
- `-` / `_` - Shrink image by one column
- `+` / `=` - Grow image by one column

Validating Levels
-----------------

A level is hand-drawn ASCII, so nothing stops it from being unbeatable - a jump
one pixel too high, a gap one tile too wide, a coin walled in. `--validate`
checks them with the game's own physics and reports whether the goal and every
coin can be reached. It runs headless: no terminal, audio, or gamepad is used.

    # every built-in level
    cargo run --release -- --validate

    # one level of your own
    cargo run --release -- --validate path/to/level.txt

It prints one line per level and exits non-zero if any of them is not fully
playable, which is what CI runs on every push.

Editing Levels
--------------

`--edit <path>` opens a level in a terminal editor. The level is drawn with the
game's own renderer; the grid, cursor, spawn/goal outlines, reached cells and
unreachable coins are blended into the frame.

    # edit an existing level (save with s)
    cargo run --release -- --edit levels/level1.txt

    # a path that does not exist starts a blank level, written by s
    cargo run --release -- --edit levels/level4.txt

- `1`-`6` - brush: ground, platform, coin, spawn, goal, erase
- arrows / `h` `j` `k` `l` - move the cursor (Shift jumps 8 cells)
- `space` / `enter` - paint; `x` - erase
- `g` - toggle the grid; `v` - re-check now
- `s` - save; `r` - reload; `q` / `Esc` - quit (a second press confirms if dirty)
- `m` / `b` - choose music / background from the game's own tables
- `n` / `z` - new / resize; type the size, `Tab` switches, `Enter` applies
- `:` - command line (`:music colors`, `:resize 72 17`, `:new 40 14`,
  `:name Level 4`, `:goto 20 8`, `:check`, `:save`)
- `?` - key help

Validation runs in the background after a short delay; the HUD says
`checking`, then `playable` or why not. Structural problems (a missing spawn or
goal, duplicate markers, an unknown tile, a bad palette or an unknown
music/background) are reported instantly and their cells outlined in red. New
levels default to 40x14; sizes are clamped to 16..256 by 8..64, and shrinking
refuses to cut off the spawn, the goal or a coin. Use `--release` for editing
too: the check is the same search `--validate` runs.

How Does This Work?
-------------------

The concept is surprisingly simple - the game logic itself is handled in Rust to
update the player, handle collision, coin collecting, etc.  The "frames" are
then rendered onto an RGBA image completely in memory.  Then, the frame is
zlib compressed and then written to the terminal via the [Kitty Graphics
Protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/).

- [args.rs](src/args.rs) - CLI arguments
- [audio.rs](src/audio.rs) - Audio Engine (music and SFX)
- [camera.rs](src/camera.rs) - Player-following camera
- [color.rs](src/color.rs) - Helpers to colorize tiles with custom palettes
- [editor.rs](src/editor.rs) - In-terminal level editor (the `--edit` flag)
- [input/mod.rs](src/input/mod.rs) - Own and combine the available input methods
  - [input/keyboard.rs](src/input/keyboard.rs) - Handle keyboard input with
      `crossterm`
  - [input/gamepad.rs](src/input/gamepad.rs) - Handle gamepad input with `gilrs`
- [level.rs](src/level.rs) - Level processing and parsing (levels in [levels/](levels))
- [main.rs](src/main.rs) - Main entry point and game loop
- [player.rs](src/player.rs) - The player and its properties
- [render.rs](src/render.rs) - Render the game frame to an RGBA image
- [screen.rs](src/screen.rs) - Abstractions over "screens" (intro, level, end, etc.)
- [stats.rs](src/stats.rs) - Maintain stats during the game (deaths, playtime, etc.)
- [terminal/mod.rs](src/terminal/mod.rs) - Abstraction over the terminal to handle
      ANSI
  - [terminal/kitty_graphics.rs](src/terminal/kitty_graphics.rs) - Helpers to
      output images to the terminal
- [validator.rs](src/validator.rs) - Check that a level can be completed (the
      `--validate` flag)

Performance
-----------

On my M1 Macbook Pro using iTerm2:

- without `--release`: ~25 fps
- with `--release`: comfortably runs at 144 fps, can go up to ~500 fps

You can test the fps yourself with:

    cargo run --release -- --fps 30
    cargo run --release -- --fps 60
    cargo run --release -- --fps 144
    cargo run --release -- --fps 144 --debug

Tested Terminals
----------------

- [Ghostty](https://ghostty.org) on macOS: works perfectly
- [Kitty](https://sw.kovidgoyal.net/kitty/) on macOS: works perfectly
- [iTerm2](https://iterm2.com) on macOS: works perfectly
- [WezTerm](https://wezterm.org) on macOS: works but choppy visuals (FPS is
    high but the image rendered appears to stutter)

Screenshots
-----------

![title](screenshots/title.jpg)
![level2](screenshots/level2.jpg)
![level3](screenshots/level3.jpg)

License
-------

MIT License
