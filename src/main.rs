//! `turnip` is an IP or Ethernet tunnel using a half-duplex serial port without any control flow signals as the transport channel.
//!
//! To allow IP over a half-duplex serial line, each side of the serial link takes turns to transmit packets to the other side.
//! Hence the name "turn IP", or "turnip".
//! Additionally, the half-duplex serial port was invented around the same time as the turnip, and they are about equally technologically advanced.
//!
//! The princicple of operation is also shown by this diagram:
//! <img src="https://raw.githubusercontent.com/de-vri-es/turnip-tunnel/main/principle-of-operation.svg" alt="diagram of two laptops connected to a root vegetable" width="100%"/>
//!
//! # Feature comparison
//!
//! |                                      | turnip (tunnel) | turnip (root vegetable) |
//! |--------------------------------------|-----------------|-------------------------|
//! | Full duplex communication            | ❌              | ❌                      |
//! | Forward error correction             | ❌              | ❌                      |
//! | Error detection                      | ❌ <sup>*</sup> | ❌                      |
//! | Automatic retransmissions            | ❌ <sup>*</sup> | ❌                      |
//! | Delicious                            | ❌              | ❌                      |
//! | Fits in a healthy diet               | ❌              | ✅                      |
//! | Useful when you have no alternative  | ✅              | ✅                      |
//!
//! <sup>*</sup> But don't worry, the transport layer or the application layer will take care of this.
//!
//! # Controller and worker
//!
//! The tunnel works with `controller` and a `worker` side.
//! They are essentially identical, except that the controller initiates all communication.
//! The `worker` may only transmit over the serial line after it received a message from the `controller`.
//! This is required to prevent message collisions on the half-duplex line.
//!
//! Even though the `worker` can not initiate communication,
//! a packet may arrive from the tunnel interface to be transmitted over the serial port at any time.
//! To allow for this, the `controller` will regularly poll the `worker` to ask if any packets have arrived.
//! To minimize overhead, transmitting packets to the `worker` and polling it for packets are done with the same message.
//! If no packets need to be sent to the `worker`, the `controller` simply sends an empty list of packets.
//!
//! The command line options of the controller and the `worker` are almost identical.
//! The `controller` only has one additional timeout parameter (`--poll-timeout`)
//! to determine how long it waits for a packet to arrive on the tunnel interface before polling the `worker` for a packet.
//! You should make sure that this timeout is *lower* than the read/write timeouts for the serial port on both ends,
//! otherwise the `worker` will encounter timeouts while waiting for the `controller` to send a message over the serial port.
//!
//! Most options have sane defaults, although you may need to increase timeout values when using lower baud rates.
//! The only required options are the serial port (`--serial ...`) and the baud rate (`--baud`).

#![forbid(unsafe_code)]

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use serial2_tokio::SerialPort;
use tracing::Instrument as _;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::controller::Controller;

mod controller;
mod protocol;
mod worker;

const DEFAULT_MTU: u16 = 512;

#[derive(clap::Parser)]
struct Options {
	#[clap(subcommand)]
	pub command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
	/// Run a controller.
	Controller(ControllerCommand),

	/// Run a worker.
	Worker(WorkerCommand),

	/// Run a worker and a controller using a PTY pair.
	#[cfg(unix)]
	Local(LocalCommand),
}

#[derive(clap::Parser)]
struct ControllerCommand {
	/// Serial port to forward traffic over.
	#[clap(short, long)]
	#[clap(value_name = "PATH")]
	pub serial: PathBuf,

	/// Baud rate to use for the serial port.
	#[clap(short, long)]
	pub baud: u32,

	/// Name of the tunnel interface to create.
	#[clap(short, long)]
	#[clap(value_name = "NAME")]
	pub interface: Option<String>,

	/// Addresses to add to the tunnel interface.
	#[clap(short, long)]
	#[clap(value_name = "ADDRESS[/NETMASK]")]
	pub address: Vec<Address>,

	/// The MTU for the tunnel interface.
	#[clap(short, long)]
	#[clap(default_value_t = DEFAULT_MTU)]
	pub mtu: u16,

	/// Timeout for reading a message from the serial port.
	#[clap(long)]
	#[clap(default_value = "50")]
	#[clap(value_parser = parse_millis)]
	pub read_timeout: Duration,

	/// Timeout for writing a message to the serial port.
	#[clap(long)]
	#[clap(default_value = "50")]
	#[clap(value_parser = parse_millis)]
	pub write_timeout: Duration,

	/// Maximum time to wait for a packet from the tunnel interface before polling the worker for packets.
	#[clap(long)]
	#[clap(default_value = "10")]
	#[clap(value_parser = parse_millis)]
	pub poll_timeout: Duration,

	/// Act as an link-layer tunnel instead of an IP tunnel.
	///
	/// On Linux this creates a TAP device instead of a TUN device.
	#[clap(long)]
	pub link_layer: bool,
}

