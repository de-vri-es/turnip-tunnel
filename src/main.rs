//! `turnip` is an IP or Ethernet tunnel using a half-duplex serial port without any control flow signals as the transport channel.
//!
//! To allow IP over a half-duplex serial line, each side of the serial link takes turns to transmit packets to the other side.
//! Hence the name "turn IP", or "turnip".
//! Additionally, the half-duplex serial port was invented around the same time as the turnip, and they are about equally technologically advanced.
//!
//! The principle of operation is also shown by this diagram:
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
//! The tunnel works with a `controller` and a `worker` side.
//! They are essentially identical, except that the controller initiates all communication.
//! The `worker` may only transmit in direct response to a message from the `controller`.
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
//!
//! # Error detection and retransmissions
//!
//! `turnip` focusses on simplicity over performance, and it sits below the transport layer (at L2 or L3, to be precise).
//! This means that it can leave most of the error detection and retransmission to the upper layers of the network, and it does.
//!
//! To be precise, `turnip` only does its best to re-synchronize the start and end of a frame if anything happens.
//! It does not try to retransmit lost or damaged packets.
//!
//! If this makes you worry, don't: this is perfectly normal in networking.
//! The transport layer is there precisely to allow the layers below it to be unreliable.
//!
//! An argument could be made for adding forward error correction, if the serial link is very noisy.
//! Sacrificing bandwidth to reduce retransmission round-trips could result in a better throughput, and certainly better latency.
//! A future version of `turnip` may include forward error correction.
//!
//! # Wire format
//!
//! The wire format of `turnip` is very simple.
//! In a nutshell: each message starts with a preamble of `0x00 0xFF 0xFF 0x01`,
//! followed by a [COBS] (Consistent Overhead Byte Stuffing) encoded payload.
//! The COBS encoding ensures that there are no `0x00` bytes in the payload itself,
//! and it adds a `0x00` byte after the payload to mark the end of the message.
//!
//! The payload itself is a packet list, which is the concatenation of a 16-bit packet length (little endian) and the packet data, for each packet.
//! Keep in mind, it has to be COBS encoded after the concatenation.
//!
//! The following table shows the layout of a `turnip` message:
//!
//! | Field           | Size (bytes) | Description                    |
//! |-----------------|--------------|--------------------------------|
//! | Preamble        | 4            | `0x00`, `0xFF`, `0xFF`, `0x01` |
//! | Payload         | Variable     | COBS encoded payload           |
//!
//! And the layout of the payload (before COBS encoding) is shown by this table:
//!
//! | Field           | Size (bytes) | Description                                    |
//! |-----------------|--------------|------------------------------------------------|
//! | Packet 1 length | 2            | The size in bytes of packet 1                  |
//! | Packet 1 data   | Variable     | The data of packet 1                           |
//! | ...             |              |                                                |
//! | Packet N length | 2            | The size in bytes of packet N                  |
//! | Packet N data   | Variable     | The data of packet N                           |
//!
//! A packet list can contain any number of packets, including zero.
//! Note that COBS encoding still means that the minimum size of the payload is 2 bytes, even when the packet list is empty.
//!
//! [COBS]: https://en.wikipedia.org/wiki/Consistent_Overhead_Byte_Stuffing

#![forbid(unsafe_code)]

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use serial2_tokio::SerialPort;
use tracing::Instrument as _;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::controller::Controller;
use crate::worker::Worker;

mod controller;
mod protocol;
mod worker;

const DEFAULT_MTU: u16 = 512;
const DEFAULT_MAX_PAYLOAD_SIZE: usize = (DEFAULT_MTU as usize + 2) * 2; // Enough space for two MTU sized packets with their length.

const DEFAULT_POLL_TIMEOUT: &str = "50";
const DEFAULT_READ_TIMEOUT: &str = "200";
const DEFAULT_WRITE_TIMEOUT: &str = "200";

#[derive(clap::Parser)]
#[clap(version, about)]
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

