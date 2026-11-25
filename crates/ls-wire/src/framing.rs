//! Incremental Connect JSON (`application/connect+json`) framing codec.
//!
//! Preserves valid messages decoded before a terminal error frame and keeps
//! terminal errors sticky across subsequent `push` / `finish` calls.

use crate::MAX_CONNECT_FRAME_BYTES;
use serde_json::Value;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    DataAfterEndFrame,
    UnsupportedFlags(u8),
    FrameExceedsBound(usize),
    InvalidJson(String),
    EndStreamError {
        code: String,
        message: Option<String>,
    },
    TruncatedFrame,
    MissingEndFrame,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DataAfterEndFrame => write!(f, "data received after Connect end frame"),
            Self::UnsupportedFlags(flags) => write!(f, "unsupported Connect frame flags: {flags}"),
            Self::FrameExceedsBound(len) => {
                write!(
                    f,
                    "Connect frame length {len} exceeds {MAX_CONNECT_FRAME_BYTES} byte limit"
                )
            }
            Self::InvalidJson(msg) => write!(f, "invalid JSON in Connect frame: {msg}"),
            Self::EndStreamError { code, message } => match message {
                Some(msg) if !msg.is_empty() => {
                    write!(f, "Connect end frame reported error code={code}: {msg}")
                }
                _ => write!(f, "Connect end frame reported error code={code}"),
            },
            Self::TruncatedFrame => write!(f, "truncated Connect frame at EOF"),
            Self::MissingEndFrame => write!(f, "stream ended without Connect end frame"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Encodes a JSON value into an uncompressed Connect envelope frame (`flags = 0`).
pub fn encode_connect_frame(value: &Value) -> Result<Vec<u8>, FrameError> {
    let payload =
        serde_json::to_vec(value).map_err(|err| FrameError::InvalidJson(err.to_string()))?;
    if payload.len() > MAX_CONNECT_FRAME_BYTES {
        return Err(FrameError::FrameExceedsBound(payload.len()));
    }
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(0);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Incremental decoder for `application/connect+json` streams.
#[derive(Default, Debug)]
pub struct ConnectFrameDecoder {
    buf: Vec<u8>,
    ended: bool,
    failure: Option<FrameError>,
}

/// Valid messages preceding a terminal error must be consumed before handling the error.
#[derive(Debug, PartialEq)]
pub struct DecodeFailure {
    pub messages: Vec<Value>,
    pub error: FrameError,
}

impl ConnectFrameDecoder {
    pub fn ended(&self) -> bool {
        self.ended
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>, DecodeFailure> {
        let mut messages = Vec::new();
        if let Some(error) = &self.failure {
            return Err(DecodeFailure {
                messages,
                error: error.clone(),
            });
        }
        match self.decode(bytes, &mut messages) {
            Ok(()) => Ok(messages),
            Err(error) => {
                self.failure = Some(error.clone());
                self.buf.clear();
                Err(DecodeFailure { messages, error })
            }
        }
    }

    fn decode(&mut self, bytes: &[u8], messages: &mut Vec<Value>) -> Result<(), FrameError> {
        if self.ended && !bytes.is_empty() {
            return Err(FrameError::DataAfterEndFrame);
        }
        self.buf.extend_from_slice(bytes);

        while self.buf.len() >= 5 {
            let flags = self.buf[0];
            if flags != 0 && flags != 2 {
                return Err(FrameError::UnsupportedFlags(flags));
            }
            let len =
                u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
            if len > MAX_CONNECT_FRAME_BYTES {
                return Err(FrameError::FrameExceedsBound(len));
            }
            if self.buf.len() < 5 + len {
                break;
            }
            let value: Value = serde_json::from_slice(&self.buf[5..5 + len])
                .map_err(|err| FrameError::InvalidJson(err.to_string()))?;
            self.buf.drain(..5 + len);

            if flags == 2 {
                self.ended = true;
                if !self.buf.is_empty() {
                    return Err(FrameError::DataAfterEndFrame);
                }
                if let Some(error) = value.get("error").filter(|v| !v.is_null()) {
                    let code = error
                        .get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned();
                    let message = error
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    return Err(FrameError::EndStreamError { code, message });
                }
            } else {
                messages.push(value);
            }
        }

        if self.buf.len() > MAX_CONNECT_FRAME_BYTES + 5 {
            return Err(FrameError::FrameExceedsBound(self.buf.len()));
        }
        Ok(())
    }

    pub fn finish(&self) -> Result<(), FrameError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if !self.buf.is_empty() {
            return Err(FrameError::TruncatedFrame);
        }
        if !self.ended {
            return Err(FrameError::MissingEndFrame);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_across_every_chunk_split_boundary() {
        let mut stream = encode_connect_frame(&json!({"update": {"seq": 1}})).unwrap();
        stream.extend(encode_connect_frame(&json!({"update": {"seq": 2}})).unwrap());
        let mut end = encode_connect_frame(&json!({})).unwrap();
        end[0] = 2;
        stream.extend(end);

        for split in 0..=stream.len() {
            let mut decoder = ConnectFrameDecoder::default();
            let mut decoded = decoder.push(&stream[..split]).unwrap();
            decoded.extend(decoder.push(&stream[split..]).unwrap());
            assert_eq!(decoded.len(), 2);
            assert!(decoder.ended());
            decoder.finish().unwrap();
        }
    }

    #[test]
    fn rejects_invalid_flags_oversized_frames_and_end_errors() {
        assert_eq!(
            ConnectFrameDecoder::default()
                .push(&[1, 0, 0, 0, 0])
                .map_err(|e| e.error),
            Err(FrameError::UnsupportedFlags(1))
        );
        assert!(matches!(
            ConnectFrameDecoder::default()
                .push(&[0, 0xff, 0xff, 0xff, 0xff])
                .map_err(|e| e.error),
            Err(FrameError::FrameExceedsBound(_))
        ));

        let mut end_err =
            encode_connect_frame(&json!({"error": {"code": "canceled", "message": "stop"}}))
                .unwrap();
        end_err[0] = 2;
        assert_eq!(
            ConnectFrameDecoder::default()
                .push(&end_err)
                .map_err(|e| e.error),
            Err(FrameError::EndStreamError {
                code: "canceled".into(),
                message: Some("stop".into()),
            })
        );

        let mut truncated = ConnectFrameDecoder::default();
        truncated.push(&[0, 0, 0]).unwrap();
        assert_eq!(truncated.finish(), Err(FrameError::TruncatedFrame));

        let mut no_end = ConnectFrameDecoder::default();
        no_end
            .push(&encode_connect_frame(&json!({"ok": true})).unwrap())
            .unwrap();
        assert_eq!(no_end.finish(), Err(FrameError::MissingEndFrame));
    }

    #[test]
    fn preserves_messages_before_errors_at_every_split() {
        let expected = json!({"update": "observed"});
        let mut end = encode_connect_frame(&json!({"error":{"code":"canceled"}})).unwrap();
        end[0] = 2;
        for tail in [end, vec![1, 0, 0, 0, 0]] {
            let mut stream = encode_connect_frame(&expected).unwrap();
            stream.extend(tail);
            for split in 0..=stream.len() {
                let mut decoder = ConnectFrameDecoder::default();
                let mut observed = Vec::new();
                let mut terminal = None;
                for chunk in [&stream[..split], &stream[split..]] {
                    match decoder.push(chunk) {
                        Ok(messages) => observed.extend(messages),
                        Err(failure) => {
                            observed.extend(failure.messages);
                            terminal = Some(failure.error);
                        }
                    }
                }
                assert_eq!(observed, vec![expected.clone()]);
                assert_eq!(decoder.finish(), Err(terminal.unwrap()));
                assert!(decoder.push(&[]).unwrap_err().messages.is_empty());
            }
        }
    }
}
