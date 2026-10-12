//! Joins the WiFi network named in wifi_credentials.rs, then reads a
//! DS18B20 temperature sensor on GP22 every two seconds and logs the
//! value over USB serial
//!
//! Wiring: red -> 3V3, black -> GND, blue -> GP22, plus a 4.7k
//! resistor between GP22 and 3V3

#![no_std]
#![no_main]

// git ignores the real credentials file, copy wifi_credentials.example.rs
// to wifi_credentials.rs and fill in your network
mod wifi_credentials;

// one use with braces lists every import, and `as` renames the two
// Runner types that both crates happen to call the same thing
use {
	cyw43::{A4, Aligned, JoinOptions, NetDriver, PowerManagementMode, Runner as RadioRunner, SpiBus},
	cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER},
	embassy_executor::Spawner,
	embassy_net::{Config, Runner as NetworkRunner, StackResources},
	embassy_rp::{
		bind_interrupts,
		clocks::RoscRng,
		dma,
		gpio::{Level, Output, OutputOpenDrain},
		peripherals::{DMA_CH0, PIO0, USB},
		pio::{self, Pio},
		usb,
	},
	embassy_time::{Delay, Duration, Ticker, Timer},
	// no_std code must supply its own panic handler and panic_halt's is
	// "stop the CPU"; `as _` imports the crate purely for that side effect
	panic_halt as _,
	pico_water_temp::one_wire::{self, Ds18b20, SensorError},
	static_cell::StaticCell,
};

// The header the RP2350 boot ROM looks for to accept this flash image as
// a valid program is planted by embassy-rp itself, nothing to declare here

// Embassy drives peripherals from interrupts instead of polling them in a
// loop. This macro wires each hardware interrupt to the handler its
// driver provides: the PIO and DMA ones serve the WiFi bus, the USB one
// the logger
bind_interrupts!(struct Irqs {
	PIO0_IRQ_0 => pio::InterruptHandler<PIO0>;
	DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>;
	USBCTRL_IRQ => usb::InterruptHandler<USB>;
});

// The WiFi chip is a small computer of its own and needs three blobs
// uploaded at every boot. Its firmware and the CLM (the table of radio
// channels and power levels it may legally use) ship in the
// cyw43-firmware crate, the nvram (settings for this exact radio module)
// lives in the firmware folder; that file is named after the Pico W's
// RP2040 but the Pico 2 W carries the same radio module. The Aligned
// wrapper gives the bytes the 4 byte alignment the driver insists on,
// and the `*` copies the crate's bytes into our aligned static
static WIFI_FIRMWARE: &Aligned<A4, [u8]> = &Aligned(*cyw43_firmware::CYW43_43439A0);
static WIFI_NVRAM: &Aligned<A4, [u8]> = &Aligned(*include_bytes!("../firmware/nvram_rp2040.bin"));

/// How often to take a reading, the sensor's conversion time is included
const READING_INTERVAL: Duration = Duration::from_secs(2);

