# Pico water thermometer

Joins your WiFi and reads a DFRobot SEN0511 (waterproof DS18B20) on a
Raspberry Pi Pico 2 W, logging the temperature over USB serial every
two seconds. Written with [Embassy](https://embassy.dev), the async
framework for embedded Rust.

## Parts

- Raspberry Pi Pico 2 W (with headers soldered on, so it sits in a breadboard)
- SEN0511 waterproof DS18B20 probe
- 4.7kΩ resistor (pull-up for the data line)
- Breadboard and jumper wires

The probe has bare stranded wire ends. Twist and tin them (or use a
screw-terminal adapter like DFRobot's DFR0055) so they sit reliably in
the breadboard.

## Wiring

The SEN0511 wire colors per DFRobot: red = VCC, black = GND, blue = DATA.

| Probe wire   | Pico pin              |
| ------------ | --------------------- |
| red          | 3V3 (physical pin 36) |
| black        | GND (physical pin 28) |
| blue         | GP22 (physical pin 29) |

Then place the 4.7kΩ resistor between the blue/GP22 row and the 3V3 row.
The firmware also switches on the chip's internal pull-up, so a single
sensor may work without the resistor, but the external one is the
reliable setup, especially with the 1m cable.

## Setup

One-time tooling:

```sh
rustup target add thumbv8m.main-none-eabihf
# macOS: brew install picotool
# Linux: sudo apt install picotool  (or build from source if your distro's version is old)
```

Your WiFi network, which git never sees:

```sh
cp src/wifi_credentials.example.rs src/wifi_credentials.rs
# then put your network name and password in src/wifi_credentials.rs
```

## Build

```sh
cargo build --release
```

## Test

The 1-Wire driver lives in the library half of the crate (`src/lib.rs`
pulls in `src/one_wire.rs`) precisely so it can be tested without any
hardware, against a mock bus that plays the sensor's side of the
conversation:

```sh
cargo test-host
```

`test-host` is an alias defined in `.cargo/config.toml`. Plain
`cargo test` cannot work here: that file points every build at the
bare-metal Pico target, which has no test harness. The alias points the
tests back at this computer, and its `--lib` flag skips the firmware
binary, which only builds for the Pico.

## Flash

1. Hold the BOOTSEL button on the Pico while plugging in the USB cable
2. Run `cargo run --release` (picotool uploads the binary and reboots the board)

Alternatively convert to a UF2 and drag it onto the `RP2350` drive that
appears in BOOTSEL mode:

```sh
picotool uf2 convert target/thumbv8m.main-none-eabihf/release/pico-water-temp target/pico-water-temp.uf2
```

Keep built images to yourself: they contain your WiFi password in plain
text (which is also why `*.uf2` is gitignored).

## Watch the readings

The Pico shows up as a USB serial port after flashing.

- macOS: `screen /dev/tty.usbmodem* 115200`
- Linux: `screen /dev/ttyACM0 115200` (or `minicom`, `tio`)
- Windows: PuTTY on the new COM port

The firmware joins the network before it touches the sensor, so the
first lines are `joined <network>, waiting for an address` and then
`connected as 192.168.x.x` (the onboard LED turns on at that moment),
followed by a line like `temperature: 23.44 C` every two seconds.
Squeeze the probe tip in your hand to watch it climb. While no terminal
is attached the board keeps only about 1 KB of output, so opening it late
shows a short burst of old lines, not the whole history.

## If it doesn't work

- `joining <network> failed (NetworkNotFound)`: a typo in the network
  name, or the network is 5 GHz only — the Pico's radio is 2.4 GHz only.
- `joining <network> failed (AuthenticationFailure)`: wrong password, or
  the network is not WPA2/WPA3 personal (an open network needs a code
  change, see the `JoinOptions` comment in `src/main.rs`).
- `joined ..., waiting for an address` and then nothing: the router is
  not handing out DHCP leases to the Pico (MAC filtering, a guest network
  that isolates devices, DHCP switched off).
- `sensor error: no sensor found`: recheck the three wires and the pull-up.
- Completely silent, LED off, serial port missing: the firmware panicked
  and halted. Re-enter BOOTSEL and reflash; a network name longer than 32
  characters or a password longer than 63 is the usual cause.

## What's where

- `src/main.rs` — the firmware: boots Embassy, joins WiFi, reads the sensor in a loop
- `src/one_wire.rs` — the hand-written DS18B20 driver with its tests
- `src/lib.rs` — the two-line library wrapper that makes the driver testable on this computer
- `memory.x` + `build.rs` — tell the linker where the RP2350's flash and RAM are
- `firmware/nvram_rp2040.bin` — the WiFi chip's board settings blob (742 bytes). The chip's main firmware ships in the `cyw43-firmware` crate, this one piece does not, so it is copied from [Embassy's cyw43-firmware folder](https://github.com/embassy-rs/embassy/tree/main/cyw43-firmware) under the Infineon Permissive Binary License 1.0 found there
- `.cargo/config.toml` — build target, flashing command, and the `test-host` alias
