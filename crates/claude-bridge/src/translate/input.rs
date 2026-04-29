//! Translate codex `Vec<UserInput>` (the inbound `turn/start.input`) into a
//! claude `stream-json` user message envelope.
//!
//! Per the bridge plan:
//! - `Text { text, .. }` → one `ClaudeUserContentBlock::Text`.
//! - `Image { url }` (data URL) → decoded into a base64 image block.
//! - `LocalImage { path }` → file read + base64-encoded into an image block.
//! - `Skill { name, path }` → prefix `/<name>\n` text block (claude's slash-
//!   command parser dispatches on a leading `/`).
//! - `Mention { name, path }` → inline `@<name>` token, joined to the
//!   surrounding text.
//!
//! When the input collapses to a single text block we emit `content` as a
//! plain string (claude accepts both string and array forms — the string form
//! keeps wire bytes / on-disk JSONL noise lower for the common case).

use std::fs;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use thiserror::Error;

use alleycat_codex_proto::UserInput;

use crate::pool::claude_protocol::{
    ClaudeImageBlock, ClaudeImageSource, ClaudeInbound, ClaudeTextBlock, ClaudeUserContent,
    ClaudeUserContentBlock, ClaudeUserMessage, ClaudeUserMessageEnvelope, ClaudeUserRole,
};

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

/// Translate a codex `Vec<UserInput>` into a claude inbound envelope ready to
/// be `serde_json::to_string`'d and pushed to claude's stdin.
pub fn translate_user_input(inputs: &[UserInput]) -> Result<ClaudeInbound, InputTranslationError> {
    if inputs.is_empty() {
        return Err(InputTranslationError::EmptyInput);
    }

    // Build one text accumulator + a sequence of image blocks. Text from
    // multiple `Text`/`Skill`/`Mention` inputs is concatenated; images
    // interleave into the block list at their original position.
    let mut blocks: Vec<ClaudeUserContentBlock> = Vec::new();
    let mut text_buffer = String::new();

    fn flush_text(blocks: &mut Vec<ClaudeUserContentBlock>, buf: &mut String) {
        if buf.is_empty() {
            return;
        }
        blocks.push(ClaudeUserContentBlock::Text(ClaudeTextBlock {
            text: std::mem::take(buf),
        }));
    }

    for input in inputs {
        match input {
            UserInput::Text { text, .. } => append_chunk(&mut text_buffer, text),
            UserInput::Skill { name, .. } => append_chunk(&mut text_buffer, &format!("/{name}")),
            UserInput::Mention { name, .. } => {
                if !text_buffer.is_empty()
                    && !text_buffer.ends_with(' ')
                    && !text_buffer.ends_with('\n')
                {
                    text_buffer.push(' ');
                }
                text_buffer.push('@');
                text_buffer.push_str(name);
            }
            UserInput::Image { url } => {
                flush_text(&mut blocks, &mut text_buffer);
                blocks.push(image_from_data_url(url)?);
            }
            UserInput::LocalImage { path } => {
                flush_text(&mut blocks, &mut text_buffer);
                blocks.push(image_from_local_file(path)?);
            }
        }
    }
    flush_text(&mut blocks, &mut text_buffer);

    // Collapse the trivial single-text-block case into the string form claude
    // also accepts. Keeps the on-disk transcript readable.
    let content = match blocks.as_slice() {
        [ClaudeUserContentBlock::Text(t)] => ClaudeUserContent::Text(t.text.clone()),
        _ => ClaudeUserContent::Blocks(blocks),
    };

    Ok(ClaudeInbound::User(ClaudeUserMessageEnvelope {
        message: ClaudeUserMessage {
            role: ClaudeUserRole::User,
            content,
        },
        parent_tool_use_id: None,
    }))
}

/// Append `chunk` to `buffer`, separating with `\n` iff `buffer` is not empty
/// and does not already end with whitespace.
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

fn image_from_data_url(url: &str) -> Result<ClaudeUserContentBlock, InputTranslationError> {
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
    // Some clients line-wrap base64; canonicalize before re-encoding.
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
    Ok(ClaudeUserContentBlock::Image(ClaudeImageBlock {
        source: ClaudeImageSource::Base64 {
            media_type: mime_type,
            data,
        },
    }))
}

