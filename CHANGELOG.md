# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-11-07

First public release. `vitals.exe` is a single unsigned x64 binary: download
it, run it, and pick **Start with Windows** from the tray menu if you want it
always up.

### Added

- A topmost, click-through hardware panel on the monitor of your choice,
  toggled with `Win+Alt+O`. It shows CPU load, clock and package temperature,
  NVIDIA GPU load, temperature, VRAM, clock, power and fan, memory load, and
  storage activity with the temperature of every NVMe drive.
- CPU package and mainboard temperatures read from HWiNFO shared memory or
  the LibreHardwareMonitor web server, so Vitals itself ships no kernel driver.
- Optional per-thread P-core/E-core bars and network throughput rows.
- `%APPDATA%\Vitals\config.toml`, reloadable from the tray without a restart.
- `%APPDATA%\Vitals\vitals.log`, capped at 64 KB, which records every GPU
  helper restart and why it happened, and why a `config.toml` was rejected.

### Fixed

- NVIDIA driver updates no longer take the panel down. Before this release
  the overlay called NVML in-process, and every driver update crashed it with
  an access violation inside `nvml.dll` a few seconds after the install
  finished: the old library stayed loaded while the driver underneath it was
  replaced. NVML now runs in a child process (`vitals.exe --gpu-helper`) that
  the panel supervises and restarts, and a display-adapter change restarts it
  on purpose. The GPU section reads `nvidia restarting` for a moment instead of
  the whole panel disappearing.

[Unreleased]: https://github.com/BBareth/vitals/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/BBareth/vitals/releases/tag/v0.1.0
