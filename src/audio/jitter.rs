use std::collections::VecDeque;

pub const FRAME_DURATION_MS: u32 = 5;
pub const DEFAULT_BUFFER_MS: u32 = 30;
pub const DEFAULT_MAX_BUFFER_MS: u32 = 80;
pub const DEFAULT_FADE_MS: u32 = 5;

/// Maximum forward sequence gap (in 5 ms packets = 80 ms) treated as packet
/// loss requiring silence insertion. Larger jumps are treated as a stream
/// discontinuity / new burst rather than flooding the buffer with silence.
const MAX_GAP_PACKETS: u32 = 16;

#[inline]
pub fn f32_to_i16(sample: f32) -> i16 {
    if !sample.is_finite() {
        return 0;
    }
    let clamped = sample.clamp(-1.0, 1.0);
    if clamped >= 0.0 {
        (clamped * i16::MAX as f32).round() as i16
    } else {
        (clamped * -(i16::MIN as f32)).round() as i16
    }
}

#[inline]
pub fn i16_to_f32(sample: i16) -> f32 {
    sample as f32 / 32768.0
}

#[inline]
pub fn u16_to_f32(sample: u16) -> f32 {
    (sample as f32 - 32768.0) / 32768.0
}

#[inline]
pub fn i32_to_f32(sample: i32) -> f32 {
    (sample as f64 / 2_147_483_648.0) as f32
}

/// Captured 5 ms PCM chunk ready to be encoded onto the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PcmChunk {
    pub seq: u32,
    pub sample_rate: u32,
    pub channels: u8,
    pub samples: Vec<i16>,
}

/// Converts arbitrary device capture buffers into fixed 5 ms `s16` chunks
/// suitable for DTLS transport (at most stereo and at most 48 kHz so each 5 ms
/// datagram stays under ~1 KB / Tailscale 1280 MTU).
pub struct FrameChunker {
    in_sample_rate: u32,
    in_channels: usize,
    out_sample_rate: u32,
    out_channels: u8,
    samples_per_chunk: usize,
    pending: Vec<i16>,
    seq: u32,
    // Stateful linear downsampler state when in_sample_rate > out_sample_rate
    resample_pos: f64,
    prev_stereo: [f32; 2],
    has_prev: bool,
}

impl FrameChunker {
    pub fn new(in_sample_rate: u32, in_channels: u16) -> Self {
        let in_channels_usize = (in_channels as usize).max(1);
        let out_channels = in_channels.clamp(1, 2) as u8;
        let out_sample_rate = if in_sample_rate > 48_000 {
            48_000
        } else {
            in_sample_rate.max(8_000)
        };
        let frames_per_chunk =
            ((out_sample_rate as usize * FRAME_DURATION_MS as usize) / 1000).max(1);
        let samples_per_chunk = frames_per_chunk * out_channels as usize;

        Self {
            in_sample_rate: in_sample_rate.max(8_000),
            in_channels: in_channels_usize,
            out_sample_rate,
            out_channels,
            samples_per_chunk,
            pending: Vec::with_capacity(samples_per_chunk * 2),
            seq: 0,
            resample_pos: 0.0,
            prev_stereo: [0.0, 0.0],
            has_prev: false,
        }
    }

    pub fn out_sample_rate(&self) -> u32 {
        self.out_sample_rate
    }

    pub fn out_channels(&self) -> u8 {
        self.out_channels
    }

    /// Discard any partial frame state left over before a pause so a resumed
    /// capture session starts cleanly on fresh samples.
    pub fn reset(&mut self) {
        self.pending.clear();
        self.resample_pos = 0.0;
        self.prev_stereo = [0.0, 0.0];
        self.has_prev = false;
    }

