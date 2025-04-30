Rusty Probe with Embassy
========================

A simple alternative debug probe firmware for the [Rusty Probe](https://github.com/probe-rs/rusty-probe) debugprobe. This firmware is written on top of embassy, and only provides CMSIS-DAP v2.

The firmware uses the [bitbang-dap] crate that acts as an
adapter between [dap-rs] and the hardware. The hardware then only needs to implement a bidirectional
GPIO driver, and a cycle-resolution delay method.

Originally based on https://github.com/embassy-rs/eprobe/

[bitbang-dap]: https://github.com/bugadani/bitbang-dap
[dap-rs]: https://crates.io/crates/dap-rs
