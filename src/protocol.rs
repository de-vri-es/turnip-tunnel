use serial2_tokio::SerialPort;
use std::io::IoSlice;
use std::time::Duration;
use tokio::io::AsyncReadExt as _;

pub const PREAMBLE: [u8; 4] = [0x00, 0xFF, 0xFF, 0x01];
pub const MAX_PAYLOAD_SIZE: usize = 65535;

#[derive(Debug)]
pub enum Error {
	SerialPort(std::io::Error),
	Interface(std::io::Error),
	TimeoutElapsed,
	InvalidPreamble { actual: [u8; 4] },
	InvalidMessagePayload { reason: &'static str },
	MessagePayloadTooLarge(usize),
}

impl std::fmt::Display for Error {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::SerialPort(e) => write!(f, "I/O error on serial port: {e}"),
			Self::Interface(e) => write!(f, "I/O error on tunnel interface: {e}"),
			Self::TimeoutElapsed => write!(f, "timeout elapsed"),
			Self::InvalidPreamble { actual } => {
				write!(f, "invalid message preamble: 0x{actual:02X?} (expected 0x{PREAMBLE:02X?})")
			}
			Self::InvalidMessagePayload { reason } => {
				write!(f, "invalid message payload: {reason}")
			}
			Self::MessagePayloadTooLarge(length) => write!(f, "message payload too large: {length} bytes, maximum allowed is {}", MAX_PAYLOAD_SIZE),
		}
	}
}

impl From<tokio::time::error::Elapsed> for Error {
	fn from(_: tokio::time::error::Elapsed) -> Self {
		Self::TimeoutElapsed
	}
}

#[tracing::instrument(skip(channel, packets))]
pub async fn send_packets(channel: &mut SerialPort, packets: &[u8], timeout: Duration) -> Result<(), Error> {
	let work = async {
		if packets.len() > MAX_PAYLOAD_SIZE {
			return Err(Error::MessagePayloadTooLarge(packets.len()));
		}
		let message_size = packets.len() as u32;

		tracing::trace!(
			"Sending message with payload of {message_size} bytes, containing {} packets",
			packets.len()
		);

		let mut frame_header = [0u8; 8];
		frame_header[0..4].copy_from_slice(&PREAMBLE);
		frame_header[4..8].copy_from_slice(&message_size.to_le_bytes());

		let mut slices = [
			IoSlice::new(&frame_header),
			IoSlice::new(packets),
		];
		let mut slices = &mut slices[..];

		while !slices.is_empty() {
			let written = channel.write_vectored(slices).await.map_err(Error::SerialPort)?;
			if written == 0 {
				return Err(Error::SerialPort(std::io::ErrorKind::UnexpectedEof.into()));
			}
			IoSlice::advance_slices(&mut slices, written);
		}

		Ok(())
	};
	tokio::time::timeout(timeout, work).await?
}

#[tracing::instrument(skip(channel))]
pub async fn read_packets(channel: &mut SerialPort, timeout: Duration) -> Result<Packets, Error> {
	let work = async {
		// Read a header, discarding any non-preamble data.
		let mut header = [0u8; 8];
		let mut filled = 0;
		loop {
			channel.read_exact(&mut header[filled..]).await.map_err(Error::SerialPort)?;
			filled = header.len();
			let preamble_offset = scan_preample_start(&header);
			if preamble_offset == 0 {
				break;
			}
			tracing::warn!("Discarding {preamble_offset} garbage bytes");
			header.copy_within(preamble_offset.., 0);
			filled -= preamble_offset;
		}

		if header[0..4] != PREAMBLE {
			return Err(Error::InvalidPreamble {
				actual: header[0..4].try_into().unwrap(),
			});
		}
		let message_size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
		tracing::trace!("Receiving message with payload of {message_size} bytes");

		if message_size > MAX_PAYLOAD_SIZE {
			tracing::trace!("Incoming payload too large, refusing to parse, discarding input buffer.");
			channel.discard_input_buffer().map_err(Error::SerialPort)?;
			return Err(Error::MessagePayloadTooLarge(message_size));
		}

		let mut data = vec![0; message_size];
		channel.read_exact(&mut data).await.map_err(Error::SerialPort)?;
		Packets::from_data(data)
	};

	tokio::time::timeout(timeout, work).await?
}

/// Scan a buffer for a (possible) start of the preamble.
fn scan_preample_start(input: &[u8]) -> usize {
	for i in 0..input.len() {
		if input[i..].iter().zip(&PREAMBLE).all(|(a, b)| a == b) {
			return i;
		}
	}
	input.len()
}

pub struct Packets {
	data: Vec<u8>,
	packets: Vec<std::ops::Range<usize>>,
}

impl Packets {
	pub fn from_data(data: Vec<u8>) -> Result<Self, Error> {
		let mut packets = Vec::new();
		let mut index = 0;
		while index < data.len() {
			let packet_len = data.get(index..index + 4).ok_or(Error::InvalidMessagePayload {
				reason: "malformed packet length",
			})?;
			let packet_len: usize = u32::from_le_bytes(packet_len.try_into().unwrap())
				.try_into()
				.expect("u32 should always fit in usize");

			if data.len() < index + 4 + packet_len {
				return Err(Error::InvalidMessagePayload {
					reason: "missing packet data",
				});
			}
			packets.push(index + 4..index + 4 + packet_len);
			index += 4 + packet_len;
		}

		Ok(Self { data, packets })
	}

	pub fn is_empty(&self) -> bool {
		self.packets.is_empty()
	}

	pub fn len(&self) -> usize {
		self.packets.len()
	}

	pub fn iter(&self) -> PacketsIterator<'_> {
		PacketsIterator {
			data: &self.data,
			packets: self.packets.iter(),
		}
	}
}

impl<'a> std::iter::IntoIterator for &'a Packets {
	type Item = &'a [u8];
	type IntoIter = PacketsIterator<'a>;

	fn into_iter(self) -> Self::IntoIter {
		self.iter()
	}
}

pub struct PacketsIterator<'a> {
	data: &'a [u8],
	packets: std::slice::Iter<'a, std::ops::Range<usize>>,
}

impl<'a> std::iter::Iterator for PacketsIterator<'a> {
	type Item = &'a [u8];

	fn next(&mut self) -> Option<Self::Item> {
		let packet = self.packets.next()?;
		Some(&self.data[packet.start..packet.end])
	}

	fn nth(&mut self, n: usize) -> Option<Self::Item> {
		let packet = self.packets.nth(n)?;
		Some(&self.data[packet.start..packet.end])
	}

	fn count(self) -> usize
	where
		Self: Sized,
	{
		self.len()
	}

	fn size_hint(&self) -> (usize, Option<usize>) {
		self.packets.size_hint()
	}
}

impl<'a> std::iter::DoubleEndedIterator for PacketsIterator<'a> {
	fn next_back(&mut self) -> Option<Self::Item> {
		let packet = self.packets.next_back()?;
		Some(&self.data[packet.start..packet.end])
	}

	fn nth_back(&mut self, n: usize) -> Option<Self::Item> {
		let packet = self.packets.nth_back(n)?;
		Some(&self.data[packet.start..packet.end])
	}
}

impl<'a> std::iter::ExactSizeIterator for PacketsIterator<'a> {
	fn len(&self) -> usize {
		self.packets.len()
	}
}
