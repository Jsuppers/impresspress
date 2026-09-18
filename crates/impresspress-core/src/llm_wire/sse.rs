//! Provider-agnostic Server-Sent Events transport parsing.
//!
//! The OpenAI and Anthropic streaming decoders share the same wire framing —
//! accumulate raw bytes, decode them as UTF-8 across network-chunk
//! boundaries, split on the blank line that ends a frame, and parse each
//! frame's `event:` / `data:` lines — and differ only in how they interpret a
//! decoded frame. [`SseFrameStream`] owns the shared transport half; each
//! provider's decoder layers its own per-frame semantics on top.

use wafer_core::interfaces::llm::service::ChatChunk;

/// One decoded SSE frame: a blank-line-terminated block of `key: value` lines.
///
/// `event` is the last `event:` value seen in the frame (if any); `data` is the
/// `data:` lines joined with `\n` (empty when the frame carried none — e.g. a
/// comment or keepalive).
pub struct SseFrame {
    /// Read only by the Anthropic decoder, which is gated on `feature = "llm"`
    /// (reqwest + tokio, neither of which builds for wasm32). OpenAI's wire
    /// ignores `event:` entirely, so in a build that carries only the OpenAI
    /// consumer — `impresspress-browser` takes this crate with no default
    /// features — the field is parsed and never read. The `allow` is scoped to
    /// exactly that configuration so a genuinely dead field still warns in the
    /// build that has both readers.
    #[cfg_attr(not(feature = "llm"), allow(dead_code))]
    pub event: Option<String>,
    pub data: String,
}

/// A batch of decoded chunks plus the terminal flag, returned by each provider
/// decoder's `push`. Shared so the providers don't each redefine it.
#[derive(Debug, Default, PartialEq)]
pub struct DecodeBatch {
    pub chunks: Vec<ChatChunk>,
    /// True once the stream has terminated (e.g. OpenAI's `[DONE]` sentinel or
    /// Anthropic's `message_stop`). Callers should stop feeding once set.
    pub done: bool,
}

/// What a [`feed`](SseFrameStream::feed) had to throw away, if anything. The
/// caller turns this into its provider-tagged warning, which keeps this type
/// free of a `tracing` dependency; the frames already buffered stay
/// retrievable either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedDiscard {
    /// The stream carried a byte sequence that is not valid UTF-8 and is not
    /// merely an incomplete tail. The offending sequence was skipped and
    /// decoding continued after it.
    InvalidUtf8,
    /// A single frame exceeded [`MAX_PENDING_FRAME_BYTES`] without a
    /// terminating blank line. Its bytes are dropped up to and including the
    /// next blank line, so the buffer cannot grow without bound.
    FrameTooLarge,
}

/// Cap on the bytes a single un-terminated frame may occupy. A provider that
/// frames its stream in a way this parser does not recognise (or a hostile
/// one that never terminates a frame) would otherwise grow `buf` for as long
/// as the connection lives. Real frames are a few kilobytes at most, so a
/// mebibyte is far above anything legitimate.
pub const MAX_PENDING_FRAME_BYTES: usize = 1024 * 1024;

/// Incremental SSE transport parser: accumulates raw bytes and yields complete
/// frames on demand. Knows nothing about chunk semantics.
///
/// Feeds are transport chunks, not message boundaries: a multi-byte character
/// or a frame separator can be split across two of them, so the decoded tail
/// of a feed is carried over rather than discarded.
pub struct SseFrameStream {
    /// Decoded text with line endings normalised to `\n`, awaiting frame
    /// extraction.
    buf: String,
    /// The trailing bytes of a UTF-8 sequence the last feed cut in half. They
    /// are decoded once the rest of the sequence arrives; at most three bytes
    /// by construction.
    partial: Vec<u8>,
    /// The last decoded character was a `\r`, which already produced a `\n` in
    /// `buf`. A `\n` at the head of the next text is that CR's LF half and is
    /// dropped, so a CRLF split across feeds still counts as one line ending.
    pending_cr: bool,
    /// A frame passed [`MAX_PENDING_FRAME_BYTES`] and is being discarded until
    /// the next blank line, so its tail is never parsed as a frame of its own.
    dropping_frame: bool,
}

impl Default for SseFrameStream {
    fn default() -> Self {
        Self::new()
    }
}

