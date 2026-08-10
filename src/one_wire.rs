//! Minimal 1-Wire driver for a single DS18B20 on the bus
//!
//! The sensor talks over one open-drain data line. Driving the line low
//! means enabling the output driver, releasing it means switching the
//! driver off and letting the pull-up resistor raise the line again.
//! All timings come from the DS18B20 datasheet, with a microsecond of
//! slack added where the timer may round a wait down
//!
//! Everything lives on the [`Ds18b20`] struct: it owns the data pin and
//! the delay source, and its methods layer the protocol from whole
//! transactions ([`Ds18b20::read_temperature`]) down to single bus
//! operations (`read_bit`). In a larger project the generic 1-Wire bus
//! handling and the DS18B20 specifics would split into two types, at
//! this size one struct keeps the whole story on one page

use embedded_hal::{
	delay::DelayNs,
	digital::{InputPin, OutputPin},
};

/// A full resolution conversion takes the sensor up to 750ms per the
/// datasheet, so wait this long between [`Ds18b20::start_conversion`]
/// and [`Ds18b20::read_temperature`]
///
/// This lives at module level rather than on the struct because an
/// associated constant on a generic type can only be reached by naming
/// the type parameters, which callers here should not have to do
pub const CONVERSION_TIME_MS: u32 = 750;

// derive asks the compiler to write these trait implementations for us:
// Copy so errors pass around by value, PartialEq so tests can assert_eq
// on them, Debug so they print in test failures
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorError {
	/// No sensor answered the reset pulse, usually a wiring problem
	NoDevicePresent,
	/// The reading arrived corrupted, usually a flaky data line
	InvalidCrc,
	/// The GPIO itself failed, which should never happen on the RP2350
	PinFailure,
}

// Display is the human-facing counterpart to Debug, it is what a
// `{sensor_error}` placeholder in a format string calls
impl core::fmt::Display for SensorError {
	fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		match self {
			Self::NoDevicePresent => write!(formatter, "no sensor found, check wiring and pull-up"),
			Self::InvalidCrc => write!(formatter, "reading failed the CRC check"),
			Self::PinFailure => write!(formatter, "GPIO error on the data pin"),
		}
	}
}

/// A DS18B20 temperature sensor on its 1-Wire bus
///
/// The struct owns everything a conversation with the sensor needs: the
/// data pin and something that can wait with microsecond precision.
/// Both are generic so the same driver runs against the real RP2350
/// pin and timer in the firmware, and against a scripted mock in the
/// tests further down this file
pub struct Ds18b20<Bus, Delay> {
	bus: Bus,
	delay: Delay,
}

