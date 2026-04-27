//! Defensive parser for `<think>...</think>` blocks interleaved in
//! assistant content.
//!
//! With `reasoning_split: true` MiniMax surfaces reasoning in the structured
//! `reasoning_content` / `reasoning_details` fields and `content` arrives
//! clean. This parser exists for the cases where:
//!  - the request was made without `reasoning_split` (legacy code path)
//!  - the model failed to honor the flag for some reason
//!  - a future tier introduces a new variant
//!
//! In all those cases content arrives with `<think>...</think>` interleaved.
//! The parser is a small streaming state machine: feed it incoming text
//! deltas, and it emits a sequence of `ParsedSegment::{Text, Reasoning}`
//! pieces that callers can fan out to the corresponding `ResponseEvent`.

#[derive(Debug, Clone, PartialEq)]
pub enum ParsedSegment {
    /// Plain assistant content visible to the user.
    Text(String),
    /// Content that was inside `<think>...</think>` and should be routed to
    /// the reasoning channel.
    Reasoning(String),
}

/// Stateful parser. Construct once, call [`Self::push`] with each text delta,
/// then call [`Self::flush`] when the stream ends to drain any buffered text.
#[derive(Debug, Default)]
pub struct ThinkParser {
    /// Buffer holding content we haven't classified yet because we're in the
    /// middle of a partial tag (e.g. saw `<thi` waiting for `nk>`).
    pending: String,
    inside_think: bool,
}

const OPEN_TAG: &str = "<think>";
const CLOSE_TAG: &str = "</think>";

impl ThinkParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of text. Returns the segments that can be emitted now.
    /// Anything that might be the start of a tag is held back until the next
    /// call or `flush`.
    pub fn push(&mut self, input: &str) -> Vec<ParsedSegment> {
        self.pending.push_str(input);
        let mut out: Vec<ParsedSegment> = Vec::new();
        loop {
            if self.inside_think {
                match self.pending.find(CLOSE_TAG) {
                    Some(idx) => {
                        if idx > 0 {
                            push_segment(&mut out, ParsedSegment::Reasoning(
                                self.pending[..idx].to_string(),
                            ));
                        }
                        self.pending.drain(..idx + CLOSE_TAG.len());
                        self.inside_think = false;
                    }
                    None => {
                        // Could the suffix be a partial close tag?
                        let safe_emit_to = trailing_partial_match(&self.pending, CLOSE_TAG);
                        if safe_emit_to > 0 {
                            push_segment(&mut out, ParsedSegment::Reasoning(
                                self.pending[..safe_emit_to].to_string(),
                            ));
                            self.pending.drain(..safe_emit_to);
                        }
                        break;
                    }
                }
            } else {
                match self.pending.find(OPEN_TAG) {
                    Some(idx) => {
                        if idx > 0 {
                            push_segment(&mut out, ParsedSegment::Text(
                                self.pending[..idx].to_string(),
                            ));
                        }
                        self.pending.drain(..idx + OPEN_TAG.len());
                        self.inside_think = true;
                    }
                    None => {
                        let safe_emit_to = trailing_partial_match(&self.pending, OPEN_TAG);
                        if safe_emit_to > 0 {
                            push_segment(&mut out, ParsedSegment::Text(
                                self.pending[..safe_emit_to].to_string(),
                            ));
                            self.pending.drain(..safe_emit_to);
                        }
                        break;
                    }
                }
            }
        }
        out
    }

    /// Drain any buffered text at end-of-stream. If we ended mid-think with
    /// no closing tag, the buffered content is reported as `Reasoning` (the
    /// safer choice — keep it out of the user-visible channel).
    pub fn flush(&mut self) -> Vec<ParsedSegment> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        let drained = std::mem::take(&mut self.pending);
        let segment = if self.inside_think {
            ParsedSegment::Reasoning(drained)
        } else {
            ParsedSegment::Text(drained)
        };
        self.inside_think = false;
        vec![segment]
    }
}

/// If the tail of `buf` could be the prefix of `tag`, return the index up to
/// which we can safely emit content. Otherwise return `buf.len()`.
fn trailing_partial_match(buf: &str, tag: &str) -> usize {
    // Scan from longest possible partial match down.
    let max = buf.len().min(tag.len() - 1);
    for n in (1..=max).rev() {
        let start = buf.len() - n;
        if buf.is_char_boundary(start) && tag.starts_with(&buf[start..]) {
            return start;
        }
    }
    buf.len()
}