impl SseFrameStream {
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            partial: Vec::new(),
            pending_cr: false,
            dropping_frame: false,
        }
    }

    /// Append raw bytes to the buffer, decoding as much of them as forms whole
    /// UTF-8 characters and holding an incomplete trailing sequence back for
    /// the next feed.
    ///
    /// Returns `Some` when bytes had to be discarded (see [`FeedDiscard`]);
    /// everything decodable is buffered regardless, so the caller warns and
    /// keeps draining frames rather than dropping the batch.
    pub fn feed(&mut self, bytes: &[u8]) -> Option<FeedDiscard> {
        let mut discard = None;

        // Re-attach the sequence the previous feed could not finish decoding.
        let joined: Vec<u8>;
        let mut rest: &[u8] = if self.partial.is_empty() {
            bytes
        } else {
            joined = self
                .partial
                .drain(..)
                .chain(bytes.iter().copied())
                .collect();
            &joined
        };

        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    self.push_text(text);
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    let prefix = std::str::from_utf8(&rest[..valid])
                        .expect("valid_up_to() bounds a valid UTF-8 prefix");
                    self.push_text(prefix);
                    match e.error_len() {
                        // A genuinely invalid sequence: skip it and keep
                        // decoding, so one bad byte costs one character
                        // rather than the rest of the stream.
                        Some(len) => {
                            discard = Some(FeedDiscard::InvalidUtf8);
                            rest = &rest[valid + len..];
                        }
                        // A character split across this chunk boundary: hold
                        // its head for the next feed to complete.
                        None => {
                            self.partial.extend_from_slice(&rest[valid..]);
                            break;
                        }
                    }
                }
            }
        }

        if self.enforce_frame_cap() {
            discard = Some(FeedDiscard::FrameTooLarge);
        }
        discard
    }

    /// Append decoded text, normalising SSE's three line endings (`\r\n`,
    /// `\r`, `\n`) to `\n` so [`next_frame`](Self::next_frame) recognises a
    /// blank line in any of them. A `\r` at the end of a feed emits its `\n`
    /// immediately and remembers itself, so the `\n` that may open the next
    /// feed is not mistaken for a second line ending — which is what would
    /// otherwise split a frame in half mid-CRLF.
    fn push_text(&mut self, text: &str) {
        for ch in text.chars() {
            match ch {
                '\n' if self.pending_cr => self.pending_cr = false,
                '\r' => {
                    self.pending_cr = true;
                    self.buf.push('\n');
                }
                other => {
                    self.pending_cr = false;
                    self.buf.push(other);
                }
            }
        }
    }

    /// Keep `buf` bounded. Returns true when this call started dropping an
    /// oversized frame, so [`feed`](Self::feed) can report it once.
    fn enforce_frame_cap(&mut self) -> bool {
        if self.dropping_frame {
            match self.buf.find("\n\n") {
                // The oversized frame finally ended: drop it and resume.
                Some(sep) => {
                    self.buf.drain(..=sep + 1);
                    self.dropping_frame = false;
                }
                // Still inside it. Keep the final character so a separator
                // straddling this discard is still seen.
                None => self.truncate_to_last_char(),
            }
            return false;
        }
        if self.buf.len() <= MAX_PENDING_FRAME_BYTES || self.buf.contains("\n\n") {
            return false;
        }
        self.truncate_to_last_char();
        self.dropping_frame = true;
        true
    }

    /// Reduce `buf` to its last character, the only part that can still form a
    /// frame separator with the bytes yet to arrive.
    fn truncate_to_last_char(&mut self) {
        let keep = self.buf.chars().next_back();
        self.buf.clear();
        if let Some(c) = keep {
            self.buf.push(c);
        }
    }

    /// Pull the next complete blank-line-terminated frame, with its `event:` /
    /// `data:` lines parsed. Returns `None` when no complete frame is buffered
    /// yet — the partial tail stays for the next [`feed`](Self::feed).
    pub fn next_frame(&mut self) -> Option<SseFrame> {
        let sep = self.buf.find("\n\n")?;
        let raw = self.buf[..sep].to_string();
        self.buf.drain(..=sep + 1);
        Some(parse_frame(&raw))
    }
}