fn image_from_local_file(path: &Path) -> Result<ClaudeUserContentBlock, InputTranslationError> {
    let bytes = fs::read(path).map_err(|source| InputTranslationError::LocalImageRead {
        path: path.display().to_string(),
        source,
    })?;
    let mime_type = guess_image_mime(path)
        .ok_or_else(|| InputTranslationError::UnknownImageMime(path.display().to_string()))?
        .to_string();
    Ok(ClaudeUserContentBlock::Image(ClaudeImageBlock {
        source: ClaudeImageSource::Base64 {
            media_type: mime_type,
            data: BASE64_STANDARD.encode(&bytes),
        },
    }))
}

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

    fn assert_text_only(env: &ClaudeInbound, expected: &str) {
        let ClaudeInbound::User(env) = env else {
            panic!("expected User envelope");
        };
        match &env.message.content {
            ClaudeUserContent::Text(s) => assert_eq!(s, expected),
            other => panic!("expected Text content, got {other:?}"),
        }
    }

    #[test]
    fn empty_input_errors() {
        let err = translate_user_input(&[]).unwrap_err();
        assert!(matches!(err, InputTranslationError::EmptyInput));
    }

    #[test]
    fn single_text_collapses_to_string_content() {
        let env = translate_user_input(&[text("hello")]).unwrap();
        assert_text_only(&env, "hello");
    }

    #[test]
    fn multiple_text_inputs_join_with_newline() {
        let env = translate_user_input(&[text("hi"), text("there")]).unwrap();
        assert_text_only(&env, "hi\nthere");
    }

    #[test]
    fn skill_becomes_slash_command_text() {
        let env = translate_user_input(&[
            UserInput::Skill {
                name: "review".into(),
                path: PathBuf::from("/skills/review"),
            },
            text("please look"),
        ])
        .unwrap();
        assert_text_only(&env, "/review\nplease look");
    }

    #[test]
    fn mention_inlines_with_space_separator() {
        let env = translate_user_input(&[
            text("ping"),
            UserInput::Mention {
                name: "alice".into(),
                path: "@alice".into(),
            },
        ])
        .unwrap();
        assert_text_only(&env, "ping @alice");
    }

    #[test]
    fn data_url_image_decoded_into_base64_block() {
        // base64("hi") = "aGk="
        let env = translate_user_input(&[UserInput::Image {
            url: "data:image/png;base64,aGk=".into(),
        }])
        .unwrap();
        let ClaudeInbound::User(env) = env else {
            panic!("expected User envelope");
        };
        match &env.message.content {
            ClaudeUserContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                match &blocks[0] {
                    ClaudeUserContentBlock::Image(img) => match &img.source {
                        ClaudeImageSource::Base64 { media_type, data } => {
                            assert_eq!(media_type, "image/png");
                            assert_eq!(data, "aGk=");
                        }
                        other => panic!("expected base64 source, got {other:?}"),
                    },
                    other => panic!("expected image block, got {other:?}"),
                }
            }
            other => panic!("expected Blocks content, got {other:?}"),
        }
    }

    #[test]
    fn data_url_strips_whitespace_in_payload() {
        let env = translate_user_input(&[UserInput::Image {
            url: "data:image/png;base64,aGk\n=".into(),
        }])
        .unwrap();
        let ClaudeInbound::User(env) = env else {
            panic!("expected User envelope");
        };
        let ClaudeUserContent::Blocks(blocks) = env.message.content else {
            panic!("expected blocks");
        };
        let ClaudeUserContentBlock::Image(img) = &blocks[0] else {
            panic!("expected image");
        };
        let ClaudeImageSource::Base64 { data, .. } = &img.source else {
            panic!("expected base64");
        };
        assert_eq!(data, "aGk=");
    }

    #[test]
    fn non_base64_data_url_rejected() {
        let err = translate_user_input(&[UserInput::Image {
            url: "data:image/png,raw".into(),
        }])
        .unwrap_err();
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
        let env = translate_user_input(&[UserInput::LocalImage {
            path: tmp.path().to_path_buf(),
        }])
        .unwrap();
        let ClaudeInbound::User(env) = env else {
            panic!("expected User envelope");
        };
        let ClaudeUserContent::Blocks(blocks) = env.message.content else {
            panic!("expected blocks");
        };
        let ClaudeUserContentBlock::Image(img) = &blocks[0] else {
            panic!("expected image");
        };
        let ClaudeImageSource::Base64 { media_type, data } = &img.source else {
            panic!("expected base64");
        };
        assert_eq!(media_type, "image/png");
        let decoded = BASE64_STANDARD.decode(data).unwrap();
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
    fn mixed_text_and_image_yields_blocks_in_order() {
        let env = translate_user_input(&[
            text("look"),
            UserInput::Image {
                url: "data:image/png;base64,aGk=".into(),
            },
            text("see?"),
        ])
        .unwrap();
        let ClaudeInbound::User(env) = env else {
            panic!("expected User envelope");
        };
        let ClaudeUserContent::Blocks(blocks) = env.message.content else {
            panic!("expected blocks");
        };
        assert_eq!(blocks.len(), 3);
        assert!(matches!(&blocks[0], ClaudeUserContentBlock::Text(t) if t.text == "look"));
        assert!(matches!(&blocks[1], ClaudeUserContentBlock::Image(_)));
        assert!(matches!(&blocks[2], ClaudeUserContentBlock::Text(t) if t.text == "see?"));
    }
}
