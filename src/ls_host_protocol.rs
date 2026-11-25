//! Shared, closed Host protocol. Never accepts arbitrary LS RPC or instructions.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnRequest {
    pub session: String,
    pub profile: String,
    pub model: String,
    pub workspace: String,
    pub permission_mode: String,
    pub timeout_seconds: u64,
    pub history: Vec<Value>,
    pub text: String,
    pub text_items: Vec<String>,
    #[serde(default)]
    pub media: Vec<Value>,
    #[serde(default)]
    pub user_content: Value,
    pub stream: bool,
}

pub struct UserPayload {
    pub text_items: Vec<String>,
    pub media: Vec<Value>,
    pub content: Value,
}

pub fn user_payload(content: &Value) -> Result<UserPayload, String> {
    use base64::Engine;
    let blocks = match content {
        Value::String(s) => vec![serde_json::json!({"type":"text","text":s})],
        Value::Array(a) => a.clone(),
        _ => return Err("Invalid user content".into()),
    };
    let image_count = blocks.iter().filter(|block| block["type"] == "image").count();
    if image_count > 100 {
        return Err("At most 100 images per message are supported".into());
    }
    if blocks.len() - image_count > 100 {
        return Err("At most 100 non-image user content blocks are supported".into());
    }
    let mut canonical = Vec::new();
    let mut text_items = Vec::new();
    let mut media = Vec::new();
    let mut bytes = 0usize;
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => {
                let text = block["text"].as_str().ok_or("Invalid text block")?;
                bytes += text.len();
                text_items.push(text.to_owned());
                canonical.push(serde_json::json!({"type":"text","text":text}));
            }
            Some("image") => {
                let source = &block["source"];
                let mime = source["media_type"]
                    .as_str()
                    .ok_or("Image MIME type missing")?;
                if source["type"] != "base64"
                    || !matches!(
                        mime,
                        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
                    )
                {
                    return Err("LS images require inline base64 PNG, JPEG, WebP or GIF; URLs are not fetched".into());
                }
                let data = source["data"].as_str().ok_or("Image data missing")?;
                const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
                if data.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
                    return Err("Image exceeds 10 MiB".into());
                }
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|_| "Invalid image base64")?;
                if decoded.len() > MAX_IMAGE_BYTES {
                    return Err("Image exceeds 10 MiB".into());
                }
                let valid = match mime {
                    "image/png" => decoded.starts_with(b"\x89PNG\r\n\x1a\n"),
                    "image/jpeg" => decoded.starts_with(b"\xff\xd8\xff"),
                    "image/gif" => decoded.starts_with(b"GIF87a") || decoded.starts_with(b"GIF89a"),
                    "image/webp" => {
                        decoded.starts_with(b"RIFF")
                            && decoded.get(8..12) == Some(b"WEBP".as_slice())
                    }
                    _ => false,
                };
                if !valid {
                    return Err("Image content does not match its MIME type".into());
                }
                bytes += decoded.len();
                media.push(serde_json::json!({"inlineData":data,"mimeType":mime}));
                canonical.push(serde_json::json!({"type":"image","source":{"type":"base64","media_type":mime,"data":data}}));
            }
            Some("document") => {
                let source = &block["source"];
                if source["media_type"] == "application/pdf" {
                    if source["type"] != "base64" {
                        return Err("LS PDF attachments require inline base64; URLs and file references are not fetched".into());
                    }
                    let data = source["data"].as_str().ok_or("PDF data missing")?;
                    const MAX_PDF_BYTES: usize = 10 * 1024 * 1024;
                    if data.len() > MAX_PDF_BYTES.div_ceil(3) * 4 {
                        return Err("PDF exceeds 10 MiB".into());
                    }
                    let decoded = base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .map_err(|_| "Invalid PDF base64")?;
                    if decoded.len() > MAX_PDF_BYTES {
                        return Err("PDF exceeds 10 MiB".into());
                    }
                    if !decoded.starts_with(b"%PDF-") {
                        return Err("PDF content does not match its MIME type".into());
                    }
                    let title = block["title"].as_str().unwrap_or("attachment.pdf");
                    if title.len() > 4096 {
                        return Err("Attachment title too long".into());
                    }
                    // Display names are filenames, not caller-controlled artifact paths.
                    let name: String = title
                        .rsplit(['/', '\\'])
                        .next()
                        .unwrap_or("")
                        .trim()
                        .chars()
                        .filter(|c| !c.is_control())
                        .map(|c| {
                            if c.is_whitespace() || c == '%' {
                                '_'
                            } else {
                                c
                            }
                        })
                        .take(255)
                        .collect();
                    let name = if matches!(name.as_str(), "" | "." | "..") {
                        "attachment.pdf"
                    } else {
                        &name
                    };
                    bytes += decoded.len() + title.len();
                    if bytes > 32 * 1024 * 1024 {
                        return Err("User input and attachments exceed 32 MiB".into());
                    }
                    media.push(serde_json::json!({"mimeType":"application/pdf","inlineData":data,"description":title,"displayName":name}));
                    canonical.push(serde_json::json!({"type":"document","title":title,"source":{"type":"base64","media_type":"application/pdf","data":data}}));
                    continue;
                }
                if source["type"] != "text" || source["media_type"] != "text/plain" {
                    return Err("Only inline text/plain document attachments are supported; no file or URL content is silently dropped".into());
                }
                let data = source["data"].as_str().ok_or("Document text missing")?;
                if data.contains('\0') {
                    return Err("Binary data cannot be treated as a text attachment".into());
                }
                let title = block
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("attachment.txt");
                if title.len() > 4096 {
                    return Err("Attachment title too long".into());
                }
                bytes += data.len() + title.len();
                text_items.push(format!(
                    "[File {}]\n{}\n[/File]",
                    serde_json::to_string(title).unwrap(),
                    data
                ));
                canonical.push(serde_json::json!({"type":"document","title":title,"source":{"type":"text","media_type":"text/plain","data":data}}));
            }
            _ => {
                return Err(
                    "Unsupported user block; client tool results are not original input".into(),
                )
            }
        }
        if bytes > 32 * 1024 * 1024 {
            return Err("User input and attachments exceed 32 MiB".into());
        }
    }
    if media.is_empty() && text_items.iter().all(|t| t.trim().is_empty()) {
        return Err("Empty user input".into());
    }
    let content = Value::Array(canonical);
    Ok(UserPayload {
        text_items,
        media,
        content,
    })
}

