# SQLKraft

A SQL Server management studio in the spirit of SSMS, written in Rust. It is a native
desktop app, a single binary with no .NET or ODBC, and it runs on Linux, Windows and macOS.

- **UI:** [egui/eframe](https://github.com/emilk/egui)
- **Driver:** [tiberius](https://github.com/prisma/tiberius), a pure-Rust TDS implementation

## Features

**Connect dialog**
- Server names in the usual SSMS forms: `host`, `host,port`, `host\INSTANCE` (through SQL Browser), `tcp:host,port`, `.`
- SQL Server authentication.
- Windows authentication using NTLM with `DOMAIN\user` and a password. This also works on Linux.
- Integrated authentication: SSPI on Windows, or Kerberos on Linux and macOS with `--features kerberos`.
- Encryption settings Mandatory, Optional or Off, plus *Trust server certificate*.
- Remembers recent connections. Passwords are never written to disk.

**Object Explorer**
- Each server is a lazily loaded tree: Databases (with System Databases), Tables, Views, Programmability (Stored Procedures, Functions), Columns (type, PK, identity, nullability), Indexes, and Security → Logins.
- Right-click actions: *New Query*, *Select Top 1000 Rows*, *Script as SELECT*, *Script as CREATE* (tables, views, procedures, functions), *Execute Stored Procedure*, *Copy name*, *Refresh*, *Reconnect*, *Disconnect*.
- Double-clicking a table or view runs Select Top 1000 Rows.

**Table designer**
- Right-click a database's *Tables* folder → *New Table…*, or a table → *Design*.
- A grid with one row per column: name, data type with length / precision / scale, allow
  nulls, default, identity (seed and increment) and primary key (one or more columns). Schema
  and table name default to `dbo`.
- **Generate Script** never changes the table itself. It opens the T-SQL in a new query tab on
  that database for you to review and run with F5: `CREATE TABLE` for a new table, or for
  *Design* one `ALTER TABLE` / `sp_rename` statement per change (add, drop, rename and alter
  columns, add / drop / change defaults, change the primary key), ordered so that it works and
  wrapped in a single transaction.
- Changes that need the table to be rebuilt (making a column an identity or not, changing
  its seed or increment, or moving columns) are refused with a message instead of scripted.
  Indexes, foreign keys and check constraints aren't edited here yet.

**Properties window**
- Press **F4** (or View → Properties Window, or right-click → *Properties*) to open a pane showing read-only properties of the item selected in Object Explorer, grouped by category like SSMS.
- Servers: edition, version, collation, authentication mode, memory and parallelism settings, plus the connection's own settings.
- Databases: owner, status, size, recovery model, compatibility level and the usual options.
- Tables, views, procedures and functions: dates, row counts and space used, filegroups, module options, and MS_Description.
- Columns, indexes and logins: type, nullability, defaults, identity, key and included columns, index options, password policy and server roles.

**Query windows**
- Each tab has its own connection (its own SPID), like SSMS.
- T-SQL syntax highlighting and line numbers.
- **F5 / Ctrl+E** executes the selection, or the whole script when nothing is selected.
- `GO` and `GO n` batch separators work. `GO` inside strings or comments is ignored.
- Several result sets per execution, each in its own virtualised grid that handles millions of rows.
- A **Messages** tab shows `PRINT` output, `(n rows affected)`, and errors in SSMS format, with line numbers that match the editor.
- **Cancel** (Alt+Break or Ctrl+Shift+C) stops the query on the server and keeps the connection usable.
- A database drop-down that follows `USE` statements.
- An SSMS-style status bar: state, server and version, login and SPID, database, elapsed time, row count.
- The grid has copy actions (cell, row, column, all with headers) and Ctrl+C. You can export a result set to CSV.
- You can open and save `.sql` files. You're prompted about unsaved changes when closing a tab or quitting.
- Dark and light themes, and an adjustable editor font size (Ctrl+= / Ctrl+-).

**Column value suggestions (experimental, off by default)**
- Turn on with *View → Suggest column values (experimental)*. Then, right of `=`, `<>`, `!=`,
  `LIKE` or `IN (` (and after a comma in the list), with a column of a table in scope on the
  left, the completion list offers that column's most common values containing what you typed:
  `WHERE c.City = 'ma` offers `Madison`, `Marshfield`, …, `Wautoma`. Prefix matches come first,
  then the most frequent, then alphabetical; the count is shown next to each value.
- Accepting inserts the value quoted for the column's type (`N'…'` for nchar / nvarchar,
  `'…'` for char / varchar, `'` doubled; wildcards bracketed inside a `LIKE`). After `=` / `<>`
  on a nullable column with NULLs, *IS NULL* / *IS NOT NULL* is offered too.
- Only character columns of tables (not views), and not `(max)`, `text`, `xml`, binary or
  non-persisted computed columns.
- Because this reads your data, not just the catalog, it is guarded:
  - It runs on its own short-lived connection, never the query tab's, so it can't touch your
    transaction or `SET` options. The query names the database explicitly.
  - It only starts after a ~300 ms pause in typing (or at once on Ctrl+Space), one lookup at
    a time.
  - It reads at most `value_scan_rows` rows (settings file, default 200,000). Beyond that the
    counts come from a sample and are shown as approximate (`~1,204`).
  - `SET LOCK_TIMEOUT 1000` and a 5-second query timeout: it never waits behind writers or
    runs long. A failed lookup is retried on Ctrl+Space.
  - Tables with more than `value_auto_max_table_rows` rows (from `sys.partitions`, default
    1,000,000) only get suggestions on Ctrl+Space.
  - Results are cached per connection, database, table, column and typed text until
    *Refresh IntelliSense Cache* (Ctrl+Shift+R); a search that returned every match also
    answers longer typed text. So values changed since are not seen until then.
  - Nothing is sent anywhere but your own SQL Server.

On macOS, every Ctrl shortcut uses Cmd instead (Cmd+E, Cmd+N, Cmd+S, ...). F5 usually needs the
**fn** key on a Mac keyboard, so Cmd+E is the easier way to execute; there is no Break key, so
cancel with Cmd+Shift+C.

## Installing on Debian / Ubuntu

Packages are available for amd64 and arm64 on Ubuntu 22.04+ and Debian 12+.

### From the APT repository

This is the recommended way, because `apt upgrade` then picks up new releases:

```sh
curl -fsSL https://ammckenzie78.github.io/SQLKraft/sqlkraft.gpg \
  | sudo tee /usr/share/keyrings/sqlkraft.gpg > /dev/null
echo "deb [signed-by=/usr/share/keyrings/sqlkraft.gpg] https://ammckenzie78.github.io/SQLKraft stable main" \
  | sudo tee /etc/apt/sources.list.d/sqlkraft.list
sudo apt update
sudo apt install sqlkraft
```

### From a single .deb

Download a `.deb` from [Releases](https://github.com/ammckenzie78/SQLKraft/releases), then run:

```sh
sudo apt install ./sqlkraft_*.deb
```

## Installing on Windows

Download `sqlkraft-vX.Y.Z-windows-x64.zip` from [Releases](https://github.com/ammckenzie78/SQLKraft/releases),
extract it, and run `sqlkraft.exe`. It is a single portable executable with no installer.
The exe is not code-signed, so SmartScreen may warn the first time you run it.

## Installing on macOS

Download `SQLKraft-macos.zip` from [Releases](https://github.com/ammckenzie78/SQLKraft/releases).
It holds `SQLKraft.app`, a universal app for Apple Silicon and Intel Macs on macOS 11 or later.
Unzip it and drag `SQLKraft.app` into `/Applications`.

The app is not signed with an Apple Developer ID or notarized, so Gatekeeper blocks the first
launch of a downloaded copy. Either:

- Control-click (right-click) `SQLKraft.app`, choose **Open**, then **Open** again. On macOS 15
  (Sequoia) and later, that only shows a warning: open **System Settings → Privacy & Security**,
  scroll down to the message about SQLKraft and click **Open Anyway**; or
- remove the quarantine attribute in Terminal:

  ```sh
  xattr -dr com.apple.quarantine /Applications/SQLKraft.app
  ```

After that it opens normally. To use `sqlkraft --mcp` (see [Claude Code integration](#claude-code-integration)),
use the binary inside the bundle: `/Applications/SQLKraft.app/Contents/MacOS/sqlkraft`.

## Releasing

1. Bump `version` in `Cargo.toml`, then commit.
2. Tag the commit with the same version and push the tag:

   ```sh
   git tag v0.2.0 && git push origin v0.2.0
   ```

The [Release workflow](.github/workflows/release.yml) then:
- builds the `.deb`s and tarballs for amd64 and arm64, a Windows x64 zip, and `SQLKraft-macos.zip`
  (a universal `SQLKraft.app`);
- creates a GitHub Release with those files and their SHA256 checksums;
- rebuilds the signed APT repository on GitHub Pages from the five most recent stable releases.

A tag with a hyphen, such as `v0.2.0-rc1`, becomes a pre-release and is left out of the APT repository.

## Building

You need Rust 1.85 or later (edition 2024). Install it from [rustup.rs](https://rustup.rs).

### Linux

Debian/Ubuntu:

```sh
sudo apt install build-essential pkg-config libxkbcommon-dev libgl1-mesa-dev \
                 libx11-dev libxcursor-dev libxrandr-dev libxi-dev libwayland-dev
cargo build --release
./target/release/sqlkraft
```

Fedora:

```sh
sudo dnf install gcc pkgconf-pkg-config libxkbcommon-devel mesa-libGL-devel \
                 libX11-devel libXcursor-devel libXrandr-devel libXi-devel wayland-devel
```

For Kerberos (integrated) authentication on Linux, also install `libkrb5-dev` and `clang`
(`krb5-devel` and `clang` on Fedora). Then build with:

```sh
cargo build --release --features kerberos
kinit you@REALM.EXAMPLE.COM   # before launching
```

### Windows

```powershell
cargo build --release
```

You need the MSVC C++ build tools with a Windows SDK. If you have several Visual Studio
installs and linking fails with `LNK1104: cannot open file 'msvcrt.lib'`, build from a
*Developer PowerShell* for a complete install (such as VS 2019 Build Tools), or run
`vcvars64.bat` first.

### macOS

Install the Xcode Command Line Tools (`xcode-select --install`) and Rust, then:

```sh
cargo build --release
./target/release/sqlkraft
```

To build `SQLKraft.app` (in `target/macos/`, with the icon and `Info.plist`, plus
`SQLKraft-macos.zip`), run the packaging script. It needs `jq` (`brew install jq`):

```sh
packaging/macos/build-app.sh                       # this Mac's architecture
rustup target add aarch64-apple-darwin x86_64-apple-darwin
packaging/macos/build-app.sh aarch64-apple-darwin x86_64-apple-darwin   # universal
open target/macos/SQLKraft.app
```

macOS ships GSSAPI, so `--features kerberos` needs no extra libraries; run `kinit` before
launching to use integrated authentication.

## Claude Code integration

SQLKraft includes an [MCP](https://modelcontextprotocol.io) server, so Claude Code (or Claude
Desktop, or any MCP client) can explore schemas, read definitions, run queries and hand you
scripts in SQLKraft. You add it to **your own** Claude Code, so your Claude subscription is
only used there; SQLKraft never sees your Claude login.

Set it up once. **Claude → Copy Claude Code setup command** in SQLKraft copies the exact
command for your install, or type it yourself:

```sh
claude mcp add --scope user sqlkraft -- sqlkraft --mcp
```

On Windows, use the full path to `sqlkraft.exe` if it isn't on `PATH`.

How it works:

- **With the SQLKraft window open** (and **Claude → Let Claude Code use this window** on,
  the default), `sqlkraft --mcp` connects to the window over `127.0.0.1`, using a random token
  stored in your config folder. Claude sees the servers in your Object Explorer and your
  query tabs, can read the tab you're working in (`get_active_query`), and can open scripts
  in new tabs for you to review (`open_query_tab`). Queries run on their own connections.
  **Claude → Show activity** lists every tool call, and the menu shows **Claude ●** while
  Claude Code is connected.
- **Without the window**, `sqlkraft --mcp` connects by itself using your recent connections
  that need no stored password (integrated authentication), or all of them if you set
  `SQLKRAFT_MCP_PASSWORD`.

Safety:

- `run_query` is **read-only** by default. Statements that change data, schema, permissions
  or server state are refused, and everything runs in a transaction that is rolled back. To
  let Claude change data on one server, right-click it in Object Explorer and choose **Allow
  Claude to modify data**. That setting lasts until you close SQLKraft.
- Results are capped (200 rows by default, 10,000 at most) and long values are cut.
- Credentials never pass through MCP; tools refer to connections by name.

Tools: `list_connections`, `list_databases`, `list_objects`, `describe_object`,
`get_definition`, `run_query`, `get_active_query`, `open_query_tab`.

## Testing

```sh
cargo test                          # unit tests: batch splitting, tokenizer, scripting, parsing
```

The live tests (`tests/live*.rs`) run against a real SQL Server when `SQLKRAFT_TEST_SERVER`
is set. Without it they print `skipped` and pass, so plain `cargo test` works anywhere:

```sh
SQLKRAFT_TEST_SERVER=localhost,1433 SQLKRAFT_TEST_USER=sqlkraft_test SQLKRAFT_TEST_PASSWORD=... \
    cargo test
```

- Leave `SQLKRAFT_TEST_USER` unset to use integrated authentication.
- Each test creates its own database named `sqlkraft_test_<uuid>`, seeds it with tables, a
  view, procedures and functions (`tests/common/mod.rs`), and drops it afterwards, even when
  the test fails. Tests and parallel runs can share one server.
- Use a dedicated login rather than `sa`. It needs only the `dbcreator` server role:

  ```sql
  CREATE LOGIN sqlkraft_test WITH PASSWORD = N'...', CHECK_EXPIRATION = OFF;
  ALTER SERVER ROLE dbcreator ADD MEMBER sqlkraft_test;
  ```
- The tests connect over TCP, so enable the TCP/IP protocol on the server.

## Notes and limitations

- **TCP only.** SQL Server must have the TCP/IP protocol enabled. Shared memory and named pipes are not supported.
- **Messages are captured from tracing events.** tiberius doesn't expose PRINT output or row counts in its API. SQLKraft reads them from the driver's `tracing` events (`src/capture.rs`). A future tiberius release could change those events, and the Messages tab would then lose them.
- **Encryption on older servers.** SQL Server 2012 and older often can't negotiate TLS 1.2. Use *Encrypt: Optional* for those servers.
- **Row limit.** A result set keeps at most *Max rows per result* rows (View menu, default 1,000,000).
- **Not yet implemented:** execution plans, Activity Monitor, and agent jobs.

## Settings

Settings are stored at:

| Platform | Path |
|---|---|
| Linux | `~/.config/SQLKraft/settings.json` |
| Windows | `%APPDATA%\SQLKraft\config\settings.json` |
| macOS | `~/Library/Application Support/SQLKraft/settings.json` |
