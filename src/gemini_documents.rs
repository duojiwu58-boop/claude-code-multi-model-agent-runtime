#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GeminiDocumentKind {
    Paginated,
    Spreadsheet,
    PlainText,
}

fn gemini_document_mime_from_extension(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        "pdf" => Some("application/pdf"),
        "docx" => Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
        "doc" => Some("application/msword"),
        "dotx" => Some("application/vnd.openxmlformats-officedocument.wordprocessingml.template"),
        "odt" => Some("application/vnd.oasis.opendocument.text"),
        "pptx" => Some("application/vnd.openxmlformats-officedocument.presentationml.presentation"),
        "ppt" => Some("application/vnd.ms-powerpoint"),
        "odp" => Some("application/vnd.oasis.opendocument.presentation"),
        "xlsx" => Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
        "xls" => Some("application/vnd.ms-excel"),
        "ods" => Some("application/vnd.oasis.opendocument.spreadsheet"),
        "txt" => Some("text/plain"),
        "md" | "markdown" => Some("text/markdown"),
        "csv" => Some("text/csv"),
        "html" | "htm" => Some("text/html"),
        "xml" => Some("text/xml"),
        "json" => Some("application/json"),
        _ => None,
    }
}

fn gemini_document_mime_from_path_or_url(path_or_url: &str) -> Option<&'static str> {
    let candidate = if let Ok(parsed) = url::Url::parse(path_or_url) {
        parsed.path().to_string()
    } else {
        path_or_url
            .split(['?', '#'])
            .next()
            .unwrap_or(path_or_url)
            .to_string()
    };
    let ext = Path::new(&candidate).extension()?.to_str()?;
    gemini_document_mime_from_extension(ext)
}

fn gemini_document_kind_for_mime(mime_type: &str) -> Option<GeminiDocumentKind> {
    let base = mime_type
        .split(';')
        .next()
        .unwrap_or(mime_type)
        .trim()
        .to_ascii_lowercase();
    match base.as_str() {
        "application/pdf"
        | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        | "application/msword"
        | "application/vnd.openxmlformats-officedocument.wordprocessingml.template"
        | "application/vnd.oasis.opendocument.text"
        | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        | "application/vnd.ms-powerpoint"
        | "application/vnd.oasis.opendocument.presentation" => Some(GeminiDocumentKind::Paginated),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        | "application/vnd.ms-excel"
        | "application/vnd.oasis.opendocument.spreadsheet" => Some(GeminiDocumentKind::Spreadsheet),
        "application/json" | "application/xml" => Some(GeminiDocumentKind::PlainText),
        value if value.starts_with("text/") => Some(GeminiDocumentKind::PlainText),
        _ => None,
    }
}

fn validate_gemini_media_resolution(value: &str) -> Result<&'static str, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "low" => Ok("low"),
        "medium" => Ok("medium"),
        "high" => Ok("high"),
        "ultra_high" => Ok("ultra_high"),
        other => Err(format!(
            "Unsupported Gemini document resolution '{other}' (expected low, medium, high, or ultra_high)"
        )),
    }
}

fn is_valid_a1_ref_part(part: &str) -> bool {
    if part.is_empty() || part.len() > 24 {
        return false;
    }
    let bytes = part.as_bytes();
    let mut index = 0;
    if bytes.get(index) == Some(&b'$') {
        index += 1;
    }
    let col_start = index;
    while index < bytes.len() && bytes[index].is_ascii_alphabetic() {
        index += 1;
    }
    let col_len = index - col_start;
    if col_len > 3 {
        return false;
    }
    if bytes.get(index) == Some(&b'$') {
        index += 1;
    }
    let row_start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    let row_len = index - row_start;
    if index != bytes.len() || (col_len == 0 && row_len == 0) {
        return false;
    }
    if row_len > 0 && part[row_start..].parse::<u64>().ok().unwrap_or(0) == 0 {
        return false;
    }
    true
}

