use input_event::{Event as InputEvent, KeyboardEvent, PointerEvent};
use num_enum::{IntoPrimitive, TryFromPrimitive, TryFromPrimitiveError};
use paste::paste;
use std::{
    fmt::{Debug, Display, Formatter},
    mem::size_of,
};
use thiserror::Error;

/// defines the maximum size an encoded event can take up
/// this is currently the pointer motion event
/// type: u8, time: u32, dx: f64, dy: f64
pub const MAX_EVENT_SIZE: usize = size_of::<u8>() + size_of::<u32>() + 2 * size_of::<f64>();

/// Fixed header size of an [`AudioFrame`] datagram:
/// `tag (u8) + seq (u32 BE) + sample_rate (u32 BE) + channels (u8)`.
pub const AUDIO_HEADER_SIZE: usize =
    size_of::<u8>() + size_of::<u32>() + size_of::<u32>() + size_of::<u8>();

/// Receive buffer size for DTLS datagrams so both control/input events
/// (`<= 21` bytes) and 5 ms raw PCM audio frames (`~970` bytes at 48 kHz
/// stereo `s16le`) fit comfortably.
pub const MAX_DATAGRAM_SIZE: usize = 2048;

/// error type for protocol violations
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// datagram has zero bytes
    #[error("empty datagram")]
    EmptyDatagram,
    /// event type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidEventId(#[from] TryFromPrimitiveError<EventType>),
    /// position type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidPosition(#[from] TryFromPrimitiveError<Position>),
    /// audio datagram routed to fixed-size ProtoEvent decoder or malformed
    #[error("invalid audio frame: {0}")]
    InvalidAudioFrame(&'static str),
}

/// Position of a client
#[derive(Clone, Copy, Debug, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

