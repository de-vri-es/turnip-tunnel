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
The `worker` may only transmit over the serial line after it received a message from the `controller`.
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

_Development sponsored by:_<br/>
[<img src="https://raw.githubusercontent.com/de-vri-es/turnip-tunnel/main/rocsys.svg" alt="ROCSYS B.V." width="200"/>](https://www.rocsys.com/)