pub fn history_content(payload: &UserPayload) -> Value {
    if payload
        .content
        .as_array()
        .is_some_and(|a| a.iter().all(|b| b["type"] == "text"))
    {
        Value::String(payload.text_items.concat())
    } else {
        payload.content.clone()
    }
}

pub fn normalize_history(messages: &[Value]) -> Result<Vec<Value>, String> {
    messages
        .iter()
        .map(|m| {
            let role = m["role"].as_str().ok_or("Missing role")?;
            if role != "user" && role != "assistant" {
                return Err("Unsupported history role".into());
            }
            if role == "user" {
                return Ok(serde_json::json!({"role":"user","content":history_content(&user_payload(&m["content"])?) }));
            }
            let text = match &m["content"] {
                Value::String(t) => t.clone(),
                Value::Array(b)
                    if b.iter()
                        .all(|v| (v["type"] == "text" && v["text"].is_string())
                            || (role == "assistant" && v["type"] == "thinking" && v["thinking"].is_string())) =>
                {
                    b.iter()
                        .filter_map(|v| v["text"].as_str())
                        .collect::<String>()
                }
                _ => return Err("Only text history is accepted; no tool replay".into()),
            };
            Ok(serde_json::json!({"role":role,"content":text}))
        })
        .collect()
}