impl Display for Position {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

/// main lan-mouse protocol event type
#[derive(Clone, Copy, Debug)]
pub enum ProtoEvent {
    /// notify a client that the cursor entered its region at the given position.
    /// The `f64` is the normalized (`0.0..=1.0`) position along the crossed
    /// edge, e.g. how far down a `Left`/`Right` edge or how far across a
    /// `Top`/`Bottom` edge, so the receiving side can warp the cursor to
    /// the matching spot instead of a fixed point on the edge.
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Enter(Position, f64),
    /// notify a client that the cursor left its region. The `f64` is the
    /// normalized (`0.0..=1.0`) position along the edge this side's
    /// cursor should reappear at — set when the *other* side detected
    /// its own local crossing back (see the `EnterOnly` capture in
    /// `src/service.rs::add_incoming`), `0.5` for a plain release with
    /// no such crossing (release-bind, explicit release request, etc).
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Leave(u32, f64),
    /// acknowledge of an [`ProtoEvent::Enter`] or [`ProtoEvent::Leave`] event
    Ack(u32),
    /// Input event
    Input(InputEvent),
    /// Ping event for tracking unresponsive clients.
    /// A client has to respond with [`ProtoEvent::Pong`].
    Ping,
    /// Response to [`ProtoEvent::Ping`], true if emulation is enabled / available
    Pong(bool),
    /// Build identification for the sending peer. Sent by the
    /// connect side once after the connection authenticates, and
    /// echoed back by the listen side in reply, so each end can
    /// display the peer's build hash and warn (soft) on mismatch.
    /// `commit` is the 8-byte ASCII short commit hash from
    /// `shadow_rs`'s `SHORT_COMMIT`. Old peers that don't
    /// recognize the event type silently skip it per the
    /// forward-compat handling in the receive loop.
    Hello { commit: [u8; 8] },
}

impl Display for ProtoEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoEvent::Enter(s, t) => write!(f, "Enter({s}, {t:.2})"),
            ProtoEvent::Leave(s, t) => write!(f, "Leave({s}, {t:.2})"),
            ProtoEvent::Ack(s) => write!(f, "Ack({s})"),
            ProtoEvent::Input(e) => write!(f, "{e}"),
            ProtoEvent::Ping => write!(f, "ping"),
            ProtoEvent::Pong(alive) => {
                write!(
                    f,
                    "pong: {}",
                    if *alive { "alive" } else { "not available" }
                )
            }
            ProtoEvent::Hello { commit } => {
                let s = std::str::from_utf8(commit).unwrap_or("????????");
                write!(f, "Hello({s})")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum EventType {
    PointerMotion,
    PointerButton,
    PointerAxis,
    PointerAxisValue120,
    KeyboardKey,
    KeyboardModifiers,
    Ping,
    Pong,
    Enter,
    Leave,
    Ack,
    Hello,
    Audio,
}

impl ProtoEvent {
    fn event_type(&self) -> EventType {
        match self {
            ProtoEvent::Input(e) => match e {
                InputEvent::Pointer(p) => match p {
                    PointerEvent::Motion { .. } => EventType::PointerMotion,
                    PointerEvent::Button { .. } => EventType::PointerButton,
                    PointerEvent::Axis { .. } => EventType::PointerAxis,
                    PointerEvent::AxisDiscrete120 { .. } => EventType::PointerAxisValue120,
                },
                InputEvent::Keyboard(k) => match k {
                    KeyboardEvent::Key { .. } => EventType::KeyboardKey,
                    KeyboardEvent::Modifiers { .. } => EventType::KeyboardModifiers,
                },
            },
            ProtoEvent::Ping => EventType::Ping,
            ProtoEvent::Pong(_) => EventType::Pong,
            ProtoEvent::Enter(..) => EventType::Enter,
            ProtoEvent::Leave(..) => EventType::Leave,
            ProtoEvent::Ack(_) => EventType::Ack,
            ProtoEvent::Hello { .. } => EventType::Hello,
        }
    }
}

impl TryFrom<[u8; MAX_EVENT_SIZE]> for ProtoEvent {
    type Error = ProtocolError;

    fn try_from(buf: [u8; MAX_EVENT_SIZE]) -> Result<Self, Self::Error> {
        let mut buf = &buf[..];
        let event_type = decode_u8(&mut buf)?;
        match EventType::try_from(event_type)? {
            EventType::PointerMotion => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Motion {
                    time: decode_u32(&mut buf)?,
                    dx: decode_f64(&mut buf)?,
                    dy: decode_f64(&mut buf)?,
                })))
            }
            EventType::PointerButton => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Button {
                    time: decode_u32(&mut buf)?,
                    button: decode_u32(&mut buf)?,
                    state: decode_u32(&mut buf)?,
                })))
            }
            EventType::PointerAxis => Ok(Self::Input(InputEvent::Pointer(PointerEvent::Axis {
                time: decode_u32(&mut buf)?,
                axis: decode_u8(&mut buf)?,
                value: decode_f64(&mut buf)?,
            }))),
            EventType::PointerAxisValue120 => Ok(Self::Input(InputEvent::Pointer(
                PointerEvent::AxisDiscrete120 {
                    axis: decode_u8(&mut buf)?,
                    value: decode_i32(&mut buf)?,
                },
            ))),
            EventType::KeyboardKey => Ok(Self::Input(InputEvent::Keyboard(KeyboardEvent::Key {
                time: decode_u32(&mut buf)?,
                key: decode_u32(&mut buf)?,
                state: decode_u8(&mut buf)?,
            }))),
            EventType::KeyboardModifiers => Ok(Self::Input(InputEvent::Keyboard(
                KeyboardEvent::Modifiers {
                    depressed: decode_u32(&mut buf)?,
                    latched: decode_u32(&mut buf)?,
                    locked: decode_u32(&mut buf)?,
                    group: decode_u32(&mut buf)?,
                },
            ))),
            EventType::Ping => Ok(Self::Ping),
            EventType::Pong => Ok(Self::Pong(decode_u8(&mut buf)? != 0)),
            EventType::Enter => Ok(Self::Enter(
                decode_u8(&mut buf)?.try_into()?,
                decode_f64(&mut buf)?,
            )),
            EventType::Leave => Ok(Self::Leave(decode_u32(&mut buf)?, decode_f64(&mut buf)?)),
            EventType::Ack => Ok(Self::Ack(decode_u32(&mut buf)?)),
            EventType::Hello => {
                let mut commit = [0u8; 8];
                for b in commit.iter_mut() {
                    *b = decode_u8(&mut buf)?;
                }
                Ok(Self::Hello { commit })
            }
            EventType::Audio => Err(ProtocolError::InvalidAudioFrame(
                "audio datagrams must be decoded via AudioFrame::decode",
            )),
        }
    }
}