/// Run a `turnip` controller on a serial port.
#[derive(clap::Parser)]
#[clap(version)]
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

	/// Create the tunnel interface in the given network namespace.
	///
	/// The network namespace can be created ahead of time with the `ip netns` tool on Linux.
	/// If the network namespace does not exist yet, it will be created.
	#[clap(long)]
	#[cfg(feature = "netns")]
	pub netns: Option<String>,

	/// Addresses to add to the tunnel interface.
	#[clap(short, long)]
	#[clap(value_name = "ADDRESS[/NETMASK]")]
	pub address: Vec<Address>,

	/// The MTU for the tunnel interface.
	#[clap(short, long)]
	#[clap(default_value_t = DEFAULT_MTU)]
	pub mtu: u16,

	/// The maximum payload size for messages going over the serial port.
	///
	/// Note that this is not the same as the MTU:
	/// The MTU gives the maximum size of packets going over the tunnel interface.
	/// But `turnip` may send multiple packets in a single message,
	/// to improve througput when the packets are very small.
	///
	/// The maximum message size should always allow for at-least the MTU plus 2 bytes overhead.
	///
	/// If not given, it defaults to the MTU * 2 + 8 bytes.
	#[clap(long)]
	pub max_payload_size: Option<usize>,

	/// Timeout for reading a message from the serial port.
	#[clap(long)]
	#[clap(default_value = DEFAULT_READ_TIMEOUT)]
	#[clap(value_parser = parse_millis)]
	pub read_timeout: Duration,

	/// Timeout for writing a message to the serial port.
	#[clap(long)]
	#[clap(default_value = DEFAULT_WRITE_TIMEOUT)]
	#[clap(value_parser = parse_millis)]
	pub write_timeout: Duration,

	/// Maximum time to wait for a packet from the tunnel interface before polling the worker for packets.
	#[clap(long)]
	#[clap(default_value = DEFAULT_POLL_TIMEOUT)]
	#[clap(value_parser = parse_millis)]
	pub poll_timeout: Duration,

	/// Act as a link-layer tunnel instead of an IP tunnel.
	///
	/// On Linux this creates a TAP device instead of a TUN device.
	#[clap(long)]
	pub link_layer: bool,
}

/// Run a `turnip` worker on a serial port.
#[derive(clap::Parser)]
#[clap(version)]
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

	/// Create the tunnel interface in the given network namespace.
	///
	/// The network namespace can be created ahead of time with the `ip netns` tool on Linux.
	/// If the network namespace does not exist yet, it will be created.
	#[clap(long)]
	#[cfg(feature = "netns")]
	pub netns: Option<String>,

	/// Addresses to add to the tunnel interface.
	#[clap(short, long)]
	#[clap(value_name = "ADDRESS[/NETMASK]")]
	pub address: Vec<Address>,

	/// The MTU for the tunnel interface.
	#[clap(short, long)]
	#[clap(default_value_t = DEFAULT_MTU)]
	pub mtu: u16,

	/// The maximum payload size for messages going over the serial port.
	///
	/// Note that this is not the same as the MTU:
	/// The MTU gives the maximum size of packets going over the tunnel interface.
	/// But `turnip` may send multiple packets in a single message,
	/// to improve througput when the packets are very small.
	///
	/// The maximum message size should always allow for at-least the MTU plus 2 bytes overhead.
	///
	/// If not given, it defaults to the MTU * 2 + 8 bytes.
	#[clap(long)]
	pub max_payload_size: Option<usize>,

	/// Timeout for reading a message from the serial port.
	#[clap(long)]
	#[clap(default_value = DEFAULT_READ_TIMEOUT)]
	#[clap(value_parser = parse_millis)]
	pub read_timeout: Duration,

	/// Timeout for writing a message to the serial port.
	#[clap(long)]
	#[clap(default_value = DEFAULT_WRITE_TIMEOUT)]
	#[clap(value_parser = parse_millis)]
	pub write_timeout: Duration,

	/// Act as a link-layer tunnel instead of an IP tunnel.
	///
	/// On Linux this creates a TAP device instead of a TUN device.
	#[clap(long)]
	pub link_layer: bool,
}

/// Run the controller and the worker on a new PTY pair.
///
/// Consider creating at-least one of the tunnel interfaces in a separate network namespace if you wish to test the interface.
/// If both interfaces exist in the same network namespace, traffic will likely not be routed through the tunnel.
/// Use `--controller-netns` and/or `--worker-netns` to create the interfaces in a different namespace.
#[cfg(unix)]
#[derive(clap::Parser)]
#[clap(version)]
struct LocalCommand {
	/// Name of the controller interface to create.
	#[clap(long)]
	#[clap(value_name = "NAME")]
	pub controller_interface: Option<String>,

