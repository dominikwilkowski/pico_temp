# Pico water thermometer

Reads a DFRobot SEN0511 (waterproof DS18B20) on a Raspberry Pi Pico 2 W
and prints the temperature over USB serial every ~2 seconds.

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

## Build

One-time setup:

```sh
rustup target add thumbv8m.main-none-eabihf
# macOS: brew install picotool
# Linux: sudo apt install picotool  (or build from source if your distro's version is old)
```

Then:

```sh
cargo build --release
```

## Test

The 1-Wire logic lives in the library half of the crate (`src/lib.rs`
pulls in `src/one_wire.rs`) precisely so it can be tested without any
hardware — the tests drive the driver against a mock bus that plays the
sensor's side of the conversation:

```sh
cargo test-host
```

`test-host` is an alias defined in `.cargo/config.toml`. Plain
`cargo test` cannot work here: that file points every build at the
bare-metal Pico target, which has no test harness (no operating system,
no process to run tests in). The alias points the tests back at this
computer, and its `--lib` flag skips the firmware binary, which only
builds for the Pico.

## Flash

1. Hold the BOOTSEL button on the Pico while plugging in the USB cable
2. Run `cargo run --release` (picotool uploads the binary and reboots the board)

Alternatively convert to a UF2 and drag it onto the `RP2350` drive that
appears in BOOTSEL mode:

```sh
picotool uf2 convert target/thumbv8m.main-none-eabihf/release/pico-water-temp firmware.uf2
```

## Watch the readings

The Pico shows up as a USB serial port after flashing.

- macOS: `screen /dev/tty.usbmodem* 115200`
- Linux: `screen /dev/ttyACM0 115200` (or `minicom`, `tio`)
- Windows: PuTTY on the new COM port

You should see a line like `temperature: 23.44 C` every two seconds.
Squeeze the probe tip in your hand to watch it climb. If you see
`no sensor found` instead, recheck the three wires and the pull-up.

## Reference: the community-crate version

`src/reference_community_crates.rs` shows what the driver would look like using the `one-wire-bus` and `ds18b20` crates instead of the hand-written protocol. It is compiled as part of the library, so the compiler keeps it honest and it cannot silently rot, but the firmware never calls it — it exists only for comparison.