// the where clause is the contract for the type parameters: these
// methods only exist when Bus can be driven and read (the `+` demands
// both traits at once) and Delay knows how to wait
impl<Bus, Delay> Ds18b20<Bus, Delay>
where
	Bus: OutputPin + InputPin,
	Delay: DelayNs,
{
	// Commands from the DS18B20 datasheet. Skip ROM addresses every
	// device on the bus at once, which is fine because we only have one.
	// Associated constants live on the type itself, so the methods below
	// reach them as Self::COMMAND_...
	const COMMAND_SKIP_ROM: u8 = 0xCC;
	const COMMAND_CONVERT_TEMPERATURE: u8 = 0x44;
	const COMMAND_READ_SCRATCHPAD: u8 = 0xBE;

	/// Bundle the data pin and the delay source into a driver
	///
	/// Nothing touches the bus until the first method call
	pub fn new(bus: Bus, delay: Delay) -> Self {
		Self { bus, delay }
	}

	/// Ask the sensor to start a temperature conversion
	///
	/// The result is only valid after [`CONVERSION_TIME_MS`] have
	/// passed, wait that long before calling [`Self::read_temperature`]
	pub fn start_conversion(&mut self) -> Result<(), SensorError> {
		self.reset()?;
		self.write_byte(Self::COMMAND_SKIP_ROM)?;
		self.write_byte(Self::COMMAND_CONVERT_TEMPERATURE)
	}

	/// Read the finished conversion back as degrees celsius
	pub fn read_temperature(&mut self) -> Result<f32, SensorError> {
		self.reset()?;
		self.write_byte(Self::COMMAND_SKIP_ROM)?;
		self.write_byte(Self::COMMAND_READ_SCRATCHPAD)?;

		let mut scratchpad = [0u8; 9];
		for byte in &mut scratchpad {
			*byte = self.read_byte()?;
		}

		// the ninth scratchpad byte is a CRC over the first eight
		if Self::crc8(&scratchpad[..8]) != scratchpad[8] {
			return Err(SensorError::InvalidCrc);
		}

		Ok(Self::parse_temperature(scratchpad[0], scratchpad[1]))
	}

	/// Turn the two temperature bytes from the scratchpad into celsius
	///
	/// The sensor reports a fixed-point number: a signed count of
	/// sixteenths of a degree. 25.0625 C is 401 sixteenths, sent as the
	/// 16 bit value 0x0191, least significant byte first.
	///
	/// This takes no `&self` (an associated function, Rust's version of
	/// a static method) because turning bytes into degrees is pure
	/// arithmetic, which also lets the tests below hit it directly
	/// without any bus
	fn parse_temperature(least_significant: u8, most_significant: u8) -> f32 {
		// from_le_bytes glues the two bytes back into one 16 bit number.
		// le is little-endian, meaning the least significant byte comes
		// first, and reading it as i16 makes the top bit the sign bit,
		// so values like 0xFFF8 come out as small negative numbers
		// (two's complement) instead of large positive ones
		let raw_sixteenths = i16::from_le_bytes([least_significant, most_significant]);
		// dividing the count of sixteenths by 16 yields whole degrees:
		// 401 / 16 = 25.0625
		f32::from(raw_sixteenths) / 16.0
	}

	/// Reset the bus and check that a sensor answers with a presence pulse
	fn reset(&mut self) -> Result<(), SensorError> {
		self.drive_low()?;
		self.delay.delay_us(500);
		self.release()?;
		// a present sensor pulls the line low 15 to 60us after the release
		// and holds it for at least 60us, so 70us lands inside that window
		self.delay.delay_us(70);
		let device_answered = self.is_low()?;
		// let the rest of the 480us presence window pass before talking
		self.delay.delay_us(410);
		if device_answered {
			Ok(())
		} else {
			Err(SensorError::NoDevicePresent)
		}
	}

	/// Send one byte, least significant bit first as 1-Wire requires
	fn write_byte(&mut self, byte: u8) -> Result<(), SensorError> {
		for bit_index in 0..8 {
			// two bitwise steps pick a single bit out of the byte:
			// `byte >> bit_index` shifts the wanted bit to the rightmost
			// position, `& 1` masks away every bit except that rightmost
			// one, and comparing with 1 turns it into a bool.
			// e.g. byte 0b0100_1100, bit_index 2:
			//   0b0100_1100 >> 2 == 0b0001_0011
			//   0b0001_0011 & 1  == 1            -> the bit was set
			self.write_bit((byte >> bit_index) & 1 == 1)?;
		}
		Ok(())
	}

	/// Receive one byte, least significant bit first as 1-Wire requires
	fn read_byte(&mut self) -> Result<u8, SensorError> {
		let mut byte = 0u8;
		for bit_index in 0..8 {
			if self.read_bit()? {
				// the mirror image of the picking in write_byte: `1 << bit_index`
				// builds a byte with a single one at the wanted position
				// (1 << 3 == 0b0000_1000) and `|=` copies that one into our
				// byte while leaving all the other bits untouched
				byte |= 1 << bit_index;
			}
		}
		Ok(byte)
	}

	/// Send a single bit as a timed low pulse
	fn write_bit(&mut self, bit: bool) -> Result<(), SensorError> {
		// both bits are the same waveform and only the timing differs: a one
		// is a short low pulse so the pull-up has raised the line again by
		// the time the sensor samples it, a zero holds the line low through
		// the whole sampling window
		let (low_microseconds, high_microseconds) = if bit { (6, 64) } else { (62, 5) };
		self.drive_low()?;
		self.delay.delay_us(low_microseconds);
		self.release()?;
		self.delay.delay_us(high_microseconds);
		Ok(())
	}

	/// Open a read slot and sample the sensor's answer
	fn read_bit(&mut self) -> Result<bool, SensorError> {
		// a short low pulse opens the read slot, then the sensor either
		// keeps the line low for a zero or lets it float high for a one
		self.drive_low()?;
		self.delay.delay_us(3);
		self.release()?;
		// the sensor guarantees its answer is valid for 15us measured
		// from the falling edge, so sample at roughly 12us
		self.delay.delay_us(9);
		let bit = self.is_high()?;
		// wait out the remainder of the 60us time slot plus recovery
		self.delay.delay_us(55);
		Ok(bit)
	}

	/// CRC-8 with the Dallas polynomial, as used by all 1-Wire parts
	///
	/// A CRC treats the whole message as one long binary number and
	/// divides it by an agreed-upon constant (the polynomial); the
	/// remainder of that division is the checksum. The sensor computes
	/// it over the bytes it sends, we compute it over the bytes we
	/// received, and any difference means the wire corrupted something.
	///
	/// Like [`Self::parse_temperature`] this is pure arithmetic, so it
	/// is an associated function without `&self`
	fn crc8(data: &[u8]) -> u8 {
		let mut crc = 0u8;
		for byte in data {
			let mut remaining_bits = *byte;
			// process the byte one bit at a time, least significant first,
			// exactly the order the bits arrived on the wire
			for _ in 0..8 {
				// `^` is XOR, which yields 1 where two bits differ. Combined
				// with the shift-and-mask idiom from write_byte this compares
				// the incoming bit with the lowest bit of the running crc
				let mix = (crc ^ remaining_bits) & 1;
				// `>>= 1` shifts the crc one bit to the right, dropping the
				// bit we just consumed and making room at the top
				crc >>= 1;
				if mix == 1 {
					// this is the "subtract the divisor" step of the long
					// division: 0x8C is the Dallas polynomial with its bits
					// reversed, matching our least-significant-first order
					crc ^= 0x8C;
				}
				// move the next incoming bit into the lowest position
				remaining_bits >>= 1;
			}
		}
		crc
	}

	// The pin traits return a Result on every operation, so these wrappers
	// keep the protocol code above free of error plumbing. map_err swaps
	// the pin's own error type for ours, and its `|_|` closure ignores
	// the original error because on the RP2350 the operations cannot
	// actually fail, so it carries nothing useful

	fn drive_low(&mut self) -> Result<(), SensorError> {
		self.bus.set_low().map_err(|_| SensorError::PinFailure)
	}

	fn release(&mut self) -> Result<(), SensorError> {
		self.bus.set_high().map_err(|_| SensorError::PinFailure)
	}

	fn is_low(&mut self) -> Result<bool, SensorError> {
		self.bus.is_low().map_err(|_| SensorError::PinFailure)
	}

	fn is_high(&mut self) -> Result<bool, SensorError> {
		self.bus.is_high().map_err(|_| SensorError::PinFailure)
	}
}

