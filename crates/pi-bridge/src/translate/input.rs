//! Translate codex `UserInput[]` (the inbound `turn/start.input`) into pi's
//! `prompt { message: String, images: ImageContent[] }` shape.
//!
//! Per the bridge design:
//! - `Text { text, .. }` → appended to the message buffer (newline-separated).
//! - `Image { url }` (data URL) → decoded into pi `ImageContent`.
//! - `LocalImage { path }` → file read + base64-encoded into pi `ImageContent`.
//! - `Skill { name, path }` → prefix `"/skill <name>\n"` so pi's slash-command
//!   handler picks it up. Path is informational.
//! - `Mention { name, path }` → expanded inline as `"@<name>"`.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use thiserror::Error;

use crate::codex_proto::items::UserInput;
use crate::pool::pi_protocol::ImageContent;

/// What pi's `prompt` (or `steer`/`follow_up`) command takes: a single message
/// string plus an optional vector of inline images.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PiPromptInput {
    pub message: String,
    pub images: Vec<ImageContent>,
}

#[derive(Debug, Error)]
pub enum InputTranslationError {
    #[error("data URL did not start with 'data:'")]
    NotADataUrl,

    #[error("data URL missing ';base64,' separator")]
    DataUrlMissingBase64,

    #[error("failed to base64-decode data URL payload: {0}")]
    Base64(#[from] base64::DecodeError),

    #[error("failed to read local image at {path}: {source}")]
    LocalImageRead {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("could not infer mime type for {0}; only common image extensions are supported")]
    UnknownImageMime(String),

    #[error("input vector was empty (codex requires at least one item)")]
    EmptyInput,
}

/// Translate a codex `Vec<UserInput>` into a pi prompt payload.
///
/// File I/O for `LocalImage` is performed eagerly here so the caller can
/// surface a clean error before the pi `prompt` command is dispatched.
pub fn translate_user_input(inputs: &[UserInput]) -> Result<PiPromptInput, InputTranslationError> {
    if inputs.is_empty() {
        return Err(InputTranslationError::EmptyInput);
    }

    let mut buffer = String::new();
    let mut images = Vec::new();
    for input in inputs {
        match input {
            UserInput::Text { text, .. } => {
                append_chunk(&mut buffer, text);
            }
            UserInput::Image { url } => {
                images.push(decode_data_url(url)?);
            }
            UserInput::LocalImage { path } => {
                images.push(read_local_image(path)?);
            }
            UserInput::Skill { name, .. } => {
                // Pi's slash-command handler dispatches on the leading
                // `/<name>` token. We don't include the path here — pi
                // resolves skills from its own resource loader.
                append_chunk(&mut buffer, &format!("/{name}"));
            }
            UserInput::Mention { name, .. } => {
                // Mentions are inline tokens, not their own line.
                if !buffer.is_empty() && !buffer.ends_with(' ') && !buffer.ends_with('\n') {
                    buffer.push(' ');
                }
                buffer.push('@');
                buffer.push_str(name);
            }
        }
    }

    Ok(PiPromptInput {
        message: buffer,
        images,
    })
}

/// Append `chunk` to `buffer`, separating with a newline iff `buffer` is not
/// empty and does not already end with whitespace.
fn append_chunk(buffer: &mut String, chunk: &str) {
    if buffer.is_empty() {
        buffer.push_str(chunk);
        return;
    }
    if !buffer.ends_with('\n') && !buffer.ends_with(' ') {
        buffer.push('\n');
    }
    buffer.push_str(chunk);
}

/// Decode an RFC 2397 `data:` URL into a pi `ImageContent`. Only the
/// `data:<mime>;base64,<payload>` form is accepted — pi expects base64.
fn decode_data_url(url: &str) -> Result<ImageContent, InputTranslationError> {
    let body = url
        .strip_prefix("data:")
        .ok_or(InputTranslationError::NotADataUrl)?;
    let (mime_section, payload) = body
        .split_once(',')
        .ok_or(InputTranslationError::DataUrlMissingBase64)?;
    let (mime_type, is_base64) = match mime_section.rsplit_once(';') {
        Some((mime, "base64")) => (mime, true),
        _ => (mime_section, false),
    };
    if !is_base64 {
        return Err(InputTranslationError::DataUrlMissingBase64);
    }
    // Some clients line-wrap base64 in data URLs; pre-strip ASCII whitespace
    // so we can re-encode canonically. Anything non-base64 still errors.
    let cleaned: String = payload
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let bytes = BASE64_STANDARD.decode(cleaned.as_bytes())?;
    let data = BASE64_STANDARD.encode(&bytes);
    let mime_type = if mime_type.is_empty() {
        "application/octet-stream".to_string()
    } else {
        mime_type.to_string()
    };
    Ok(ImageContent { data, mime_type })
}

/// Read a local image file and produce a pi `ImageContent`.
fn read_local_image(path: &Path) -> Result<ImageContent, InputTranslationError> {
    let bytes = fs::read(path).map_err(|source| InputTranslationError::LocalImageRead {
        path: path.display().to_string(),
        source,
    })?;
    let mime_type = guess_image_mime(path)
        .ok_or_else(|| InputTranslationError::UnknownImageMime(path.display().to_string()))?
        .to_string();
    Ok(ImageContent {
        data: BASE64_STANDARD.encode(&bytes),
        mime_type,
    })
}

/// Best-effort mime-type lookup based on file extension. We deliberately
/// support only the formats pi-ai's image content block can handle.
fn guess_image_mime(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => return None,
    })
}

/// Helper: turn a single text string into a `Vec<UserInput>` for callers that
/// only need to send a textual prompt (e.g. internal tests, synthetic
/// review-mode prompts).
pub fn text_only_input(text: impl Into<String>) -> Vec<UserInput> {
    vec![UserInput::Text {
        text: text.into(),
        text_elements: Vec::new(),
    }]
}