	/// Addresses to add to the controller interface.
	#[clap(long)]
	#[clap(value_name = "ADDRESS[/NETMASK]")]
	pub controller_address: Vec<Address>,

	/// Create the tunnel interface for the controller in the given network namespace.
	///
	/// The network namespace can be created ahead of time with the `ip netns` tool on Linux.
	/// If the network namespace does not exist yet, it will be created.
	#[clap(long)]
	#[cfg(feature = "netns")]
	pub controller_netns: Option<String>,

	/// Name of the worker interface to create.
	#[clap(long)]
	#[clap(value_name = "NAME")]
	pub worker_interface: Option<String>,

	/// Create the tunnel interface for the worker in the given network namespace.
	///
	/// The network namespace can be created ahead of time with the `ip netns` tool on Linux.
	/// If the network namespace does not exist yet, it will be created.
	#[clap(long)]
	#[cfg(feature = "netns")]
	pub worker_netns: Option<String>,

	/// Addresses to add to the worker interface.
	#[clap(long)]
	#[clap(value_name = "ADDRESS[/NETMASK]")]
	pub worker_address: Vec<Address>,

	/// The MTU for the tunnel interface.
	#[clap(short, long)]
	#[clap(default_value_t = DEFAULT_MTU)]
	pub mtu: u16,

	/// The maximum payload size for messages going over the serial port.
	///
	/// Note that this is not the same as the MTU:
	/// The MTU gives the maximum size of packets going over the tunnel interface.
	/// But `turnip` may send multiple packets in a single message,
	/// to improve througput when the packets are very small.
	///
	/// The maximum message size should always allow for at-least the MTU plus 2 bytes overhead.
	///
	/// If not given, it defaults to the MTU * 2 + 8 bytes.
	#[clap(long)]
	pub max_payload_size: Option<usize>,

	/// Timeout for reading a message from the serial port.
	#[clap(long)]
	#[clap(default_value = DEFAULT_READ_TIMEOUT)]
	#[clap(value_parser = parse_millis)]
	pub read_timeout: Duration,

	/// Timeout for writing a message to the serial port.
	#[clap(long)]
	#[clap(default_value = DEFAULT_WRITE_TIMEOUT)]
	#[clap(value_parser = parse_millis)]
	pub write_timeout: Duration,

	/// Maximum time to wait for a packet from the tunnel interface.
	#[clap(long)]
	#[clap(default_value = DEFAULT_POLL_TIMEOUT)]
	#[clap(value_parser = parse_millis)]
	pub poll_timeout: Duration,

	/// Act as a link-layer tunnel instead of an IP tunnel.
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
	pub async fn run(self) -> Result<std::convert::Infallible, ()> {
		let Self {
			serial,
			baud,
			interface,
			#[cfg(feature = "netns")]
			netns,
			address,
			mtu,
			max_payload_size,
			read_timeout,
			write_timeout,
			poll_timeout,
			link_layer,
		} = self;

		let interface_options = InterfaceOptions {
			name: interface,
			#[cfg(feature = "netns")]
			netns,
			addresses: address,
			mtu,
			link_layer,
		};

		let max_payload_size = check_mtu_message_size(mtu, max_payload_size)?;

		let serial_port = SerialPort::open(&serial, baud)
			.map_err(|e| tracing::error!("Failed to open serial port {}: {e}", serial.display()))?;

		let interface = interface_options.make("controller")?;

		let mut controller = Controller::new(serial_port, interface)?;
		controller.set_read_timeout(read_timeout);
		controller.set_write_timeout(write_timeout);
		controller.set_poll_timeout(poll_timeout);
		controller.set_max_payload_size(max_payload_size);

		#[cfg(unix)]
		sd_notify::notify(&[
			sd_notify::NotifyState::Status(&format!("Running on {}", serial.display())),
			sd_notify::NotifyState::Ready,
		]).ok();
		controller.run().await
	}
}