#[cfg(test)]
mod one_wire_tests {
	use super::*;

	// A fake 1-Wire bus that plays the sensor's side of the conversation.
	//
	// The real bus is open-drain: the controller and the sensor can both
	// pull the single line low, and a pull-up raises it when neither does.
	// This mock captures that. When the driver releases the line, the mock
	// answers with the next bit from a canned script, which is exactly what
	// a real sensor drives onto the bus during a read slot
	struct MockBus {
		// the level the sensor presents after each release of the line,
		// consumed one per release across the whole conversation
		responses: heapless::Deque<bool, 128>,
		// whether the controller is currently pulling the line low
		controller_pulling_low: bool,
		// the level the mock presents when the controller has released
		sensor_level_high: bool,
	}

	impl MockBus {
		// build the response script for a full read_temperature call, which
		// mirrors the shape of the real transaction: the driver releases the
		// line once during reset, once per bit while writing the two command
		// bytes, and once per bit while reading the nine scratchpad bytes
		fn for_scratchpad(scratchpad: &[u8; 9]) -> Self {
			let mut responses = heapless::Deque::new();
			// the presence pulse: the sensor pulls the line low after reset
			responses.push_back(false).expect("mock response queue overflow");
			// while the controller writes the 16 command bits the sensor
			// leaves the line alone, so the pull-up keeps it high (the
			// driver never samples these, the level just has to be there)
			for _ in 0..16 {
				responses.push_back(true).expect("mock response queue overflow");
			}
			// the scratchpad data, each byte least significant bit first,
			// using the same shift-and-mask idiom explained in write_byte
			for byte in scratchpad {
				for bit_index in 0..8 {
					// capacity is fixed at 128 and 1 + 16 + 72 stays under it
					responses.push_back((byte >> bit_index) & 1 == 1).expect("mock response queue overflow");
				}
			}
			Self {
				responses,
				controller_pulling_low: false,
				sensor_level_high: true,
			}
		}
	}