    /// Push interleaved `f32` samples from the capture callback and emit any
    /// completed 5 ms chunks.
    pub fn push_f32(&mut self, data: &[f32], mut on_chunk: impl FnMut(PcmChunk)) {
        if self.in_channels <= 2 && self.in_sample_rate == self.out_sample_rate {
            // Fast path (e.g. 48 kHz or 44.1 kHz mono/stereo): direct conversion.
            for &s in data {
                self.pending.push(f32_to_i16(s));
                if self.pending.len() == self.samples_per_chunk {
                    self.emit(&mut on_chunk);
                }
            }
            return;
        }

        let in_frames = data.len() / self.in_channels;
        let step = self.in_sample_rate as f64 / self.out_sample_rate as f64;

        for frame_idx in 0..in_frames {
            let base = frame_idx * self.in_channels;
            let frame = &data[base..base + self.in_channels];
            let curr = downmix_to_stereo(frame);

            if self.in_sample_rate == self.out_sample_rate {
                self.push_adapted_frame(curr, &mut on_chunk);
                continue;
            }

            if !self.has_prev {
                self.prev_stereo = curr;
                self.has_prev = true;
                self.push_adapted_frame(curr, &mut on_chunk);
                self.resample_pos = step;
                continue;
            }

            // Advance through fractional positions landing in (0.0, 1.0] between prev and curr.
            while self.resample_pos <= 1.0 {
                let t = self.resample_pos as f32;
                let interp = [
                    self.prev_stereo[0] + (curr[0] - self.prev_stereo[0]) * t,
                    self.prev_stereo[1] + (curr[1] - self.prev_stereo[1]) * t,
                ];
                self.push_adapted_frame(interp, &mut on_chunk);
                self.resample_pos += step;
            }
            self.resample_pos -= 1.0;
            self.prev_stereo = curr;
        }
    }

    pub fn push_i16(&mut self, data: &[i16], mut on_chunk: impl FnMut(PcmChunk)) {
        if self.in_channels <= 2 && self.in_sample_rate == self.out_sample_rate {
            for &s in data {
                self.pending.push(s);
                if self.pending.len() == self.samples_per_chunk {
                    self.emit(&mut on_chunk);
                }
            }
            return;
        }
        let floats: Vec<f32> = data.iter().copied().map(i16_to_f32).collect();
        self.push_f32(&floats, on_chunk);
    }

    fn push_adapted_frame(&mut self, stereo: [f32; 2], on_chunk: &mut impl FnMut(PcmChunk)) {
        if self.out_channels == 1 {
            self.pending.push(f32_to_i16(stereo[0]));
        } else {
            self.pending.push(f32_to_i16(stereo[0]));
            self.pending.push(f32_to_i16(stereo[1]));
        }
        if self.pending.len() == self.samples_per_chunk {
            self.emit(on_chunk);
        }
    }

    fn emit(&mut self, on_chunk: &mut impl FnMut(PcmChunk)) {
        let mut samples = Vec::with_capacity(self.samples_per_chunk);
        samples.extend_from_slice(&self.pending);
        self.pending.clear();
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        on_chunk(PcmChunk {
            seq,
            sample_rate: self.out_sample_rate,
            channels: self.out_channels,
            samples,
        });
    }
}

