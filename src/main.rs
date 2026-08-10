//! Reads a DS18B20 temperature sensor on GP22 and prints the value
//! over USB serial roughly every two seconds
//!
//! Wiring: red -> 3V3, black -> GND, blue -> GP22, plus a 4.7k
//! resistor between GP22 and 3V3

#![no_std]
#![no_main]

use {
	core::fmt::Write,
	embedded_hal::delay::DelayNs,
	heapless::String,
	// no_std code must supply its own panic handler and panic_halt's is
	// "stop the CPU"; `as _` imports the crate purely for that side
	// effect without giving it a name
	panic_halt as _,
	pico_water_temp::one_wire::{self, Ds18b20},
	rp235x_hal::{self as hal, Timer, gpio::InOutPin, timer::CopyableTimer0, usb::UsbBus},
	usb_device::{
		bus::UsbBusAllocator,
		device::{StringDescriptors, UsbDevice, UsbDeviceBuilder, UsbVidPid},
	},
	usbd_serial::SerialPort,
};

/// Tell the RP2350 boot ROM that this flash image is a valid program
#[unsafe(link_section = ".start_block")]
#[used]
pub static IMAGE_DEF: hal::block::ImageDef = hal::block::ImageDef::secure_exe();

/// The crystal on the Pico 2 W board runs at 12MHz
const CRYSTAL_FREQUENCY_HZ: u32 = 12_000_000;

/// Pause between readings so the cycle lands at roughly two seconds
const PAUSE_BETWEEN_READINGS_MS: u32 = 1250;

/// How many bytes one output line may grow to, see [`UsbSerial`]
const LINE_BUFFER_BYTES: usize = 64;

// hal::entry marks the function the boot code jumps to after reset, and
// the `-> !` "never" return type says this function never returns:
// there is no operating system to hand control back to, so main ends in
// an endless loop instead
#[hal::entry]
fn main() -> ! {
	// take only fails when called twice, which a single main cannot do
	let mut peripherals = hal::pac::Peripherals::take().expect("peripherals already taken");

	let mut watchdog = hal::Watchdog::new(peripherals.WATCHDOG);
	let clocks = hal::clocks::init_clocks_and_plls(
		CRYSTAL_FREQUENCY_HZ,
		peripherals.XOSC,
		peripherals.CLOCKS,
		peripherals.PLL_SYS,
		peripherals.PLL_USB,
		&mut peripherals.RESETS,
		&mut watchdog,
	)
	// without working clocks there is nothing sensible left to do
	.expect("clock setup failed");

	let sio = hal::Sio::new(peripherals.SIO);
	let pins =
		hal::gpio::Pins::new(peripherals.IO_BANK0, peripherals.PADS_BANK0, sio.gpio_bank0, &mut peripherals.RESETS);

	// ticks at 1MHz which gives us the microsecond waits 1-Wire needs.
	// the hal marks Timer0 as copyable, so the sensor and the USB struct
	// below each own their own handle to the same hardware timer and we
	// never have to thread one shared reference through everything
	let timer = Timer::new_timer0(peripherals.TIMER0, &mut peripherals.RESETS, &clocks);

	// InOutPin emulates the open-drain behavior 1-Wire expects, and the
	// internal pull-up backs up the external 4.7k resistor
	let mut sensor = Ds18b20::new(InOutPin::new(pins.gpio22.into_pull_up_input()), timer);

	// the allocator owns the USB hardware, it has to outlive the device
	// and class handles that UsbSerial borrows from it, so it lives here
	let usb_allocator = UsbBusAllocator::new(UsbBus::new(
		peripherals.USB,
		peripherals.USB_DPRAM,
		clocks.usb_clock,
		true,
		&mut peripherals.RESETS,
	));
	let mut usb = UsbSerial::new(&usb_allocator, timer);

	loop {
		// gives the host time to enumerate after boot and paces the loop
		usb.keep_alive_for(PAUSE_BETWEEN_READINGS_MS);

		// and_then runs the read only when starting the conversion worked,
		// otherwise it passes the error straight through. The `|()| {...}`
		// is a closure, an inline function; its odd-looking `()` parameter
		// is the empty success value start_conversion returned
		let measurement = sensor.start_conversion().and_then(|()| {
			// the sensor needs the full conversion time before the result is valid
			usb.keep_alive_for(one_wire::CONVERSION_TIME_MS);
			sensor.read_temperature()
		});

		// format_args! bundles the format string and its arguments up
		// without building any text yet, print_line's buffer is where
		// the actual characters materialize
		match measurement {
			Ok(temperature_celsius) => usb.print_line(format_args!("temperature: {temperature_celsius:.2} C\r\n")),
			Err(sensor_error) => usb.print_line(format_args!("sensor error: {sensor_error}\r\n")),
		}
	}
}

