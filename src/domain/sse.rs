//! Server-sent events: the raw byte streams upstreams answer with and the
//! frame parser over them.

use std::pin::Pin;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};

/// A failure while reading a streamed body.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct StreamError(pub String);

/// A raw response body, delivered as it arrives.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, StreamError>> + Send>>;

/// A body made of one in-memory chunk.
// Return type is already must_use; older clippy still asks for the attribute.
#[allow(clippy::must_use_candidate)]
pub fn once(body: impl Into<Bytes>) -> ByteStream {
    let chunk: Bytes = body.into();
    Box::pin(futures_util::stream::once(async move { Ok(chunk) }))
}

/// Read a whole body into memory. A read error ends the body early.
pub async fn collect(mut body: ByteStream) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(Ok(chunk)) = body.next().await {
        out.extend_from_slice(&chunk);
    }
    out
}

/// A single parsed SSE frame: optional event name and data payload.
#[derive(Debug, Clone)]
pub struct SseFrame {
    /// Optional SSE `event:` name, if present in the frame.
    pub event: Option<String>,
    /// The frame's `data:` payload.
    pub data: String,
    /// `SSE` comment lines (starting with `:`), e.g. `NeuralWatt`'s
    /// `: energy {...}` / `: cost {...}`. Comments carry no event semantics
    /// but may hold provider-specific metadata.
    pub comments: Vec<String>,
}

impl SseFrame {
    /// Parse the data payload as JSON, tolerating failures.
    #[must_use]
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_str(self.data.trim()).ok()
    }

    /// The `[DONE]` sentinel used by OpenAI-style streams.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.data.trim() == "[DONE]"
    }
}

/// Errors that can occur while reading the upstream SSE stream.
#[derive(Debug, thiserror::Error)]
pub enum SseError {
    /// The upstream stream failed to produce further bytes.
    #[error("failed reading upstream stream: {0}")]
    Read(StreamError),
}

struct FrameState {
    stream: ByteStream,
    buf: String,
    eof: bool,
}

/// Adapt a streamed body into a stream of SSE frames. Handles
/// `event:`/`data:` lines, multi-line data, CRLF line endings and the
/// trailing `[DONE]` sentinel.
pub fn sse_frames(body: ByteStream) -> impl Stream<Item = Result<SseFrame, SseError>> + Send {
    let state = FrameState {
        stream: body,
        buf: String::new(),
        eof: false,
    };
    futures_util::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(frame) = take_frame(&mut st.buf) {
                return Some((Ok(frame), st));
            }
            if st.eof {
                if st.buf.trim().is_empty() {
                    return None;
                }
                let frame = parse_frame(&std::mem::take(&mut st.buf));
                return Some((Ok(frame), st));
            }
            match st.stream.next().await {
                Some(Ok(bytes)) => {
                    st.buf.push_str(&String::from_utf8_lossy(&bytes));
                    st.buf = st.buf.replace("\r\n", "\n");
                }
                Some(Err(e)) => return Some((Err(SseError::Read(e)), st)),
                None => st.eof = true,
            }
        }
    })
}

fn take_frame(buf: &mut String) -> Option<SseFrame> {
    let idx = buf.find("\n\n")?;
    let frame_text = buf[..idx].to_string();
    buf.drain(..idx + 2);
    Some(parse_frame(&frame_text))
}

/// Feed raw SSE bytes into an accumulator buffer, normalizing CRLF, and return
/// every complete frame that became available. Partial trailing data stays in
/// `buf` for the next call.
pub fn feed_frames(buf: &mut String, data: &[u8]) -> Vec<SseFrame> {
    buf.push_str(&String::from_utf8_lossy(data));
    *buf = buf.replace("\r\n", "\n");
    let mut out = Vec::new();
    while let Some(f) = take_frame(buf) {
        out.push(f);
    }
    out
}

/// Flush any remaining (incomplete) buffered data as a final frame, or `None`
/// when the buffer holds nothing meaningful. Use at end-of-stream.
pub fn flush_frames(buf: &mut String) -> Option<SseFrame> {
    if buf.trim().is_empty() {
        return None;
    }
    Some(parse_frame(&std::mem::take(buf)))
}

fn parse_frame(text: &str) -> SseFrame {
    let mut event = None;
    let mut data = String::new();
    let mut comments = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        } else if let Some(rest) = line.strip_prefix(':') {
            comments.push(rest.trim_start().to_string());
        }
        // unknown fields are ignored
    }
    SseFrame {
        event,
        data,
        comments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_event_and_data() {
        let f = parse_frame("event: message_start\ndata: {\"a\":1}");
        assert_eq!(f.event.as_deref(), Some("message_start"));
        assert_eq!(f.data, "{\"a\":1}");
    }

    #[test]
    fn captures_comment_lines() {
        let f = parse_frame(
            "data: x\n: energy {\"energy_kwh\": 1.385e-06}\n: cost {\"r\": 1}\ndata: [DONE]",
        );
        assert_eq!(
            f.comments,
            vec![
                "energy {\"energy_kwh\": 1.385e-06}".to_string(),
                "cost {\"r\": 1}".to_string(),
            ]
        );
        assert_eq!(f.data, "x\n[DONE]");
    }

    #[test]
    fn parses_multiline_data() {
        let f = parse_frame("data: line1\ndata: line2");
        assert_eq!(f.data, "line1\nline2");
    }

    #[test]
    fn done_sentinel() {
        let f = parse_frame("data: [DONE]");
        assert!(f.is_done());
    }

    #[test]
    fn take_frame_handles_crlf_and_remainder() {
        let mut buf = "event: x\r\ndata: y\r\n\r\n".to_string();
        buf = buf.replace("\r\n", "\n");
        let f = take_frame(&mut buf).unwrap();
        assert_eq!(f.event.as_deref(), Some("x"));
        assert_eq!(f.data, "y");
        assert_eq!(buf, "");

        let mut buf = "data: a\n\ndata: b".to_string();
        let f = take_frame(&mut buf).unwrap();
        assert_eq!(f.data, "a");
        assert_eq!(buf, "data: b");
    }

    #[test]
    fn feed_frames_splits_across_chunks_and_flush_handles_tail() {
        let mut buf = String::new();
        // first chunk carries a full frame plus the start of the next
        let f1 = feed_frames(&mut buf, b"event: a\ndata: {\"n\":1}\n\ndata: [D");
        assert_eq!(f1.len(), 1);
        assert_eq!(f1[0].event.as_deref(), Some("a"));
        // the partial `data: [D...` is still buffered
        assert!(!buf.is_empty(), "partial frame should stay buffered");

        // second chunk completes `[DONE]`
        let f2 = feed_frames(&mut buf, b"ONE]\n\n");
        assert_eq!(f2.len(), 1);
        assert!(f2[0].is_done());
        assert!(buf.is_empty(), "buffer should be drained after [DONE]");

        // a dangling tail with no trailing blank line is flushed at EOF
        let mut buf2 = String::new();
        feed_frames(&mut buf2, b"data: x");
        let tail = flush_frames(&mut buf2);
        assert_eq!(tail.unwrap().data, "x");
    }

    #[tokio::test]
    async fn unfolds_frames_from_bytes() {
        let body = "event: e\ndata: {\"n\":1}\n\ndata: [DONE]\n\n";
        let frames: Vec<_> = sse_frames(once(body)).map(|f| f.unwrap()).collect().await;
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event.as_deref(), Some("e"));
        assert!(frames[1].is_done());
    }
}
