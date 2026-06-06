use serial2_tokio::SerialPort;
use std::io::IoSlice;
use std::time::Duration;
use tokio::io::AsyncReadExt as _;

pub const PREAMBLE: [u8; 4] = [0x00, 0xFF, 0xFF, 0x01];
pub const MAX_PAYLOAD_SIZE: usize = 65535;

#[derive(Debug)]
pub enum Error {
	SerialPort(std::io::Error),
	TimeoutElapsed,
	InvalidPreamble { actual: [u8; 4] },
	InvalidMessagePayload { reason: &'static str },
	MessagePayloadTooLarge(usize),
}

impl std::fmt::Display for Error {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::SerialPort(e) => write!(f, "I/O error on serial port: {e}"),
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

	/// Build a wire-format payload containing the given packets.
	fn encode_payload(packets: &[&[u8]]) -> Vec<u8> {
		let mut payload = Vec::new();
		for packet in packets {
			payload.extend_from_slice(&(packet.len() as u32).to_le_bytes());
			payload.extend_from_slice(packet);
		}
		payload
	}

	/// Build a complete wire-format frame (preamble + length + payload).
	fn encode_frame(payload: &[u8]) -> Vec<u8> {
		let mut frame = Vec::new();
		frame.extend_from_slice(&PREAMBLE);
		frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
		frame.extend_from_slice(payload);
		frame
	}


	#[test]
	fn parse_empty_message() {
		assert!(let Ok(packets) = Packets::from_data(vec![]));
		assert!(packets.is_empty());
	}

	#[test]
	fn parse_single_packet() {
		let payload = encode_payload(&[b"hello"]);
		assert!(let Ok(packets) = Packets::from_data(payload));
		assert!(packets.to_vec() == &[b"hello".as_slice()]);
	}

	#[test]
	fn parse_multiple_packets() {
		let payload = encode_payload(&[b"aaa", b"bb", b"c"]);
		assert!(let Ok(packets) = Packets::from_data(payload));
		assert!(packets.to_vec() == &["aaa".as_bytes(), "bb".as_bytes(), "c".as_bytes()]);
	}

	#[test]
	fn parse_zero_length_packet() {
		let payload = encode_payload(&[b""]);
		assert!(let Ok(packets) = Packets::from_data(payload));
		assert!(packets.to_vec() == &[b""]);
	}

	#[test]
	fn parse_truncated_packet_length() {
		// Only 2 bytes where a 4-byte length header is expected.
		assert!(let Err(Error::InvalidMessagePayload { .. }) = Packets::from_data(vec![0x01, 0x02]));
	}

	#[test]
	fn parse_truncated_packet_data() {
		// Header says 10 bytes, but only 3 are present.
		let mut data = encode_payload(&[b"0123456789"]);
		data.truncate(data.len() - 7);
		assert!(let Err(Error::InvalidMessagePayload { .. }) = Packets::from_data(data));
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
		let payload = encode_payload(&[b"data"]);
		let frame = encode_frame(&payload);

		// Write garbage followed by a valid frame.
		assert!(let Ok(()) = tx.write_all(b"garbage!").await);
		assert!(let Ok(()) = tx.write_all(&frame).await);

		assert!(let Ok(packets) = read_packets(&mut rx, TIMEOUT).await);
		assert!(packets.to_vec() == &[b"data".as_slice()]);
	}

	#[tokio::test]
	async fn read_skips_partial_preamble_in_garbage() {
		assert!(let Ok((tx, mut rx)) = SerialPort::pair());
		let payload = encode_payload(&[b"ok"]);
		let frame = encode_frame(&payload);

		// Write a partial preamble (first 2 bytes) as garbage, then a valid frame.
		assert!(let Ok(()) = tx.write_all(&PREAMBLE[..2]).await);
		assert!(let Ok(()) = tx.write_all(&frame).await);

		assert!(let Ok(packets) = read_packets(&mut rx, TIMEOUT).await);
		assert!(packets.to_vec() == &[b"ok".as_slice()]);
	}

