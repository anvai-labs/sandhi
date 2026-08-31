//! A bounded, O(n) newline splitter shared by both streaming planes (TD-0014 P1, gap G01).
//!
//! Every provider stream Sandhi decodes is newline-delimited — SSE for five families, NDJSON for
//! Ollama — so every decoder needs the same thing: accumulate chunks, hand back complete lines,
//! and never let a pathological upstream grow the buffer without bound.
//!
//! [`TD-0006`] fixed exactly that in the raw plane's `metered_passthrough` and the fix was never
//! backported: all six *typed* decoders kept an unbounded `Vec<u8>` and a `position()` call that
//! rescanned the whole accumulated buffer on every chunk. With no newline in the stream that is
//! O(chunks²) work and unbounded memory. This module is the single implementation both planes use,
//! so they cannot drift apart again.
//!
//! **The splitter carries no policy.** It reports [`over_budget`](LineSplitter::over_budget) and
//! the caller decides, because the right answer differs by plane:
//!
//! - The **raw** plane calls [`reset`](LineSplitter::reset) and keeps streaming. Its bytes were
//!   already forwarded verbatim, so an over-budget line costs only *usage* accuracy.
//! - The **typed** decoders raise `ProviderError::Transport`. They emit decoded *content*, so
//!   silently dropping a line would corrupt the response with no signal at all.
//!
//! [`TD-0006`]: https://github.com/anvai-labs/sandhi/blob/develop/docs/td/TD-0006-two-plane-proxy-transparent-metering.md

/// Accumulates stream bytes and yields complete newline-terminated lines.
pub(crate) struct LineSplitter {
    buf: Vec<u8>,
    /// Bytes at the head of `buf` already returned as lines. Advancing an offset instead of
    /// draining is what makes this genuinely O(n): `Vec::drain` from the head memmoves the whole
    /// remainder on **every** line, so a chunk holding k lines cost O(k · len) — quadratic in
    /// lines-per-chunk, which is the common case for small SSE deltas. The head is reclaimed by
    /// an amortised compaction below.
    consumed: usize,
    /// Absolute index into `buf` up to which we have already looked for a `\n`. Only newly-arrived
    /// bytes are searched, which is what makes a newline-*free* stream O(n) rather than O(n²).
    searched_to: usize,
    budget: usize,
    /// Bytes examined by newline searches. Compiled out of release builds — it exists so the O(n)
    /// property can be asserted deterministically instead of by timing, which would flake.
    #[cfg(test)]
    scanned: usize,
    /// Bytes moved by compaction, for the same reason. Together with `scanned` these cover both
    /// halves of the cost; an earlier version tracked only the search and certified a linearity
    /// the code did not have.
    #[cfg(test)]
    compacted: usize,
}

impl LineSplitter {
    pub(crate) fn new(budget: usize) -> Self {
        Self {
            buf: Vec::new(),
            consumed: 0,
            searched_to: 0,
            budget,
            #[cfg(test)]
            scanned: 0,
            #[cfg(test)]
            compacted: 0,
        }
    }

    /// Append a freshly-arrived chunk. Never scans.
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// Drain the next complete line, **including** its trailing `\n`, or `None` when the buffer
    /// holds no further line boundary.
    pub(crate) fn next_line(&mut self) -> Option<Vec<u8>> {
        if self.searched_to >= self.buf.len() {
            return None;
        }
        let found = self.buf[self.searched_to..]
            .iter()
            .position(|byte| *byte == b'\n');
        #[cfg(test)]
        {
            let examined = match found {
                Some(rel) => rel + 1,
                None => self.buf.len() - self.searched_to,
            };
            self.scanned = self.scanned.saturating_add(examined);
        }
        match found {
            Some(rel) => {
                let newline = self.searched_to + rel;
                let line = self.buf[self.consumed..=newline].to_vec();
                self.consumed = newline + 1;
                self.searched_to = self.consumed;
                self.compact_if_worthwhile();
                Some(line)
            }
            None => {
                self.searched_to = self.buf.len();
                None
            }
        }
    }

    /// Reclaim the consumed head once it is at least half the buffer, so the memmove is paid at
    /// most once per byte overall rather than once per line.
    fn compact_if_worthwhile(&mut self) {
        if self.consumed >= 4096 && self.consumed * 2 >= self.buf.len() {
            #[cfg(test)]
            {
                self.compacted = self
                    .compacted
                    .saturating_add(self.buf.len() - self.consumed);
            }
            self.buf.drain(..self.consumed);
            self.searched_to -= self.consumed;
            self.consumed = 0;
        }
    }