/// Downmixes a multi-channel frame (mono, stereo, 5.1, 7.1, ...) to `[L, R]`.
#[inline]
fn downmix_to_stereo(frame: &[f32]) -> [f32; 2] {
    const SURROUND_GAIN: f32 = std::f32::consts::FRAC_1_SQRT_2;
    match frame.len() {
        0 => [0.0, 0.0],
        1 => [frame[0], frame[0]],
        2 => [frame[0], frame[1]],
        // Standard WAVEFORMATEXTENSIBLE channel order:
        // 0: FL, 1: FR, 2: FC, 3: LFE, 4: BL, 5: BR, 6: SL, 7: SR
        _ => {
            let mut l = frame[0];
            let mut r = frame[1];
            if frame.len() > 2 {
                let center = frame[2] * SURROUND_GAIN;
                l += center;
                r += center;
            }
            if frame.len() >= 6 {
                l += frame[4] * SURROUND_GAIN;
                r += frame[5] * SURROUND_GAIN;
            }
            if frame.len() >= 8 {
                l += frame[6] * SURROUND_GAIN;
                r += frame[7] * SURROUND_GAIN;
            }
            [l.clamp(-1.0, 1.0), r.clamp(-1.0, 1.0)]
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlayoutState {
    /// Waiting until buffered frames reach `target_frames` before starting/resuming.
    Buffering,
    /// Actively draining frames from the queue.
    Playing,
}

#[derive(Clone, Copy, Debug)]
pub struct JitterConfig {
    pub target_ms: u32,
    pub max_ms: u32,
    pub fade_ms: u32,
}

impl Default for JitterConfig {
    fn default() -> Self {
        Self::with_target_ms(DEFAULT_BUFFER_MS)
    }
}

impl JitterConfig {
    pub fn with_target_ms(target_ms: u32) -> Self {
        let target_ms = target_ms.clamp(5, 500);
        let max_ms = if target_ms <= DEFAULT_BUFFER_MS {
            DEFAULT_MAX_BUFFER_MS
        } else {
            (target_ms * 8 / 3).max(DEFAULT_MAX_BUFFER_MS)
        };
        Self {
            target_ms,
            max_ms,
            fade_ms: DEFAULT_FADE_MS,
        }
    }
}

/// Pure, I/O-free adaptive jitter buffer for incoming PCM packets.
///
/// Handles:
/// - Priming on startup / underrun until `target_ms` is buffered.
/// - Overrun trimming back to `target_ms` when buffered audio exceeds `max_ms`.
/// - Missing packet concealment (inserts exact silence for skipped `seq`s).
/// - Out-of-order recovery (fills silence placeholders in-place if not yet played)
///   and duplicate suppression.
/// - Linear fade-in and fade-out (`fade_ms`) to prevent clicks on start/stop.
pub struct JitterBuffer {
    cfg: JitterConfig,
    sample_rate: u32,
    channels: u8,
    target_frames: usize,
    max_frames: usize,
    fade_frames: usize,
    /// Samples per packet in the current burst (used to locate out-of-order slots).
    packet_samples: usize,
    queue: VecDeque<i16>,
    expected_seq: Option<u32>,
    /// Bit `k` (`0..64`) is 1 iff packet `(expected_seq - 1 - k)` was actually received.
    recent_mask: u64,
    state: PlayoutState,
    gain: f32,
}

impl JitterBuffer {
    pub fn new(sample_rate: u32, channels: u8, cfg: JitterConfig) -> Self {
        let mut jb = Self {
            cfg,
            sample_rate: 48_000,
            channels: 2,
            target_frames: 0,
            max_frames: 0,
            fade_frames: 0,
            packet_samples: 0,
            queue: VecDeque::new(),
            expected_seq: None,
            recent_mask: 0,
            state: PlayoutState::Buffering,
            gain: 0.0,
        };
        jb.reconfigure_format(sample_rate, channels);
        jb
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }

    pub fn buffered_frames(&self) -> usize {
        self.queue.len() / self.channels as usize
    }

    pub fn is_playing(&self) -> bool {
        self.state == PlayoutState::Playing
    }

    pub fn set_target_ms(&mut self, target_ms: u32) {
        self.cfg = JitterConfig {
            fade_ms: self.cfg.fade_ms,
            ..JitterConfig::with_target_ms(target_ms)
        };
        self.update_thresholds();
    }

    fn reconfigure_format(&mut self, sample_rate: u32, channels: u8) {
        self.sample_rate = sample_rate.max(1);
        self.channels = channels.max(1);
        self.update_thresholds();
        let cap_frames = self.max_frames + (self.sample_rate as usize * 40) / 1000;
        let cap_samples = cap_frames * self.channels as usize;
        self.queue.clear();
        if self.queue.capacity() < cap_samples {
            self.queue.reserve(cap_samples - self.queue.capacity());
        }
        self.packet_samples = 0;
        self.expected_seq = None;
        self.recent_mask = 0;
        self.state = PlayoutState::Buffering;
        self.gain = 0.0;
    }

    fn update_thresholds(&mut self) {
        self.target_frames =
            ((self.sample_rate as usize * self.cfg.target_ms as usize) / 1000).max(1);
        self.max_frames = ((self.sample_rate as usize * self.cfg.max_ms as usize) / 1000)
            .max(self.target_frames + 1);
        self.fade_frames = (self.sample_rate as usize * self.cfg.fade_ms as usize) / 1000;
    }

    /// Push a decoded audio packet into the jitter buffer.
    pub fn push(&mut self, seq: u32, sample_rate: u32, channels: u8, samples: &[i16]) {
        if sample_rate == 0 || channels == 0 || samples.is_empty() {
            return;
        }
        let ch = channels as usize;
        // Align to whole frames so channel ordering can never shift.
        let aligned_len = (samples.len() / ch) * ch;
        if aligned_len == 0 {
            return;
        }
        let samples = &samples[..aligned_len];

        if sample_rate != self.sample_rate || channels != self.channels {
            self.reconfigure_format(sample_rate, channels);
        }

        if self.packet_samples != samples.len() {
            // Packet size changed; reset sequence slot tracking cleanly.
            self.packet_samples = samples.len();
            self.expected_seq = None;
            self.recent_mask = 0;
        }

        match self.expected_seq {
            None => {
                self.queue.extend(samples.iter().copied());
                self.expected_seq = Some(seq.wrapping_add(1));
                self.recent_mask = 1;
            }
            Some(expected) if seq == expected => {
                self.queue.extend(samples.iter().copied());
                self.expected_seq = Some(seq.wrapping_add(1));
                self.recent_mask = (self.recent_mask << 1) | 1;
            }
            Some(expected) => {
                let forward_diff = seq.wrapping_sub(expected);
                let behind = expected.wrapping_sub(seq);

                if (1..=MAX_GAP_PACKETS).contains(&forward_diff) {
                    // Missing packet(s): conceal with exact silence length.
                    let missing_samples = forward_diff as usize * self.packet_samples;
                    self.queue
                        .extend(std::iter::repeat_n(0i16, missing_samples));
                    self.queue.extend(samples.iter().copied());
                    self.expected_seq = Some(seq.wrapping_add(1));
                    let shift = forward_diff + 1;
                    self.recent_mask = self.recent_mask.checked_shl(shift).unwrap_or(0) | 1;
                } else if (1..=64).contains(&behind) {
                    // Late or duplicate packet within the recent window.
                    let bit = 1u64 << (behind - 1);
                    if self.recent_mask & bit == 0 {
                        self.recent_mask |= bit;
                        let offset_from_back = behind as usize * self.packet_samples;
                        if self.queue.len() >= offset_from_back {
                            let start = self.queue.len() - offset_from_back;
                            for (idx, &s) in samples.iter().enumerate() {
                                self.queue[start + idx] = s;
                            }
                        }
                    }
                    return;
                } else {
                    // Large discontinuity (e.g. sender resumed after pause or restarted).
                    self.queue.extend(samples.iter().copied());
                    self.expected_seq = Some(seq.wrapping_add(1));
                    self.recent_mask = 1;
                }
            }
        }

        // Trim oldest samples back to target level if overrun exceeds max_frames.
        let buffered = self.buffered_frames();
        if buffered > self.max_frames {
            let drop_frames = buffered - self.target_frames;
            self.queue.drain(..drop_frames * ch);
        }
    }

    /// Pop one interleaved frame (up to 2 channels as `f32` in `[-1.0, 1.0]`).
    /// Returns `[0.0; 2]` during buffering / underrun.
    pub fn pop_stereo_frame_f32(&mut self) -> [f32; 2] {
        let mut raw = [0i16; 8];
        let ch = (self.channels as usize).min(raw.len());
        if !self.pop_raw_frame(&mut raw[..ch]) {
            return [0.0, 0.0];
        }
        if ch == 1 {
            let s = i16_to_f32(raw[0]);
            [s, s]
        } else {
            [i16_to_f32(raw[0]), i16_to_f32(raw[1])]
        }
    }

    /// Read interleaved `i16` samples at the buffer's own `(sample_rate, channels)`.
    pub fn read_i16(&mut self, out: &mut [i16]) {
        let ch = self.channels as usize;
        if ch == 0 {
            out.fill(0);
            return;
        }
        let frames = out.len() / ch;
        for f in 0..frames {
            let slice = &mut out[f * ch..(f + 1) * ch];
            if !self.pop_raw_frame(slice) {
                out[f * ch..].fill(0);
                return;
            }
        }
        let rem = frames * ch;
        if rem < out.len() {
            out[rem..].fill(0);
        }
    }

    fn pop_raw_frame(&mut self, out_frame: &mut [i16]) -> bool {
        let remaining_frames = self.buffered_frames();
        if self.state == PlayoutState::Buffering {
            if remaining_frames < self.target_frames {
                out_frame.fill(0);
                return false;
            }
            self.state = PlayoutState::Playing;
        }

        if remaining_frames == 0 {
            self.state = PlayoutState::Buffering;
            self.gain = 0.0;
            self.expected_seq = None;
            self.recent_mask = 0;
            out_frame.fill(0);
            return false;
        }

        let gain = if self.fade_frames == 0 {
            self.gain = 1.0;
            1.0
        } else {
            let step = 1.0 / self.fade_frames as f32;
            let next_gain = (self.gain + step).min(1.0);
            let g = if remaining_frames <= self.fade_frames {
                let ceiling = (remaining_frames - 1) as f32 * step;
                next_gain.min(ceiling)
            } else {
                next_gain
            };
            self.gain = g;
            g
        };

        for dst in out_frame.iter_mut() {
            let s = self.queue.pop_front().unwrap_or(0);
            *dst = if gain >= 1.0 {
                s
            } else {
                (s as f32 * gain).round() as i16
            };
        }
        true
    }
}

/// Allocation-free streaming linear resampler and channel mapper for pulling
/// from [`JitterBuffer`] when the output hardware runs at a different rate or
/// channel count than the sender.
pub struct LinearResampler {
    in_rate: u32,
    out_rate: u32,
    frac_pos: f64,
    prev: [f32; 2],
    curr: [f32; 2],
    initialized: bool,
}

impl LinearResampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            in_rate: in_rate.max(1),
            out_rate: out_rate.max(1),
            frac_pos: 0.0,
            prev: [0.0, 0.0],
            curr: [0.0, 0.0],
            initialized: false,
        }
    }

    pub fn set_rates(&mut self, in_rate: u32, out_rate: u32) {
        let in_rate = in_rate.max(1);
        let out_rate = out_rate.max(1);
        if self.in_rate != in_rate || self.out_rate != out_rate {
            self.in_rate = in_rate;
            self.out_rate = out_rate;
            self.frac_pos = 0.0;
            self.initialized = false;
        }
    }

    /// Fill `out` (`out_channels` interleaved `f32` samples) by pulling from `jb`.
    pub fn fill_f32(&mut self, jb: &mut JitterBuffer, out_channels: usize, out: &mut [f32]) {
        if out_channels == 0 {
            out.fill(0.0);
            return;
        }
        self.set_rates(jb.sample_rate(), self.out_rate);

        let frames = out.len() / out_channels;
        if self.in_rate == self.out_rate {
            for f in 0..frames {
                let stereo = jb.pop_stereo_frame_f32();
                write_channels(stereo, &mut out[f * out_channels..(f + 1) * out_channels]);
            }
            out[frames * out_channels..].fill(0.0);
            return;
        }

        let step = self.in_rate as f64 / self.out_rate as f64;
        if !self.initialized {
            self.prev = jb.pop_stereo_frame_f32();
            self.curr = jb.pop_stereo_frame_f32();
            self.frac_pos = 0.0;
            self.initialized = true;
        }

        for f in 0..frames {
            while self.frac_pos >= 1.0 {
                self.prev = self.curr;
                self.curr = jb.pop_stereo_frame_f32();
                self.frac_pos -= 1.0;
            }
            let t = self.frac_pos as f32;
            let stereo = [
                self.prev[0] + (self.curr[0] - self.prev[0]) * t,
                self.prev[1] + (self.curr[1] - self.prev[1]) * t,
            ];
            write_channels(stereo, &mut out[f * out_channels..(f + 1) * out_channels]);
            self.frac_pos += step;
        }
        out[frames * out_channels..].fill(0.0);
    }
}

