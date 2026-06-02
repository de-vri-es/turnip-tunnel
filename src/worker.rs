use serial2_tokio::SerialPort;
use std::time::Duration;

use crate::protocol;
use crate::protocol::Error;

pub struct Worker {
	serial_port: SerialPort,
	interface: tun_rs::AsyncDevice,
	write_timeout: Duration,
	read_timeout: Duration,
}

impl Worker {
	pub fn new(serial_port: SerialPort, interface: tun_rs::AsyncDevice) -> Self {
		Self {
			serial_port,
			interface,
			read_timeout: Duration::from_millis(50),
			write_timeout: Duration::from_millis(50),
		}
	}

	pub fn set_read_timeout(&mut self, timeout: Duration) {
		self.read_timeout = timeout;
	}

	pub fn set_write_timeout(&mut self, timeout: Duration) {
		self.write_timeout = timeout;
	}

	pub async fn run(&mut self) -> Result<std::convert::Infallible, ()> {
		let mut alive = true;
		loop {
			// Read packets from serial port.
			let packets = match protocol::read_packets(&mut self.serial_port, self.read_timeout).await {
				Ok(packets) => {
					if !alive {
						tracing::info!("Succesfully received packets from serial port, connection is back");
						alive = true;
					}
					packets
				}
				Err(Error::TimeoutElapsed) => {
					if alive {
						tracing::error!("Time-out reading from serial port, assuming connection is gone");
						alive = false;
					}
					// NOTE: If we didn't read anything, we don't have permission to send.
					// The link is half-duplex, so we can only respond after we receive a message.
					continue;
				}
				Err(e) => {
					tracing::error!("Failed to read packet(s) from serial port: {e}");
					return Err(());
				}
			};

			// Collect packets from the tunnel interface (without waiting).
			let mut queue: Vec<Vec<u8>> = Vec::new();
			while queue.len() < 16 {
				let mut packet = vec![0; 65535];
				match self.interface.try_recv(&mut packet) {
					Ok(0) => {
						tracing::error!("Read 0-sized packet from tunnel interface, interface was deleted?");
						return Err(());
					}
					Ok(size) => {
						tracing::debug!("Received packet of {} bytes from tunnel interface", size);
						packet.truncate(size);
						queue.push(packet);
					}
					Err(e) => {
						if e.kind() == std::io::ErrorKind::WouldBlock {
							break;
						} else {
							tracing::error!("Failed to receive packet from tunnel interface: {e}");
							return Err(());
						}
					}
				}
			}

			// First transmit queued packets back over the serial port, since this is our only chance to do it.
			protocol::send_packets(&mut self.serial_port, &queue, self.write_timeout)
				.await
				.map_err(|e| tracing::error!("Failed to write packet(s) to serial port: {e}"))?;

			// Now deliver packets to the tunnel interface.
			for packet in &packets {
				tracing::debug!("Writing packet of {} bytes to tunnel interface", packet.len());
				self.interface
					.send(packet)
					.await
					.map_err(|e| tracing::error!("Failed to write packet to tunnel interface: {e}"))?;
			}
		}
	}
}