#[derive(clap::Parser)]
struct WorkerCommand {
	/// Serial port to forward traffic over.
	#[clap(short, long)]
	#[clap(value_name = "PATH")]
	pub serial: PathBuf,

	/// Baud rate to use for the serial port.
	#[clap(short, long)]
	pub baud: u32,

	/// Name of the tunnel interface to create.
	#[clap(short, long)]
	#[clap(value_name = "NAME")]
	pub interface: Option<String>,

	/// Addresses to add to the tunnel interface.
	#[clap(short, long)]
	#[clap(value_name = "ADDRESS[/NETMASK]")]
	pub address: Vec<Address>,

	/// The MTU for the tunnel interface.
	#[clap(short, long)]
	#[clap(default_value_t = DEFAULT_MTU)]
	pub mtu: u16,

	/// Timeout for reading a message from the serial port.
	#[clap(long)]
	#[clap(default_value = "50")]
	#[clap(value_parser = parse_millis)]
	pub read_timeout: Duration,

	/// Timeout for writing a message to the serial port.
	#[clap(long)]
	#[clap(default_value = "50")]
	#[clap(value_parser = parse_millis)]
	pub write_timeout: Duration,

	/// Act as an link-layer tunnel instead of an IP tunnel.
	///
	/// On Linux this creates a TAP device instead of a TUN device.
	#[clap(long)]
	pub link_layer: bool,
}

/// Run the controller and the worker on a new PTY pair.
#[derive(clap::Parser)]
#[cfg(unix)]
struct LocalCommand {
	/// Name of the controller interface to create.
	#[clap(long)]
	#[clap(value_name = "NAME")]
	pub controller_interface: Option<String>,

	/// Addresses to add to the controller interface.
	#[clap(long)]
	#[clap(value_name = "ADDRESS[/NETMASK]")]
	pub controller_address: Vec<Address>,

	/// Name of the worker interface to create.
	#[clap(long)]
	#[clap(value_name = "NAME")]
	pub worker_interface: Option<String>,

	/// Addresses to add to the worker interface.
	#[clap(long)]
	#[clap(value_name = "ADDRESS[/NETMASK]")]
	pub worker_address: Vec<Address>,

	/// The MTU for the tunnel interface.
	#[clap(short, long)]
	#[clap(default_value_t = DEFAULT_MTU)]
	pub mtu: u16,

	/// Timeout for reading a message from the serial port.
	#[clap(long)]
	#[clap(default_value = "50")]
	#[clap(value_parser = parse_millis)]
	pub read_timeout: Duration,

	/// Timeout for writing a message to the serial port.
	#[clap(long)]
	#[clap(default_value = "50")]
	#[clap(value_parser = parse_millis)]
	pub write_timeout: Duration,

	/// Maximum time to wait for a packet from the tunnel interface.
	#[clap(long)]
	#[clap(default_value = "50")]
	#[clap(value_parser = parse_millis)]
	pub poll_timeout: Duration,

	/// Act as an link-layer tunnel instead of an IP tunnel.
	///
	/// On Linux this creates a TAP device instead of a TUN device.
	#[clap(long)]
	pub link_layer: bool,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
	tracing_subscriber::registry()
		.with(
			tracing_subscriber::EnvFilter::builder()
				.with_default_directive(tracing::Level::INFO.into())
				.from_env_lossy(),
		)
		.with(tracing_subscriber::fmt::layer())
		.init();
	match do_main(clap::Parser::parse()).await {
		Err(()) => std::process::ExitCode::FAILURE,
	}
}

async fn do_main(options: Options) -> Result<std::convert::Infallible, ()> {
	match options.command {
		Command::Controller(command) => command.run().await,
		Command::Worker(command) => command.run().await,
		#[cfg(unix)]
		Command::Local(command) => command.run().await,
	}
}

impl ControllerCommand {
	pub async fn run(&self) -> Result<std::convert::Infallible, ()> {
		let Self {
			serial,
			baud,
			interface,
			address,
			mtu,
			read_timeout,
			write_timeout,
			poll_timeout,
			link_layer,
		} = self;

		let interface = make_interface(interface.as_deref(), address, *mtu, *link_layer, "controller")?;
		let serial_port =
			SerialPort::open(serial, *baud).map_err(|e| tracing::error!("Failed to open serial port {}: {e}", self.serial.display()))?;

		let mut controller = Controller::new(serial_port, interface).await?;
		controller.set_read_timeout(*read_timeout);
		controller.set_write_timeout(*write_timeout);
		controller.set_poll_timeout(*poll_timeout);

		let span = tracing::span!(tracing::Level::INFO, "controller");
		let work = async move {
			#[cfg(unix)]
			sd_notify::notify(&[
				sd_notify::NotifyState::Status(&format!("Running on {}", serial.display())),
				sd_notify::NotifyState::Ready,
			])
			.ok();
			controller.run().await
		};
		work.instrument(span).await
	}
}

