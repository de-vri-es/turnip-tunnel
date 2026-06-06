use serial2_tokio::SerialPort;
use std::io::IoSlice;
use std::time::Duration;
use tokio::io::AsyncReadExt as _;

pub const PREAMBLE: [u8; 4] = [0x00, 0xFF, 0xFF, 0x01];

#[derive(Debug)]
pub enum Error {
	SerialPort(std::io::Error),
	TimeoutElapsed,
	InvalidMessagePayload { reason: &'static str },
	MessagePayloadTooLarge { actual: usize, max: u32 },
}

impl std::fmt::Display for Error {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::SerialPort(e) => write!(f, "I/O error on serial port: {e}"),
			Self::TimeoutElapsed => write!(f, "timeout elapsed"),
			Self::InvalidMessagePayload { reason } => {
				write!(f, "invalid message payload: {reason}")
			}
			Self::MessagePayloadTooLarge { actual, max } => write!(f, "message payload too large: {actual} bytes, maximum allowed is {max}"),
		}
	}
}

impl From<tokio::time::error::Elapsed> for Error {
	fn from(_: tokio::time::error::Elapsed) -> Self {
		Self::TimeoutElapsed
	}
}

#[tracing::instrument(skip(channel, packets))]
pub async fn send_packets(channel: &mut SerialPort, packets: &[u8], timeout: Duration, max_payload_size: u32) -> Result<(), Error> {
	let work = async {
		if packets.len() > max_payload_size as usize {
			return Err(Error::MessagePayloadTooLarge { actual: packets.len(), max: max_payload_size });
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
pub async fn read_packets(channel: &mut SerialPort, timeout: Duration, max_payload_size: u32) -> Result<Packets, Error> {
	let work = async {
		// Read a header, discarding any non-preamble data.
		let mut buffer = vec![0; max_payload_size as usize];
		let mut filled = 0;
		loop {
			let this_read = channel.read(&mut buffer[filled..]).await.map_err(Error::SerialPort)?;
			tracing::trace!("Read {this_read} bytes: 0x{:02X?}", &buffer[filled..this_read]);
			filled += this_read;
			let preamble_offset = scan_preample_start(&buffer[..filled]);
			if preamble_offset != 0 {
				tracing::warn!("Discarding {preamble_offset} garbage bytes");
				buffer.copy_within(preamble_offset..filled, 0);
				filled -= preamble_offset;
			}

			// If we don't have a full header after removing garbage, try to read more data.
			if filled < 8 {
				continue;
			}

			let payload_size = u32::from_le_bytes(buffer[4..8].try_into().unwrap()) as usize;
			tracing::trace!("Received message header with payload of {payload_size} bytes");
			if payload_size > max_payload_size as usize {
				tracing::trace!("Incoming payload too large, refusing to parse, discarding input buffer.");
				channel.discard_input_buffer().map_err(Error::SerialPort)?;
				return Err(Error::MessagePayloadTooLarge { actual: payload_size, max: max_payload_size });
			}

			if filled >= 8 + payload_size {
				break;
			}
		}

		buffer.truncate(filled);
		Packets::from_message(buffer)
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

#[derive(Debug)]
pub struct Packets {
	data: Vec<u8>,
	packets: Vec<std::ops::Range<usize>>,
}

impl Packets {
	/// Take ownership of a buffer and parse it as a message containing packets.
	///
	/// The first 8 header bytes are skipped, but not checked for errors.
	fn from_message(data: Vec<u8>) -> Result<Self, Error> {
		let mut packets = Vec::new();
		let mut index = 8;
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

	pub fn to_vec(&self) -> Vec<&[u8]> {
		self.iter().collect()
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

#[cfg(test)]
mod tests {
	use assert2::assert;

	use super::*;

	const TIMEOUT: Duration = Duration::from_millis(50);
	const MAX_PAYLOAD_SIZE: u32 = 516;

	/// Build a payload containing the given packets (without message header).
	fn encode_packets(packets: &[&[u8]]) -> Vec<u8> {
		let mut payload = Vec::new();
		for packet in packets {
			payload.extend_from_slice(&(packet.len() as u32).to_le_bytes());
			payload.extend_from_slice(packet);
		}
		payload
	}

	/// Build a full message containing the given packets.
	fn encode_message(packets: &[&[u8]]) -> Vec<u8> {
		let mut message = Vec::new();
		message.extend_from_slice(&PREAMBLE);
		message.extend_from_slice(&[0, 0, 0, 0]);
		for packet in packets {
			message.extend_from_slice(&(packet.len() as u32).to_le_bytes());
			message.extend_from_slice(packet);
		}
		let payload_len = (message.len() - 8) as u32;
		message[4..8].copy_from_slice(&payload_len.to_le_bytes());
		message
	}

	#[test]
	fn parse_empty_message() {
		assert!(let Ok(packets) = Packets::from_message(vec![]));
		assert!(packets.is_empty());
	}

	#[test]
	fn parse_single_packet() {
		let message = encode_message(&[b"hello"]);
		assert!(let Ok(packets) = Packets::from_message(message));
		assert!(packets.to_vec() == &[b"hello".as_slice()]);
	}

	#[test]
	fn parse_multiple_packets() {
		let message = encode_message(&[b"aaa", b"bb", b"c"]);
		assert!(let Ok(packets) = Packets::from_message(message));
		assert!(packets.to_vec() == &["aaa".as_bytes(), "bb".as_bytes(), "c".as_bytes()]);
	}

	#[test]
	fn parse_zero_length_packet() {
		let message = encode_message(&[b""]);
		assert!(let Ok(packets) = Packets::from_message(message));
		assert!(packets.to_vec() == &[b""]);
	}

	#[test]
	fn parse_truncated_packet_length() {
		// Only 2 bytes where a 4-byte length header is expected.
		let mut message = encode_message(&[b""]);
		message.truncate(message.len() - 2);
		assert!(let Err(Error::InvalidMessagePayload { .. }) = Packets::from_message(message));
	}

	#[test]
	fn parse_truncated_packet_data() {
		// Header says 10 bytes, but only 3 are present.
		let mut data = encode_message(&[b"0123456789"]);
		data.truncate(data.len() - 7);
		assert!(let Err(Error::InvalidMessagePayload { .. }) = Packets::from_message(data));
	}


	#[test]
	fn scan_finds_preamble_at_start() {
		let input = [0x00, 0xFF, 0xFF, 0x01, 0xAA, 0xBB, 0xCC, 0xDD];
		assert!(scan_preample_start(&input) == 0);
	}

	#[test]
	fn scan_finds_preamble_after_garbage() {
		let input = [0xAA, 0xBB, 0x00, 0xFF, 0xFF, 0x01, 0xCC, 0xDD];
		assert!(scan_preample_start(&input) == 2);
	}

	#[test]
	fn scan_finds_partial_preamble_at_end() {
		// Only first 2 bytes of preamble at the end, still a valid start.
		let input = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0xFF];
		assert!(scan_preample_start(&input) == 6);
	}

	#[test]
	fn scan_returns_length_for_no_match() {
		let input = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x11, 0x22];
		assert!(scan_preample_start(&input) == input.len());
	}

	#[tokio::test]
	async fn read_skips_garbage_before_preamble() {
		assert!(let Ok((tx, mut rx)) = SerialPort::pair());
		let message = encode_message(&[b"data"]);

		// Write garbage followed by a valid message.
		assert!(let Ok(()) = tx.write_all(b"garbage!").await);
		assert!(let Ok(()) = tx.write_all(&message).await);

		assert!(let Ok(packets) = read_packets(&mut rx, TIMEOUT, MAX_PAYLOAD_SIZE).await);
		assert!(packets.to_vec() == &[b"data".as_slice()]);
	}

	#[tokio::test]
	async fn read_skips_partial_preamble_in_garbage() {
		assert!(let Ok((tx, mut rx)) = SerialPort::pair());
		let message = encode_message(&[b"ok"]);

		// Write a partial preamble (first 2 bytes) as garbage, then a valid message.
		assert!(let Ok(()) = tx.write_all(&PREAMBLE[..2]).await);
		assert!(let Ok(()) = tx.write_all(&message).await);

		assert!(let Ok(packets) = read_packets(&mut rx, TIMEOUT, MAX_PAYLOAD_SIZE).await);
		assert!(packets.to_vec() == &[b"ok".as_slice()]);
	}

	#[tokio::test]
	async fn read_rejects_oversized_payload() {
		assert!(let Ok((tx, mut rx)) = SerialPort::pair());

		// Craft a header with a payload size exceeding MAX_PAYLOAD_SIZE.
		let mut message = Vec::new();
		message.extend_from_slice(&PREAMBLE);
		let oversized = MAX_PAYLOAD_SIZE + 1;
		message.extend_from_slice(&oversized.to_le_bytes());
		assert!(let Ok(()) = tx.write_all(&message).await);

		assert!(let Err(Error::MessagePayloadTooLarge { actual, max }) = read_packets(&mut rx, TIMEOUT, MAX_PAYLOAD_SIZE).await);
		assert!(actual == (MAX_PAYLOAD_SIZE as usize) + 1);
		assert!(max == MAX_PAYLOAD_SIZE);
	}

	#[tokio::test]
	async fn read_times_out_on_no_data() {
		assert!(let Ok((_tx, mut rx)) = SerialPort::pair());
		assert!(let Err(Error::TimeoutElapsed) = read_packets(&mut rx, TIMEOUT, MAX_PAYLOAD_SIZE).await);
	}


	#[tokio::test]
	async fn roundtrip_empty_message() {
		assert!(let Ok((mut a, mut b)) = SerialPort::pair());
		assert!(let Ok(()) = send_packets(&mut a, &[], TIMEOUT, MAX_PAYLOAD_SIZE).await);

		assert!(let Ok(packets) = read_packets(&mut b, TIMEOUT, MAX_PAYLOAD_SIZE).await);
		assert!(packets.is_empty());
	}

	#[tokio::test]
	async fn roundtrip_single_packet() {
		assert!(let Ok((mut a, mut b)) = SerialPort::pair());
		assert!(let Ok(()) = send_packets(&mut a, &encode_packets(&[b"roundtrip"]), TIMEOUT, MAX_PAYLOAD_SIZE).await);

		assert!(let Ok(packets) = read_packets(&mut b, TIMEOUT, MAX_PAYLOAD_SIZE).await);
		assert!(packets.to_vec() == &[b"roundtrip"]);
	}

	#[tokio::test]
	async fn roundtrip_multiple_packets() {
		assert!(let Ok((mut a, mut b)) = SerialPort::pair());
		assert!(let Ok(()) = send_packets(&mut a, &encode_packets(&[b"alpha", b"beta", b"gamma"]), TIMEOUT, MAX_PAYLOAD_SIZE).await);

		assert!(let Ok(packets) = read_packets(&mut b, TIMEOUT, MAX_PAYLOAD_SIZE).await);
		assert!(packets.to_vec() == &["alpha".as_bytes(), "beta".as_bytes(), "gamma".as_bytes()]);
	}

	#[tokio::test]
	async fn roundtrip_two_consecutive_messages() {
		assert!(let Ok((mut a, mut b)) = SerialPort::pair());

		assert!(let Ok(()) = send_packets(&mut a, &encode_packets(&[b"first"]), TIMEOUT, MAX_PAYLOAD_SIZE).await);
		assert!(let Ok(packets) = read_packets(&mut b, TIMEOUT, MAX_PAYLOAD_SIZE).await);
		assert!(packets.to_vec() == &[b"first"]);

		assert!(let Ok(()) = send_packets(&mut a, &encode_packets(&[b"second"]), TIMEOUT, MAX_PAYLOAD_SIZE).await);
		assert!(let Ok(packets) = read_packets(&mut b, TIMEOUT, MAX_PAYLOAD_SIZE).await);
		assert!(packets.to_vec() == &[b"second"]);
	}

	#[tokio::test]
	async fn recovery_after_garbage_between_messages() {
		assert!(let Ok((mut a, mut b)) = SerialPort::pair());

		// Send a valid message, then garbage, then another valid message.
		assert!(let Ok(()) = send_packets(&mut a, &encode_packets(&[b"first"]), TIMEOUT, MAX_PAYLOAD_SIZE).await);
		assert!(let Ok(packets) = read_packets(&mut b, TIMEOUT, MAX_PAYLOAD_SIZE).await);
		assert!(packets.to_vec() == &[b"first"]);

		assert!(let Ok(()) = a.write_all(b"GARBAGE").await);
		assert!(let Ok(()) = send_packets(&mut a, &encode_packets(&[b"second"]), TIMEOUT, MAX_PAYLOAD_SIZE).await);
		assert!(let Ok(packets) = read_packets(&mut b, TIMEOUT, MAX_PAYLOAD_SIZE).await);
		assert!(packets.to_vec() == &[b"second"]);
	}
}
