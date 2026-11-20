//! Host implementation of `soil:inference`.
//!
//! The host owns endpoint discovery, Geode credentials, scrubbing, model alias
//! resolution and transport. A Spore names a model, messages and an optional
//! stream; it never sees a provider route or credential.

use std::io::Read;

use anyhow::{Context, Result};
use base64::Engine;
use serde_json::{Value, json};
use soil_inference::{GeodeResolver, SoilConfig};
use soil_scrub::Scrubber;
use wasmtime::component::{HasSelf, Linker, Resource, ResourceTable};

wasmtime::component::bindgen!({
    path: "wit",
    world: "host",
    with: {
        "soil:inference/inference.completion-stream": CompletionStream,
    },
});

pub mod api {
    pub use crate::soil::inference::inference::*;
}

const MAX_CHUNK: usize = 16 * 1024;
const BASE64_STANDARD: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

pub struct InferenceHost {
    table: ResourceTable,
    agent: ureq::Agent,
    base_url: String,
    voice_url: Option<String>,
    stt_url: Option<String>,
    stt_model: Option<String>,
    api_key: Option<String>,
    provider: String,
    model: String,
    session_id: Option<String>,
    scrubber: Scrubber,
}

pub struct CompletionStream {
    reader: Box<dyn Read + Send + Sync>,
    done: bool,
}

impl InferenceHost {
    pub fn open(config_path: impl AsRef<std::path::Path>, profile: Option<&str>) -> Result<Self> {
        let config = SoilConfig::load(config_path.as_ref())?;
        Self::from_config(config, profile)
    }

