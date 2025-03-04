use crate::adapter::openai::OpenAIStreamer;
use crate::adapter::support::get_api_key;
use crate::adapter::{Adapter, AdapterKind, ServiceType, WebRequestData};
use crate::chat::{
	ChatOptionsSet, ChatRequest, ChatResponse, ChatResponseFormat, ChatRole, ChatStream, ChatStreamResponse,
	MessageContent, MetaUsage,
};
use crate::webc::WebResponse;
use crate::{ClientConfig, ModelIden};
use crate::{Error, Result};
use reqwest::RequestBuilder;
use reqwest_eventsource::EventSource;
use serde_json::{json, Value};
use value_ext::JsonValueExt;

pub struct OpenAIAdapter;

const BASE_URL: &str = "https://api.openai.com/v1/";
// Latest models
const MODELS: &[&str] = &[
	//
	"gpt-4o",
	"gpt-4o-2024-08-06",
	"gpt-4o-2024-05-13",
	"gpt-4o-mini",
];

impl Adapter for OpenAIAdapter {
	fn default_key_env_name(_kind: AdapterKind) -> Option<&'static str> {
		Some("OPENAI_API_KEY")
	}

	/// Note: For now returns the common ones (see above)
	async fn all_model_names(_kind: AdapterKind) -> Result<Vec<String>> {
		Ok(MODELS.iter().map(|s| s.to_string()).collect())
	}

	fn get_service_url(model_iden: ModelIden, service_type: ServiceType) -> String {
		Self::util_get_service_url(model_iden, service_type, BASE_URL)
	}

	fn to_web_request_data(
		model_iden: ModelIden,
		client_config: &ClientConfig,
		service_type: ServiceType,
		chat_req: ChatRequest,
		chat_options: ChatOptionsSet<'_, '_>,
	) -> Result<WebRequestData> {
		let url = Self::get_service_url(model_iden.clone(), service_type);

		OpenAIAdapter::util_to_web_request_data(model_iden, client_config, chat_req, service_type, chat_options, url)
	}

	fn to_chat_response(model_iden: ModelIden, web_response: WebResponse) -> Result<ChatResponse> {
		let WebResponse { mut body, .. } = web_response;

		let usage = body.x_take("usage").map(OpenAIAdapter::into_usage).unwrap_or_default();

		let first_choice: Option<Value> = body.x_take("/choices/0")?;
		let content: Option<String> = first_choice.map(|mut c| c.x_take("/message/content")).transpose()?;
		let content = content.map(MessageContent::from);

		Ok(ChatResponse {
			model_iden,
			content,
			usage,
		})
	}

	fn to_chat_stream(
		model_iden: ModelIden,
		reqwest_builder: RequestBuilder,
		options_sets: ChatOptionsSet<'_, '_>,
	) -> Result<ChatStreamResponse> {
		let event_source = EventSource::new(reqwest_builder)?;
		let openai_stream = OpenAIStreamer::new(event_source, model_iden.clone(), options_sets);
		let chat_stream = ChatStream::from_inter_stream(openai_stream);

		Ok(ChatStreamResponse {
			model_iden,
			stream: chat_stream,
		})
	}
}

/// Support function for other Adapter that share OpenAI APIs
impl OpenAIAdapter {
	pub(in crate::adapter::adapters) fn util_get_service_url(
		_model_iden: ModelIden,
		service_type: ServiceType,
		// -- util args
		base_url: &str,
	) -> String {
		match service_type {
			ServiceType::Chat | ServiceType::ChatStream => format!("{base_url}chat/completions"),
		}
	}