impl WorkerCommand {
	pub async fn run(self) -> Result<std::convert::Infallible, ()> {
		let Self {
			serial,
			baud,
			interface,
			#[cfg(feature = "netns")]
			netns,
			address,
			mtu,
			max_payload_size,
			read_timeout,
			write_timeout,
			link_layer,
		} = self;

		let interface_options = InterfaceOptions {
			name: interface,
			#[cfg(feature = "netns")]
			netns,
			addresses: address,
			mtu,
			link_layer,
		};

		let max_payload_size = check_mtu_message_size(mtu, max_payload_size)?;

		let serial_port = SerialPort::open(&serial, baud)
			.map_err(|e| tracing::error!("Failed to open serial port {}: {e}", serial.display()))?;

		let interface = interface_options.make("worker")?;

		let mut worker = Worker::new(serial_port, interface);
		worker.set_read_timeout(read_timeout);
		worker.set_write_timeout(write_timeout);
		worker.set_max_payload_size(max_payload_size);

		#[cfg(unix)]
		sd_notify::notify(&[
			sd_notify::NotifyState::Status(&format!("Running on {}", serial.display())),
			sd_notify::NotifyState::Ready,
		])
		.ok();

		worker.run().await
	}
}

#[cfg(unix)]
impl LocalCommand {
	pub async fn run(self) -> Result<std::convert::Infallible, ()> {
		let Self {
			controller_interface,
			#[cfg(feature = "netns")]
			controller_netns,
			controller_address,
			worker_interface,
			#[cfg(feature = "netns")]
			worker_netns,
			worker_address,
			mtu,
			max_payload_size,
			read_timeout,
			write_timeout,
			poll_timeout,
			link_layer,
		} = self;

		let controller_interface = InterfaceOptions {
			name: controller_interface,
			#[cfg(feature = "netns")]
			netns: controller_netns,
			addresses: controller_address,
			link_layer,
			mtu,
		};

		let worker_interface = InterfaceOptions {
			name: worker_interface,
			#[cfg(feature = "netns")]
			netns: worker_netns,
			addresses: worker_address,
			link_layer,
			mtu,
		};

		let max_payload_size = check_mtu_message_size(mtu, max_payload_size)?;

		let (a, b) = SerialPort::pair().map_err(|e| tracing::error!("Failed to create PTY pair: {e}"))?;

		let controller_span = tracing::span!(tracing::Level::INFO, "controller");
		let worker_span = tracing::span!(tracing::Level::INFO, "worker");

		let mut controller = {
			let _entered = controller_span.enter();
			let interface = controller_interface.make("controller")?;
			let mut controller = Controller::new(a, interface)?;
			controller.set_read_timeout(read_timeout);
			controller.set_write_timeout(write_timeout);
			controller.set_poll_timeout(poll_timeout);
			controller.set_max_payload_size(max_payload_size);
			controller
		};
		let controller = controller.run().instrument(controller_span);

		let mut worker = {
			let _entered = worker_span.enter();
			let interface = worker_interface.make("worker")?;
			let mut worker = Worker::new(b, interface);
			worker.set_read_timeout(read_timeout);
			worker.set_write_timeout(write_timeout);
			worker.set_max_payload_size(max_payload_size);
			worker
		};
		let worker = worker.run().instrument(worker_span);

		#[cfg(unix)]
		sd_notify::notify(&[
			sd_notify::NotifyState::Status("Running on local PTY pair"),
			sd_notify::NotifyState::Ready,
		]).ok();
		match tokio::try_join!(controller, worker) {
			Err(()) => Err(()),
		}
	}
}

struct InterfaceOptions {
	name: Option<String>,
	#[cfg(feature = "netns")]
	netns: Option<String>,
	addresses: Vec<Address>,
	mtu: u16,
	link_layer: bool,
}

impl InterfaceOptions {
	pub fn make(&self, task: &str) -> Result<tun_rs::AsyncDevice, ()> {
		#[cfg(feature = "netns")]
		if let Some(netns) = &self.netns {
			let runtime = tokio::runtime::Handle::current();
			// Enter the requested netns in a thread to create the interface.
			let span = tracing::Span::current();
			let interface = std::thread::scope(|scope| {
				let join_handle = scope.spawn(|| {
					let _entered = span.enter();
					let _runtime_guard = runtime.enter();
					NetNs::get_or_create(netns)?.enter()?;
					self.make_internal(task)
				});
				join_handle.join().unwrap()
			});
			return interface;
		}

		// No netns specified (possibly because feature is disabled).
		self.make_internal(task)
	}