/// Merge consecutive `Text` or `Reasoning` segments to keep the output tidy
/// for callers iterating over the result.
fn push_segment(out: &mut Vec<ParsedSegment>, segment: ParsedSegment) {
    if let Some(last) = out.last_mut() {
        match (last, &segment) {
            (ParsedSegment::Text(prev), ParsedSegment::Text(next)) => {
                prev.push_str(next);
                return;
            }
            (ParsedSegment::Reasoning(prev), ParsedSegment::Reasoning(next)) => {
                prev.push_str(next);
                return;
            }
            _ => {}
        }
    }
    out.push(segment);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn run(parser: &mut ThinkParser, chunks: &[&str]) -> Vec<ParsedSegment> {
        let mut all = Vec::new();
        for chunk in chunks {
            all.extend(parser.push(chunk));
        }
        all.extend(parser.flush());
        // Coalesce adjacent same-kind segments for stable assertions.
        let mut coalesced: Vec<ParsedSegment> = Vec::new();
        for seg in all {
            push_segment(&mut coalesced, seg);
        }
        coalesced
    }

    #[test]
    fn passthrough_when_no_tags() {
        let mut p = ThinkParser::new();
        let out = run(&mut p, &["hello, ", "world"]);
        assert_eq!(out, vec![ParsedSegment::Text("hello, world".to_string())]);
    }

    #[test]
    fn extracts_single_think_block() {
        let mut p = ThinkParser::new();
        let out = run(&mut p, &["before <think>thoughts</think>after"]);
        assert_eq!(
            out,
            vec![
                ParsedSegment::Text("before ".to_string()),
                ParsedSegment::Reasoning("thoughts".to_string()),
                ParsedSegment::Text("after".to_string()),
            ]
        );
    }

    #[test]
    fn handles_open_tag_split_across_chunks() {
        let mut p = ThinkParser::new();
        let out = run(&mut p, &["before <thi", "nk>thoughts</think>after"]);
        assert_eq!(
            out,
            vec![
                ParsedSegment::Text("before ".to_string()),
                ParsedSegment::Reasoning("thoughts".to_string()),
                ParsedSegment::Text("after".to_string()),
            ]
        );
    }

    #[test]
    fn handles_close_tag_split_across_chunks() {
        let mut p = ThinkParser::new();
        let out = run(&mut p, &["<think>thoughts</thi", "nk>after"]);
        assert_eq!(
            out,
            vec![
                ParsedSegment::Reasoning("thoughts".to_string()),
                ParsedSegment::Text("after".to_string()),
            ]
        );
    }

    #[test]
    fn handles_unclosed_think_at_eof() {
        let mut p = ThinkParser::new();
        let out = run(&mut p, &["<think>still thinking..."]);
        assert_eq!(
            out,
            vec![ParsedSegment::Reasoning("still thinking...".to_string())]
        );
    }

    #[test]
    fn handles_only_reasoning() {
        let mut p = ThinkParser::new();
        let out = run(&mut p, &["<think>just thinking</think>"]);
        assert_eq!(
            out,
            vec![ParsedSegment::Reasoning("just thinking".to_string())]
        );
    }

    #[test]
    fn handles_multiple_think_blocks() {
        let mut p = ThinkParser::new();
        let out = run(
            &mut p,
            &["a <think>r1</think> b <think>r2</think> c"],
        );
        assert_eq!(
            out,
            vec![
                ParsedSegment::Text("a ".to_string()),
                ParsedSegment::Reasoning("r1".to_string()),
                ParsedSegment::Text(" b ".to_string()),
                ParsedSegment::Reasoning("r2".to_string()),
                ParsedSegment::Text(" c".to_string()),
            ]
        );
    }

    #[test]
    fn does_not_misclassify_lt_in_text() {
        let mut p = ThinkParser::new();
        let out = run(&mut p, &["1 < 2 is true"]);
        assert_eq!(out, vec![ParsedSegment::Text("1 < 2 is true".to_string())]);
    }

    #[test]
    fn handles_byte_split_inside_open_tag() {
        let mut p = ThinkParser::new();
        // Split right between `<` and `think>`.
        let out = run(&mut p, &["<", "think>r", "</think>"]);
        assert_eq!(out, vec![ParsedSegment::Reasoning("r".to_string())]);
    }

    #[test]
    fn empty_input_yields_no_segments() {
        let mut p = ThinkParser::new();
        let out = run(&mut p, &[]);
        assert!(out.is_empty());
    }
}