	// the pin traits hang their shared error type off this supertrait,
	// and the mock bus can never fail
	impl embedded_hal::digital::ErrorType for MockBus {
		type Error = core::convert::Infallible;
	}

	impl OutputPin for MockBus {
		fn set_low(&mut self) -> Result<(), Self::Error> {
			// the controller is pulling the line down
			self.controller_pulling_low = true;
			Ok(())
		}

		fn set_high(&mut self) -> Result<(), Self::Error> {
			// the controller released, so the sensor decides the level: it
			// serves the next scripted bit, defaulting high once the script
			// is exhausted, which mimics the idle pull-up
			self.controller_pulling_low = false;
			self.sensor_level_high = self.responses.pop_front().unwrap_or(true);
			Ok(())
		}
	}

	impl InputPin for MockBus {
		fn is_high(&mut self) -> Result<bool, Self::Error> {
			// low if either side pulls down, high only when both release
			Ok(!self.controller_pulling_low && self.sensor_level_high)
		}

		fn is_low(&mut self) -> Result<bool, Self::Error> {
			Ok(!self.is_high()?)
		}
	}

	// the driver only ever waits on the clock, so a no-op is enough here.
	// DelayNs gives delay_us and delay_ms default implementations built
	// on delay_ns, so implementing the one required method covers all
	// the calls the driver makes
	struct NoOpDelay;

	impl DelayNs for NoOpDelay {
		fn delay_ns(&mut self, _nanoseconds: u32) {}
	}

	// the pure associated functions (crc8, parse_temperature) need the
	// type named to be called, this alias keeps that short
	type TestSensor = Ds18b20<MockBus, NoOpDelay>;

	// The power-on scratchpad reads +85.0 C and carries a known-good CRC
	// in its ninth byte, which proves our polynomial matches the part
	const POWER_ON_SCRATCHPAD: [u8; 9] = [0x50, 0x05, 0x4B, 0x46, 0x7F, 0xFF, 0x0C, 0x10, 0x1C];

	#[test]
	fn crc8_test_matches_datasheet_example() {
		assert_eq!(TestSensor::crc8(&POWER_ON_SCRATCHPAD[..8]), POWER_ON_SCRATCHPAD[8]);
	}

	#[test]
	fn crc8_test_of_all_zeroes_is_zero() {
		assert_eq!(TestSensor::crc8(&[0, 0, 0, 0]), 0);
	}

	#[test]
	fn crc8_test_detects_a_flipped_bit() {
		let mut corrupted = POWER_ON_SCRATCHPAD;
		corrupted[0] ^= 0b0000_0001;
		assert_ne!(TestSensor::crc8(&corrupted[..8]), corrupted[8]);
	}

	// Rows from the temperature/data relationship table in the datasheet,
	// each the raw little-endian encoding and the celsius it represents
	#[test]
	fn parse_temperature_test_matches_datasheet_table() {
		let datasheet_rows = [
			(0xD0, 0x07, 125.0),   // maximum reading, 0x07D0
			(0x50, 0x05, 85.0),    // power-on value, 0x0550
			(0x91, 0x01, 25.0625), // room temperature, 0x0191
			(0x00, 0x00, 0.0),     // freezing point
			(0xF8, 0xFF, -0.5),    // negative values are two's complement, 0xFFF8
			(0x90, 0xFC, -55.0),   // minimum reading, 0xFC90
		];
		for (least_significant, most_significant, expected_celsius) in datasheet_rows {
			assert_eq!(TestSensor::parse_temperature(least_significant, most_significant), expected_celsius);
		}
	}

	// Higher-level flow: with the mock playing a valid scratchpad, the read
	// path resets, clocks out all nine bytes, passes the CRC, and returns
	// the decoded temperature
	#[test]
	fn read_temperature_test_decodes_a_valid_scratchpad() {
		let mut sensor = Ds18b20::new(MockBus::for_scratchpad(&POWER_ON_SCRATCHPAD), NoOpDelay);
		let temperature = sensor.read_temperature().expect("valid read");
		assert_eq!(temperature, 85.0);
	}

	#[test]
	fn read_temperature_test_rejects_a_bad_crc() {
		let mut broken = POWER_ON_SCRATCHPAD;
		broken[8] ^= 0xFF;
		let mut sensor = Ds18b20::new(MockBus::for_scratchpad(&broken), NoOpDelay);
		assert_eq!(sensor.read_temperature(), Err(SensorError::InvalidCrc));
	}
}