impl WorkerCommand {
	pub async fn run(&self) -> Result<std::convert::Infallible, ()> {
		let Self {
			serial,
			baud,
			interface,
			address,
			mtu,
			read_timeout,
			write_timeout,
			link_layer,
		} = self;

		let interface = make_interface(interface.as_deref(), address, *mtu, *link_layer, "worker")?;
		let serial_port =
			SerialPort::open(serial, *baud).map_err(|e| tracing::error!("Failed to open serial port {}: {e}", serial.display()))?;

		let mut worker = worker::Worker::new(serial_port, interface);
		worker.set_read_timeout(*read_timeout);
		worker.set_write_timeout(*write_timeout);

		let span = tracing::span!(tracing::Level::INFO, "worker");
		#[cfg(unix)]
		sd_notify::notify(&[
			sd_notify::NotifyState::Status(&format!("Running on {}", serial.display())),
			sd_notify::NotifyState::Ready,
		])
		.ok();

		worker.run().instrument(span).await
	}
}

#[cfg(unix)]
impl LocalCommand {
	pub async fn run(&self) -> Result<std::convert::Infallible, ()> {
		let Self {
			controller_interface,
			controller_address,
			worker_interface,
			worker_address,
			mtu,
			read_timeout,
			write_timeout,
			poll_timeout,
			link_layer,
		} = self;

		let controller_interface = make_interface(controller_interface.as_deref(), controller_address, *mtu, *link_layer, "controller")?;
		let worker_interface = make_interface(worker_interface.as_deref(), worker_address, *mtu, *link_layer, "worker")?;

		let (a, b) = SerialPort::pair().map_err(|e| tracing::error!("Failed to create PTY pair: {e}"))?;

		let mut controller = Controller::new(a, controller_interface).await?;
		controller.set_read_timeout(*read_timeout);
		controller.set_write_timeout(*write_timeout);
		controller.set_poll_timeout(*poll_timeout);

		let mut worker = worker::Worker::new(b, worker_interface);
		worker.set_read_timeout(*read_timeout);
		worker.set_write_timeout(*write_timeout);

		let span = tracing::span!(tracing::Level::INFO, "controller");
		let controller = async move { tokio::spawn(async move { controller.run().instrument(span).await }).await.unwrap() };

		let span = tracing::span!(tracing::Level::INFO, "worker");
		let worker = async move { tokio::spawn(async move { worker.run().instrument(span).await }).await.unwrap() };

		#[cfg(unix)]
		sd_notify::notify(&[
			sd_notify::NotifyState::Status("Running on local PTY pair"),
			sd_notify::NotifyState::Ready,
		])
		.ok();
		match tokio::try_join!(controller, worker) {
			Err(()) => Err(()),
		}
	}
}

fn make_interface(name: Option<&str>, addresses: &[Address], mtu: u16, link_layer: bool, task: &str) -> Result<tun_rs::AsyncDevice, ()> {
	let mut builder = tun_rs::DeviceBuilder::new().mtu(mtu);
	if link_layer {
		builder = builder.layer(tun_rs::Layer::L2)
	} else {
		builder = builder.layer(tun_rs::Layer::L3)
	}
	if let Some(name) = name {
		builder = builder.name(name);
	}
	for address in addresses {
		match address.address {
			IpAddr::V4(ip) => builder = builder.ipv4(ip, address.prefix, None),
			IpAddr::V6(ip) => builder = builder.ipv6(ip, address.prefix),
		}
	}
	let device = builder
		.build_async()
		.map_err(|e| tracing::error!("Failed to create interface for {task}: {e}"))?;

	let name = device
		.name()
		.map_err(|e| tracing::error!("Failed to get name of {task} interface: {e}"))?;

	tracing::info!("Created {task} tunnel interface {name} with MTU {mtu}");
	for address in addresses {
		tracing::info!("  Address: {address}");
	}

	Ok(device)
}

#[derive(Debug, Copy, Clone)]
struct Address {
	pub address: IpAddr,
	pub prefix: u8,
}

impl std::str::FromStr for Address {
	type Err = &'static str;

	fn from_str(input: &str) -> Result<Self, Self::Err> {
		if let Some((address, prefix)) = input.split_once('/') {
			let address = address.parse().map_err(|_| "invalid IP address")?;
			let prefix = prefix.parse().map_err(|_| "invalid prefix length")?;
			Ok(Self { address, prefix })
		} else {
			let address = input.parse().map_err(|_| "invalid IP address")?;
			let prefix = match address {
				IpAddr::V4(_) => 32,
				IpAddr::V6(_) => 128,
			};
			Ok(Self { address, prefix })
		}
	}
}

impl std::fmt::Display for Address {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let Self { address, prefix } = self;
		write!(f, "{address}/{prefix}")
	}
}

fn parse_millis(value: &str) -> Result<Duration, &'static str> {
	let millis: u64 = value.parse().map_err(|_| "expected a number of milliseconds")?;
	Ok(Duration::from_millis(millis))
}
