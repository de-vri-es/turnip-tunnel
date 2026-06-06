use serial2_tokio::SerialPort;
use std::time::Duration;

use crate::protocol;
use crate::protocol::Error;

pub struct Controller {
	serial_port: SerialPort,
	interface: tun_rs::AsyncDevice,
	write_timeout: Duration,
	read_timeout: Duration,
	poll_timeout: Duration,
}

impl Controller {
	pub async fn new(serial_port: SerialPort, interface: tun_rs::AsyncDevice) -> Result<Self, ()> {
		Ok(Self {
			serial_port,
			interface,
			read_timeout: Duration::from_millis(50),
			write_timeout: Duration::from_millis(50),
			poll_timeout: Duration::from_millis(10),
		})
	}

	pub fn set_read_timeout(&mut self, timeout: Duration) {
		self.read_timeout = timeout;
	}

	pub fn set_write_timeout(&mut self, timeout: Duration) {
		self.write_timeout = timeout;
	}

	pub fn set_poll_timeout(&mut self, timeout: Duration) {
		self.poll_timeout = timeout;
	}

	pub async fn run(&mut self) -> Result<std::convert::Infallible, ()> {
		let mut packets = vec![0u8; 65535];
		let mut alive = true;
		loop {
			// Receive packets from the tunnel interface and transmit them over the serial port.
			let payload_size = self.receive_from_interface(&mut packets).await?;
			protocol::send_packets(&mut self.serial_port, &packets[..payload_size], self.write_timeout)
				.await
				.map_err(|e| tracing::error!("Failed to write packet over serial port: {e}"))?;

			let packets = match protocol::read_packets(&mut self.serial_port, self.read_timeout).await {
				Ok(packets) => {
					if !alive {
						tracing::info!("Successfully received packets from serial port, connection is back");
						alive = true;
					}
					packets
				}
				Err(Error::TimeoutElapsed) => {
					if alive {
						tracing::error!("Time-out reading from serial port, assuming connection is gone");
						alive = false;
					}
					continue;
				}
				Err(e) => {
					tracing::error!("Failed to read packet(s) from serial port: {e}");
					return Err(());
				}
			};
			for packet in &packets {
				tracing::debug!("Writing packet of {} bytes to tunnel interface", packet.len());
				self.interface
					.send(packet)
					.await
					.map_err(|e| tracing::error!("Failed to write packet to tunnel interface: {e}"))?;
			}
		}
	}

	/// Receive packets from the tunnel interface, putting them in the wire format in `payload_buffer`.
	///
	/// Waits up to `self.poll_timeout` for the first packet to be available,
	/// then reads any directly available packets until the buffer is full.
	async fn receive_from_interface(&mut self, payload_buffer: &mut [u8]) -> Result<usize, ()> {
		// First parse one packet asynchronousy with a timeout.
		let Some((len_buffer, data_buffer)) = payload_buffer.split_first_chunk_mut::<4>() else {
			return Ok(0);
		};

		let mut total_size = 0;
		let packet_size = match tokio::time::timeout(self.poll_timeout, self.interface.recv(data_buffer)).await {
			Err(tokio::time::error::Elapsed { .. }) => return Ok(0),
			Ok(Ok(0)) => {
				tracing::error!("Read 0-sized packet from tunnel interface, interface was deleted?");
				return Err(());
			}
			Ok(Err(e)) => {
				tracing::error!("Failed to receive packet from tunnel interface: {e}");
				return Err(());
			}
			Ok(Ok(size)) => {
				tracing::debug!("Received packet of {} bytes from tunnel interface", size);
				size
			}
		};

		total_size += 4 + packet_size;
		*len_buffer = (packet_size as u32).to_le_bytes();

		let mtu = self.interface.mtu()
			.map_err(|e| tracing::error!("Failed to query interface MTU: {e}"))?;

		// Then opportunistically try reading more packets until the buffer is full,
		// but only if they are directly available.
		while let Some((len_buffer, data_buffer)) = payload_buffer.split_first_chunk_mut::<4>() {
			if data_buffer.len() < mtu.into() {
				break;
			}
			let packet_size = match self.interface.try_recv(data_buffer) {
				Ok(0) => {
					tracing::error!("Read 0-sized packet from tunnel interface, interface was deleted?");
					return Err(());
				}
				Err(e) => {
					if e.kind() == std::io::ErrorKind::WouldBlock {
						break;
					} else {
						tracing::error!("Failed to receive packet from tunnel interface: {e}");
						return Err(());
					}
				}
				Ok(size) => {
					tracing::debug!("Received packet of {} bytes from tunnel interface", size);
					size
				}
			};
			total_size += 4 + packet_size;
			*len_buffer = (packet_size as u32).to_le_bytes();
		}

		Ok(total_size)
	}
}

