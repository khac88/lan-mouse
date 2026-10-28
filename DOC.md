# General Software Architecture

## Events

Each instance of lan-mouse can emit and receive events, where
an event is either a mouse or keyboard event for now.

The general Architecture is shown in the following flow chart:
```mermaid
graph TD
    A[Wayland Backend] -->|WaylandEvent| D{Input}
    B[X11 Backend] -->|X11Event| D{Input}
    C[Windows Backend] -->|WindowsEvent| D{Input}
    D -->|Abstract Event| E[Emitter]
    E -->|Udp Event| F[Receiver]
    F -->|Abstract Event| G{Dispatcher}
    G -->|Wayland Event| H[Wayland Backend]
    G -->|X11 Event| I[X11 Backend]
    G -->|Windows Event| J[Windows Backend]
```

### Input
The input component is responsible for translating inputs from a given backend
to a standardized format and passing them to the event emitter.

### Emitter
The event emitter serializes events and sends them over the network
to the correct client.

### Receiver
The receiver receives events over the network and deserializes them into
the standardized event format.

### Dispatcher
The dispatcher component takes events from the event receiver and passes them
to the correct backend corresponding to the type of client.


## Requests

// TODO this currently works differently

Aside from events, requests can be sent via a simple protocol.
For this, a simple tcp server is listening on the same port as the udp
event receiver and accepts requests for connecting to a device or to
request the keymap of a device.

```mermaid
sequenceDiagram
    Alice->>+Bob: Request Connection (secret)
    Bob-->>-Alice: Ack (Keyboard Layout)
```

## Problems
The general Idea is to have a bidirectional connection by default, meaning
any connected device can not only receive events but also send events back.

This way when connecting e.g. a PC to a Laptop, either device can be used
to control the other.

It needs to be ensured, that whenever a device is controlled the controlled
device does not transmit the events back to the original sender.
Otherwise events are multiplied and either one of the instances crashes.

To keep the implementation of input backends simple this needs to be handled
on the server level.

## Device State - Active and Inactive
To solve this problem, each device can be in exactly two states:

Either events are sent or received.

This ensures that
- a) Events can never result in a feedback loop.
- b) As soon as a virtual input enters another client, lan-mouse will stop receiving events,
which ensures clients can only be controlled directly and not indirectly through other clients.

## Audio Streaming (`[audio]`)

When built with `--features audio` (opt-in Cargo feature, off by default), `lan-mouse` can stream system output audio from a controlled Windows host back to the controlling peer over the existing authenticated DTLS UDP connection without installing virtual audio drivers:

- **Capture (Windows):** Captures default render-endpoint audio via WASAPI loopback (`cpal`), downmixes/clamps to at most 48 kHz stereo `s16le`, and slices audio into 5 ms chunks (`240` stereo frames = `960` bytes PCM + `10`-byte header = `970` bytes per DTLS datagram, `EventType::Audio = 12`). Automatically reopens after ~2 s when the default output endpoint changes or disconnects.
- **Playback (macOS / cross-platform `cpal`):** Decodes incoming `AudioFrame` datagrams into an adaptive jitter buffer (default `30` ms target, `80` ms overrun trim, missing-packet silence concealment, out-of-order recovery, `5` ms linear fade-in/out, and linear resampling if the output device rate differs), pausing the output unit after 5 s of inactivity.
- **Cursor-gated routing:** On the listen side, streaming starts on `ProtoEvent::Enter` for the most recently entered peer and stops on `ProtoEvent::Leave`, `EmulationRequest::Release`, peer timeout, or disconnect (unless `only_while_active = false`). Both peers must run matching builds with widened (`2048`-byte) DTLS receive buffers.

### Configuration

```toml
[audio]
send = false              # Windows listen side: capture WASAPI loopback and send
receive = false           # Connect side (e.g. macOS): play incoming AudioFrame packets
only_while_active = true  # Stream only while the remote cursor is inside this machine
buffer_ms = 30            # Receiver jitter buffer target latency in milliseconds
```

### Diagnostic Subcommands

```sh
lan-mouse test-audio-capture --duration-secs 5
lan-mouse test-audio-playback --duration-secs 2 --freq-hz 440
```