/// Everything USB serial in one place: the device state machine the
/// host talks to, the serial class that carries our bytes, a timer
/// handle for pacing, and the buffer lines are formatted into
///
/// Grouping these in a struct means the main loop hands around one
/// value instead of four, and the line buffer has a natural owner
/// instead of floating around in main. It lives here in main.rs rather
/// than in the library because it is welded to the RP2350's USB
/// hardware and could never run in the host tests.
///
/// The `'a` is a lifetime parameter: `device` and `port` each keep a
/// borrow of the `UsbBusAllocator` in main, and `'a` records that
/// borrow in the type so the compiler can prove the allocator lives
/// longer than this struct
struct UsbSerial<'a> {
	device: UsbDevice<'a, UsbBus>,
	port: SerialPort<'a, UsbBus>,
	timer: Timer<CopyableTimer0>,
	// heapless::String keeps its bytes inline in the struct, since
	// no_std has no allocator to grow into, and the capacity is a const
	// generic: a number that is part of the type itself
	line_buffer: String<LINE_BUFFER_BYTES>,
}

impl<'a> UsbSerial<'a> {
	// Every USB device announces a vendor/product ID pair so the host
	// knows what it is talking to; this pair is set aside for hobbyist
	// serial devices
	const VID_PID: UsbVidPid = UsbVidPid(0x16c0, 0x27dd);

	/// Build the serial class and the USB device announcing it, both
	/// borrowing from the allocator in main (that is what ties the
	/// `&'a` parameter to the `'a` on the returned struct)
	fn new(allocator: &'a UsbBusAllocator<UsbBus>, timer: Timer<CopyableTimer0>) -> Self {
		// the serial class has to exist before the device that announces
		// it to the host is built
		let port = SerialPort::new(allocator);
		let device = UsbDeviceBuilder::new(allocator, Self::VID_PID)
			.strings(&[StringDescriptors::default()
				.manufacturer("workbench")
				.product("pico water thermometer")
				.serial_number("0001")])
			// only fails for more descriptors than the spec allows, ours are static
			.expect("string descriptors are valid")
			.device_class(usbd_serial::USB_CLASS_CDC)
			.build();
		Self {
			device,
			port,
			timer,
			line_buffer: String::new(),
		}
	}

	/// Wait the given time while keeping the USB connection serviced
	///
	/// The host expects the device to answer polls within a few
	/// milliseconds, so one long sleep would get us disconnected
	fn keep_alive_for(&mut self, milliseconds: u32) {
		for _ in 0..milliseconds {
			if self.device.poll(&mut [&mut self.port]) {
				// discard anything the host sent so its buffer never fills
				// up, into a scratch array that only needs to be roughly
				// one USB packet in size
				let mut incoming = [0u8; 64];
				let _ = self.port.read(&mut incoming);
			}
			self.timer.delay_ms(1);
		}
	}

	/// Format one line into the owned buffer and send it to the host
	///
	/// `Arguments` is the bundled-but-not-yet-formatted value that
	/// `format_args!` in the main loop produces, and the `'_` lets the
	/// compiler infer how long its borrowed pieces live
	fn print_line(&mut self, message: core::fmt::Arguments<'_>) {
		self.line_buffer.clear();
		// formatting only fails if the line outgrows the 64 byte buffer,
		// which these short messages never do
		let _ = self.line_buffer.write_fmt(message);
		// nobody listening on the USB side is fine, the line is just dropped
		let _ = self.port.write(self.line_buffer.as_bytes());
	}
}