// The `main` macro turns this async function into the firmware's entry
// point: it builds an executor, spawns main as the first task and starts
// running tasks. There is no operating system here, the executor is all
// the scheduling there is: an async fn can pause at every `.await` and
// let another task run meanwhile, that is the whole trick. The two
// arguments name embassy-rp's own executor (its executor-thread feature
// in Cargo.toml) and cortex-m-rt's reset entry point
#[embassy_executor::main(executor = "embassy_rp::executor::Executor", entry = "cortex_m_rt::entry")]
async fn main(spawner: Spawner) {
	// init sets up the clocks (Default::default() asks for the stock
	// 150MHz settings) and hands back every peripheral exactly once, so
	// the type system can prove no two parts of the program fight over one
	let p = embassy_rp::init(Default::default());

	// from here on log::info! lines show up on the computer. Tasks run
	// side by side with main, spawning one is like starting a thread.
	// Calling a task function does not run it, it hands back a token for
	// spawn, or an error if the task already runs, which expect rules out
	spawner.spawn(usb_logger_task(usb::Driver::new(p.USB, Irqs)).expect("spawned once"));

	// --- the WiFi chip ---
	// The CYW43439 radio hangs off fixed pins on the board and speaks a
	// non standard SPI, which a PIO state machine bit-bangs for us while
	// DMA shovels the bytes, so the CPU is free to run our tasks. PIN_23
	// switches the chip's power, PIN_25 is chip select, PIN_24 carries
	// data both ways and PIN_29 the clock
	let power = Output::new(p.PIN_23, Level::Low);
	let chip_select = Output::new(p.PIN_25, Level::High);
	let mut pio = Pio::new(p.PIO0, Irqs);
	let spi = PioSpi::new(
		&mut pio.common,
		pio.sm0,
		// the Pico 2 W's radio module is not reliable at the default bus
		// speed, so it gets this slower clock
		RM2_CLOCK_DIVIDER,
		pio.irq0,
		chip_select,
		p.PIN_24,
		p.PIN_29,
		dma::Channel::new(p.DMA_CH0, Irqs),
	);

	// the driver keeps its state in a static so the background task can
	// borrow it for the whole life of the program; StaticCell is how a
	// static gets initialized at runtime exactly once
	static RADIO_STATE: StaticCell<cyw43::State> = StaticCell::new();
	let (net_device, mut control, radio_runner) =
		cyw43::new(RADIO_STATE.init(cyw43::State::new()), power, spi, WIFI_FIRMWARE, WIFI_NVRAM).await;
	// the runner is the driver's own event loop, it lives in its own task
	spawner.spawn(radio_task(radio_runner).expect("spawned once"));
	// the CLM goes up separately, once the chip has booted its firmware
	control.init(cyw43_firmware::CYW43_43439A0_CLM).await;
	// let the radio doze between router beacons: a lot less power for a
	// little latency on traffic arriving at the Pico
	control.set_power_management(PowerManagementMode::PowerSave).await;

	// --- the network stack ---
	// embassy-net speaks TCP/IP on top of the raw packets the radio moves.
	// DHCP asks the router for an address, the random seed makes port
	// numbers unpredictable, and the resources hold room for four sockets:
	// DHCP and DNS take one each, two are spare for whatever comes next
	static NETWORK_RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();
	let (stack, network_runner) = embassy_net::new(
		net_device,
		Config::dhcpv4(Default::default()),
		NETWORK_RESOURCES.init(StackResources::new()),
		RoscRng.next_u64(),
	);
	spawner.spawn(network_task(network_runner).expect("spawned once"));

	// --- joining the network ---
	// the driver wants the password as raw bytes; these defaults join WPA2
	// or WPA3 networks on the 2.4GHz band, the only band the radio has
	loop {
		match control.join(wifi_credentials::SSID, JoinOptions::new(wifi_credentials::PASSWORD.as_bytes())).await {
			Ok(()) => break,
			Err(error) => {
				// `:?` prints the Debug form of the error, e.g. NetworkNotFound
				log::warn!("joining {} failed ({error:?}), retrying", wifi_credentials::SSID);
				Timer::after_secs(1).await;
			},
		}
	}
	// being on the network is not the same as having an address, DHCP
	// takes a moment longer (and if this line is the last one you ever
	// see, the router is not handing out addresses to the Pico)
	log::info!("joined {}, waiting for an address", wifi_credentials::SSID);
	stack.wait_config_up().await;
	if let Some(config) = stack.config_v4() {
		log::info!("connected as {}", config.address.address());
	}
	// the onboard LED is wired to the WiFi chip rather than the RP2350, so
	// lighting it goes through the radio's control handle
	control.gpio_set(0, true).await;

	// --- the sensor ---
	// an open-drain pin is exactly the "pull low or let go" behavior
	// 1-Wire needs, and the internal pull-up backs up the external resistor
	let mut sensor_bus = OutputOpenDrain::new(p.PIN_22, Level::High);
	sensor_bus.set_pullup(true);
	// Delay is embassy-time's blocking waiter, which gives the driver the
	// microsecond pauses the protocol needs. Blocking stalls every other
	// task, fine for the few milliseconds one bus transaction takes
	let mut sensor = Ds18b20::new(sensor_bus, Delay);

	// a ticker fires on a fixed schedule no matter how long the work took
	let mut ticker = Ticker::every(READING_INTERVAL);
	loop {
		match measure(&mut sensor).await {
			Ok(temperature_celsius) => log::info!("temperature: {temperature_celsius:.2} C"),
			Err(sensor_error) => log::warn!("sensor error: {sensor_error}"),
		}
		// a reading that should travel somewhere would leave here, over `stack`
		ticker.next().await;
	}
}

/// One full measurement: ask for a conversion, let the sensor work, read it back
async fn measure(sensor: &mut Ds18b20<OutputOpenDrain<'static>, Delay>) -> Result<f32, SensorError> {
	// the `?` hands any error straight back to the caller
	sensor.start_conversion()?;
	// an async wait yields to the other tasks, so USB and WiFi keep being
	// serviced while the sensor takes its 750ms; after_millis wants a u64
	// and `.into()` widens the u32 constant for us
	Timer::after_millis(one_wire::CONVERSION_TIME_MS.into()).await;
	sensor.read_temperature()
}

// The three background tasks. Each is an endless loop that the executor
// wakes whenever its hardware has something to do, which is why they
// cost nothing while idle. The attribute turns an async fn into something
// the executor can spawn, `-> !` says it never returns, and `'static`
// means a task may only hold onto things that live as long as the
// program, since nobody knows when it will next run

#[embassy_executor::task]
async fn radio_task(runner: RadioRunner<'static, SpiBus<Output<'static>, PioSpi<'static, PIO0, 0>>>) -> ! {
	runner.run().await
}

#[embassy_executor::task]
async fn network_task(mut runner: NetworkRunner<'static, NetDriver<'static>>) -> ! {
	runner.run().await
}

#[embassy_executor::task]
async fn usb_logger_task(driver: usb::Driver<'static, USB>) {
	embassy_usb_logger::run!(1024, log::LevelFilter::Info, driver);
}
