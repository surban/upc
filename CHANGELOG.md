# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog],
and this project adheres to [Semantic Versioning].

[Keep a Changelog]: https://keepachangelog.com/en/1.1.0/
[Semantic Versioning]: https://semver.org/spec/v2.0.0.html

## 0.10.4 - 2026-03-16
### Fixed
- host (web): cancel orphaned IN transfers from previous connection on reconnect
- host (web): release USB interface on connection close

## 0.10.3 - 2026-03-16
### Change
- device: flush hardware FIFOs at connection close and device disable

## 0.10.2 - 2026-03-15
### Fixed
- device: flush hardware FIFOs and drain stale data during connection setup
- host: increase flush timeout from 10 ms to 100 ms

## 0.10.1 - 2026-03-15
### Fixed
- device: complete failure after unknown control requests

## 0.10.0 - 2026-03-15
### Added
- device: configurable receive memory pool (`recv_pool_size`, `set_recv_pool_size`)
### Changed
- host (native): use DMA buffers to avoid unnecessary copies on receive path
- device: use receive memory pool to avoid copying
- update webusb-web dependency to 0.5

## 0.9.1 - 2026-03-14
### Fixed
- docs.rs build

## 0.9.0 - 2026-03-14
### Added
- protocol specification document (`PROTOCOL.md`)
- half-close support: either side can independently close its send or receive direction
- `UpcSender::closed()` and device `UpcSender::closed()` notify when the remote side closes its receive direction
- `UpcReceiver::recv()` returns `Ok(None)` on clean half-close EOF
- `connect_with()` for host (native and web) accepting `UpcOptions` for advanced connection settings
- `UpcOptions` builder with `with_topic()`, `with_ping_interval()`, and `with_max_size()` methods
- periodic ping/status mechanism with configurable timeout to detect hung peers
- `UpcFunction::set_ping_timeout()` and `UpcFunction::ping_timeout()` on device side
- `UpcFunction::set_max_size()` and `UpcFunction::max_size()` on device side
- host and device capabilities exchange (TLV-encoded max packet size negotiation)
- `probe()` function for auto-discovery of UPC interfaces
- `upc` CLI tool with `scan`, `list`, `connect`, and `device` subcommands (feature `cli`)
- retry logic for claiming USB interface on connect
### Changed
- **Breaking:** `UpcReceiver::recv()` returns `Result<Option<..>>` instead of `Result<..>`

## 0.8.1 - 2025-07-28
### Fixed
- docs

## 0.8.0 - 2025-07-28
### Changed
- host: switch from rusb to nusb

## 0.7.1 - 2025-02-19
### Fixed
- MSRV

## 0.7.0 - 2025-02-18
### Changed
- Update webusb-web to 0.4.0

## 0.6.0 - 2025-02-10
### Added
- WebUSB support
### Fixed
- WebUSB: flush buffer on connect

## 0.5.0 - 2024-04-29
### Added
- feature trace-packets for debugging
### Modified
- accept shared rusb DeviceHandle
- use usb-gadget version 0.7

## 0.4.0 - 2023-11-11
### Added
- allow usage with existing FunctionFS mount

## 0.3.1 - 2023-11-07
### Fixed
- Stream wrapper is now Sync

## 0.3.0 - 2023-11-07
### Added
- Sink and Stream wrappers
### Changed
- host errors are now std::io::Error

## 0.2.4 - 2023-11-03
### Fixed
- flush connection properly when connecting

## 0.2.3 - 2023-11-03
### Fixed
- ignore endpoint halt failure

## 0.2.2 - 2023-11-02
### Fixed
- host::connect future is now Send

## 0.2.1 - 2023-11-01
### Added
- examples

## 0.2.0 - 2023-11-01
### Added
- request WinUSB driver for Microsoft OS

## 0.1.0 - 2023-10-13
- initial release
