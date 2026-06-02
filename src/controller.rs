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
		let mut rx_buffer = vec![0u8; 65535];
		let mut alive = true;
		loop {
			let packet_size = match tokio::time::timeout(self.poll_timeout, self.interface.recv(&mut rx_buffer)).await {
				Err(tokio::time::error::Elapsed { .. }) => None,
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
					Some(size)
				}
			};

			if let Some(packet_size) = packet_size {
				let packet_data = &rx_buffer[..packet_size];
				protocol::send_packets(&mut self.serial_port, &[packet_data], self.write_timeout)
					.await
					.map_err(|e| tracing::error!("Failed to write packet over serial port: {e}"))?;
			} else {
				protocol::send_packets::<&[u8]>(&mut self.serial_port, &[], self.write_timeout)
					.await
					.map_err(|e| tracing::error!("Failed to ask for packets over serial port: {e}"))?;
			}

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
}
