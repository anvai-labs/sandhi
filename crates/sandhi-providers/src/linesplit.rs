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
    /// How far into `buf` we have already looked for a `\n`. Only newly-arrived bytes are
    /// searched on each call, which is what makes a newline-free stream O(n) rather than O(n²).
    searched_to: usize,
    budget: usize,
    /// Bytes examined by newline searches. Not used in production logic — it exists so the O(n)
    /// property can be asserted deterministically instead of by timing, which would flake.
    scanned: usize,
}

impl LineSplitter {
    pub(crate) fn new(budget: usize) -> Self {
        Self {
            buf: Vec::new(),
            searched_to: 0,
            budget,
            scanned: 0,
        }
    }

    /// Append a freshly-arrived chunk. Never scans.
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// Drain the next complete line, **including** its trailing `\n`, or `None` when the buffer
    /// holds no further line boundary.
    ///
    /// Searches only the bytes not yet examined. After a drain the buffer shifts, so the cursor
    /// restarts at the new head — the same discipline the raw plane has used since TD-0006.
    pub(crate) fn next_line(&mut self) -> Option<Vec<u8>> {
        if self.searched_to >= self.buf.len() {
            return None;
        }
        let found = self.buf[self.searched_to..]
            .iter()
            .position(|byte| *byte == b'\n');
        let examined = match found {
            Some(rel) => rel + 1,
            None => self.buf.len() - self.searched_to,
        };
        self.scanned = self.scanned.saturating_add(examined);
        match found {
            Some(rel) => {
                let newline = self.searched_to + rel;
                let line: Vec<u8> = self.buf.drain(..=newline).collect();
                self.searched_to = 0;
                Some(line)
            }
            None => {
                self.searched_to = self.buf.len();
                None
            }
        }
    }

    /// Whether the pending (still incomplete) line has outgrown the configured budget. The caller
    /// owns the response — see the module docs.
    pub(crate) fn over_budget(&self) -> bool {
        self.buf.len() > self.budget
    }

    /// Discard the pending line and keep going (the raw plane's drop-and-continue policy).
    pub(crate) fn reset(&mut self) {
        self.buf.clear();
        self.searched_to = 0;
    }

    /// The trailing bytes that never got a newline, for an end-of-stream flush.
    pub(crate) fn remainder(&self) -> &[u8] {
        &self.buf
    }

    /// Bytes currently buffered — the quantity the budget bounds.
    #[cfg(test)]
    pub(crate) fn buffered_len(&self) -> usize {
        self.buf.len()
    }

    /// Total bytes examined by newline searches, so complexity is assertable without timing.
    #[cfg(test)]
    pub(crate) fn bytes_scanned(&self) -> usize {
        self.scanned
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
            splitter.bytes_scanned() <= total,
            "scanned {} bytes for {total} received — the search cursor is not holding",
            splitter.bytes_scanned()
        );
    }

    #[test]
    fn many_lines_in_one_chunk_do_not_rescan_consumed_bytes() {
        let mut splitter = LineSplitter::new(usize::MAX);
        let wire: Vec<u8> = std::iter::repeat(b"line\n".as_slice())
            .take(5_000)
            .flatten()
            .copied()
            .collect();
        splitter.push(&wire);
        assert_eq!(lines(&mut splitter).len(), 5_000);
        assert_eq!(
            splitter.bytes_scanned(),
            wire.len(),
            "each byte should be examined exactly once"
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
