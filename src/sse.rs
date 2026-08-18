use std::pin::Pin;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};

/// A single parsed SSE frame: optional event name and data payload.
#[derive(Debug, Clone)]
pub struct SseFrame {
    /// Optional SSE `event:` name, if present in the frame.
    pub event: Option<String>,
    /// The frame's `data:` payload.
    pub data: String,
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
    Read(reqwest::Error),
}

struct FrameState {
    stream: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    buf: String,
    eof: bool,
}

/// Adapt a reqwest streaming response into a stream of SSE frames. Handles
/// `event:`/`data:` lines, multi-line data, CRLF line endings and the
/// trailing `[DONE]` sentinel.
pub fn sse_frames(
    resp: reqwest::Response,
) -> impl Stream<Item = Result<SseFrame, SseError>> + Send {
    let state = FrameState {
        stream: Box::pin(resp.bytes_stream()),
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

fn parse_frame(text: &str) -> SseFrame {
    let mut event = None;
    let mut data = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
        // comments (lines starting with ':') and unknown fields are ignored
    }
    SseFrame { event, data }
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

    #[tokio::test]
    async fn unfolds_frames_from_bytes() {
        let body = "event: e\ndata: {\"n\":1}\n\ndata: [DONE]\n\n";
        let inner = http::Response::builder()
            .status(200)
            .body(reqwest::Body::from(body))
            .unwrap();
        let resp = reqwest::Response::from(inner);
        let frames: Vec<_> = sse_frames(resp).map(|f| f.unwrap()).collect().await;
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event.as_deref(), Some("e"));
        assert!(frames[1].is_done());
    }
}
