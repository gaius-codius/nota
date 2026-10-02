//! Pause-cut chunks: where the live pass cuts a track's audio before it goes
//! to the recogniser.
//!
//! The chunker sees the audio and what the voice activity detector has
//! settled about it ([`Labels`]), and cuts:
//! 1. in the first pause of at least [`ChunkerConfig::min_pause`] whose
//!    middle is [`ChunkerConfig::target`] or more into the chunk, so text
//!    follows speech by a few seconds;
//! 2. otherwise, when the chunk reaches [`ChunkerConfig::cap`], in the
//!    middle of the widest pause within the cap;
//! 3. failing that (continuous speech), in the middle of the quietest
//!    [`ChunkerConfig::frame`] in the last [`ChunkerConfig::fallback_window`]
//!    before the cap.
//!
//! Chunks tile the stream exactly: chunk *i* covers `[a_i, a_(i+1))`, none is
//! empty, none is longer than the cap, and a chunk is marked
//! [`Chunk::has_speech`] whenever any of it might hold speech, so the
//! caller can skip decoding silence without ever skipping speech.
//! (Research note "Chunking and VAD", recommended chunker.)

use nota_core::{SampleCount, SampleIndex, SampleRange, SampleRate};

/// Where and how the chunker cuts. Build with [`ChunkerConfig::new`] or
/// [`ChunkerConfig::live`]; the fields can't be set out of range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkerConfig {
    min_pause: u64,
    target: u64,
    cap: u64,
    fallback_window: u64,
    frame: u64,
}

impl ChunkerConfig {
    /// The live pass at `rate`: pauses of 0.15 s, a 3 s target, a 10 s cap,
    /// and a fallback cut at the quietest 30 ms in the last 8 s.
    #[must_use]
    pub fn live(rate: SampleRate) -> Self {
        let hz = u64::from(rate.hz());
        // In range for every rate: the frame (at least 1 sample) is below
        // the pause (at least 3), which is below the target, the window and
        // the cap.
        Self {
            min_pause: (hz * 15 / 100).max(3),
            target: (hz * 3).max(4),
            cap: (hz * 10).max(5),
            fallback_window: (hz * 8).max(5),
            frame: (hz * 3 / 100).max(1),
        }
    }

    /// A config from sample counts, or `None` unless
    /// `0 < frame`, `2 <= min_pause <= target <= cap`, and
    /// `frame <= fallback_window <= cap`.
    #[must_use]
    pub fn new(
        min_pause: SampleCount,
        target: SampleCount,
        cap: SampleCount,
        fallback_window: SampleCount,
        frame: SampleCount,
    ) -> Option<Self> {
        let config = Self {
            min_pause: min_pause.get(),
            target: target.get(),
            cap: cap.get(),
            fallback_window: fallback_window.get(),
            frame: frame.get(),
        };
        let ok = config.frame > 0
            && config.min_pause >= 2
            && config.min_pause <= config.target
            && config.target <= config.cap
            && config.frame <= config.fallback_window
            && config.fallback_window <= config.cap
            // Bounds the buffer to something addressable.
            && usize::try_from(config.cap).is_ok();
        ok.then_some(config)
    }

    /// The shortest silence that counts as a pause.
    #[must_use]
    pub const fn min_pause(&self) -> SampleCount {
        SampleCount::new(self.min_pause)
    }

    /// The longest chunk.
    #[must_use]
    pub const fn cap(&self) -> SampleCount {
        SampleCount::new(self.cap)
    }
}

/// What the voice activity detector has settled, in track samples.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Labels {
    /// Speech segments the detector has finished since the last update, in
    /// order. A segment is finished once the pause after it is confirmed.
    pub segments: Vec<SampleRange>,
    /// Every sample before this that isn't in a finished segment is
    /// silence. It never goes back, and speech still in progress lies at or
    /// after it.
    pub silent_until: SampleIndex,
}

/// A run of audio cut from the stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    /// The samples it covers.
    pub range: SampleRange,
    /// The audio, one value per sample in `range`.
    pub audio: Vec<f32>,
    /// Whether any of it might be speech. `false` only when all of it is
    /// settled silence.
    pub has_speech: bool,
}

/// Cuts one track's audio into chunks. Feed it with [`Chunker::push`] and
/// end with [`Chunker::finish`].
#[derive(Debug)]
pub struct Chunker {
    config: ChunkerConfig,
    /// The first sample of the chunk being built.
    start: u64,
    /// The audio from `start` on.
    audio: Vec<f32>,
    /// Finished speech segments that end after `start`, in order, clipped to
    /// start at or after it.
    segments: Vec<(u64, u64)>,
    /// Everything before this is settled: speech if in `segments`, silence
    /// otherwise.
    known: u64,
}

impl Chunker {
    /// A chunker whose first chunk starts at `first`.
    #[must_use]
    pub const fn new(config: ChunkerConfig, first: SampleIndex) -> Self {
        Self {
            config,
            start: first.get(),
            audio: Vec::new(),
            segments: Vec::new(),
            known: first.get(),
        }
    }

    /// The sample the next pushed audio starts at.
    #[must_use]
    pub fn next_sample(&self) -> SampleIndex {
        SampleIndex::new(self.end())
    }

    /// The first sample not yet in a returned chunk.
    #[must_use]
    pub const fn chunk_start(&self) -> SampleIndex {
        SampleIndex::new(self.start)
    }