fn is_valid_a1_cell_range(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > 64 {
        return false;
    }
    let mut parts = trimmed.split(':');
    let Some(first) = parts.next() else {
        return false;
    };
    let Some(second) = parts.next() else {
        return is_valid_a1_ref_part(first)
            && first.bytes().any(|b| b.is_ascii_alphabetic())
            && first.bytes().any(|b| b.is_ascii_digit());
    };
    if parts.next().is_some() {
        return false;
    }
    if !is_valid_a1_ref_part(first) || !is_valid_a1_ref_part(second) {
        return false;
    }
    let first_has_col = first.bytes().any(|b| b.is_ascii_alphabetic());
    let first_has_row = first.bytes().any(|b| b.is_ascii_digit());
    let second_has_col = second.bytes().any(|b| b.is_ascii_alphabetic());
    let second_has_row = second.bytes().any(|b| b.is_ascii_digit());
    (first_has_col == second_has_col) && (first_has_row == second_has_row)
}

fn validate_gemini_page_options(
    options: &Value,
    mime_type: &str,
) -> Result<Value, String> {
    if gemini_document_kind_for_mime(mime_type) != Some(GeminiDocumentKind::Paginated) {
        return Err(format!(
            "Gemini page_options is only valid for paginated documents (PDF, Word, PowerPoint, ODT, ODP), not '{mime_type}'"
        ));
    }
    let object = options
        .as_object()
        .ok_or_else(|| "Gemini processing.page_options must be a JSON object".to_string())?;
    let mut normalized = Map::new();
    let start_page = match object.get("start_page") {
        Some(value) if !value.is_null() => {
            let page = value
                .as_u64()
                .filter(|page| *page >= 1)
                .ok_or_else(|| "Gemini page_options.start_page must be an integer >= 1".to_string())?;
            normalized.insert("start_page".to_string(), json!(page));
            Some(page)
        }
        _ => None,
    };
    let end_page = match object.get("end_page") {
        Some(value) if !value.is_null() => {
            let page = value
                .as_u64()
                .filter(|page| *page >= 1)
                .ok_or_else(|| "Gemini page_options.end_page must be an integer >= 1".to_string())?;
            normalized.insert("end_page".to_string(), json!(page));
            Some(page)
        }
        _ => None,
    };
    if let (Some(start), Some(end)) = (start_page, end_page) {
        if end < start {
            return Err(format!(
                "Gemini page_options.end_page ({end}) must be >= start_page ({start})"
            ));
        }
    }
    if let Some(layers) = object.get("content_layers").filter(|value| !value.is_null()) {
        let layers = layers
            .as_array()
            .ok_or_else(|| "Gemini page_options.content_layers must be an array".to_string())?;
        if layers.is_empty() {
            return Err("Gemini page_options.content_layers must not be empty".to_string());
        }
        let mut normalized_layers = Vec::with_capacity(layers.len());
        for layer in layers {
            let layer = layer
                .as_str()
                .ok_or_else(|| "Gemini page_options.content_layers entries must be strings".to_string())?;
            let canonical = match layer.trim().to_ascii_lowercase().as_str() {
                "page_image" => "page_image",
                "page_text" => "page_text",
                other => {
                    return Err(format!(
                        "Unsupported Gemini page_options.content_layers value '{other}' (expected page_image or page_text)"
                    ))
                }
            };
            if !normalized_layers.iter().any(|existing| existing == canonical) {
                normalized_layers.push(canonical.to_string());
            }
        }
        normalized.insert("content_layers".to_string(), json!(normalized_layers));
    }
    if normalized.is_empty() {
        return Err(
            "Gemini page_options must specify at least one of start_page, end_page, or content_layers"
                .to_string(),
        );
    }
    Ok(Value::Object(normalized))
}