	fn make_internal(&self, task: &str) -> Result<tun_rs::AsyncDevice, ()> {
		let mut builder = tun_rs::DeviceBuilder::new().mtu(self.mtu);
		if self.link_layer {
			builder = builder.layer(tun_rs::Layer::L2)
		} else {
			builder = builder.layer(tun_rs::Layer::L3)
		}
		if let Some(name) = &self.name {
			builder = builder.name(name);
		}
		for address in &self.addresses {
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

		let netns = cfg_select! {
			feature = "netns" => { self.netns.as_deref() }
			_ => { None::<&str> }
		};

		match netns {
			Some(netns) => tracing::info!("Created {task} tunnel interface {name} with MTU {} in netns {netns:?}", self.mtu),
			None => tracing::info!("Created {task} tunnel interface {name} with MTU {}", self.mtu),
		};

		for address in &self.addresses {
			tracing::info!("  Address: {address}");
		}

		Ok(device)
	}
}

#[derive(Debug, Copy, Clone)]
struct Address {
	pub address: IpAddr,
	pub prefix: u8,
}

impl std::str::FromStr for Address {
	type Err = String;

	fn from_str(input: &str) -> Result<Self, Self::Err> {
		let (address, prefix) = match input.split_once('/') {
			None => (input, None),
			Some((a, b)) => (a, Some(b)),
		};

		let address = address.parse().map_err(|_| "invalid IP address")?;
		let (family, max_prefix) = match address {
			IpAddr::V4(_) => ("IPv4", 32),
			IpAddr::V6(_) => ("IPv6", 128),
		};

		let prefix = match prefix {
			None => max_prefix,
			Some(prefix) => {
				let prefix = prefix.parse().map_err(|_| "invalid prefix length")?;
				if prefix > max_prefix {
					return Err(format!("invalid prefix length for {family}: maximum is {max_prefix}"));
				}
				prefix
			}
		};

		Ok(Self { address, prefix })
	}
}

impl std::fmt::Display for Address {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let Self { address, prefix } = self;
		write!(f, "{address}/{prefix}")
	}
}

#[cfg(feature = "netns")]
struct NetNs {
	name: String,
	ns: netns_rs::NetNs,
}

#[cfg(feature = "netns")]
impl NetNs {
	fn get_or_create(name: impl Into<String>) -> Result<Self, ()> {
		let name = name.into();
		let ns = match netns_rs::NetNs::new(&name) {
			Ok(ns) => {
				tracing::debug!("Created new netns {name:?}");
				ns
			},
			Err(netns_rs::Error::CreateNsError(e)) if e.raw_os_error() == Some(libc::EPERM) => {
				let ns = netns_rs::NetNs::get(&name)
					.map_err(|e| tracing::error!("Failed to open existing netns {name:?}: {e}"))?;
				tracing::debug!("Opened existing netns {name:?}");
				ns
			},
			Err(e) => {
				tracing::error!("Failed to create netns {name:?}: {e:?}");
				return Err(())
			},
		};

		Ok(Self {
			name,
			ns,
		})
	}

	fn enter(&self) -> Result<(), ()> {
		self.ns.enter()
			.map_err(|e| tracing::error!("Failed to enter netns {:?}: {e}", self.name))
	}
}

fn parse_millis(value: &str) -> Result<Duration, &'static str> {
	let millis: u64 = value.parse().map_err(|_| "expected a number of milliseconds")?;
	Ok(Duration::from_millis(millis))
}

fn check_mtu_message_size(mtu: u16, max_payload_size: Option<usize>) -> Result<usize, ()> {
	let mtu = usize::from(mtu);
	if mtu < 64 {
		tracing::error!("invalid --mtu value: may not be less than 64 bytes");
	}
	match max_payload_size {
		None => Ok(mtu * 2 + 8),
		Some(max_payload_size) => {
			if max_payload_size < mtu + 2 {
				tracing::error!("invalid --max-payload-size and --mtu combination: --max-payload-size must be at-least 2 bytes more than the MTU");
				return Err(());
			}
			Ok(max_payload_size)
		},
	}
}
