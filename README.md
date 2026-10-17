# puppet

**Give your coding agent eyes and hands for your Windows desktop.**

## Easy install

```text
git clone https://github.com/nathwn12/puppet
cargo build --release --locked
```

There are no releases yet, so building from source is the only route.
The client setup below takes about a minute.

## Let your AI install it

```text
Clone https://github.com/nathwn12/puppet, build it with
cargo build --release --locked, and add the built puppet.exe
to my MCP client config. Then prove it works by running
puppet --version and making one read-only list_windows call.
```

## Install

1. Install the stable Rust toolchain from https://rustup.rs.
2. Clone the repo and build it:

```text
git clone https://github.com/nathwn12/puppet
cargo build --release --locked
```

3. The build produces a single `target\release\puppet.exe`.
   Add it to your MCP client:

```json
{
  "mcpServers": {
    "puppet": {
      "command": "C:\\tools\\puppet\\puppet.exe",
      "args": []
    }
  }
}
```

Replace the command path with the folder holding your built `puppet.exe`,
then restart the client. The client spawns the server over stdio,
so you never launch the binary by hand.

## Safety: what it touches

puppet drives your real Windows desktop. The seeing tools change nothing:
`screenshot` captures without raising or focusing anything, `list_windows`
and `cursor` only read state, and `elements` lists a window's controls with
no screenshot, no focus change, and no input. The doing tools act for real:
`do` moves the mouse, types, and sends keys, and `window` can focus, move,
resize, minimize, maximize, restore, or close a window. Nothing asks you
first, so preview a risky burst with `"dry_run": true`, which assembles
the input without sending it.

## Use it

1. See what is open. Read-only, changes nothing. Call `list_windows` with:

```json
{}
```

You get back each visible window with `hwnd`, `pid`, `exe`, `title`,
and its screen rect.

2. See the screen. Call `screenshot` with:

```json
{"max_px": 1280}
```

You get back an image frame plus a coordinate map that turns image pixels
back into screen coordinates.

3. Send input, previewed first. Call `do` with:

```json
{"actions": [{"type": "type", "text": "hello"}], "dry_run": true}
```

You get back per-action results with `index`, `type`, `ok`, and `ms`.
Remove `dry_run` to send the input for real.

## What you get

- See the desktop from any MCP client: screenshots, window lists,
  cursor position, and UI element trees.
- Drive the desktop when you mean to: mouse, keyboard, and window
  control batched into one call.
- Preview before you act: dry-run mode reports what a burst would do
  without touching anything.
- Run one file: a single `puppet.exe` with no interpreter or venv
  to manage.
- Trust the coordinates: every screenshot carries the math that maps
  image pixels back to screen pixels.

## Requirements

- Windows 10 or 11, 64-bit. puppet calls the Win32 desktop APIs
  directly, so there is no macOS or Linux build.
- A stable Rust toolchain, needed only to build the binary.
- An MCP client that can spawn a local stdio server.

## Links

- Repo: https://github.com/nathwn12/puppet
- Releases: https://github.com/nathwn12/puppet/releases
- Issues: https://github.com/nathwn12/puppet/issues
- License: MIT, see [LICENSE](LICENSE).