fn validate_gemini_spreadsheet_options(
    options: &Value,
    mime_type: &str,
) -> Result<Value, String> {
    if gemini_document_kind_for_mime(mime_type) != Some(GeminiDocumentKind::Spreadsheet) {
        return Err(format!(
            "Gemini spreadsheet_options is only valid for spreadsheets (.xlsx, .xls, .ods), not '{mime_type}'"
        ));
    }
    let object = options
        .as_object()
        .ok_or_else(|| "Gemini processing.spreadsheet_options must be a JSON object".to_string())?;
    let mut normalized = Map::new();
    if let Some(value) = object.get("sheet_index").filter(|value| !value.is_null()) {
        let index = value
            .as_u64()
            .filter(|index| *index >= 1)
            .ok_or_else(|| {
                "Gemini spreadsheet_options.sheet_index must be an integer >= 1".to_string()
            })?;
        normalized.insert("sheet_index".to_string(), json!(index));
    }
    if let Some(value) = object.get("visible_cells").filter(|value| !value.is_null()) {
        let range = value
            .as_str()
            .map(str::trim)
            .filter(|range| is_valid_a1_cell_range(range))
            .ok_or_else(|| {
                "Gemini spreadsheet_options.visible_cells must be a valid A1 cell or range (for example 'A1:G40' or 'B5')"
                    .to_string()
            })?;
        normalized.insert("visible_cells".to_string(), json!(range));
    }
    if let Some(layers) = object.get("content_layers").filter(|value| !value.is_null()) {
        let layers = layers.as_array().ok_or_else(|| {
            "Gemini spreadsheet_options.content_layers must be an array".to_string()
        })?;
        if layers.is_empty() {
            return Err("Gemini spreadsheet_options.content_layers must not be empty".to_string());
        }
        let mut normalized_layers = Vec::with_capacity(layers.len());
        for layer in layers {
            let layer = layer.as_str().ok_or_else(|| {
                "Gemini spreadsheet_options.content_layers entries must be strings".to_string()
            })?;
            let canonical = match layer.trim().to_ascii_lowercase().as_str() {
                "data" => "data",
                "formulas" => "formulas",
                "visual" => "visual",
                other => {
                    return Err(format!(
                        "Unsupported Gemini spreadsheet_options.content_layers value '{other}' (expected data, formulas, or visual)"
                    ))
                }
            };
            if !normalized_layers.iter().any(|existing| existing == canonical) {
                normalized_layers.push(canonical.to_string());
            }
        }
        normalized.insert("content_layers".to_string(), json!(normalized_layers));
    }
    if normalized.is_empty() {
        return Err(
            "Gemini spreadsheet_options must specify at least one of sheet_index, visible_cells, or content_layers"
                .to_string(),
        );
    }
    Ok(Value::Object(normalized))
}

fn build_gemini_document_processing(
    mime_type: &str,
    raw_processing: Option<&Value>,
    raw_page_options: Option<&Value>,
    raw_spreadsheet_options: Option<&Value>,
) -> Result<Option<Value>, String> {
    let mut page_options = raw_page_options.filter(|value| !value.is_null());
    let mut spreadsheet_options = raw_spreadsheet_options.filter(|value| !value.is_null());
    let mut explicit_static = false;

    if let Some(processing) = raw_processing.filter(|value| !value.is_null()) {
        if let Some(mode) = processing.as_str() {
            if !mode.eq_ignore_ascii_case("static") {
                return Err(format!(
                    "Unsupported Gemini document processing mode '{mode}' (expected 'static')"
                ));
            }
            explicit_static = true;
        } else if let Some(object) = processing.as_object() {
            if let Some(kind) = object.get("type").and_then(Value::as_str) {
                if !kind.eq_ignore_ascii_case("static") {
                    return Err(format!(
                        "Unsupported Gemini document processing.type '{kind}' (expected 'static')"
                    ));
                }
            } else if object.get("type").is_some_and(|value| !value.is_null()) {
                return Err("Gemini document processing.type must be 'static'".to_string());
            }
            explicit_static = true;
            if page_options.is_none() {
                page_options = object.get("page_options").filter(|value| !value.is_null());
            }
            if spreadsheet_options.is_none() {
                spreadsheet_options = object
                    .get("spreadsheet_options")
                    .filter(|value| !value.is_null());
            }
        } else {
            return Err("Gemini document processing must be an object or 'static'".to_string());
        }
    }

    if page_options.is_some() && spreadsheet_options.is_some() {
        return Err(
            "Gemini document processing cannot set both page_options and spreadsheet_options"
                .to_string(),
        );
    }

    let mut result = Map::new();
    if let Some(options) = page_options {
        result.insert("type".to_string(), json!("static"));
        result.insert(
            "page_options".to_string(),
            validate_gemini_page_options(options, mime_type)?,
        );
        return Ok(Some(Value::Object(result)));
    }
    if let Some(options) = spreadsheet_options {
        result.insert("type".to_string(), json!("static"));
        result.insert(
            "spreadsheet_options".to_string(),
            validate_gemini_spreadsheet_options(options, mime_type)?,
        );
        return Ok(Some(Value::Object(result)));
    }
    if explicit_static {
        return Ok(Some(json!({"type": "static"})));
    }
    Ok(None)
}