    pub fn from_config(config: SoilConfig, profile: Option<&str>) -> Result<Self> {
        let resolved = config.resolve(profile)?;
        let base_url = resolved
            .base_url
            .clone()
            .context("inference profile has no base URL or service endpoint")?;
        let api_key = match resolved.api_key.clone() {
            Some(key) => Some(key),
            None => resolved
                .credential
                .as_deref()
                .map(|handle| GeodeResolver::new(config.geode.clone()).resolve(handle))
                .transpose()?
                .flatten(),
        };
        if api_key.is_none() && resolved.credential.is_some() {
            anyhow::bail!("Geode credential did not resolve");
        }
        Ok(Self::new(
            base_url,
            api_key,
            resolved.provider.unwrap_or_else(|| "unknown".into()),
            resolved.model.unwrap_or_else(|| "default".into()),
        ))
    }

    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            table: ResourceTable::new(),
            agent: ureq::AgentBuilder::new().build(),
            base_url: base_url.into(),
            voice_url: None,
            stt_url: None,
            stt_model: None,
            api_key,
            provider: provider.into(),
            model: model.into(),
            session_id: None,
            scrubber: Scrubber::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        let path = path.trim_start_matches('/');
        if base.ends_with("/v1") {
            format!("{base}/{}", path.strip_prefix("v1/").unwrap_or(path))
        } else {
            format!("{base}/{path}")
        }
    }

    /// Set the session identifier used by providers that route a conversation
    /// across requests, such as OpenCode Go (`x-opencode-session`).
    pub fn with_session(mut self, session: impl Into<String>) -> Self {
        self.session_id = Some(session.into());
        self
    }

    /// Use a dedicated speaker-embedding service. Text inference and voice
    /// inference can have different hosts and model runtimes.
    pub fn with_voice_url(mut self, url: impl Into<String>) -> Self {
        self.voice_url = Some(url.into());
        self
    }

    /// Use a dedicated speech-to-text service. The chat route is never used
    /// for audio, even when both live behind the same gateway.
    pub fn with_stt_url(mut self, url: impl Into<String>) -> Self {
        self.stt_url = Some(url.into());
        self
    }

    pub fn with_stt_model(mut self, model: impl Into<String>) -> Self {
        self.stt_model = Some(model.into());
        self
    }

    fn url_at(base: &str, path: &str) -> String {
        let base = base.trim_end_matches('/');
        let path = path.trim_start_matches('/');
        if base.ends_with("/v1") {
            format!("{base}/{}", path.strip_prefix("v1/").unwrap_or(path))
        } else {
            format!("{base}/{path}")
        }
    }

    fn request(&self, method: &str, url: &str) -> ureq::Request {
        let mut request = self.agent.request(method, url);
        if let Some(key) = &self.api_key {
            request = request.set("authorization", &format!("Bearer {key}"));
        }
        if let Some(session) = &self.session_id {
            request = request.set("x-opencode-session", session);
        }
        request
    }

    fn body(&mut self, request: &api::Request, stream: bool) -> Result<Value, api::Error> {
        let messages: Vec<Value> = request
            .messages
            .iter()
            .map(|message| {
                let scrubbed = message
                    .content
                    .as_deref()
                    .map(|content| self.scrubber.scrub(content));
                let mut value = json!({
                    "role": role_name(message.role),
                    "content": scrubbed.clone().map(Value::String).unwrap_or(Value::Null),
                });
                if !message.attachments.is_empty() {
                    let mut parts = Vec::new();
                    if let Some(text) = scrubbed.filter(|text| !text.is_empty()) {
                        parts.push(json!({"type": "text", "text": text}));
                    }
                    for attachment in &message.attachments {
                        if !attachment.mime_type.starts_with("image/") {
                            return Err(api::Error::Invalid);
                        }
                        if let Some(text) = &attachment.extracted_text {
                            parts.push(json!({
                                "type": "text",
                                "text": format!("[{}]\n{}", attachment.name, self.scrubber.scrub(text)),
                            }));
                        }
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": {
                                "url": format!(
                                    "data:{};base64,{}",
                                    attachment.mime_type,
                                    BASE64_STANDARD.encode(&attachment.data),
                                ),
                            },
                        }));
                    }
                    value["content"] = Value::Array(parts);
                }
                if let Some(name) = &message.name {
                    value["name"] = json!(name);
                }
                if let Some(call_id) = &message.tool_call_id {
                    value["tool_call_id"] = json!(call_id);
                }
                if !message.tool_calls.is_empty() {
                    value["tool_calls"] = json!(
                        message
                            .tool_calls
                            .iter()
                            .map(|call| json!({
                                "id": call.id,
                                "type": "function",
                                "function": {
                                    "name": call.name,
                                    "arguments": self.scrubber.scrub(&call.arguments),
                                },
                            }))
                            .collect::<Vec<_>>()
                    );
                }
                Ok(value)
            })
            .collect::<Result<_, api::Error>>()?;
        let mut body = json!({
            "model": request.model,
            "messages": messages,
            "stream": stream,
        });
        if !request.tools.is_empty() {
            let tools: Vec<Value> = request
                .tools
                .iter()
                .map(|tool| {
                    let parameters: Value =
                        serde_json::from_str(&tool.parameters).map_err(|_| api::Error::Invalid)?;
                    Ok(json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": self.scrubber.scrub(&tool.description),
                            "parameters": parameters,
                        },
                    }))
                })
                .collect::<Result<_, api::Error>>()?;
            body["tools"] = Value::Array(tools);
            body["tool_choice"] = json!("auto");
        }
        if let Some(max) = request.max_output_tokens {
            body["max_tokens"] = json!(max);
        }
        if let Some(temperature) = request.temperature {
            body["temperature"] = json!(temperature);
        }
        if let Some(hint) = &request.prompt_cache_key {
            body["prompt_cache_key"] = json!(hint);
        }
        if stream {
            body["stream_options"] = json!({"include_usage": true});
        }
        Ok(body)
    }

    fn error(error: ureq::Error) -> api::Error {
        match error {
            ureq::Error::Status(401 | 403, _) => api::Error::Auth,
            ureq::Error::Status(408, _) => api::Error::Unavailable,
            ureq::Error::Status(429, _) => api::Error::OverBudget,
            ureq::Error::Status(_, _) | ureq::Error::Transport(_) => api::Error::Unavailable,
        }
    }
}

impl InferenceHost {
    fn voice_embed(
        &mut self,
        request: api::VoiceEmbeddingRequest,
    ) -> Result<api::VoiceEmbeddingResponse, api::Error> {
        if request.pcm16.is_empty()
            || request.pcm16.len() > 2 * 1024 * 1024
            || !(8_000..=192_000).contains(&request.sample_rate)
        {
            return Err(api::Error::Invalid);
        }
        let model = request.model.clone();
        let boundary = format!(
            "soil-voice-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let mut body = Vec::with_capacity(request.pcm16.len() + 1024);
        let mut field = |name: &str, value: &str| {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            );
            body.extend_from_slice(value.as_bytes());
            body.extend_from_slice(b"\r\n");
        };
        field("model", &request.model);
        field("checkpoint", &request.checkpoint);
        field("sample_rate", &request.sample_rate.to_string());
        if let Some(dimensions) = request.dimensions {
            field("dimensions", &dimensions.to_string());
        }
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"utterance.pcm\"\r\n",
        );
        body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
        body.extend_from_slice(&request.pcm16);
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

