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
	max_payload_size: usize,
}

impl Controller {
	pub async fn new(serial_port: SerialPort, interface: tun_rs::AsyncDevice) -> Result<Self, ()> {
		Ok(Self {
			serial_port,
			interface,
			read_timeout: Duration::from_millis(50),
			write_timeout: Duration::from_millis(50),
			poll_timeout: Duration::from_millis(10),
			max_payload_size: crate::DEFAULT_MAX_PAYLOAD_SIZE,
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

	pub fn set_max_payload_size(&mut self, max_size: usize) {
		self.max_payload_size = max_size;
	}

	pub async fn run(&mut self) -> Result<std::convert::Infallible, ()> {
		let mut packets = vec![0u8; 65535];
		let mut alive = true;
		loop {
			// Receive packets from the tunnel interface and transmit them over the serial port.
			let payload_size = self.receive_from_interface(&mut packets).await?;
			protocol::send_packets(&mut self.serial_port, &packets[..payload_size], self.write_timeout, self.max_payload_size)
				.await
				.map_err(|e| tracing::error!("Failed to write packet over serial port: {e}"))?;

			let packets = match protocol::read_packets(&mut self.serial_port, self.read_timeout, self.max_payload_size).await {
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
					tracing::error!("{e}");
					if let Error::SerialPort(_) = e {
						// Failure to read from the serial port is likely a permanent error, so return with an error.
						return Err(());
					} else {
						continue;
					}
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
		// First parse one packet asynchronously with a timeout.
		let Some((len_buffer, data_buffer)) = payload_buffer.split_first_chunk_mut() else {
			return Ok(0);
		};

		let mut total_size = 0;
		let packet_size = match tokio::time::timeout(self.poll_timeout, self.interface.recv(data_buffer)).await {
			Err(tokio::time::error::Elapsed { .. }) => {
				tracing::trace!("Timeout waiting for packet on tunnel interface, poll timeout: {:?}", self.poll_timeout);
				return Ok(0);
			},
			Ok(Ok(0)) => {
				tracing::error!("Read 0-sized packet from tunnel interface, interface was deleted?");
				return Err(());
			}
			Ok(Err(e)) => {
				tracing::error!("Failed to receive packet from tunnel interface: {e}");
				return Err(());
			}
			Ok(Ok(packet_size)) => {
				tracing::debug!("Received packet of {packet_size} bytes from tunnel interface");
				match u16::try_from(packet_size) {
					Ok(packet_size) => packet_size,
					Err(_) => {
						tracing::warn!("Dropping extremely large packet ({packet_size} bytes) received from tunnel interface");
						0
					}
				}
			}
		};

		// We used a packet size of 0 to indicate we received but dropped a packet.
		// If we did an actual 0 sized read we already returned an error.
		if packet_size > 0 {
			total_size += std::mem::size_of::<u16>() + usize::from(packet_size);
			*len_buffer = packet_size.to_le_bytes();
		}

		let mtu = self.interface.mtu()
			.map_err(|e| tracing::error!("Failed to query interface MTU: {e}"))?;

		// Then opportunistically try reading more packets until the buffer is full,
		// but only if they are directly available.
		while let Some((len_buffer, data_buffer)) = payload_buffer[total_size..].split_first_chunk_mut() {
			if data_buffer.len() < mtu.into() {
				tracing::trace!("Not enough space left in packet buffer to receive a full MTU");
				break;
			}
			let packet_size = match self.interface.try_recv(data_buffer) {
				Ok(0) => {
					tracing::error!("Read 0-sized packet from tunnel interface");
					return Err(());
				}
				Err(e) => {
					if e.kind() == std::io::ErrorKind::WouldBlock {
						tracing::trace!("No more packets available from tunnel interface");
						break;
					} else {
						tracing::error!("Failed to receive packet from tunnel interface: {e}");
						return Err(());
					}
				}
				Ok(packet_size) => {
					tracing::debug!("Received packet of {packet_size} bytes from tunnel interface");
					match u16::try_from(packet_size) {
						Ok(packet_size) => packet_size,
						Err(_) => {
							tracing::warn!("Dropping extremely large packet ({packet_size} bytes) received from tunnel interface");
							continue;
						}
					}
				}
			};
			total_size += std::mem::size_of_val(&packet_size) + usize::from(packet_size);
			*len_buffer = packet_size.to_le_bytes();
		}

		Ok(total_size)
	}
}