fn normalize_gemini_file_uri(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Gemini file source requires a non-empty file_id or uri".to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("files/") {
        if rest.is_empty()
            || !rest
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        {
            return Err(format!("Invalid Gemini Files API resource name '{trimmed}'"));
        }
        return Ok(format!(
            "https://generativelanguage.googleapis.com/v1beta/files/{rest}"
        ));
    }
    let parsed = url::Url::parse(trimmed)
        .map_err(|_| format!("Invalid Gemini document file URI '{trimmed}'"))?;
    if !matches!(parsed.scheme(), "https" | "http") || parsed.host_str().is_none() {
        return Err(format!("Invalid Gemini document file URI '{trimmed}'"));
    }
    Ok(trimmed.to_string())
}

fn is_trusted_gemini_files_uri(uri: &str) -> bool {
    let Ok(parsed) = url::Url::parse(uri) else {
        return false;
    };
    let path_ok = parsed.path().starts_with("/v1beta/files/") || parsed.path().starts_with("/v1/files/");
    if !path_ok {
        return false;
    }
    match (parsed.scheme(), parsed.host_str()) {
        ("https", Some(host)) if host.eq_ignore_ascii_case("generativelanguage.googleapis.com") => {
            true
        }
        #[cfg(test)]
        ("http" | "https", Some("127.0.0.1" | "localhost")) => true,
        _ => false,
    }
}

fn gemini_files_endpoints(base_url: &str) -> Result<(String, String), String> {
    let parsed = url::Url::parse(base_url)
        .map_err(|err| format!("Invalid Gemini base_url '{base_url}': {err}"))?;
    let mut origin = parsed.clone();
    origin.set_query(None);
    origin.set_fragment(None);
    let mut upload = origin.clone();
    upload.set_path("/upload/v1beta/files");
    let mut api_base = origin;
    api_base.set_path("/v1beta");
    Ok((upload.to_string(), api_base.to_string()))
}

fn document_tool_name_is(name: &str) -> bool {
    computer_tool_name_is(name, "inspect_document")
}

fn inspect_document_tool() -> Value {
    json!({
        "name": "inspect_document",
        "title": "Inspect local document with Gemini native processing",
        "description": "Upload (with 48h SHA-256 caching via Gemini Files API) and slice a local PDF, Word (.docx/.doc/.dotx/.odt), PowerPoint (.pptx/.ppt/.odp), Excel/ODS spreadsheet (.xlsx/.xls/.ods), or text document so Gemini inspects only the requested pages, slides, worksheet, cell range, content layers, and resolution. For large PDFs/Office docs, scan with content_layers=['page_text'] first, then request specific pages with content_layers=['page_image'] and resolution='high'. For spreadsheets, inspect worksheet data/formulas and narrow with sheet_index and visible_cells (e.g. 'A1:G40'), adding 'visual' for charts/layout.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute local file path to a PDF, Word, PowerPoint, Excel/ODS, or plain-text document (max 50 MB)."
                },
                "start_page": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional 1-indexed inclusive start page/slide for paginated documents (PDF, DOCX, DOC, DOTX, ODT, PPTX, PPT, ODP)."
                },
                "end_page": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional 1-indexed inclusive end page/slide (>= start_page) for paginated documents."
                },
                "sheet_index": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional 1-indexed worksheet tab position for spreadsheets (.xlsx, .xls, .ods). Omit to inspect all sheets."
                },
                "visible_cells": {
                    "type": "string",
                    "description": "Optional A1 cell or range (for example 'A1:G40' or 'B5') for spreadsheets."
                },
                "content_layers": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": ["page_text", "page_image", "data", "formulas", "visual"]
                    },
                    "description": "Optional content layers. Paginated documents accept 'page_text' and/or 'page_image'. Spreadsheets accept 'data', 'formulas', and/or 'visual'."
                },
                "resolution": {
                    "type": "string",
                    "enum": ["low", "medium", "high", "ultra_high"],
                    "description": "Optional rendering resolution when page_image or visual layer is used (default: medium)."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        },
        "annotations": {
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": true
        }
    })
}

