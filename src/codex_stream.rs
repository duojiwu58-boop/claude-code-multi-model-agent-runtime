// Preserve native item boundaries. Public reasoning summaries are display
// content; the actual reasoning remains in the paused Codex turn.
struct CodexOutput {
    id: String,
    model: String,
    content: Vec<Value>,
    items: HashMap<String, usize>,
    active: Option<usize>,
    bytes: usize,
    max_chars: Option<usize>,
    emitted_chars: usize,
    stops: Vec<String>,
    local_stop: Option<&'static str>,
    matched_stop: Option<String>,
    text_pending: IndexMap<String, String>,
    text_received: HashMap<String, String>,
}

impl CodexOutput {
    fn new(model: String) -> Self {
        Self {
            id: format!("msg_{}", Uuid::new_v4().simple()),
            model,
            content: Vec::new(),
            items: HashMap::new(),
            active: None,
            bytes: 0,
            max_chars: None,
            emitted_chars: 0,
            stops: Vec::new(),
            local_stop: None,
            matched_stop: None,
            text_pending: IndexMap::new(),
            text_received: HashMap::new(),
        }
    }

    fn text_delta(&mut self, key: &str, delta: &str) -> Result<Vec<Event>, String> {
        let received = self.text_received.entry(key.to_string()).or_default();
        if received.len().saturating_add(delta.len()) > MAX_UPSTREAM_RESPONSE_BYTES {
            return Err("Codex text exceeds the response limit".into());
        }
        received.push_str(delta);
        self.text_pending
            .entry(key.to_string())
            .or_default()
            .push_str(delta);
        self.release_text(key, false)
    }

    fn release_text(&mut self, key: &str, flush: bool) -> Result<Vec<Event>, String> {
        let pending = self.text_pending.entry(key.to_string()).or_default();
        let matched = self
            .stops
            .iter()
            .filter_map(|s| pending.find(s).map(|n| (n, s.clone())))
            .min_by_key(|(n, _)| *n);
        let hold = if flush || matched.is_some() {
            0
        } else {
            self.stops
                .iter()
                .flat_map(|s| s.char_indices().skip(1).map(move |(i, _)| &s[..i]))
                .filter(|prefix| pending.ends_with(prefix))
                .map(str::len)
                .max()
                .unwrap_or(0)
        };
        let end = matched
            .as_ref()
            .map(|(i, _)| *i)
            .unwrap_or(pending.len() - hold);
        let allowed = self
            .max_chars
            .map(|n| n.saturating_sub(self.emitted_chars))
            .unwrap_or(usize::MAX);
        let visible: String = pending[..end].chars().take(allowed).collect();
        let cut = visible.len() < end;
        let count = visible.chars().count();
        pending.drain(..visible.len());
        self.emitted_chars += count;
        if cut || (matched.is_none() && self.max_chars == Some(self.emitted_chars)) {
            self.local_stop = Some("max_tokens");
        } else if let Some((_, stop)) = matched {
            self.local_stop = Some("stop_sequence");
            self.matched_stop = Some(stop);
        }
        self.delta(key, false, &visible)
    }

    fn text_complete(&mut self, key: &str, full: &str) -> Result<Vec<Event>, String> {
        let received = self
            .text_received
            .get(key)
            .map(String::as_str)
            .unwrap_or("");
        if !full.starts_with(received) {
            return Err("Codex completed text disagrees with streamed text".into());
        }
        let missing = full[received.len()..].to_string();
        let mut events = self.text_delta(key, &missing)?;
        if self.local_stop.is_none() {
            events.extend(self.release_text(key, true)?);
        }
        self.close(&mut events)?;
        Ok(events)
    }

    fn flush_text(&mut self) -> Result<Vec<Event>, String> {
        let keys: Vec<String> = self.text_pending.keys().cloned().collect();
        let mut events = Vec::new();
        for key in keys {
            if self.local_stop.is_none() {
                events.extend(self.release_text(&key, true)?);
            }
        }
        Ok(events)
    }

    fn start(&self) -> Result<Vec<Event>, String> {
        let mut events = Vec::new();
        push_anthropic_event(
            &mut events,
            "message_start",
            json!({"type":"message_start","message":{"id":self.id,"type":"message","role":"assistant","model":self.model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}}),
        )?;
        Ok(events)
    }

    fn close(&mut self, events: &mut Vec<Event>) -> Result<(), String> {
        if let Some(index) = self.active.take() {
            push_anthropic_event(
                events,
                "content_block_stop",
                json!({"type":"content_block_stop","index":index}),
            )?;
        }
        Ok(())
    }

