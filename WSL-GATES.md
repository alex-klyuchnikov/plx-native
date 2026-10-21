# Running the compile gates on this machine (WSL)

The Windows-side Rust toolkit cannot compile this workspace: `plx_base` (and friends)
use POSIX-only APIs (`std::os::unix`, `libc::dlopen`, …) with no Windows gate, so no
MSVC/GNU Windows build gets past it. CI runs the host gates on **ubuntu-latest**, so a
WSL Ubuntu distro is the right environment. This file is the human-facing setup; the
agent continues writing code blind until this works.

## 1. Install

```powershell
wsl --install -d Ubuntu-24.04
```

## 2. Clone INSIDE the Linux filesystem (not /mnt/c — symlinks + flock + speed)

```bash
cd ~
git clone https://github.com/<you>/plx-native plx-native
cd plx-native
git fetch origin && git checkout browse-by-folders
```

If the branch is not pushed yet, transfer it from the Windows checkout:

```powershell
# in C:\Hobbies\plx-native
git bundle create C:\Hobbies\plx-native\branch.bundle main..browse-by-folders
```
then `git fetch branch.bundle browse-by-folders:browse-by-folders` in WSL
(copy the .bundle into ~ first).

## 3. Toolchain + system packages

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
sudo apt-get update
sudo apt-get install -y build-essential python3 pkg-config libsdl2-dev libsdl2-ttf-dev libgl1-mesa-dev
```

The pinned Rust (1.99.1 + clippy + rustfmt) auto-installs from
`rust-modules/rust-toolchain.toml` on the first `make`.

## 4. Run the gates (from the repo root)

```bash
make check          # host unit tests + lint gates (long cold build; run it and walk away)
make check-nodef    # the shipping feature set (--no-default-features)
make sim-macos      # simulator — on WSLg use the ui-sim skill's Linux path, not this name
```

Paste back to the agent, verbatim, the first failing stage's output — especially the
`error[E...]` lines and the final `test result:` line. Do not summarize them.

## 5. What is NOT covered

- `make` (ARM cross-build) needs the webOS NDK — CI installs it on ubuntu; local setup
  follows the `setup-environment` skill (written for macOS; the NDK tarball itself is
  Linux, so it may work — unverified here).
- The TV itself (SSH from WSL may work; TV lock rules still apply).
- `sim-macos` is macOS-only.

## 6. State of the branch when this was written

`browse-by-folders` carries the Browse-by-Folder feature: wire ops + mock PMS (tested),
the folders data model/store, view-state fan-out, app bridge + screens registry wiring,
the `FoldersScreen`, folder tile/icon, i18n keys in all 8 locales, and the advertisement
probe. NONE of the Rust half has ever been compiled. Expect many `error[E...]` fixes on
first `make check`; that is exactly what this file is for.