struct GeminiDocumentProvider {
    client: Client,
    api_key: String,
    base_url: String,
}

fn gemini_document_provider(state: &AppState) -> Result<GeminiDocumentProvider, String> {
    let mut profile = active_provider_profile(state)
        .ok_or_else(|| "No active provider profile is configured".to_string())?;
    if profile.transport != ProviderTransport::GeminiInteractions {
        return Err(
            "inspect_document requires an active Gemini Interactions profile (gemini-interactions)"
                .to_string(),
        );
    }
    apply_bridge_managed_gemini_credentials(state, &mut profile)?;
    let api_key = profile
        .api_key
        .clone()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "Active Gemini Interactions profile '{}' has no API key for Files API upload",
                profile.file_name
            )
        })?;
    Ok(GeminiDocumentProvider {
        client: profile.client,
        api_key,
        base_url: profile.base_url,
    })
}

fn gemini_file_cache_key(
    api_key: &str,
    base_url: &str,
    mime_type: &str,
    bytes: &[u8],
) -> String {
    let mut key_digest = Sha256::new();
    key_digest.update(api_key.as_bytes());
    let key_hash = format!("{:x}", key_digest.finalize());
    let mut file_digest = Sha256::new();
    file_digest.update(bytes);
    let file_hash = format!("{:x}", file_digest.finalize());
    format!("{}:{}:{}:{}", &key_hash[..16], base_url, mime_type, file_hash)
}

