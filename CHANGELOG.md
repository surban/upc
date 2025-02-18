# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog],
and this project adheres to [Semantic Versioning].

## 0.7.0 - 2025-02-18
### Changed
- Update webusb-web to 0.4.0

## 0.6.0 - 2025-02-10
### Fixed
- WebUSB: flush buffer on connect

## 0.6.0 - 2025-02-10
### Added
- WebUSB support

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