/// Parse one raw frame body (the text before a `\n\n`) into its `event` / `data`
/// fields. Strips a leading BOM per line, captures the last `event:` value, and
/// joins multiple `data:` lines with `\n` (the SSE spec's concatenation rule).
fn parse_frame(raw: &str) -> SseFrame {
    let mut event = None;
    let mut data = String::new();
    for line in raw.lines() {
        let line = line.trim_start_matches('\u{feff}');
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.trim_start();
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest);
        }
        // Other SSE fields (`id:`, `retry:`, comments) are ignored.
    }
    SseFrame { event, data }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yields_only_complete_frames() {
        let mut s = SseFrameStream::new();
        // Partial frame — no blank line yet.
        assert_eq!(s.feed(b"data: hello"), None);
        assert!(s.next_frame().is_none());
        // Completing it yields exactly one frame.
        assert_eq!(s.feed(b"\n\n"), None);
        let f = s.next_frame().expect("frame");
        assert_eq!(f.data, "hello");
        assert!(s.next_frame().is_none());
    }

    #[test]
    fn joins_multiple_data_lines() {
        let mut s = SseFrameStream::new();
        s.feed(b"data: line1\ndata: line2\n\n");
        let f = s.next_frame().expect("frame");
        assert_eq!(f.data, "line1\nline2");
    }

    #[test]
    fn captures_and_trims_event_name() {
        let mut s = SseFrameStream::new();
        s.feed(b"event: content_block_delta\ndata: {}\n\n");
        let f = s.next_frame().expect("frame");
        assert_eq!(f.event.as_deref(), Some("content_block_delta"));
        assert_eq!(f.data, "{}");
    }

    #[test]
    fn strips_leading_bom() {
        let mut s = SseFrameStream::new();
        s.feed("\u{feff}data: x\n\n".as_bytes());
        let f = s.next_frame().expect("frame");
        assert_eq!(f.data, "x");
    }

    #[test]
    fn comment_or_keepalive_frame_has_empty_data() {
        let mut s = SseFrameStream::new();
        s.feed(b": keepalive\n\n");
        let f = s.next_frame().expect("frame");
        assert!(f.event.is_none());
        assert_eq!(f.data, "");
    }

    #[test]
    fn invalid_utf8_is_skipped_without_losing_the_rest_of_the_feed() {
        let mut s = SseFrameStream::new();
        // 0xFF is never valid in UTF-8 — and it must cost exactly itself: the
        // frame it sits in still arrives, minus the bad byte.
        let mut bytes = b"data: o".to_vec();
        bytes.extend_from_slice(&[0xff]);
        bytes.extend_from_slice(b"k\n\n");
        assert_eq!(s.feed(&bytes), Some(FeedDiscard::InvalidUtf8));
        assert_eq!(s.next_frame().expect("frame").data, "ok");
        // A subsequent valid feed still works (buffer wasn't corrupted).
        assert_eq!(s.feed(b"data: next\n\n"), None);
        assert_eq!(s.next_frame().expect("frame").data, "next");
    }

    #[test]
    fn pull_drains_one_frame_at_a_time_leaving_the_rest() {
        let mut s = SseFrameStream::new();
        // Two complete frames plus a partial tail in one buffer.
        s.feed(b"data: a\n\ndata: b\n\ndata: c");
        assert_eq!(s.next_frame().expect("first").data, "a");
        // Pulling one frame must leave the second retrievable...
        assert_eq!(s.next_frame().expect("second").data, "b");
        // ...and the partial tail unparsed.
        assert!(s.next_frame().is_none());
        s.feed(b"\n\n");
        assert_eq!(s.next_frame().expect("third").data, "c");
    }

    /// Drain every frame the stream currently holds, joining their `data`.
    fn drain(s: &mut SseFrameStream) -> String {
        let mut out = String::new();
        while let Some(f) = s.next_frame() {
            out.push_str(&f.data);
        }
        out
    }

    /// A network chunk boundary is a transport boundary, not a character
    /// boundary: reqwest hands over whatever arrived. Splitting a frame at
    /// *every* byte offset must still decode to the same text — the split that
    /// lands inside a multi-byte character is the one that used to lose both
    /// halves (an incomplete lead sequence ends chunk N, bare continuation
    /// bytes open chunk N+1, so both chunks failed `from_utf8`).
    #[test]
    fn a_character_split_across_chunks_survives_at_every_offset() {
        for payload in ["héllo 🙂 wörld", "日本語のテキスト", "🙂🙂🙂"] {
            let wire = format!("data: {payload}\n\n");
            let bytes = wire.as_bytes();
            for split in 0..=bytes.len() {
                let mut s = SseFrameStream::new();
                assert_eq!(
                    s.feed(&bytes[..split]),
                    None,
                    "a split at {split} is a chunk boundary, not a decode error"
                );
                assert_eq!(s.feed(&bytes[split..]), None, "second half of {split}");
                assert_eq!(
                    drain(&mut s),
                    payload,
                    "payload {payload:?} lost content when split at byte {split}"
                );
            }
        }
    }

    /// The same, with the character split three ways — one continuation byte
    /// per feed — which is what a slow connection produces.
    #[test]
    fn a_character_split_byte_by_byte_survives() {
        let wire = "data: 🙂 ok\n\n";
        let mut s = SseFrameStream::new();
        for b in wire.as_bytes() {
            assert_eq!(
                s.feed(&[*b]),
                None,
                "byte-at-a-time feed is never a discard"
            );
        }
        assert_eq!(drain(&mut s), "🙂 ok");
    }

    /// A multi-byte character split across the boundary must not corrupt the
    /// *next* frame either: the halves belong to one character, so the frames
    /// around them stay intact.
    #[test]
    fn frames_after_a_split_character_are_not_corrupted() {
        let wire = "data: {\"t\":\"é\"}\n\ndata: {\"t\":\"b\"}\n\n";
        let bytes = wire.as_bytes();
        for split in 0..=bytes.len() {
            let mut s = SseFrameStream::new();
            s.feed(&bytes[..split]);
            s.feed(&bytes[split..]);
            let mut frames = Vec::new();
            while let Some(f) = s.next_frame() {
                frames.push(f.data);
            }
            assert_eq!(
                frames,
                vec!["{\"t\":\"é\"}".to_string(), "{\"t\":\"b\"}".to_string()],
                "split at {split} corrupted the frame sequence"
            );
        }
    }

    /// SSE terminates a line with CRLF, LF or CR, so a blank line — the frame
    /// separator — can arrive as any of the three. A provider that uses CRLF
    /// used to yield no frame at all while its bytes accumulated forever.
    #[test]
    fn crlf_and_cr_framing_yield_frames() {
        for wire in [
            "event: e\r\ndata: hi\r\n\r\n",
            "event: e\rdata: hi\r\r",
            "event: e\r\ndata: hi\n\n",
        ] {
            let mut s = SseFrameStream::new();
            assert_eq!(s.feed(wire.as_bytes()), None);
            let f = s
                .next_frame()
                .unwrap_or_else(|| panic!("{wire:?} must yield a frame"));
            assert_eq!(f.event.as_deref(), Some("e"), "wire {wire:?}");
            assert_eq!(f.data, "hi", "wire {wire:?}");
        }
    }

    /// A CRLF separator split between the CR and the LF is still one line
    /// ending, not two — otherwise the frame would be cut in half.
    #[test]
    fn a_crlf_split_across_feeds_is_one_line_ending() {
        let wire = "data: hi\r\n\r\n";
        let bytes = wire.as_bytes();
        for split in 0..=bytes.len() {
            let mut s = SseFrameStream::new();
            s.feed(&bytes[..split]);
            s.feed(&bytes[split..]);
            let mut frames = Vec::new();
            while let Some(f) = s.next_frame() {
                frames.push(f.data);
            }
            assert_eq!(frames, vec!["hi".to_string()], "split at {split}");
        }
    }

    /// An un-terminated frame cannot grow without bound: past the cap its
    /// bytes are dropped until the next blank line, and the stream recovers on
    /// the frame after it.
    #[test]
    fn an_unterminated_frame_is_bounded_and_the_stream_recovers() {
        let mut s = SseFrameStream::new();
        let mut discards = Vec::new();
        // 2 MiB of a single frame that never ends.
        for _ in 0..32 {
            if let Some(d) = s.feed(&vec![b'x'; 64 * 1024]) {
                discards.push(d);
            }
        }
        assert_eq!(
            discards,
            vec![FeedDiscard::FrameTooLarge],
            "the overflow is reported once, not per feed"
        );
        assert!(
            s.buf.len() <= MAX_PENDING_FRAME_BYTES,
            "buffer stayed bounded, got {} bytes",
            s.buf.len()
        );
        // The rest of the oversized frame is discarded, and the next frame
        // decodes normally.
        assert_eq!(s.feed(b"more junk\n\ndata: ok\n\n"), None);
        assert_eq!(drain(&mut s), "ok");
    }
}