async fn upload_or_get_cached_gemini_file(
    state: &AppState,
    provider: &GeminiDocumentProvider,
    path: &Path,
    mime_type: &str,
) -> Result<(CachedGeminiFile, usize, bool), String> {
    let path_owned = path.to_path_buf();
    let (bytes, display_name) = tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, String), String> {
        let metadata = fs::metadata(&path_owned).map_err(|err| {
            format!("Cannot access document '{}': {err}", path_owned.display())
        })?;
        if !metadata.is_file() {
            return Err(format!(
                "Document path '{}' is not a regular file",
                path_owned.display()
            ));
        }
        let len = metadata.len();
        if len == 0 {
            return Err(format!("Document '{}' is empty", path_owned.display()));
        }
        if len > GEMINI_MAX_DOCUMENT_BYTES {
            return Err(format!(
                "Document '{}' ({len} bytes) exceeds Gemini's 50 MiB limit",
                path_owned.display()
            ));
        }
        let bytes = fs::read(&path_owned).map_err(|err| {
            format!("Cannot read document '{}': {err}", path_owned.display())
        })?;
        let display_name = path_owned
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("document")
            .to_string();
        Ok((bytes, display_name))
    })
    .await
    .map_err(|err| format!("Cannot join document reader task: {err}"))??;

    let byte_len = bytes.len();
    let cache_key = gemini_file_cache_key(&provider.api_key, &provider.base_url, mime_type, &bytes);
    let now = SystemTime::now();
    let safety_margin = Duration::from_secs(5 * 60);
    {
        let mut cache = state.gemini_file_cache.lock().await;
        cache.retain(|_, entry| entry.expires_at > now + safety_margin);
        if let Some(entry) = cache.get(&cache_key).cloned() {
            return Ok((entry, byte_len, true));
        }
    }

    let (upload_url, api_base_url) = gemini_files_endpoints(&provider.base_url)?;
    let operation = async {
        let start_response = provider
            .client
            .post(&upload_url)
            .header("x-goog-api-key", &provider.api_key)
            .header("X-Goog-Upload-Protocol", "resumable")
            .header("X-Goog-Upload-Command", "start")
            .header("X-Goog-Upload-Header-Content-Length", byte_len.to_string())
            .header("X-Goog-Upload-Header-Content-Type", mime_type)
            .json(&json!({"file": {"display_name": display_name}}))
            .send()
            .await
            .map_err(|err| format!("Gemini Files API upload start failed: {err}"))?;
        let start_status = start_response.status();
        let session_url = start_response
            .headers()
            .get("x-goog-upload-url")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        if !start_status.is_success() {
            let text = read_response_text_limited(start_response)
                .await
                .unwrap_or_default();
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .map(|value| safe_error_message(&value))
                .unwrap_or(text);
            return Err(format!(
                "Gemini Files API upload start returned HTTP {}: {message}",
                start_status.as_u16()
            ));
        }
        let session_url = session_url.ok_or_else(|| {
            "Gemini Files API upload start response missing X-Goog-Upload-URL header".to_string()
        })?;

        let finalize_response = provider
            .client
            .post(&session_url)
            .header("X-Goog-Upload-Offset", "0")
            .header("X-Goog-Upload-Command", "upload, finalize")
            .header("Content-Type", mime_type)
            .body(bytes)
            .send()
            .await
            .map_err(|err| format!("Gemini Files API upload finalize failed: {err}"))?;
        let finalize_status = finalize_response.status();
        let finalize_body = read_response_json_limited(finalize_response)
            .await
            .map_err(|err| format!("Gemini Files API returned invalid JSON: {err}"))?;
        if !finalize_status.is_success() {
            return Err(format!(
                "Gemini Files API upload finalize returned HTTP {}: {}",
                finalize_status.as_u16(),
                safe_error_message(&finalize_body)
            ));
        }
        let mut file_obj = finalize_body
            .get("file")
            .cloned()
            .unwrap_or(finalize_body);
        let name = file_obj
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        let mut polls = 0usize;
        while file_obj
            .get("state")
            .and_then(Value::as_str)
            .is_some_and(|state| state.eq_ignore_ascii_case("PROCESSING"))
        {
            if name.is_empty() || polls >= GEMINI_FILE_PROCESSING_MAX_POLLS {
                return Err(
                    "Gemini Files API file remained in PROCESSING state beyond the wait limit"
                        .to_string(),
                );
            }
            polls += 1;
            tokio::time::sleep(GEMINI_FILE_PROCESSING_POLL_INTERVAL).await;
            let poll_url = format!("{}/{}", api_base_url.trim_end_matches('/'), name.trim_start_matches('/'));
            let poll_response = provider
                .client
                .get(&poll_url)
                .header("x-goog-api-key", &provider.api_key)
                .send()
                .await
                .map_err(|err| format!("Gemini Files API status poll failed: {err}"))?;
            let poll_status = poll_response.status();
            let poll_body = read_response_json_limited(poll_response)
                .await
                .map_err(|err| format!("Gemini Files API poll returned invalid JSON: {err}"))?;
            if !poll_status.is_success() {
                return Err(format!(
                    "Gemini Files API poll returned HTTP {}: {}",
                    poll_status.as_u16(),
                    safe_error_message(&poll_body)
                ));
            }
            file_obj = poll_body.get("file").cloned().unwrap_or(poll_body);
        }

        if file_obj
            .get("state")
            .and_then(Value::as_str)
            .is_some_and(|state| state.eq_ignore_ascii_case("FAILED"))
        {
            return Err(format!(
                "Gemini Files API failed to process file: {}",
                safe_error_message(&file_obj)
            ));
        }

        let uri = file_obj
            .get("uri")
            .and_then(Value::as_str)
            .filter(|uri| !uri.is_empty())
            .ok_or_else(|| "Gemini Files API response missing file.uri".to_string())?
            .to_string();
        let expires_at = SystemTime::now() + Duration::from_secs(47 * 3600);
        Ok::<_, String>(CachedGeminiFile {
            uri,
            name,
            expires_at,
        })
    };

    let uploaded = tokio::time::timeout(GEMINI_FILE_UPLOAD_TIMEOUT, operation)
        .await
        .map_err(|_| {
            format!(
                "Gemini Files API upload timed out after {} seconds",
                GEMINI_FILE_UPLOAD_TIMEOUT.as_secs()
            )
        })??;

    {
        let mut cache = state.gemini_file_cache.lock().await;
        if cache.len() >= GEMINI_FILE_CACHE_CAPACITY {
            cache.shift_remove_index(0);
        }
        cache.insert(cache_key, uploaded.clone());
    }

    Ok((uploaded, byte_len, false))
}