	#[tokio::test]
	async fn read_rejects_oversized_payload() {
		assert!(let Ok((tx, mut rx)) = SerialPort::pair());

		// Craft a header with a payload size exceeding MAX_PAYLOAD_SIZE.
		let mut frame = Vec::new();
		frame.extend_from_slice(&PREAMBLE);
		let oversized = (MAX_PAYLOAD_SIZE as u32) + 1;
		frame.extend_from_slice(&oversized.to_le_bytes());
		assert!(let Ok(()) = tx.write_all(&frame).await);

		assert!(let Err(Error::MessagePayloadTooLarge(_)) = read_packets(&mut rx, TIMEOUT).await);
	}

	#[tokio::test]
	async fn read_times_out_on_no_data() {
		assert!(let Ok((_tx, mut rx)) = SerialPort::pair());

		assert!(let Err(Error::TimeoutElapsed) = read_packets(&mut rx, TIMEOUT).await);
	}


	#[tokio::test]
	async fn roundtrip_empty_message() {
		assert!(let Ok((mut a, mut b)) = SerialPort::pair());
		assert!(let Ok(()) = send_packets(&mut a, &[], TIMEOUT).await);

		assert!(let Ok(packets) = read_packets(&mut b, TIMEOUT).await);
		assert!(packets.is_empty());
	}

	#[tokio::test]
	async fn roundtrip_single_packet() {
		assert!(let Ok((mut a, mut b)) = SerialPort::pair());
		let payload = encode_payload(&[b"roundtrip"]);
		assert!(let Ok(()) = send_packets(&mut a, &payload, TIMEOUT).await);

		assert!(let Ok(packets) = read_packets(&mut b, TIMEOUT).await);
		assert!(packets.to_vec() == &[b"roundtrip"]);
	}

	#[tokio::test]
	async fn roundtrip_multiple_packets() {
		assert!(let Ok((mut a, mut b)) = SerialPort::pair());
		let payload = encode_payload(&[b"alpha", b"beta", b"gamma"]);
		assert!(let Ok(()) = send_packets(&mut a, &payload, TIMEOUT).await);

		assert!(let Ok(packets) = read_packets(&mut b, TIMEOUT).await);
		assert!(packets.to_vec() == &["alpha".as_bytes(), "beta".as_bytes(), "gamma".as_bytes()]);
	}

	#[tokio::test]
	async fn roundtrip_two_consecutive_messages() {
		assert!(let Ok((mut a, mut b)) = SerialPort::pair());

		assert!(let Ok(()) = send_packets(&mut a, &encode_payload(&[b"first"]), TIMEOUT).await);
		assert!(let Ok(()) = send_packets(&mut a, &encode_payload(&[b"second"]), TIMEOUT).await);

		assert!(let Ok(packets) = read_packets(&mut b, TIMEOUT).await);
		assert!(packets.to_vec() == &[b"first"]);

		assert!(let Ok(packets) = read_packets(&mut b, TIMEOUT).await);
		assert!(packets.to_vec() == &[b"second"]);
	}

	#[tokio::test]
	async fn recovery_after_garbage_between_messages() {
		assert!(let Ok((mut a, mut b)) = SerialPort::pair());

		// Send a valid message, then garbage, then another valid message.
		assert!(let Ok(()) = send_packets(&mut a, &encode_payload(&[b"first"]), TIMEOUT).await);
		assert!(let Ok(()) = a.write_all(b"GARBAGE").await);
		assert!(let Ok(()) = send_packets(&mut a, &encode_payload(&[b"second"]), TIMEOUT).await);

		assert!(let Ok(packets) = read_packets(&mut b, TIMEOUT).await);
		assert!(packets.to_vec() == &[b"first"]);

		assert!(let Ok(packets) = read_packets(&mut b, TIMEOUT).await);
		assert!(packets.to_vec() == &[b"second"]);
	}
}