    /// Whether the pending (still incomplete) line has outgrown the configured budget. The caller
    /// owns the response — see the module docs.
    pub(crate) fn over_budget(&self) -> bool {
        self.buf.len() - self.consumed > self.budget
    }

    /// Discard the pending line and keep going (the raw plane's drop-and-continue policy).
    pub(crate) fn reset(&mut self) {
        self.buf.clear();
        self.consumed = 0;
        self.searched_to = 0;
    }

    /// The trailing bytes that never got a newline, for an end-of-stream flush.
    pub(crate) fn remainder(&self) -> &[u8] {
        &self.buf[self.consumed..]
    }

    /// Terminate the trailing remainder so [`next_line`](Self::next_line) yields it once, then
    /// report whether there was anything to flush. Called once at end of stream by the typed
    /// decoders: a provider's final frame may lack its newline (Ollama's NDJSON `done` frame is
    /// the motivating case), and dropping it silently lost the `Finish` — the asymmetry with the
    /// raw plane's flush, recorded in TD-0014 and closed in P2b.
    ///
    /// A garbage remainder is harmless: it fails the caller's existing parse guards.
    pub(crate) fn flush_newline(&mut self) -> bool {
        // Callers drain before flushing, but don't rely on it: if a complete line is still
        // pending, it needs no flush — signal that there is something to drain.
        if self.searched_to < self.buf.len() {
            if self.buf[self.searched_to..].contains(&b'\n') {
                return true;
            }
            self.searched_to = self.buf.len();
        }
        if self.consumed >= self.buf.len() {
            return false;
        }
        // The appended newline sits exactly at the unsearched edge, so the next next_line()
        // finds it and yields the whole remainder as one line.
        self.buf.push(b'\n');
        self.searched_to = self.buf.len() - 1;
        true
    }

    /// Bytes currently buffered — the quantity the budget bounds.
    #[cfg(test)]
    pub(crate) fn buffered_len(&self) -> usize {
        self.buf.len() - self.consumed
    }

    /// Total bytes examined by newline searches plus bytes moved by compaction — the whole cost,
    /// so complexity is assertable without timing.
    #[cfg(test)]
    pub(crate) fn work_done(&self) -> usize {
        self.scanned + self.compacted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGET: usize = 64 * 1024;

    fn lines(splitter: &mut LineSplitter) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(line) = splitter.next_line() {
            out.push(String::from_utf8(line).unwrap());
        }
        out
    }

    #[test]
    fn yields_complete_lines_including_the_newline() {
        let mut splitter = LineSplitter::new(BUDGET);
        splitter.push(b"one\ntwo\n");
        assert_eq!(lines(&mut splitter), vec!["one\n", "two\n"]);
        assert_eq!(splitter.remainder(), b"");
    }

    #[test]
    fn a_partial_line_is_held_until_its_newline_arrives() {
        let mut splitter = LineSplitter::new(BUDGET);
        splitter.push(b"par");
        assert_eq!(lines(&mut splitter), Vec::<String>::new());
        splitter.push(b"tial\n");
        assert_eq!(lines(&mut splitter), vec!["partial\n"]);
    }

    #[test]
    fn splitting_is_invariant_across_every_byte_boundary() {
        // The property the six decoders depend on: where the transport chunks the bytes must not
        // change the lines produced.
        let wire = b"alpha\nbeta\ngamma\n";
        for split in 0..=wire.len() {
            let mut splitter = LineSplitter::new(BUDGET);
            splitter.push(&wire[..split]);
            let mut got = lines(&mut splitter);
            splitter.push(&wire[split..]);
            got.extend(lines(&mut splitter));
            assert_eq!(got, vec!["alpha\n", "beta\n", "gamma\n"], "split {split}");
        }
    }

    #[test]
    fn a_large_but_complete_line_survives_chunked_delivery() {
        // The distinction the budget must respect: "one line larger than typical, still
        // terminated" is legitimate traffic — OpenAI Responses puts the whole final response,
        // usage included, in one SSE line. "No line boundary at all" is pathological.
        //
        // Note what `over_budget` actually means: it is true whenever the PENDING line exceeds
        // the budget, which for a large line is true mid-delivery, before its newline lands. The
        // decoders consult it after every chunk, so the only thing keeping legitimate traffic
        // alive is that the ceiling sits far above any real frame. This test therefore models
        // production — ceiling well above the line — and asserts the bound never fires at ANY
        // point during delivery, not merely at the end.
        let budget = 8 * 1024 * 1024;
        let mut splitter = LineSplitter::new(budget);
        let mut wire = vec![b'a'; 130 * 1024]; // a realistic long-generation terminal frame
        wire.push(b'\n');
        let mut lines = Vec::new();
        for chunk in wire.chunks(16 * 1024) {
            splitter.push(chunk);
            while let Some(line) = splitter.next_line() {
                lines.push(line);
            }
            assert!(
                !splitter.over_budget(),
                "the bound must not fire part-way through a legitimate frame"
            );
        }
        assert_eq!(lines.len(), 1, "the complete line must be delivered");
        assert_eq!(
            lines[0].len(),
            wire.len(),
            "delivered intact, not truncated"
        );
    }