async fn execute_inspect_document(
    state: &AppState,
    arguments: Option<&Value>,
) -> Result<Value, String> {
    let arguments = arguments
        .and_then(Value::as_object)
        .ok_or_else(|| "inspect_document arguments must be a JSON object".to_string())?;
    let raw_path = arguments
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "inspect_document requires a non-empty 'path'".to_string())?;
    let path = PathBuf::from(raw_path);
    let mime_type = gemini_document_mime_from_path_or_url(raw_path).ok_or_else(|| {
        format!(
            "Unsupported document extension for '{}'; expected pdf, docx, doc, dotx, odt, pptx, ppt, odp, xlsx, xls, ods, txt, md, csv, html, xml, or json",
            path.display()
        )
    })?;
    let kind = gemini_document_kind_for_mime(mime_type).ok_or_else(|| {
        format!("Unsupported document MIME type '{mime_type}'")
    })?;

    let resolution = match arguments.get("resolution").filter(|value| !value.is_null()) {
        Some(value) => {
            let text = value
                .as_str()
                .ok_or_else(|| "inspect_document 'resolution' must be a string".to_string())?;
            Some(validate_gemini_media_resolution(text)?)
        }
        None => None,
    };

    let has_page_fields = arguments.get("start_page").is_some_and(|v| !v.is_null())
        || arguments.get("end_page").is_some_and(|v| !v.is_null());
    let has_sheet_fields = arguments.get("sheet_index").is_some_and(|v| !v.is_null())
        || arguments.get("visible_cells").is_some_and(|v| !v.is_null());
    let layers = arguments.get("content_layers").filter(|v| !v.is_null());

    if has_page_fields && has_sheet_fields {
        return Err(
            "inspect_document cannot combine page options (start_page/end_page) with spreadsheet options (sheet_index/visible_cells)"
                .to_string(),
        );
    }

    let mut page_options = Map::new();
    if let Some(start) = arguments.get("start_page").filter(|v| !v.is_null()) {
        page_options.insert("start_page".to_string(), start.clone());
    }
    if let Some(end) = arguments.get("end_page").filter(|v| !v.is_null()) {
        page_options.insert("end_page".to_string(), end.clone());
    }

    let mut spreadsheet_options = Map::new();
    if let Some(sheet) = arguments.get("sheet_index").filter(|v| !v.is_null()) {
        spreadsheet_options.insert("sheet_index".to_string(), sheet.clone());
    }
    if let Some(cells) = arguments.get("visible_cells").filter(|v| !v.is_null()) {
        spreadsheet_options.insert("visible_cells".to_string(), cells.clone());
    }

    if let Some(layers) = layers {
        match kind {
            GeminiDocumentKind::Paginated => {
                page_options.insert("content_layers".to_string(), layers.clone());
            }
            GeminiDocumentKind::Spreadsheet => {
                spreadsheet_options.insert("content_layers".to_string(), layers.clone());
            }
            GeminiDocumentKind::PlainText => {
                return Err(format!(
                    "content_layers is not supported for plain text document '{mime_type}'"
                ));
            }
        }
    }

    let page_value = (!page_options.is_empty()).then_some(Value::Object(page_options));
    let sheet_value =
        (!spreadsheet_options.is_empty()).then_some(Value::Object(spreadsheet_options));
    let processing = build_gemini_document_processing(
        mime_type,
        None,
        page_value.as_ref(),
        sheet_value.as_ref(),
    )?;

    let provider = gemini_document_provider(state)?;
    let (uploaded, bytes, cached) =
        upload_or_get_cached_gemini_file(state, &provider, &path, mime_type).await?;

    let mut document = Map::new();
    document.insert("type".to_string(), json!("document"));
    document.insert("uri".to_string(), json!(uploaded.uri));
    document.insert("mime_type".to_string(), json!(mime_type));
    if let Some(resolution) = resolution {
        document.insert("resolution".to_string(), json!(resolution));
    }
    if let Some(processing) = processing {
        document.insert("processing".to_string(), processing);
    }

    let display_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("document")
        .to_string();
    let envelope = json!({
        "protocol_version": DOCUMENT_PROTOCOL_VERSION,
        "kind": "gemini_document_slice",
        "display_name": display_name,
        "path": path,
        "bytes": bytes,
        "cached": cached,
        "file_name": uploaded.name,
        "document": Value::Object(document)
    });

    Ok(json!({
        "content": [{"type": "text", "text": envelope.to_string()}],
        "structuredContent": envelope,
        "isError": false
    }))
}

