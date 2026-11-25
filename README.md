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
- Integrated authentication: SSPI on Windows, or Kerberos on Linux with `--features kerberos`.
- Encryption settings Mandatory, Optional or Off, plus *Trust server certificate*.
- Remembers recent connections. Passwords are never written to disk.

**Object Explorer**
- Each server is a lazily loaded tree: Databases (with System Databases), Tables, Views, Programmability (Stored Procedures, Functions), Columns (type, PK, identity, nullability), Indexes, and Security → Logins.
- Right-click actions: *New Query*, *Select Top 1000 Rows*, *Script as SELECT*, *Script as CREATE* (tables, views, procedures, functions), *Execute Stored Procedure*, *Copy name*, *Refresh*, *Reconnect*, *Disconnect*.
- Double-clicking a table or view runs Select Top 1000 Rows.

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

## Releasing

1. Bump `version` in `Cargo.toml`, then commit.
2. Tag the commit with the same version and push the tag:

   ```sh
   git tag v0.2.0 && git push origin v0.2.0
   ```

The [Release workflow](.github/workflows/release.yml) then:
- builds the `.deb`s and tarballs for amd64 and arm64, and a Windows x64 zip;
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

## Testing

```sh
cargo test                          # unit tests: batch splitting, tokenizer, scripting, parsing
```

The live tests run against a real server. Every statement in them is read-only and uses
temp tables only:

```sh
SQLKRAFT_TEST_SERVER=localhost,1433 SQLKRAFT_TEST_USER=sa SQLKRAFT_TEST_PASSWORD=... \
    cargo test --test live -- --ignored --nocapture
```

Leave `SQLKRAFT_TEST_USER` unset to use integrated authentication.

## Notes and limitations

- **TCP only.** SQL Server must have the TCP/IP protocol enabled. Shared memory and named pipes are not supported.
- **Messages are captured from tracing events.** tiberius doesn't expose PRINT output or row counts in its API. SQLKraft reads them from the driver's `tracing` events (`src/capture.rs`). A future tiberius release could change those events, and the Messages tab would then lose them.
- **Encryption on older servers.** SQL Server 2012 and older often can't negotiate TLS 1.2. Use *Encrypt: Optional* for those servers.
- **Row limit.** A result set keeps at most *Max rows per result* rows (View menu, default 1,000,000).
- **Not yet implemented:** execution plans, IntelliSense, Activity Monitor, the table designer, and agent jobs.

## Settings

Settings are stored at:

| Platform | Path |
|---|---|
| Linux | `~/.config/SQLKraft/settings.json` |
| Windows | `%APPDATA%\SQLKraft\config\settings.json` |
| macOS | `~/Library/Application Support/SQLKraft/settings.json` |
