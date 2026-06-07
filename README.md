# turnip-tunnel

`turnip` is an IP or Ethernet tunnel using a half-duplex serial port without any control flow signals as the transport channel.

To allow IP over a half-duplex serial line, each side of the serial link takes turns to transmit packets to the other side.
Hence the name "turn IP", or "turnip".
Additionally, the half-duplex serial port was invented around the same time as the turnip, and they are about equally technologically advanced.

The principle of operation is also shown by this diagram:
<img src="https://raw.githubusercontent.com/de-vri-es/turnip-tunnel/main/principle-of-operation.svg" alt="diagram of two laptops connected to a root vegetable" width="100%"/>

## Feature comparison

|                                      | turnip (tunnel) | turnip (root vegetable) |
|--------------------------------------|-----------------|-------------------------|
| Full duplex communication            | ❌              | ❌                      |
| Forward error correction             | ❌              | ❌                      |
| Error detection                      | ❌ <sup>*</sup> | ❌                      |
| Automatic retransmissions            | ❌ <sup>*</sup> | ❌                      |
| Delicious                            | ❌              | ❌                      |
| Fits in a healthy diet               | ❌              | ✅                      |
| Useful when you have no alternative  | ✅              | ✅                      |

<sup>*</sup> But don't worry, the transport layer or the application layer will take care of this.

## Controller and worker

The tunnel works with `controller` and a `worker` side.
They are essentially identical, except that the controller initiates all communication.
The `worker` may only transmit in direct response to a message from the `controller`.
This is required to prevent message collisions on the half-duplex line.

Even though the `worker` can not initiate communication,
a packet may arrive from the tunnel interface to be transmitted over the serial port at any time.
To allow for this, the `controller` will regularly poll the `worker` to ask if any packets have arrived.
To minimize overhead, transmitting packets to the `worker` and polling it for packets are done with the same message.
If no packets need to be sent to the `worker`, the `controller` simply sends an empty list of packets.

The command line options of the controller and the `worker` are almost identical.
The `controller` only has one additional timeout parameter (`--poll-timeout`)
to determine how long it waits for a packet to arrive on the tunnel interface before polling the `worker` for a packet.
You should make sure that this timeout is *lower* than the read/write timeouts for the serial port on both ends,
otherwise the `worker` will encounter timeouts while waiting for the `controller` to send a message over the serial port.

Most options have sane defaults, although you may need to increase timeout values when using lower baud rates.
The only required options are the serial port (`--serial ...`) and the baud rate (`--baud`).

## Error detection and retransmissions

`turnip` focusses on simplicity over performance, and it sits below the transport layer (at L2 or L3, to be precise).
This means that it can leave most of the error detection and retransmission to the upper layers of the network, and it does.

To be precise, `turnip` only does its best to re-synchronize the start and end of a frame if anything happens.
It does not try to retransmit lost or damaged packets.

If this makes you worry, don't: this is perfectly normal in networking.
The transport layer is there precisely to allow the layers below it to be unreliable.

An argument could be made for adding forward error correction, if the serial link is very noisy.
Sacrificing bandwidth to reduce retransmission round-trips could result in a better throughput, and certainly better latency.
A future version of `turnip` may include forward error correction.

## Wire format

The wire format of `turnip` is very simple.
In a nutshell: each message starts with a preamble of `0x00 0xFF 0xFF 0x01`,
followed by a [COBS] (Consistent Overhead Byte Stuffing) encoded payload.
The COBS encoding ensures that there are no `0x00` bytes in the payload itself,
and it adds a `0x00` byte after the payload to mark the end of the message.

The payload itself is a packet list, which is the concatenation of a 16-bit packet length (little endian) and the packet data, for each packet.
Keep in mind, it has to be COBS encoded after the concatenation.

The following table shows the layout of a `turnip` message:

| Field           | Size (bytes) | Description                    |
|-----------------|--------------|--------------------------------|
| Preamble        | 4            | `0x00`, `0xFF`, `0xFF`, `0x01` |
| Payload         | Variable     | COBS encoded payload           |

And the layout of the payload (before COBS encoding) is shown by this table:

| Field           | Size (bytes) | Description                                    |
|-----------------|--------------|------------------------------------------------|
| Packet 1 length | 2            | The size in bytes of packet 1                  |
| Packet 1 data   | Variable     | The data of packet 1                           |
| ...             |              |                                                |
| Packet N length | 2            | The size in bytes of packet N                  |
| Packet N data   | Variable     | The data of packet N                           |

A packet list can contain any number of packets, including zero.
Note that COBS encoding still means that the minimum size of the payload is 2 bytes, even when the packet list is empty.

[COBS]: https://en.wikipedia.org/wiki/Consistent_Overhead_Byte_Stuffing

_Development sponsored by:_<br/>
[<img src="https://raw.githubusercontent.com/de-vri-es/turnip-tunnel/main/rocsys.svg" alt="ROCSYS B.V." width="200"/>](https://www.rocsys.com/)