        let voice_base = self.voice_url.as_deref().unwrap_or(&self.base_url);
        let response = self
            .request("POST", &Self::url_at(voice_base, "/v1/audio/embeddings"))
            .set(
                "content-type",
                &format!("multipart/form-data; boundary={boundary}"),
            )
            .send_bytes(&body)
            .map_err(Self::error)?;
        let value: Value = response.into_json().map_err(|_| api::Error::Io)?;
        let embedding = value
            .get("embedding")
            .and_then(Value::as_array)
            .ok_or(api::Error::Io)?
            .iter()
            .map(|value| value.as_f64().map(|value| value as f32))
            .collect::<Option<Vec<f32>>>()
            .ok_or(api::Error::Io)?;
        if embedding.is_empty()
            || request
                .dimensions
                .is_some_and(|dimensions| embedding.len() != dimensions as usize)
        {
            return Err(api::Error::Invalid);
        }
        let usage = value.get("usage").and_then(|usage| {
            Some(api::EmbedUsage {
                prompt_tokens: u32::try_from(usage.get("prompt_tokens")?.as_u64()?).ok()?,
                total_tokens: u32::try_from(usage.get("total_tokens")?.as_u64()?).ok()?,
            })
        });
        Ok(api::VoiceEmbeddingResponse {
            model: value
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or(&model)
                .to_string(),
            embedding,
            usage,
        })
    }
}


impl InferenceHost {
    fn transcribe(
        &mut self,
        request: api::TranscribeRequest,
    ) -> Result<api::TranscribeResponse, api::Error> {
        if request.pcm16.is_empty()
            || request.pcm16.len() > 64 * 1024 * 1024
            || !request.pcm16.len().is_multiple_of(2)
            || !(8_000..=192_000).contains(&request.sample_rate)
        {
            return Err(api::Error::Invalid);
        }
        let model = if request.model.trim().is_empty() {
            self.stt_model
                .clone()
                .unwrap_or_else(|| self.model.clone())
        } else {
            request.model.clone()
        };
        let wav = soil_codec::wav_mono(&request.pcm16, request.sample_rate)
            .map_err(|_| api::Error::Invalid)?;
        let boundary = format!(
            "soil-stt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let mut body = Vec::with_capacity(wav.len() + 1024);
        let mut field = |name: &str, value: &str| {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            );
            body.extend_from_slice(value.as_bytes());
            body.extend_from_slice(b"\r\n");
        };
        field("model", &model);
        field("response_format", "verbose_json");
        if request.timestamps {
            field("timestamp_granularities[]", "word");
            field("timestamp_granularities[]", "segment");
        }
        if request.diarize {
            field("diarize", "true");
        }
        if let Some(language) = &request.language {
            field("language", language);
        }
        if let Some(prompt) = &request.prompt {
            field("prompt", prompt);
        }
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\n",
        );
        body.extend_from_slice(b"Content-Type: audio/wav\r\n\r\n");
        body.extend_from_slice(&wav);
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