impl From<ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize) {
    fn from(event: ProtoEvent) -> Self {
        let mut buf = [0u8; MAX_EVENT_SIZE];
        let mut len = 0usize;
        {
            let mut buf = &mut buf[..];
            let buf = &mut buf;
            let len = &mut len;
            encode_u8(buf, len, event.event_type() as u8);
            match event {
                ProtoEvent::Input(event) => match event {
                    InputEvent::Pointer(p) => match p {
                        PointerEvent::Motion { time, dx, dy } => {
                            encode_u32(buf, len, time);
                            encode_f64(buf, len, dx);
                            encode_f64(buf, len, dy);
                        }
                        PointerEvent::Button {
                            time,
                            button,
                            state,
                        } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, button);
                            encode_u32(buf, len, state);
                        }
                        PointerEvent::Axis { time, axis, value } => {
                            encode_u32(buf, len, time);
                            encode_u8(buf, len, axis);
                            encode_f64(buf, len, value);
                        }
                        PointerEvent::AxisDiscrete120 { axis, value } => {
                            encode_u8(buf, len, axis);
                            encode_i32(buf, len, value);
                        }
                    },
                    InputEvent::Keyboard(k) => match k {
                        KeyboardEvent::Key { time, key, state } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, key);
                            encode_u8(buf, len, state);
                        }
                        KeyboardEvent::Modifiers {
                            depressed,
                            latched,
                            locked,
                            group,
                        } => {
                            encode_u32(buf, len, depressed);
                            encode_u32(buf, len, latched);
                            encode_u32(buf, len, locked);
                            encode_u32(buf, len, group);
                        }
                    },
                },
                ProtoEvent::Ping => {}
                ProtoEvent::Pong(alive) => encode_u8(buf, len, alive as u8),
                ProtoEvent::Enter(pos, t) => {
                    encode_u8(buf, len, pos as u8);
                    encode_f64(buf, len, t);
                }
                ProtoEvent::Leave(serial, t) => {
                    encode_u32(buf, len, serial);
                    encode_f64(buf, len, t);
                }
                ProtoEvent::Ack(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Hello { commit } => {
                    for b in commit.iter() {
                        encode_u8(buf, len, *b);
                    }
                }
            }
        }
        (buf, len)
    }
}

macro_rules! decode_impl {
    ($t:ty) => {
        paste! {
            fn [<decode_ $t>](data: &mut &[u8]) -> Result<$t, ProtocolError> {
                let (int_bytes, rest) = data.split_at(size_of::<$t>());
                *data = rest;
                Ok($t::from_be_bytes(int_bytes.try_into().unwrap()))
            }
        }
    };
}

decode_impl!(u8);
decode_impl!(u32);
decode_impl!(i32);
decode_impl!(f64);

macro_rules! encode_impl {
    ($t:ty) => {
        paste! {
            fn [<encode_ $t>](buf: &mut &mut [u8], amt: &mut usize, n: $t) {
                let src = n.to_be_bytes();
                let data = std::mem::take(buf);
                let (int_bytes, rest) = data.split_at_mut(size_of::<$t>());
                int_bytes.copy_from_slice(&src);
                *amt += size_of::<$t>();
                *buf = rest
            }
        }
    };
}

encode_impl!(u8);
encode_impl!(u32);
encode_impl!(i32);
encode_impl!(f64);

/// Variable-length raw PCM audio datagram sent over the DTLS session.
///
/// Wire layout:
/// - `byte 0`    : [`EventType::Audio`] (`= 12`)
/// - `byte 1..5` : `seq` (`u32` big-endian)
/// - `byte 5..9` : `sample_rate` (`u32` big-endian)
/// - `byte 9`    : `channels` (`u8`)
/// - `byte 10..` : interleaved `s16le` PCM samples
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioFrame {
    pub seq: u32,
    pub sample_rate: u32,
    pub channels: u8,
    pub samples: Vec<i16>,
}

