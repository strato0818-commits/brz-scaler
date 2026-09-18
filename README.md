# Brickadia BRZ Scaler

A standalone Windows GUI for resizing existing Brickadia `.brz` builds.

It supports uniform or separate X/Y/Z scaling, procedural-brick subdivision at
Brickadia's size limit, dynamic-grid entities, rigid-bearing connections, and
nested frozen joint hierarchies. Non-scalable basic bricks are omitted except
for structural joint hosts required to keep entity grids connected.

## Build

Install the stable Rust toolchain from <https://rustup.rs>, then run:

```powershell
cargo build --release
```

The executable is written to `target\release\brz-scaler-gui.exe`.

On Windows, `build.bat` builds the release and `run.bat` builds and launches it.

## Use

1. Copy a `.brz` file in File Explorer.
2. Open the scaler and click **Paste BRZ**.
3. Choose **Overall** or **Separate X/Y/Z** scaling.
4. Click **Scale BRZ**.
5. The output file is automatically copied to the clipboard.

Scale factors must be whole numbers of `1` or greater. This avoids fractional
rounding overlaps that can cause Brickadia to discard bricks.

## Distribution

Distribute `target\release\brz-scaler-gui.exe`. End users do not need Rust or
the source repository.
