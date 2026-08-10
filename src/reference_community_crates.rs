//! REFERENCE ONLY, never called by the firmware
//!
//! This module shows what the driver would look like if we leaned on
//! the community crates instead of hand-writing the 1-Wire protocol in
//! one_wire.rs. It is compiled together with the library so the
//! compiler keeps it honest and it cannot silently rot, but nothing
//! ever calls it
//!
//! One caveat worth knowing: both crates are built against embedded-hal
//! 0.2, while the rest of our project uses embedded-hal 1.0. The RP2350
//! hal still implements the older 0.2 pin traits as well, so this does
//! work, but Cargo.toml carries two versions of embedded-hal (see the
//! embedded-hal-0-2 rename there) and the pin handed to these crates
//! has to implement the 0.2 flavor of the traits
//!
//! The trade compared to our hand-written driver: far less code to own,
//! but the protocol and the timings disappear inside the crates, and
//! you pull in two dependencies plus the older trait version. This
//! module also stays a plain function on purpose, matching the style of
//! the crates' own documentation, where our own driver wraps everything
//! in the [`crate::one_wire::Ds18b20`] struct

use {
	ds18b20::{Ds18b20, Resolution},
	one_wire_bus::OneWire,
};

/// Read the single DS18B20 on the bus and return degrees celsius
///
/// `bus_pin` is an open-drain capable pin implementing the embedded-hal
/// 0.2 InputPin and OutputPin traits, `delay` anything that can wait in
/// both micro- and milliseconds. The crates also demand the pin's error
/// type be printable, hence the `Debug` bound
pub fn read_celsius<Pin, PinError, Delay>(
	bus_pin: Pin,
	delay: &mut Delay,
) -> Result<f32, one_wire_bus::OneWireError<PinError>>
where
	Pin: embedded_hal_0_2::digital::v2::InputPin<Error = PinError>
		+ embedded_hal_0_2::digital::v2::OutputPin<Error = PinError>,
	Delay: embedded_hal_0_2::blocking::delay::DelayUs<u16> + embedded_hal_0_2::blocking::delay::DelayMs<u16>,
	PinError: core::fmt::Debug,
{
	let mut one_wire_bus = OneWire::new(bus_pin)?;

	// kick off a conversion on every device on the bus, then wait the
	// resolution's worth of conversion time before reading back
	ds18b20::start_simultaneous_temp_measurement(&mut one_wire_bus, delay)?;
	Resolution::Bits12.delay_for_measurement_time(delay);

	// find the first device on the bus and read its scratchpad, the crate
	// handles the ROM command, the scratchpad read, and the CRC check
	if let Some((device_address, _)) = one_wire_bus.device_search(None, false, delay)? {
		// the `::<PinError>` "turbofish" spells out a type parameter the
		// compiler cannot infer from the arguments alone
		let sensor = Ds18b20::new::<PinError>(device_address)?;
		let scratchpad = sensor.read_data(&mut one_wire_bus, delay)?;
		return Ok(scratchpad.temperature);
	}

	// note the silent not-a-number for "no device found", where the
	// hand-written driver returns a proper NoDevicePresent error instead
	Ok(f32::NAN)
}
