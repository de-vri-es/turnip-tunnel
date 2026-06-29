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
	/// Payload of outgoing message is too large.
	MessagePayloadTooLarge { actual: usize, max_payload_size: usize },
	/// Payload of incoming message is too large (the buffer is full before we found the end).
	BufferFull { max_payload_size: usize },
}

impl std::fmt::Display for Error {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::SerialPort(e) => write!(f, "I/O error on serial port: {e}"),
			Self::TimeoutElapsed => write!(f, "timeout elapsed"),
			Self::InvalidMessagePayload { reason } => {
				write!(f, "invalid message payload: {reason}")
			}
			Self::MessagePayloadTooLarge { actual, max_payload_size } => write!(f, "message payload too large: {actual} bytes, maximum allowed is {max_payload_size}"),
			Self::BufferFull { max_payload_size } => write!(f, "incoming message too large: receive buffer is already full ({max_payload_size} bytes)"),
		}
	}
}

impl From<tokio::time::error::Elapsed> for Error {
	fn from(_: tokio::time::error::Elapsed) -> Self {
		Self::TimeoutElapsed
	}
}

#[tracing::instrument(skip(channel, packets))]
pub async fn send_packets(channel: &mut SerialPort, packets: &[u8], timeout: Duration, max_payload_size: usize) -> Result<(), Error> {
	if packets.len() > max_payload_size {
		return Err(Error::MessagePayloadTooLarge {
			actual: packets.len(),
			max_payload_size,
		});
	}

	let mut stuffed = Vec::new();
	corncobs::encode(packets, &mut stuffed);

	let work = async {
		tracing::trace!(
			"Sending message with payload of {} bytes ({} after byte-stuffing)",
			packets.len(),
			stuffed.len(),
		);

		let mut slices = [
			IoSlice::new(&PREAMBLE),
			IoSlice::new(&stuffed),
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
pub async fn read_packets(channel: &mut SerialPort, timeout: Duration, max_payload_size: usize) -> Result<Packets, Error> {
	let work = async {
		let mut buffer = vec![0; corncobs::max_encoded_len(max_payload_size)];
		let mut filled = 0;

		let message_end = loop {
			if filled == buffer.len() {
				tracing::trace!("Receive buffer is full, discarding input buffer and raising an error.");
				channel.discard_input_buffer().map_err(Error::SerialPort)?;
				return Err(Error::BufferFull { max_payload_size });
			}

			let read_bytes = channel.read(&mut buffer[filled..]).await.map_err(Error::SerialPort)?;
			if read_bytes == 0 {
				// NOTE: Reading 0 is no guarantee you will always read 0.
				// But at the moment, in practise, for [`serial2_tokio::SerialPort`], it does mean that.
				return Err(Error::SerialPort(std::io::ErrorKind::UnexpectedEof.into()));
			}
			tracing::trace!("Read {read_bytes} bytes: 0x{:02X?}", &buffer[filled..][..read_bytes]);
			filled += read_bytes;
			let preamble_offset = scan_preamble_start(&buffer[..filled]);
			if preamble_offset != 0 {
				tracing::warn!("Discarding {preamble_offset} garbage bytes");
				buffer.copy_within(preamble_offset..filled, 0);
				filled -= preamble_offset;
			}

			// If we don't have a full message after removing garbage, try to read more data.
			if filled < 5 {
				continue;
			}

			let scan_from = filled.saturating_sub(read_bytes).clamp(PREAMBLE.len(), usize::MAX);
			// Find the terminating 0 byte of the message, or continue to read more data.
			match buffer[scan_from..filled].iter().position(|&x| x == 0) {
				Some(i) => break scan_from + i + 1, // + 1 to include the zero byte itself
				None => continue,
			};
		};

		if filled > message_end {
			tracing::warn!("Discarding {} trailing garbage bytes", filled - message_end);
		}

		let stuffed_payload = &buffer[PREAMBLE.len()..message_end];
		tracing::trace!("Received stuffed payload of {} bytes", stuffed_payload.len());

		let mut unstuffed_payload = Vec::new();
		corncobs::decode(stuffed_payload, &mut unstuffed_payload)
			.map_err(|e| match e {
				corncobs::CobsError::Truncated => Error::InvalidMessagePayload { reason: "incomplete COBS payload" },
				corncobs::CobsError::Corrupt => unreachable!("the payload was terminated at the first zero byte, so it can not contain one"),
			})?;

		Packets::from_payload(unstuffed_payload)
	};

	tokio::time::timeout(timeout, work).await?
}

/// Scan a buffer for a (possible) start of the preamble.
fn scan_preamble_start(input: &[u8]) -> usize {
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
	/// Take ownership of a buffer and parse it as a payload containing packets.
	fn from_payload(data: Vec<u8>) -> Result<Self, Error> {
		let mut packets = Vec::new();
		let mut index = 0;
		while index < data.len() {
			let (packet_len, rest) = data[index..].split_first_chunk()
				.ok_or(Error::InvalidMessagePayload {
					reason: "malformed packet length",
				})?;
			let packet_len = u16::from_le_bytes(*packet_len);

			if rest.len() < packet_len.into() {
				return Err(Error::InvalidMessagePayload {
					reason: "missing packet data",
				});
			}
			let packet_start = index + std::mem::size_of_val(&packet_len);
			let packet_end = packet_start + usize::from(packet_len);
			packets.push(packet_start..packet_end);
			index = packet_end;
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

impl std::fmt::Debug for Packets {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_tuple("Packets")
			.field(&std::fmt::from_fn(|f| {
				let mut list = f.debug_list();
				for packet in self {
					list.entry(&format_args!("0x{packet:02X?}"));
				}
				list.finish()
			}))
			.finish()
	}
}

impl<'a> std::iter::IntoIterator for &'a Packets {
	type IntoIter = PacketsIterator<'a>;
	type Item = &'a [u8];

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
	const MAX_PAYLOAD_SIZE: usize = 516;

	/// Build a payload containing the given packets (without message header).
	fn encode_packets(packets: &[&[u8]]) -> Vec<u8> {
		let mut payload = Vec::new();
		for packet in packets {
			payload.extend_from_slice(&(packet.len() as u16).to_le_bytes());
			payload.extend_from_slice(packet);
		}
		payload
	}

	/// Build a full message containing the given packets.
	fn encode_message(packets: &[&[u8]]) -> Vec<u8> {
		let mut message = Vec::new();
		message.extend_from_slice(&PREAMBLE);
		corncobs::encode(&encode_packets(packets), &mut message);
		message
	}

	#[test]
	#[tracing_test::traced_test]
	fn parse_empty_payload() {
		assert!(let Ok(packets) = Packets::from_payload(vec![]));
		assert!(packets.is_empty());
	}

	#[test]
	#[tracing_test::traced_test]
	fn parse_single_packet() {
		let payload = encode_packets(&[b"hello"]);
		assert!(let Ok(packets) = Packets::from_payload(payload));
		assert!(packets.to_vec() == &[b"hello".as_slice()]);
	}

	#[test]
	#[tracing_test::traced_test]
	fn parse_multiple_packets() {
		let payload = encode_packets(&[b"aaa", b"bb", b"c"]);
		assert!(let Ok(packets) = Packets::from_payload(payload));
		assert!(packets.to_vec() == &["aaa".as_bytes(), "bb".as_bytes(), "c".as_bytes()]);
	}

	#[test]
	#[tracing_test::traced_test]
	fn parse_zero_length_packet() {
		let payload = encode_packets(&[b""]);
		assert!(let Ok(packets) = Packets::from_payload(payload));
		assert!(packets.to_vec() == &[b""]);
	}

	#[test]
	#[tracing_test::traced_test]
	fn parse_truncated_packet_length() {
		// Only 1 byte where a 2-byte length header is expected.
		let mut payload = encode_packets(&[b""]);
		payload.truncate(payload.len() - 1);
		assert!(let Err(Error::InvalidMessagePayload { .. }) = Packets::from_payload(payload));
	}

	#[test]
	#[tracing_test::traced_test]
	fn parse_truncated_packet_data() {
		// Header says 10 bytes, but only 3 are present.
		let mut data = encode_packets(&[b"0123456789"]);
		data.truncate(data.len() - 7);
		assert!(let Err(Error::InvalidMessagePayload { .. }) = Packets::from_payload(data));
	}

	#[test]
	#[tracing_test::traced_test]
	fn scan_finds_preamble_at_start() {
		let input = [0x00, 0xFF, 0xFF, 0x01, 0xAA, 0xBB, 0xCC, 0xDD];
		assert!(scan_preamble_start(&input) == 0);
	}

	#[test]
	#[tracing_test::traced_test]
	fn scan_finds_preamble_after_garbage() {
		let input = [0xAA, 0xBB, 0x00, 0xFF, 0xFF, 0x01, 0xCC, 0xDD];
		assert!(scan_preamble_start(&input) == 2);
	}

	#[test]
	#[tracing_test::traced_test]
	fn scan_finds_partial_preamble_at_end() {
		// Only first 2 bytes of preamble at the end, still a valid start.
		let input = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0xFF];
		assert!(scan_preamble_start(&input) == 6);
	}

	#[test]
	#[tracing_test::traced_test]
	fn scan_returns_length_for_no_match() {
		let input = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x11, 0x22];
		assert!(scan_preamble_start(&input) == input.len());
	}

	#[tokio::test]
	#[tracing_test::traced_test]
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
	#[tracing_test::traced_test]
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
	#[tracing_test::traced_test]
	async fn read_rejects_oversized_payload() {
		assert!(let Ok((tx, mut rx)) = SerialPort::pair());

		let message = encode_message(&[b"very big"]);
		let max_payload_size = 7; // Smaller than "very big".

		assert!(let Ok(()) = tx.write_all(&message).await);

		assert!(let Err(Error::BufferFull { max_payload_size: 7 }) = read_packets(&mut rx, TIMEOUT, max_payload_size).await);
	}

	#[tokio::test]
	#[tracing_test::traced_test]
	async fn read_times_out_on_no_data() {
		assert!(let Ok((_tx, mut rx)) = SerialPort::pair());
		assert!(let Err(Error::TimeoutElapsed) = read_packets(&mut rx, TIMEOUT, MAX_PAYLOAD_SIZE).await);
	}

	#[tokio::test]
	#[tracing_test::traced_test]
	async fn roundtrip_empty_message() {
		assert!(let Ok((mut a, mut b)) = SerialPort::pair());
		assert!(let Ok(()) = send_packets(&mut a, &[], TIMEOUT, MAX_PAYLOAD_SIZE).await);

		assert!(let Ok(packets) = read_packets(&mut b, TIMEOUT, MAX_PAYLOAD_SIZE).await);
		assert!(packets.is_empty());
	}

	#[tokio::test]
	#[tracing_test::traced_test]
	async fn roundtrip_single_packet() {
		assert!(let Ok((mut a, mut b)) = SerialPort::pair());
		assert!(let Ok(()) = send_packets(&mut a, &encode_packets(&[b"roundtrip"]), TIMEOUT, MAX_PAYLOAD_SIZE).await);

		assert!(let Ok(packets) = read_packets(&mut b, TIMEOUT, MAX_PAYLOAD_SIZE).await);
		assert!(packets.to_vec() == &[b"roundtrip"]);
	}

	#[tokio::test]
	#[tracing_test::traced_test]
	async fn roundtrip_multiple_packets() {
		assert!(let Ok((mut a, mut b)) = SerialPort::pair());
		assert!(let Ok(()) = send_packets(&mut a, &encode_packets(&[b"alpha", b"beta", b"gamma"]), TIMEOUT, MAX_PAYLOAD_SIZE).await);

		assert!(let Ok(packets) = read_packets(&mut b, TIMEOUT, MAX_PAYLOAD_SIZE).await);
		assert!(packets.to_vec() == &["alpha".as_bytes(), "beta".as_bytes(), "gamma".as_bytes()]);
	}

	#[tokio::test]
	#[tracing_test::traced_test]
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
	#[tracing_test::traced_test]
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