        let stt_base = self.stt_url.as_deref().unwrap_or(&self.base_url);
        let response = self
            .request("POST", &Self::url_at(stt_base, "/v1/audio/transcriptions"))
            .set(
                "content-type",
                &format!("multipart/form-data; boundary={boundary}"),
            )
            .send_bytes(&body)
            .map_err(Self::error)?;
        let value: Value = response.into_json().map_err(|_| api::Error::Io)?;
        let text = value
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let duration_ms = value
            .get("duration")
            .and_then(Value::as_f64)
            .map(|seconds| (seconds * 1000.0).max(0.0) as u32);
        let segments: Vec<api::Segment> = value
            .get("segments")
            .and_then(Value::as_array)
            .map(|segments| {
                segments
                    .iter()
                    .filter_map(|segment| {
                        let text = segment.get("text")?.as_str()?.trim().to_string();
                        if text.is_empty() {
                            return None;
                        }
                        let start_ms = segment
                            .get("start")
                            .and_then(Value::as_f64)
                            .map(|value| (value * 1000.0).max(0.0) as u32)
                            .unwrap_or(0);
                        let end_ms = segment
                            .get("end")
                            .and_then(Value::as_f64)
                            .map(|value| (value * 1000.0).max(0.0) as u32)
                            .unwrap_or(start_ms);
                        let speaker = segment.get("speaker").and_then(|speaker| {
                            speaker
                                .as_str()
                                .map(str::to_string)
                                .or_else(|| speaker.as_u64().map(|id| format!("SPEAKER_{id:02}")))
                        });
                        let words = segment
                            .get("words")
                            .and_then(Value::as_array)
                            .map(|words| {
                                words
                                    .iter()
                                    .filter_map(|word| {
                                        let text = word.get("word")?.as_str()?.to_string();
                                        if text.is_empty() {
                                            return None;
                                        }
                                        let start = word
                                            .get("start")
                                            .and_then(Value::as_f64)
                                            .map(|value| (value * 1000.0).max(0.0) as u32)
                                            .unwrap_or(start_ms);
                                        let end = word
                                            .get("end")
                                            .and_then(Value::as_f64)
                                            .map(|value| (value * 1000.0).max(0.0) as u32)
                                            .unwrap_or(end_ms);
                                        Some(api::Word {
                                            text,
                                            start_ms: start,
                                            end_ms: end,
                                            speaker: word
                                                .get("speaker")
                                                .and_then(Value::as_str)
                                                .map(str::to_string),
                                            confidence: word
                                                .get("probability")
                                                .and_then(Value::as_f64)
                                                .map(|value| value as f32),
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        Some(api::Segment {
                            text,
                            start_ms,
                            end_ms,
                            speaker,
                            words,
                            confidence: segment
                                .get("avg_logprob")
                                .and_then(Value::as_f64)
                                .map(|value| value as f32),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let segments = if segments.is_empty() && !text.is_empty() {
            vec![api::Segment {
                text: text.clone(),
                start_ms: 0,
                end_ms: duration_ms.unwrap_or(0),
                speaker: None,
                words: Vec::new(),
                confidence: None,
            }]
        } else {
            segments
        };
        Ok(api::TranscribeResponse {
            model: value
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or(&model)
                .to_string(),
            text,
            language: value
                .get("language")
                .and_then(Value::as_str)
                .map(str::to_string),
            duration_ms,
            segments,
        })
    }
}

impl api::Host for InferenceHost {
    fn models(&mut self) -> Result<Vec<api::Model>, api::Error> {
        let response = self
            .request("GET", &self.url("/v1/models"))
            .call()
            .map_err(Self::error)?;
        let value: Value = response.into_json().map_err(|_| api::Error::Io)?;
        let models = value
            .get("data")
            .and_then(Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .filter_map(|model| {
                        let id = model.get("id")?.as_str()?.to_string();
                        let context = model
                            .get("context_window")
                            .or_else(|| model.get("context_length"))
                            .and_then(Value::as_u64)
                            .and_then(|value| u32::try_from(value).ok());
                        let tier = model
                            .get("tier")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .or_else(|| id.strip_prefix("tier/").map(str::to_string));
                        let capabilities = capabilities(model, &id, tier.as_deref());
                        let embedding_dimensions = model
                            .get("embedding_dimensions")
                            .or_else(|| model.get("embedding-dimensions"))
                            .or_else(|| model.get("dimensions"))
                            .and_then(Value::as_u64)
                            .and_then(|value| u32::try_from(value).ok());
                        Some(api::Model {
                            id,
                            provider: self.provider.clone(),
                            context_window: context,
                            tier,
                            capabilities,
                            embedding_dimensions,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(models)
    }

    fn complete(&mut self, request: api::Request) -> Result<api::Response, api::Error> {
        let body = self.body(&request, false)?;
        let response = self
            .request("POST", &self.url("/v1/chat/completions"))
            .set("content-type", "application/json")
            .send_json(body)
            .map_err(Self::error)?;
        let value: Value = response.into_json().map_err(|_| api::Error::Io)?;
        let choice = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .ok_or(api::Error::Io)?;
        let message = choice.get("message").ok_or(api::Error::Io)?;
        let content = message
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_string);
        let reasoning = message
            .get("reasoning_content")
            .or_else(|| message.get("reasoning"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let tool_calls = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|calls| {
                calls
                    .iter()
                    .enumerate()
                    .filter_map(|(index, call)| {
                        let function = call.get("function")?;
                        Some(api::ToolCall {
                            id: call
                                .get("id")
                                .and_then(Value::as_str)
                                .filter(|id| !id.is_empty())
                                .map(str::to_string)
                                .unwrap_or_else(|| format!("call_{index}")),
                            name: function.get("name")?.as_str()?.to_string(),
                            arguments: function
                                .get("arguments")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let finish_reason = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(str::to_string);
        let usage = value.get("usage").and_then(|usage| {
            Some(api::Usage {
                prompt_tokens: u32::try_from(usage.get("prompt_tokens")?.as_u64()?).ok()?,
                completion_tokens: u32::try_from(usage.get("completion_tokens")?.as_u64()?).ok()?,
                total_tokens: u32::try_from(usage.get("total_tokens")?.as_u64()?).ok()?,
            })
        });
        Ok(api::Response {
            model: value
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or(&self.model)
                .to_string(),
            message: api::Message {
                role: api::Role::Assistant,
                content,
                name: None,
                tool_call_id: None,
                tool_calls,
                attachments: Vec::new(),
                reasoning,
            },
            finish_reason,
            usage,
        })
    }

    fn embed(&mut self, request: api::EmbedRequest) -> Result<api::EmbedResponse, api::Error> {
        let model = request.model.clone();
        let expected = request.inputs.len();
        let inputs: Vec<String> = request
            .inputs
            .iter()
            .map(|input| self.scrubber.scrub(input))
            .collect();
        let mut body = json!({"model": model, "input": inputs});
        if let Some(dimensions) = request.dimensions {
            body["dimensions"] = json!(dimensions);
        }
        if let Some(hint) = &request.prompt_cache_key {
            body["prompt_cache_key"] = json!(hint);
        }
        let response = self
            .request("POST", &self.url("/v1/embeddings"))
            .set("content-type", "application/json")
            .send_json(body)
            .map_err(Self::error)?;
        let value: Value = response.into_json().map_err(|_| api::Error::Io)?;
        let mut rows: Vec<(usize, Vec<f32>)> = value
            .get("data")
            .and_then(Value::as_array)
            .ok_or(api::Error::Io)?
            .iter()
            .enumerate()
            .filter_map(|(position, row)| {
                let index = row
                    .get("index")
                    .and_then(Value::as_u64)
                    .map(|index| index as usize)
                    .unwrap_or(position);
                let vector = row
                    .get("embedding")?
                    .as_array()?
                    .iter()
                    .map(|value| value.as_f64().map(|value| value as f32))
                    .collect::<Option<Vec<f32>>>()?;
                Some((index, vector))
            })
            .collect();
        rows.sort_by_key(|(index, _)| *index);
        let vectors: Vec<Vec<f32>> = rows.into_iter().map(|(_, vector)| vector).collect();
        if vectors.len() != expected {
            return Err(api::Error::Invalid);
        }
        let usage = value.get("usage").and_then(|usage| {
            Some(api::EmbedUsage {
                prompt_tokens: u32::try_from(usage.get("prompt_tokens")?.as_u64()?).ok()?,
                total_tokens: u32::try_from(usage.get("total_tokens")?.as_u64()?).ok()?,
            })
        });
        Ok(api::EmbedResponse {
            model: value
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or(&model)
                .to_string(),
            vectors,
            usage,
        })
    }

    fn voice_embed(
        &mut self,
        request: api::VoiceEmbeddingRequest,
    ) -> Result<api::VoiceEmbeddingResponse, api::Error> {
        InferenceHost::voice_embed(self, request)
    }

    fn transcribe(
        &mut self,
        request: api::TranscribeRequest,
    ) -> Result<api::TranscribeResponse, api::Error> {
        InferenceHost::transcribe(self, request)
    }

    fn open_stream(
        &mut self,
        request: api::Request,
    ) -> Result<Resource<CompletionStream>, api::Error> {
        let body = self.body(&request, true)?;
        let response = self
            .request("POST", &self.url("/v1/chat/completions"))
            .set("content-type", "application/json")
            .set("accept", "text/event-stream")
            .send_json(body)
            .map_err(Self::error)?;
        self.table
            .push(CompletionStream {
                reader: response.into_reader(),
                done: false,
            })
            .map_err(|_| api::Error::OverBudget)
    }

    fn state(&mut self) -> Result<api::InferenceState, api::Error> {
        Ok(api::InferenceState {
            ready: true,
            provider: self.provider.clone(),
            model: self.model.clone(),
        })
    }
}

impl api::HostCompletionStream for InferenceHost {
    fn next(
        &mut self,
        stream: Resource<CompletionStream>,
        _timeout_ms: u32,
    ) -> Result<Option<api::Chunk>, api::Error> {
        let stream = self
            .table
            .get_mut(&stream)
            .map_err(|_| api::Error::Invalid)?;
        if stream.done {
            return Ok(Some(api::Chunk {
                data: Vec::new(),
                done: true,
            }));
        }
        let mut buffer = vec![0u8; MAX_CHUNK];
        match stream.reader.read(&mut buffer) {
            Ok(0) => {
                stream.done = true;
                Ok(Some(api::Chunk {
                    data: Vec::new(),
                    done: true,
                }))
            }
            Ok(read) => {
                buffer.truncate(read);
                Ok(Some(api::Chunk {
                    data: buffer,
                    done: false,
                }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => Ok(None),
            Err(_) => Err(api::Error::Io),
        }
    }

    fn cancel(&mut self, stream: Resource<CompletionStream>) -> Result<(), api::Error> {
        let stream = self
            .table
            .get_mut(&stream)
            .map_err(|_| api::Error::Invalid)?;
        stream.done = true;
        Ok(())
    }

    fn drop(&mut self, stream: Resource<CompletionStream>) -> Result<(), wasmtime::Error> {
        if let Ok(mut stream) = self.table.delete(stream) {
            stream.done = true;
        }
        Ok(())
    }
}

fn role_name(role: api::Role) -> &'static str {
    match role {
        api::Role::System => "system",
        api::Role::User => "user",
        api::Role::Assistant => "assistant",
        api::Role::Tool => "tool",
    }
}

fn capabilities(model: &Value, id: &str, tier: Option<&str>) -> Vec<api::Capability> {
    let mut capabilities: Vec<api::Capability> = model
        .get("capabilities")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|value| match value {
                    "chat" => Some(api::Capability::Chat),
                    "tools" => Some(api::Capability::Tools),
                    "vision" => Some(api::Capability::Vision),
                    "embeddings" => Some(api::Capability::Embeddings),
                    "rerank" => Some(api::Capability::Rerank),
                    "transcription" | "stt" | "audio-transcription" => {
                        Some(api::Capability::Transcription)
                    }
                    "speech" | "tts" | "audio-speech" => Some(api::Capability::Speech),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    if capabilities.is_empty() {
        if id.contains("embed") || tier == Some("embed") {
            capabilities.push(api::Capability::Embeddings);
        } else {
            capabilities.push(api::Capability::Chat);
        }
    }
    capabilities
}

pub fn add_to_linker<T>(linker: &mut Linker<T>) -> anyhow::Result<()>
where
    T: api::Host + 'static,
{
    api::add_to_linker::<T, HasSelf<T>>(linker, |state| state)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn voice_embed_sends_multipart_pcm_to_the_audio_route() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let worker = thread::spawn(move || {
            let mut request = server.recv().unwrap();
            assert!(request.url().ends_with("/audio/embeddings"));
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body).unwrap();
            let body = String::from_utf8_lossy(&body);
            assert!(body.contains("name=\"model\""));
            assert!(body.contains("fpl/spkrec-ecapa-voxceleb"));
            assert!(body.contains("name=\"checkpoint\""));
            assert!(body.contains("name=\"file\"; filename=\"utterance.pcm\""));
            request
                .respond(
                    tiny_http::Response::from_string(
                        r#"{"model":"fpl/spkrec-ecapa-voxceleb","embedding":[0.1,0.2],"usage":{"prompt_tokens":4,"total_tokens":4}}"#,
                    )
                    .with_header(
                        tiny_http::Header::from_bytes("content-type", "application/json").unwrap(),
                    ),
                )
                .unwrap();
        });
        let mut host = InferenceHost::new(format!("http://{address}"), None, "soil", "default");
        let response = api::Host::voice_embed(
            &mut host,
            api::VoiceEmbeddingRequest {
                model: "fpl/spkrec-ecapa-voxceleb".into(),
                checkpoint: "sha256:test".into(),
                sample_rate: 16_000,
                dimensions: Some(2),
                pcm16: vec![0, 0, 1, 0],
            },
        )
        .unwrap();
        assert_eq!(response.embedding, vec![0.1, 0.2]);
        assert_eq!(response.usage.unwrap().total_tokens, 4);
        worker.join().unwrap();
    }

    #[test]
    fn transcribe_sends_wav_to_the_stt_route() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let worker = thread::spawn(move || {
            let mut request = server.recv().unwrap();
            assert!(request.url().ends_with("/audio/transcriptions"));
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body).unwrap();
            let body = String::from_utf8_lossy(&body);
            assert!(body.contains("name=\"model\""));
            assert!(body.contains("scribe/stt"));
            assert!(body.contains("name=\"file\"; filename=\"audio.wav\""));
            assert!(body.contains("RIFF"));
            request
                .respond(
                    tiny_http::Response::from_string(
                        r#"{"model":"scribe/stt","text":"hello world","language":"en","duration":1.5,"segments":[{"text":"hello world","start":0.0,"end":1.5,"speaker":0,"words":[{"word":"hello","start":0.0,"end":0.6,"probability":0.9}]}]}"#,
                    )
                    .with_header(
                        tiny_http::Header::from_bytes("content-type", "application/json").unwrap(),
                    ),
                )
                .unwrap();
        });
        let mut host = InferenceHost::new(format!("http://{address}"), None, "soil", "default")
            .with_stt_url(format!("http://{address}"))
            .with_stt_model("scribe/stt");
        let response = api::Host::transcribe(
            &mut host,
            api::TranscribeRequest {
                model: "scribe/stt".into(),
                sample_rate: 16_000,
                pcm16: vec![0, 0, 1, 0],
                language: Some("en".into()),
                prompt: None,
                timestamps: true,
                diarize: true,
            },
        )
        .unwrap();
        assert_eq!(response.text, "hello world");
        assert_eq!(response.segments[0].speaker.as_deref(), Some("SPEAKER_00"));
        assert_eq!(response.segments[0].words[0].text, "hello");
        assert_eq!(response.duration_ms, Some(1500));
        worker.join().unwrap();
    }

    #[test]
    fn lists_models_and_completes_through_an_upstream() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let worker = thread::spawn(move || {
            for _ in 0..2 {
                let request = server.recv().unwrap();
                if request.url().ends_with("/models") {
                    request
                        .respond(
                            tiny_http::Response::from_string(
                                r#"{"data":[{"id":"laya","context_window":4096}]}"#,
                            )
                            .with_header(
                                tiny_http::Header::from_bytes("content-type", "application/json")
                                    .unwrap(),
                            ),
                        )
                        .unwrap();
                } else {
                    request
                        .respond(
                            tiny_http::Response::from_string(
                                r#"{"model":"laya","choices":[{"message":{"content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}"#,
                            )
                            .with_header(
                                tiny_http::Header::from_bytes(
                                    "content-type",
                                    "application/json",
                                )
                                .unwrap(),
                            ),
                        )
                        .unwrap();
                }
            }
        });
        let mut host = InferenceHost::new(format!("http://{address}"), None, "umie", "laya");
        let models = api::Host::models(&mut host).unwrap();
        assert_eq!(models[0].id, "laya");
        let response = api::Host::complete(
            &mut host,
            api::Request {
                model: "laya".into(),
                messages: vec![api::Message {
                    role: api::Role::User,
                    content: Some("hi".into()),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                    attachments: Vec::new(),
                    reasoning: None,
                }],
                tools: Vec::new(),
                max_output_tokens: None,
                temperature: None,
                prompt_cache_key: None,
            },
        )
        .unwrap();
        assert_eq!(response.message.content.as_deref(), Some("hello"));
        worker.join().unwrap();
    }

    #[test]
    fn maps_tools_and_tool_calls_through_the_host_contract() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let worker = thread::spawn(move || {
            let mut request = server.recv().unwrap();
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body).unwrap();
            let body: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["tools"][0]["function"]["name"], "read_file");
            assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
            assert_eq!(body["messages"][1]["role"], "tool");
            assert_eq!(body["messages"][1]["tool_call_id"], "call_1");
            request
                .respond(
                    tiny_http::Response::from_string(
                        r#"{"model":"laya","choices":[{"message":{"content":null,"tool_calls":[{"id":"call_2","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"notes.md\"}"}}]},"finish_reason":"tool_calls"}]}"#,
                    )
                    .with_header(
                        tiny_http::Header::from_bytes("content-type", "application/json").unwrap(),
                    ),
                )
                .unwrap();
        });
        let mut host = InferenceHost::new(format!("http://{address}"), None, "umie", "laya");
        let response = api::Host::complete(
            &mut host,
            api::Request {
                model: "laya".into(),
                messages: vec![
                    api::Message {
                        role: api::Role::User,
                        content: Some("read notes".into()),
                        name: None,
                        tool_call_id: None,
                        tool_calls: Vec::new(),
                        attachments: Vec::new(),
                        reasoning: None,
                    },
                    api::Message {
                        role: api::Role::Tool,
                        content: Some("contents".into()),
                        name: None,
                        tool_call_id: Some("call_1".into()),
                        tool_calls: Vec::new(),
                        attachments: Vec::new(),
                        reasoning: None,
                    },
                ],
                tools: vec![api::Tool {
                    name: "read_file".into(),
                    description: "read a file".into(),
                    parameters: r#"{"type":"object","properties":{"path":{"type":"string"}}}"#
                        .into(),
                }],
                max_output_tokens: None,
                temperature: None,
                prompt_cache_key: None,
            },
        )
        .unwrap();
        assert!(response.message.content.is_none());
        assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(response.message.tool_calls[0].id, "call_2");
        assert_eq!(response.message.tool_calls[0].name, "read_file");
        assert_eq!(
            response.message.tool_calls[0].arguments,
            r#"{"path":"notes.md"}"#
        );
        worker.join().unwrap();
    }

    #[test]
    fn streams_sse_chunks() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let worker = thread::spawn(move || {
            let request = server.recv().unwrap();
            request
                .respond(
                    tiny_http::Response::from_string(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
                    )
                    .with_header(
                        tiny_http::Header::from_bytes("content-type", "text/event-stream").unwrap(),
                    ),
                )
                .unwrap();
        });
        let mut host = InferenceHost::new(format!("http://{address}"), None, "umie", "laya");
        let stream = api::Host::open_stream(
            &mut host,
            api::Request {
                model: "laya".into(),
                messages: vec![],
                tools: Vec::new(),
                max_output_tokens: None,
                temperature: None,
                prompt_cache_key: None,
            },
        )
        .unwrap();
        let chunk = api::HostCompletionStream::next(&mut host, stream, 1000)
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&chunk.data).contains("data:"));
        worker.join().unwrap();
    }

    #[test]
    fn embeds_through_an_upstream() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let worker = thread::spawn(move || {
            let mut request = server.recv().unwrap();
            assert!(request.url().ends_with("/embeddings"));
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body).unwrap();
            let body: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["input"].as_array().unwrap().len(), 2);
            assert_eq!(body["dimensions"], 2);
            request
                .respond(
                    tiny_http::Response::from_string(
                        r#"{"model":"tier/embed","data":[{"index":1,"embedding":[3.0,4.0]},{"index":0,"embedding":[1.0,2.0]}],"usage":{"prompt_tokens":4,"total_tokens":4}}"#,
                    )
                    .with_header(
                        tiny_http::Header::from_bytes("content-type", "application/json").unwrap(),
                    ),
                )
                .unwrap();
        });
        let mut host = InferenceHost::new(format!("http://{address}"), None, "umie", "tier/embed");
        let response = api::Host::embed(
            &mut host,
            api::EmbedRequest {
                model: "tier/embed".into(),
                inputs: vec!["first".into(), "second".into()],
                dimensions: Some(2),
                prompt_cache_key: None,
            },
        )
        .unwrap();
        assert_eq!(response.model, "tier/embed");
        assert_eq!(response.vectors, vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
        assert_eq!(response.usage.unwrap().total_tokens, 4);
        worker.join().unwrap();
    }
}