    fn delta(&mut self, key: &str, thinking: bool, delta: &str) -> Result<Vec<Event>, String> {
        if delta.is_empty() {
            return Ok(Vec::new());
        }
        self.bytes = self.bytes.saturating_add(delta.len());
        if self.bytes > MAX_UPSTREAM_RESPONSE_BYTES {
            return Err("Codex answer exceeds the response limit".to_string());
        }
        let mut events = Vec::new();
        let index = match self.items.get(key).copied() {
            Some(index) if self.active == Some(index) => index,
            Some(_) => {
                return Err("Codex interleaved output after closing a content block".to_string())
            }
            None => {
                self.close(&mut events)?;
                let index = self.content.len();
                // Empty signature explicitly carries no Anthropic authentication.
                let block = if thinking {
                    json!({"type":"thinking","thinking":"","signature":""})
                } else {
                    json!({"type":"text","text":""})
                };
                push_anthropic_event(
                    &mut events,
                    "content_block_start",
                    json!({"type":"content_block_start","index":index,"content_block":block}),
                )?;
                self.content.push(block);
                self.items.insert(key.to_string(), index);
                self.active = Some(index);
                index
            }
        };
        let field = if thinking { "thinking" } else { "text" };
        let existing = self.content[index][field]
            .as_str()
            .ok_or("Codex changed content type for an item")?;
        self.content[index][field] = json!(format!("{existing}{delta}"));
        let payload = if thinking {
            json!({"type":"thinking_delta","thinking":delta})
        } else {
            json!({"type":"text_delta","text":delta})
        };
        push_anthropic_event(
            &mut events,
            "content_block_delta",
            json!({"type":"content_block_delta","index":index,"delta":payload}),
        )?;
        Ok(events)
    }

    fn complete(&mut self, key: &str, thinking: bool, text: &str) -> Result<Vec<Event>, String> {
        let field = if thinking { "thinking" } else { "text" };
        let received = self
            .items
            .get(key)
            .map(|index| self.content[*index][field].as_str().unwrap_or(""))
            .unwrap_or("");
        if !text.starts_with(received) {
            return Err("Codex completed item disagrees with streamed text".to_string());
        }
        let missing = text[received.len()..].to_string();
        let mut events = self.delta(key, thinking, &missing)?;
        if self.items.get(key).copied() == self.active {
            self.close(&mut events)?;
        }
        Ok(events)
    }

    fn tool(&mut self, id: &str, name: &str, arguments: &Value) -> Result<Vec<Event>, String> {
        let mut events = Vec::new();
        self.close(&mut events)?;
        let index = self.content.len();
        let args = arguments.to_string();
        self.bytes = self.bytes.saturating_add(args.len());
        if self.bytes > MAX_UPSTREAM_RESPONSE_BYTES {
            return Err("Codex answer exceeds the response limit".to_string());
        }
        push_anthropic_event(
            &mut events,
            "content_block_start",
            json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}),
        )?;
        push_anthropic_event(
            &mut events,
            "content_block_delta",
            json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":args}}),
        )?;
        push_anthropic_event(
            &mut events,
            "content_block_stop",
            json!({"type":"content_block_stop","index":index}),
        )?;
        self.content
            .push(json!({"type":"tool_use","id":id,"name":name,"input":arguments}));
        Ok(events)
    }

    fn finish(&mut self, stop_reason: &str, usage: &Value) -> Result<Vec<Event>, String> {
        let mut events = Vec::new();
        self.close(&mut events)?;
        push_anthropic_event(
            &mut events,
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":stop_reason,"stop_sequence":self.matched_stop},"usage":usage}),
        )?;
        push_anthropic_event(&mut events, "message_stop", json!({"type":"message_stop"}))?;
        Ok(events)
    }

    fn message(&self, stop_reason: &str, usage: &Value) -> Value {
        json!({"id":self.id,"type":"message","role":"assistant","model":self.model,"content":self.content,"stop_reason":stop_reason,"stop_sequence":self.matched_stop,"usage":usage})
    }
}

async fn codex_replay_message(
    message: &Value,
    sender: &Option<tokio::sync::mpsc::Sender<Event>>,
) -> Result<(), String> {
    if sender.is_none() {
        return Ok(());
    }
    let mut output = CodexOutput::new(message["model"].as_str().unwrap_or_default().to_string());
    output.id = message["id"].as_str().unwrap_or_default().to_string();
    output.matched_stop = message["stop_sequence"].as_str().map(str::to_owned);
    codex_emit(sender, output.start()?).await?;
    for (index, block) in message["content"]
        .as_array()
        .ok_or("Invalid cached Codex response")?
        .iter()
        .enumerate()
    {
        let events = match block["type"].as_str() {
            Some("text") => output.complete(
                &index.to_string(),
                false,
                block["text"].as_str().unwrap_or_default(),
            )?,
            Some("thinking") => output.complete(
                &index.to_string(),
                true,
                block["thinking"].as_str().unwrap_or_default(),
            )?,
            Some("tool_use") => output.tool(
                block["id"].as_str().unwrap_or_default(),
                block["name"].as_str().unwrap_or_default(),
                &block["input"],
            )?,
            _ => return Err("Invalid cached Codex content block".to_string()),
        };
        codex_emit(sender, events).await?;
    }
    codex_emit(
        sender,
        output.finish(
            message["stop_reason"].as_str().unwrap_or("end_turn"),
            &message["usage"],
        )?,
    )
    .await
}