fn document_result_envelope(content: &Value) -> Option<Value> {
    let try_parse = |text: &str| {
        serde_json::from_str::<Value>(text).ok().filter(|value| {
            value.get("protocol_version").and_then(Value::as_str)
                == Some(DOCUMENT_PROTOCOL_VERSION)
                && value.get("kind").and_then(Value::as_str) == Some("gemini_document_slice")
        })
    };
    if let Some(text) = content.as_str() {
        return try_parse(text);
    }
    content.as_array()?.iter().find_map(|part| {
        (part.get("type").and_then(Value::as_str) == Some("text"))
            .then(|| part.get("text").and_then(Value::as_str))
            .flatten()
            .and_then(try_parse)
    })
}

fn expand_document_slice_tool_result(content: &Value) -> Option<(Value, Value)> {
    let envelope = document_result_envelope(content)?;
    let raw_doc = envelope.get("document")?.as_object()?;
    if raw_doc.get("type").and_then(Value::as_str) != Some("document") {
        return None;
    }
    let uri = raw_doc.get("uri").and_then(Value::as_str)?;
    if !is_trusted_gemini_files_uri(uri) {
        return None;
    }
    let mime_type = raw_doc
        .get("mime_type")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())?;
    let mut document = Map::new();
    document.insert("type".to_string(), json!("document"));
    document.insert("uri".to_string(), json!(uri));
    document.insert("mime_type".to_string(), json!(mime_type));
    if let Some(resolution) = raw_doc.get("resolution").and_then(Value::as_str) {
        let validated = validate_gemini_media_resolution(resolution).ok()?;
        document.insert("resolution".to_string(), json!(validated));
    }
    if let Some(processing) = build_gemini_document_processing(
        mime_type,
        raw_doc.get("processing"),
        None,
        None,
    )
    .ok()?
    {
        document.insert("processing".to_string(), processing);
    }
    let mut summary = Map::new();
    for field in ["display_name", "bytes", "cached", "file_name"] {
        if let Some(value) = envelope.get(field) {
            summary.insert(field.to_string(), value.clone());
        }
    }
    summary.insert("mime_type".to_string(), json!(mime_type));
    if let Some(resolution) = document.get("resolution") {
        summary.insert("resolution".to_string(), resolution.clone());
    }
    if let Some(processing) = document.get("processing") {
        summary.insert("processing".to_string(), processing.clone());
    }
    let text_block = json!({"type": "text", "text": Value::Object(summary).to_string()});
    Some((text_block, Value::Object(document)))
}