    /// Adds `samples`, which follow on from [`Self::next_sample`], and what
    /// the detector now knows. Returns the chunks this completes, in order.
    pub fn push(&mut self, samples: &[f32], labels: &Labels) -> Vec<Chunk> {
        self.audio.extend_from_slice(samples);
        self.learn(labels);
        let mut chunks = Vec::new();
        while let Some(chunk) = self.next_cut().and_then(|at| self.cut(at)) {
            chunks.push(chunk);
        }
        chunks
    }

    /// Ends the stream: everything pushed is settled by `labels` (the
    /// detector's flush) and comes back as chunks.
    pub fn finish(&mut self, labels: &Labels) -> Vec<Chunk> {
        self.learn(labels);
        self.known = self.end();
        let mut chunks = Vec::new();
        while let Some(chunk) = self.cut(self.next_cut().unwrap_or_else(|| self.end())) {
            chunks.push(chunk);
        }
        chunks
    }

    fn end(&self) -> u64 {
        self.start.saturating_add(self.audio.len() as u64)
    }

    fn learn(&mut self, labels: &Labels) {
        for segment in &labels.segments {
            let (from, to) = (segment.start().get().max(self.start), segment.end().get());
            if to > from {
                self.known = self.known.max(to);
                self.segments.push((from, to));
            }
        }
        // Labels past the audio pushed so far can't be trusted to be about
        // it; settle only what's here.
        self.known = self
            .known
            .max(labels.silent_until.get())
            .clamp(self.start, self.end());
        self.segments.retain(|&(_, to)| to > self.start);
    }

    /// The pauses (maximal settled silences of at least `min_pause`) in the
    /// chunk, as `(from, to)`.
    fn pauses(&self) -> Vec<(u64, u64)> {
        let mut pauses = Vec::new();
        let mut silent_from = self.start;
        let mut push = |from: u64, to: u64| {
            if to.saturating_sub(from) >= self.config.min_pause {
                pauses.push((from, to));
            }
        };
        for &(from, to) in &self.segments {
            let from = from.min(self.known);
            push(silent_from, from);
            silent_from = silent_from.max(to.min(self.known));
        }
        push(silent_from, self.known);
        pauses
    }

    /// Where to cut next, if anywhere yet.
    fn next_cut(&self) -> Option<u64> {
        let pauses = self.pauses();
        let middle = |&(from, to): &(u64, u64)| from + (to - from) / 2;
        let into = |at: u64| at - self.start;

        // 1. The first pause whose middle is past the target.
        if let Some(at) = pauses
            .iter()
            .map(middle)
            .find(|&at| into(at) >= self.config.target && into(at) <= self.config.cap)
        {
            return Some(at);
        }
        if into(self.end()) < self.config.cap {
            return None;
        }
        // 2. At the cap: the widest pause with its middle inside the cap;
        //    the later one on a tie.
        let cap_at = self.start + self.config.cap;
        if let Some(&pause) = pauses
            .iter()
            .filter(|pause| middle(pause) <= cap_at)
            .max_by_key(|&&(from, to)| (to - from, from))
        {
            return Some(middle(&pause));
        }
        // 3. The quietest frame in the window before the cap.
        Some(self.quietest_frame_middle(cap_at))
    }

    fn quietest_frame_middle(&self, cap_at: u64) -> u64 {
        let window_start = cap_at - self.config.fallback_window;
        let frame = self.config.frame;
        let mut best = (f64::INFINITY, cap_at);
        let mut from = window_start;
        while from + frame <= cap_at {
            let energy = self.energy(from, from + frame);
            // Strictly lower, so the earliest of equally quiet frames wins.
            if energy < best.0 {
                best = (energy, from + frame / 2);
            }
            from += frame;
        }
        // A frame's middle is past the window start, which is at or after
        // the chunk start, and a frame is at least one sample, so the cut
        // is never at the chunk start unless the frame is one sample long.
        best.1.max(self.start + 1)
    }

    fn energy(&self, from: u64, to: u64) -> f64 {
        let slice = self.slice(from, to);
        slice.iter().map(|&s| f64::from(s) * f64::from(s)).sum()
    }

    /// The buffered audio for `[from, to)`, clipped to what's buffered.
    fn slice(&self, from: u64, to: u64) -> &[f32] {
        let at = |pos: u64| {
            usize::try_from(pos.saturating_sub(self.start))
                .unwrap_or(usize::MAX)
                .min(self.audio.len())
        };
        &self.audio[at(from)..at(to).max(at(from))]
    }

    /// Cuts the chunk `[start, at)` off the front, or nothing if that would
    /// be empty.
    fn cut(&mut self, at: u64) -> Option<Chunk> {
        let at = at.min(self.end());
        let range = SampleRange::new(SampleIndex::new(self.start), SampleIndex::new(at))
            .filter(|range| !range.is_empty())?;
        let len = usize::try_from(at - self.start).unwrap_or(self.audio.len());
        let rest = self.audio.split_off(len.min(self.audio.len()));
        let audio = std::mem::replace(&mut self.audio, rest);
        // Anything past `known` is unsettled and might be speech.
        let has_speech = at > self.known
            || self
                .segments
                .iter()
                .any(|&(from, to)| from < at && to > self.start);
        self.start = at;
        self.known = self.known.max(at);
        for segment in &mut self.segments {
            segment.0 = segment.0.max(at);
        }
        self.segments.retain(|&(from, to)| to > from);
        Some(Chunk {
            range,
            audio,
            has_speech,
        })
    }
}

#[cfg(test)]
mod tests;