/// Wrap [`translate_user_input`] in [`anyhow::Result`] for handler call sites
/// that just want to bubble translation failures up alongside other errors.
pub fn translate_user_input_anyhow(inputs: &[UserInput]) -> Result<PiPromptInput> {
    translate_user_input(inputs)
        .map_err(|e| anyhow!(e))
        .context("translating turn input")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn text(s: &str) -> UserInput {
        UserInput::Text {
            text: s.into(),
            text_elements: Vec::new(),
        }
    }

    #[test]
    fn empty_input_errors() {
        let err = translate_user_input(&[]).unwrap_err();
        assert!(matches!(err, InputTranslationError::EmptyInput));
    }

    #[test]
    fn text_inputs_concatenated_with_newlines() {
        let result = translate_user_input(&[text("hello"), text("world")]).unwrap();
        assert_eq!(result.message, "hello\nworld");
        assert!(result.images.is_empty());
    }

    #[test]
    fn skill_becomes_slash_command() {
        let result = translate_user_input(&[
            UserInput::Skill {
                name: "review".into(),
                path: PathBuf::from("/skills/review"),
            },
            text("please look at this"),
        ])
        .unwrap();
        assert_eq!(result.message, "/review\nplease look at this");
    }

    #[test]
    fn mention_inlines_with_space_separator() {
        let result = translate_user_input(&[
            text("ping"),
            UserInput::Mention {
                name: "alice".into(),
                path: "@alice".into(),
            },
        ])
        .unwrap();
        assert_eq!(result.message, "ping @alice");
    }

    #[test]
    fn mention_at_start_has_no_leading_space() {
        let result = translate_user_input(&[UserInput::Mention {
            name: "alice".into(),
            path: "@alice".into(),
        }])
        .unwrap();
        assert_eq!(result.message, "@alice");
    }

    #[test]
    fn data_url_image_decoded_to_image_content() {
        // base64("hi") = "aGk="
        let url = "data:image/png;base64,aGk=";
        let result = translate_user_input(&[UserInput::Image { url: url.into() }]).unwrap();
        assert!(result.message.is_empty());
        assert_eq!(result.images.len(), 1);
        assert_eq!(result.images[0].mime_type, "image/png");
        assert_eq!(result.images[0].data, "aGk=");
    }

    #[test]
    fn data_url_strips_whitespace_in_payload() {
        // Some clients line-wrap base64; we re-canonicalize.
        let url = "data:image/png;base64,aGk\n=";
        let result = translate_user_input(&[UserInput::Image { url: url.into() }]).unwrap();
        assert_eq!(result.images[0].data, "aGk=");
    }

    #[test]
    fn non_base64_data_url_rejected() {
        let url = "data:image/png,raw-not-base64";
        let err = translate_user_input(&[UserInput::Image { url: url.into() }]).unwrap_err();
        assert!(matches!(err, InputTranslationError::DataUrlMissingBase64));
    }

    #[test]
    fn non_data_url_rejected() {
        let err = translate_user_input(&[UserInput::Image {
            url: "https://example.com/img.png".into(),
        }])
        .unwrap_err();
        assert!(matches!(err, InputTranslationError::NotADataUrl));
    }

    #[test]
    fn local_image_read_and_base64_encoded() {
        let tmp = tempfile::NamedTempFile::with_suffix(".png").unwrap();
        std::fs::write(tmp.path(), b"binary-bytes").unwrap();
        let result = translate_user_input(&[UserInput::LocalImage {
            path: tmp.path().to_path_buf(),
        }])
        .unwrap();
        assert_eq!(result.images.len(), 1);
        assert_eq!(result.images[0].mime_type, "image/png");
        let decoded = BASE64_STANDARD.decode(&result.images[0].data).unwrap();
        assert_eq!(decoded, b"binary-bytes");
    }

    #[test]
    fn local_image_unknown_extension_errors() {
        let tmp = tempfile::NamedTempFile::with_suffix(".xyz").unwrap();
        std::fs::write(tmp.path(), b"hi").unwrap();
        let err = translate_user_input(&[UserInput::LocalImage {
            path: tmp.path().to_path_buf(),
        }])
        .unwrap_err();
        assert!(matches!(err, InputTranslationError::UnknownImageMime(_)));
    }

    #[test]
    fn local_image_missing_file_errors() {
        let err = translate_user_input(&[UserInput::LocalImage {
            path: PathBuf::from("/nonexistent/missing.png"),
        }])
        .unwrap_err();
        assert!(matches!(err, InputTranslationError::LocalImageRead { .. }));
    }

    #[test]
    fn mixed_inputs_compose_correctly() {
        // base64("ok") = "b2s="
        let inputs = vec![
            text("hello"),
            UserInput::Mention {
                name: "bob".into(),
                path: "@bob".into(),
            },
            UserInput::Image {
                url: "data:image/jpeg;base64,b2s=".into(),
            },
            text("please?"),
            UserInput::Skill {
                name: "explain".into(),
                path: PathBuf::from("/skills/explain"),
            },
        ];
        let result = translate_user_input(&inputs).unwrap();
        assert_eq!(result.message, "hello @bob\nplease?\n/explain");
        assert_eq!(result.images.len(), 1);
        assert_eq!(result.images[0].mime_type, "image/jpeg");
    }

    #[test]
    fn text_only_input_helper_produces_one_text_input() {
        let v = text_only_input("hi");
        assert_eq!(v.len(), 1);
        match &v[0] {
            UserInput::Text {
                text,
                text_elements,
            } => {
                assert_eq!(text, "hi");
                assert!(text_elements.is_empty());
            }
            _ => panic!("expected Text"),
        }
    }
}
