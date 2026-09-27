# MinWin

MinWin is an experimental Windows 11 optimization tool focused on making Windows lighter, faster, and easier to control without breaking compatibility.

The idea is simple:

- reduce unnecessary background processes
- optimize services and startup behavior
- benchmark changes instead of relying on placebo tweaks
- keep every change explainable
- make everything reversible

MinWin is not a custom Windows build. It works on top of a standard Windows installation.

## Goals

- Lower idle resource usage
- Faster startup
- Less background activity
- Preserve gaming, drivers, Windows Update, and security
- Provide profiles such as `minimal`, `gaming`, and `developer`
- Support rollback for every change

## Stack

- Rust
- Win32 APIs
- `windows-rs`
- TOML
- SQLite
- Windows tools such as DISM and `powercfg`

## Planned CLI

```bash
minwin status
minwin benchmark
minwin apply minimal
minwin apply gaming
minwin diff
minwin rollback
```

## Status

Early development / experimental.

Expect things to break.