#[inline]
fn write_channels(stereo: [f32; 2], dst_frame: &mut [f32]) {
    match dst_frame.len() {
        0 => {}
        1 => dst_frame[0] = 0.5 * (stereo[0] + stereo[1]),
        2 => {
            dst_frame[0] = stereo[0];
            dst_frame[1] = stereo[1];
        }
        _ => {
            dst_frame[0] = stereo[0];
            dst_frame[1] = stereo[1];
            dst_frame[2..].fill(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 1000; // 1 frame per ms keeps test math exact and readable
    const CH: u8 = 2;
    const PKT_FRAMES: usize = 5; // 5 ms packet
    const PKT_SAMPLES: usize = PKT_FRAMES * CH as usize; // 10 samples

    fn no_fade_cfg() -> JitterConfig {
        JitterConfig {
            target_ms: 30,
            max_ms: 80,
            fade_ms: 0,
        }
    }

    fn const_packet(val: i16) -> Vec<i16> {
        vec![val; PKT_SAMPLES]
    }

    #[test]
    fn underrun_outputs_silence_until_reprimed_to_target() {
        let mut jb = JitterBuffer::new(RATE, CH, no_fade_cfg());

        // Push 25 ms (< 30 ms target): must stay in Buffering and output silence.
        for seq in 0..5 {
            jb.push(seq, RATE, CH, &const_packet(1000));
        }
        let mut out = vec![99i16; PKT_SAMPLES];
        jb.read_i16(&mut out);
        assert!(out.iter().all(|&s| s == 0));
        assert!(!jb.is_playing());
        assert_eq!(jb.buffered_frames(), 25);

        // Push 6th packet -> 30 ms buffered == target -> starts playing!
        jb.push(5, RATE, CH, &const_packet(1000));
        jb.read_i16(&mut out);
        assert!(out.iter().all(|&s| s == 1000));
        assert!(jb.is_playing());

        // Drain the remaining 25 ms completely, plus one extra read -> triggers underrun.
        let mut rest = vec![0i16; 25 * CH as usize];
        jb.read_i16(&mut rest);
        assert!(rest.iter().all(|&s| s == 1000));

        jb.read_i16(&mut out);
        assert!(out.iter().all(|&s| s == 0));
        assert!(!jb.is_playing());

        // Push only 10 ms after underrun -> must NOT play early; waits for 30 ms again.
        jb.push(6, RATE, CH, &const_packet(2000));
        jb.push(7, RATE, CH, &const_packet(2000));
        jb.read_i16(&mut out);
        assert!(out.iter().all(|&s| s == 0));
        assert!(!jb.is_playing());

        for seq in 8..12 {
            jb.push(seq, RATE, CH, &const_packet(2000));
        }
        jb.read_i16(&mut out);
        assert!(out.iter().all(|&s| s == 2000));
        assert!(jb.is_playing());
    }

    #[test]
    fn overrun_trims_oldest_samples_back_to_target() {
        let mut jb = JitterBuffer::new(RATE, CH, no_fade_cfg());

        // Push 17 packets = 85 ms (> 80 ms max). Each packet carries `(seq + 1) * 100`
        // on L and `-(seq + 1) * 100` on R to verify channel alignment is preserved.
        for seq in 0..17u32 {
            let v = ((seq + 1) * 100) as i16;
            let mut pkt = Vec::with_capacity(PKT_SAMPLES);
            for _ in 0..PKT_FRAMES {
                pkt.push(v);
                pkt.push(-v);
            }
            jb.push(seq, RATE, CH, &pkt);
        }

        // Trimmed back to 30 ms (6 packets: seq 11..=16).
        assert_eq!(jb.buffered_frames(), 30);

        let mut out = vec![0i16; PKT_SAMPLES];
        jb.read_i16(&mut out);
        // Oldest surviving packet must be seq 11 -> v = 1200, with L=+1200, R=-1200.
        for frame in out.chunks_exact(2) {
            assert_eq!(frame, &[1200, -1200]);
        }
    }

    #[test]
    fn seq_gap_inserts_exact_silence_samples() {
        let mut jb = JitterBuffer::new(RATE, CH, no_fade_cfg());

        // seq 0, then jump to seq 3 (seq 1 and seq 2 missing = 10 frames of silence),
        // then seq 4, 5 to reach 30 ms target (6 packets worth of frames total).
        jb.push(0, RATE, CH, &const_packet(500));
        jb.push(3, RATE, CH, &const_packet(800));
        jb.push(4, RATE, CH, &const_packet(900));
        jb.push(5, RATE, CH, &const_packet(1000));

        assert_eq!(jb.buffered_frames(), 30);

        let mut all = vec![0i16; 30 * CH as usize];
        jb.read_i16(&mut all);

        assert!(all[0..10].iter().all(|&s| s == 500));
        assert!(all[10..30].iter().all(|&s| s == 0)); // 2 missing packets = 20 samples
        assert!(all[30..40].iter().all(|&s| s == 800));
        assert!(all[40..50].iter().all(|&s| s == 900));
        assert!(all[50..60].iter().all(|&s| s == 1000));
    }

    #[test]
    fn out_of_order_packet_recovers_placeholder_and_ignores_duplicates() {
        let mut jb = JitterBuffer::new(RATE, CH, no_fade_cfg());

        // Arrive out of order: 0, 2, 1, 3, plus duplicate 2, then 4, 5.
        jb.push(0, RATE, CH, &const_packet(100));
        jb.push(2, RATE, CH, &const_packet(300));
        jb.push(1, RATE, CH, &const_packet(200)); // fills seq 1 placeholder!
        jb.push(2, RATE, CH, &const_packet(9999)); // duplicate seq 2: must be ignored!
        jb.push(3, RATE, CH, &const_packet(400));
        jb.push(4, RATE, CH, &const_packet(500));
        jb.push(5, RATE, CH, &const_packet(600));

        assert_eq!(jb.buffered_frames(), 30);

        let mut all = vec![0i16; 30 * CH as usize];
        jb.read_i16(&mut all);
        assert!(all[0..10].iter().all(|&s| s == 100));
        assert!(all[10..20].iter().all(|&s| s == 200));
        assert!(all[20..30].iter().all(|&s| s == 300));
        assert!(all[30..40].iter().all(|&s| s == 400));
        assert!(all[40..50].iter().all(|&s| s == 500));
        assert!(all[50..60].iter().all(|&s| s == 600));
    }

    #[test]
    fn fade_in_and_fade_out_ramp_smoothly_without_clicks() {
        let cfg = JitterConfig {
            target_ms: 15,
            max_ms: 80,
            fade_ms: 5,
        };
        let mut jb = JitterBuffer::new(RATE, 1, cfg);
        // 15 ms at 1000 Hz mono = 15 samples (3 packets of 5 samples).
        for seq in 0..3 {
            jb.push(seq, RATE, 1, &[10_000; 5]);
        }

        let mut out = [0i16; 15];
        jb.read_i16(&mut out);

        // First 5 frames ramp up: 2000, 4000, 6000, 8000, 10000
        assert_eq!(&out[0..5], &[2000, 4000, 6000, 8000, 10_000]);
        // Middle 5 frames steady at full amplitude
        assert_eq!(&out[5..10], &[10_000; 5]);
        // Last 5 frames ramp down to exact 0 before underrun: 8000, 6000, 4000, 2000, 0
        assert_eq!(&out[10..15], &[8000, 6000, 4000, 2000, 0]);
    }

    #[test]
    fn chunker_downmixes_surround_and_resamples_96k_to_48k() {
        let mut chunker = FrameChunker::new(96_000, 8);
        assert_eq!(chunker.out_sample_rate(), 48_000);
        assert_eq!(chunker.out_channels(), 2);

        // 10 ms at 96 kHz = 960 frames of 8 channels -> should produce 2 chunks of 240 stereo frames.
        let input = vec![0.5f32; 960 * 8];
        let mut chunks = Vec::new();
        chunker.push_f32(&input, |c| chunks.push(c));
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].seq, 0);
        assert_eq!(chunks[0].samples.len(), 240 * 2);
        assert_eq!(chunks[1].seq, 1);
        assert_eq!(chunks[1].samples.len(), 240 * 2);
    }

    #[test]
    fn linear_resampler_adapts_rate_and_channels() {
        let mut jb = JitterBuffer::new(48_000, 2, no_fade_cfg());
        // Push 30 ms (6 packets of 240 stereo frames = 480 samples).
        for seq in 0..6 {
            jb.push(seq, 48_000, 2, &vec![16_384i16; 480]);
        }
        let mut resampler = LinearResampler::new(48_000, 44_100);
        // Pull 10 ms at 44.1 kHz mono (441 samples).
        let mut out = vec![0.0f32; 441];
        resampler.fill_f32(&mut jb, 1, &mut out);
        assert!(out.iter().all(|&s| (s - 0.5).abs() < 1e-3));
    }

    #[test]
    fn burst_drain_silence_then_restarted_seq_fades_in_cleanly() {
        let cfg = JitterConfig {
            target_ms: 15,
            max_ms: 80,
            fade_ms: 5,
        };
        let mut jb = JitterBuffer::new(RATE, 1, cfg);

        // First burst (seq 0..3 = 15 ms).
        for seq in 0..3 {
            jb.push(seq, RATE, 1, &[10_000; 5]);
        }
        let mut out = [0i16; 20];
        jb.read_i16(&mut out);
        assert_eq!(&out[0..5], &[2000, 4000, 6000, 8000, 10_000]);
        assert_eq!(&out[5..10], &[10_000; 5]);
        assert_eq!(&out[10..15], &[8000, 6000, 4000, 2000, 0]);
        assert_eq!(&out[15..20], &[0; 5]);
        assert!(!jb.is_playing());

        // Peer restarts or re-enters starting again at seq = 0: must not be
        // mistaken for duplicate packets, and must ramp smoothly from 0 again.
        for seq in 0..3 {
            jb.push(seq, RATE, 1, &[5_000; 5]);
        }
        let mut out2 = [0i16; 15];
        jb.read_i16(&mut out2);
        assert_eq!(&out2[0..5], &[1000, 2000, 3000, 4000, 5000]);
        assert_eq!(&out2[5..10], &[5000; 5]);
        assert_eq!(&out2[10..15], &[4000, 3000, 2000, 1000, 0]);
    }

    #[test]
    fn sub_buffer_pause_resume_with_seq_jump_never_drops_as_duplicate() {
        let mut jb = JitterBuffer::new(RATE, CH, no_fade_cfg());
        for seq in 0..6 {
            jb.push(seq, RATE, CH, &const_packet(1000));
        }
        // Drain only 15 ms, leaving 15 ms still queued.
        let mut half = vec![0i16; 15 * CH as usize];
        jb.read_i16(&mut half);
        assert_eq!(jb.buffered_frames(), 15);

        // Sender paused and resumed with a 128-packet sequence jump.
        jb.push(134, RATE, CH, &const_packet(2000));
        jb.push(135, RATE, CH, &const_packet(2000));
        assert_eq!(jb.buffered_frames(), 25);

        let mut drained = vec![0i16; 25 * CH as usize];
        jb.read_i16(&mut drained);
        assert!(drained[..30].iter().all(|&s| s == 1000));
        assert!(drained[30..].iter().all(|&s| s == 2000));
    }

    #[test]
    fn chunker_reset_discards_stale_partial_frame() {
        let mut chunker = FrameChunker::new(48_000, 2);
        // Push 100 samples (< 480 samples needed for a 5 ms stereo chunk).
        let mut chunks = Vec::new();
        chunker.push_i16(&[9999i16; 100], |c| chunks.push(c));
        assert!(chunks.is_empty());

        chunker.reset();
        chunker.push_i16(&[1234i16; 480], |c| chunks.push(c));
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].samples.iter().all(|&s| s == 1234));
    }
}