impl AudioFrame {
    /// Encodes this audio frame into a reusable byte buffer, clearing `out`
    /// beforehand.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.clear();
        out.reserve(AUDIO_HEADER_SIZE + self.samples.len() * size_of::<i16>());
        out.push(EventType::Audio as u8);
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.sample_rate.to_be_bytes());
        out.push(self.channels);
        for &sample in &self.samples {
            out.extend_from_slice(&sample.to_le_bytes());
        }
    }

    /// Encodes this audio frame into a newly allocated byte vector.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(AUDIO_HEADER_SIZE + self.samples.len() * size_of::<i16>());
        self.encode_into(&mut out);
        out
    }

    /// Decodes a variable-length [`AudioFrame`] from a raw DTLS datagram slice.
    pub fn decode(buf: &[u8]) -> Result<Self, ProtocolError> {
        if buf.len() < AUDIO_HEADER_SIZE {
            return Err(ProtocolError::InvalidAudioFrame(
                "packet shorter than 10-byte audio header",
            ));
        }
        let event_type = EventType::try_from(buf[0])?;
        if event_type != EventType::Audio {
            return Err(ProtocolError::InvalidAudioFrame(
                "unexpected event tag for audio frame",
            ));
        }
        let seq = u32::from_be_bytes(buf[1..5].try_into().unwrap());
        let sample_rate = u32::from_be_bytes(buf[5..9].try_into().unwrap());
        let channels = buf[9];
        if sample_rate == 0 {
            return Err(ProtocolError::InvalidAudioFrame(
                "sample_rate must be non-zero",
            ));
        }
        if channels == 0 {
            return Err(ProtocolError::InvalidAudioFrame(
                "channels must be non-zero",
            ));
        }

        let pcm = &buf[AUDIO_HEADER_SIZE..];
        if !pcm.len().is_multiple_of(size_of::<i16>()) {
            return Err(ProtocolError::InvalidAudioFrame(
                "PCM payload byte length is not a multiple of 2",
            ));
        }
        let sample_count = pcm.len() / size_of::<i16>();
        if !sample_count.is_multiple_of(channels as usize) {
            return Err(ProtocolError::InvalidAudioFrame(
                "PCM sample count is not divisible by channel count",
            ));
        }

        let mut samples = Vec::with_capacity(sample_count);
        for chunk in pcm.chunks_exact(size_of::<i16>()) {
            samples.push(i16::from_le_bytes([chunk[0], chunk[1]]));
        }

        Ok(Self {
            seq,
            sample_rate,
            channels,
            samples,
        })
    }
}

/// Top-level DTLS datagram dispatcher distinguishing fixed-size control/input
/// events from variable-length audio frames.
#[derive(Clone, Debug)]
pub enum Datagram {
    Event(ProtoEvent),
    Audio(AudioFrame),
}