    #[test]
    fn over_budget_tracks_the_pending_line_and_clears_when_it_completes() {
        // Pins the semantics the test above depends on, so nobody has to infer them: the bound is
        // about bytes held with no boundary, and a completed line releases them.
        let mut splitter = LineSplitter::new(1024);
        splitter.push(&vec![b'a'; 2048]);
        assert!(
            splitter.over_budget(),
            "2 KiB pending against a 1 KiB budget"
        );
        splitter.push(b"\n");
        assert!(splitter.next_line().is_some());
        assert!(
            !splitter.over_budget(),
            "delivering the line clears the pending bytes"
        );
    }

    #[test]
    fn a_newline_free_stream_is_reported_over_budget() {
        // The bound that did not exist in the typed decoders (gap G01).
        let mut splitter = LineSplitter::new(BUDGET);
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..64 {
            splitter.push(&chunk);
            while splitter.next_line().is_some() {}
        }
        assert!(
            splitter.over_budget(),
            "4 MiB with no newline must report over-budget"
        );
        splitter.reset();
        assert!(!splitter.over_budget());
        assert_eq!(splitter.buffered_len(), 0);
    }

    #[test]
    fn a_newline_free_stream_scans_each_byte_once() {
        // Deterministic stand-in for "O(n), not O(n²)". The pre-TD-0006 implementation rescanned
        // the whole accumulated buffer on every chunk, so 10k chunks of 64 B would examine ~3.2e9
        // bytes; the cursor keeps it at one pass over the 640 KB actually received.
        let mut splitter = LineSplitter::new(usize::MAX);
        let chunk = vec![b'x'; 64];
        let chunks = 10_000;
        for _ in 0..chunks {
            splitter.push(&chunk);
            while splitter.next_line().is_some() {}
        }
        let total = chunks * chunk.len();
        assert_eq!(splitter.buffered_len(), total);
        assert!(
            splitter.work_done() <= total,
            "scanned {} bytes for {total} received — the search cursor is not holding",
            splitter.work_done()
        );
    }

    #[test]
    fn many_lines_in_one_chunk_stay_linear_in_total_work() {
        let mut splitter = LineSplitter::new(usize::MAX);
        let wire: Vec<u8> = std::iter::repeat(b"line\n".as_slice())
            .take(5_000)
            .flatten()
            .copied()
            .collect();
        splitter.push(&wire);
        assert_eq!(lines(&mut splitter).len(), 5_000);
        // Total work — search plus compaction memmove — must stay a small multiple of the input.
        // This is the assertion an earlier version got wrong: it counted only the SEARCH, which is
        // linear by construction, and so certified a linearity the code did not have. Draining
        // per line memmoves the remainder every time, which is O(k · len) for k lines in one
        // buffer; adversarial review measured 4x input -> 8.5x time. The head offset plus
        // amortised compaction is what actually makes this linear.
        assert!(
            splitter.work_done() <= 3 * wire.len(),
            "5k lines in one chunk cost {} for {} bytes — a per-line drain would be quadratic",
            splitter.work_done(),
            wire.len()
        );
    }

    #[test]
    fn flush_newline_terminates_the_remainder_exactly_once() {
        let mut splitter = LineSplitter::new(BUDGET);
        assert!(
            !splitter.flush_newline(),
            "nothing buffered: nothing to flush"
        );
        splitter.push(b"{\"done\":true}");
        assert!(splitter.flush_newline());
        assert_eq!(
            lines(&mut splitter),
            vec!["{\"done\":true}\n"],
            "the flushed remainder must come back as one line"
        );
        assert!(
            !splitter.flush_newline(),
            "drained: a second flush must be a no-op"
        );
    }

    #[test]
    fn remainder_exposes_trailing_bytes_without_a_newline() {
        let mut splitter = LineSplitter::new(BUDGET);
        splitter.push(b"done\ntrailing");
        assert_eq!(lines(&mut splitter), vec!["done\n"]);
        assert_eq!(splitter.remainder(), b"trailing");
    }
}