	pub(in crate::adapter::adapters) fn util_to_web_request_data(
		model_iden: ModelIden,
		client_config: &ClientConfig,
		chat_req: ChatRequest,
		service_type: ServiceType,
		options_set: ChatOptionsSet<'_, '_>,
		base_url: String,
	) -> Result<WebRequestData> {
		let stream = matches!(service_type, ServiceType::ChatStream);

		// -- Get the key
		let api_key = get_api_key(model_iden.clone(), client_config)?;

		// -- Build the header
		let headers = vec![
			// headers
			("Authorization".to_string(), format!("Bearer {api_key}")),
		];

		// -- Build the basic payload
		let model_name = model_iden.model_name.to_string();
		let OpenAIRequestParts { messages } = Self::into_openai_request_parts(model_iden, chat_req)?;
		let mut payload = json!({
			"model": model_name,
			"messages": messages,
			"stream": stream
		});

		// -- Add options
		let response_format = if let Some(response_format) = options_set.response_format() {
			match response_format {
				ChatResponseFormat::JsonMode => Some(json!({"type": "json_object"})),
				ChatResponseFormat::JsonSpec(st_json) => {
					// "type": "json_schema", "json_schema": {...}

					let mut schema = st_json.schema.clone();
					schema.x_walk(|parent_map, name| {
						if name == "type" {
							let typ = parent_map.get("type").and_then(|v| v.as_str()).unwrap_or("");
							if typ == "object" {
								parent_map.insert("additionalProperties".to_string(), false.into());
							}
						}
						true
					});

					Some(json!({
						"type": "json_schema",
						"json_schema": {
							"name": st_json.name.clone(),
							"strict": true,
							// TODO: add description
							"schema": schema,
						}
					}))
				}
			}
		} else {
			None
		};

		if let Some(response_format) = response_format {
			payload["response_format"] = response_format;
		}

		// --
		if stream & options_set.capture_usage().unwrap_or(false) {
			payload.x_insert("stream_options", json!({"include_usage": true}))?;
		}

		// -- Add supported ChatOptions
		if let Some(temperature) = options_set.temperature() {
			payload.x_insert("temperature", temperature)?;
		}
		if let Some(max_tokens) = options_set.max_tokens() {
			payload.x_insert("max_tokens", max_tokens)?;
		}
		if let Some(top_p) = options_set.top_p() {
			payload.x_insert("top_p", top_p)?;
		}

		Ok(WebRequestData {
			url: base_url,
			headers,
			payload,
		})
	}

	/// Note: needs to be called from super::streamer as well
	pub(super) fn into_usage(mut usage_value: Value) -> MetaUsage {
		let input_tokens: Option<i32> = usage_value.x_take("prompt_tokens").ok();
		let output_tokens: Option<i32> = usage_value.x_take("completion_tokens").ok();
		let total_tokens: Option<i32> = usage_value.x_take("total_tokens").ok();
		MetaUsage {
			input_tokens,
			output_tokens,
			total_tokens,
		}
	}

	/// Takes the genai ChatMessages and build the OpenAIChatRequestParts
	/// - `genai::ChatRequest.system`, if present, goes as first message with role 'system'.
	/// - All messages get added with the corresponding roles (does not support tools for now)
	///
	/// NOTE: here, the last `true` is for the ollama variant
	///       It seems the Ollama compatibility layer does not work well with multiple System message.
	///       So, when `true`, it will concatenate the system message as a single on at the beginning
	fn into_openai_request_parts(model_iden: ModelIden, chat_req: ChatRequest) -> Result<OpenAIRequestParts> {
		let mut system_messages: Vec<String> = Vec::new();
		let mut messages: Vec<Value> = Vec::new();

		let ollama_variant = matches!(model_iden.adapter_kind, AdapterKind::Ollama);

		if let Some(system_msg) = chat_req.system {
			if ollama_variant {
				system_messages.push(system_msg)
			} else {
				messages.push(json!({"role": "system", "content": system_msg}));
			}
		}

		for msg in chat_req.messages {
			// Note: Will handle more types later
			let MessageContent::Text(content) = msg.content;

			match msg.role {
				// for now, system and tool goes to system
				ChatRole::System => {
					// see note in the function comment
					if ollama_variant {
						system_messages.push(content);
					} else {
						messages.push(json!({"role": "system", "content": content}))
					}
				}
				ChatRole::User => messages.push(json! ({"role": "user", "content": content})),
				ChatRole::Assistant => messages.push(json! ({"role": "assistant", "content": content})),
				ChatRole::Tool => {
					return Err(Error::MessageRoleNotSupported {
						model_iden,
						role: ChatRole::Tool,
					})
				}
			}
		}

		if !system_messages.is_empty() {
			let system_message = system_messages.join("\n");
			messages.insert(0, json!({"role": "system", "content": system_message}));
		}

		Ok(OpenAIRequestParts { messages })
	}
}

// region:    --- Support

struct OpenAIRequestParts {
	messages: Vec<Value>,
}

// endregion: --- Support