impl Datagram {
    pub fn decode(buf: &[u8]) -> Result<Self, ProtocolError> {
        let Some(&tag) = buf.first() else {
            return Err(ProtocolError::EmptyDatagram);
        };
        if tag == EventType::Audio as u8 {
            AudioFrame::decode(buf).map(Self::Audio)
        } else {
            let mut event_buf = [0u8; MAX_EVENT_SIZE];
            let copy_len = buf.len().min(MAX_EVENT_SIZE);
            event_buf[..copy_len].copy_from_slice(&buf[..copy_len]);
            ProtoEvent::try_from(event_buf).map(Self::Event)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_event_type_is_12() {
        assert_eq!(EventType::Audio as u8, 12);
    }

    #[test]
    fn audio_frame_round_trip_and_wire_layout() {
        let samples: Vec<i16> = (0..480).map(|i| (i * 137 - 32000) as i16).collect();
        let frame = AudioFrame {
            seq: 0x0102_0304,
            sample_rate: 48_000,
            channels: 2,
            samples: samples.clone(),
        };

        let wire = frame.encode();
        assert_eq!(wire.len(), 10 + 960);
        assert_eq!(wire[0], 12);
        assert_eq!(&wire[1..5], &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(&wire[5..9], &48_000u32.to_be_bytes());
        assert_eq!(wire[9], 2);
        assert_eq!(&wire[10..12], &samples[0].to_le_bytes());

        let decoded = AudioFrame::decode(&wire).expect("valid audio frame");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn audio_frame_rejects_malformed_inputs() {
        // Shorter than 10-byte header
        assert!(AudioFrame::decode(&[12, 0, 0, 0, 1]).is_err());

        // Zero sample rate
        let mut bad_rate = AudioFrame {
            seq: 1,
            sample_rate: 48_000,
            channels: 2,
            samples: vec![100, -100],
        }
        .encode();
        bad_rate[5..9].copy_from_slice(&0u32.to_be_bytes());
        assert!(AudioFrame::decode(&bad_rate).is_err());

        // Zero channels
        let mut bad_ch = AudioFrame {
            seq: 1,
            sample_rate: 48_000,
            channels: 2,
            samples: vec![100, -100],
        }
        .encode();
        bad_ch[9] = 0;
        assert!(AudioFrame::decode(&bad_ch).is_err());

        // Odd PCM byte length
        let mut odd = AudioFrame {
            seq: 1,
            sample_rate: 48_000,
            channels: 2,
            samples: vec![100, -100],
        }
        .encode();
        odd.push(0xAA);
        assert!(AudioFrame::decode(&odd).is_err());

        // Sample count not divisible by channels (3 samples for 2 channels)
        let mut misaligned = AudioFrame {
            seq: 1,
            sample_rate: 48_000,
            channels: 2,
            samples: vec![100, -100],
        }
        .encode();
        misaligned.extend_from_slice(&42i16.to_le_bytes());
        assert!(AudioFrame::decode(&misaligned).is_err());
    }

    #[test]
    fn datagram_dispatch_isolates_audio_and_proto_events() {
        // Audio datagram dispatches to Datagram::Audio and is rejected by ProtoEvent::try_from
        let audio = AudioFrame {
            seq: 77,
            sample_rate: 48_000,
            channels: 2,
            samples: (0..480).map(|i| (i * 19 - 4000) as i16).collect(),
        };
        let audio_wire = audio.encode();
        match Datagram::decode(&audio_wire).expect("datagram decode") {
            Datagram::Audio(decoded) => assert_eq!(decoded, audio),
            Datagram::Event(ev) => panic!("audio packet misdecoded as ProtoEvent: {ev}"),
        }

        let mut fixed = [0u8; MAX_EVENT_SIZE];
        let copy_len = audio_wire.len().min(MAX_EVENT_SIZE);
        fixed[..copy_len].copy_from_slice(&audio_wire[..copy_len]);
        assert!(ProtoEvent::try_from(fixed).is_err());

        // ProtoEvent datagrams dispatch to Datagram::Event and are rejected by AudioFrame::decode
        let events = [
            ProtoEvent::Ping,
            ProtoEvent::Pong(true),
            ProtoEvent::Enter(Position::Right, 0.25),
            ProtoEvent::Leave(42, 0.75),
            ProtoEvent::Hello {
                commit: *b"1234abcd",
            },
            ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Motion {
                time: 99,
                dx: -3.5,
                dy: 12.25,
            })),
        ];
        for ev in events {
            let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ev.into();
            let slice = &buf[..len];
            assert!(AudioFrame::decode(slice).is_err());
            match Datagram::decode(slice).expect("proto event decode") {
                Datagram::Event(decoded) => assert_eq!(format!("{decoded}"), format!("{ev}")),
                Datagram::Audio(_) => panic!("ProtoEvent misdecoded as AudioFrame"),
            }
        }

        // Empty and unknown event IDs fail cleanly
        assert!(Datagram::decode(&[]).is_err());
        assert!(Datagram::decode(&[250, 1, 2, 3]).is_err());
    }
}
