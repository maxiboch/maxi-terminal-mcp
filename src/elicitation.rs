use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

/// Tracks whether the client supports elicitation
#[derive(Debug, Clone, Default)]
pub struct ClientCapabilities {
    pub elicitation: bool,
}

impl ClientCapabilities {
    /// Parse capabilities from MCP initialize request params
    pub fn from_initialize_params(params: &Value) -> Self {
        let elicitation = params
            .get("capabilities")
            .and_then(|c| c.get("elicitation"))
            .is_some();
        
        Self { elicitation }
    }
}

/// Elicitation schema types (MCP spec: flat objects with primitives only)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PrimitiveSchema {
    String {
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        format: Option<String>,
        #[serde(rename = "enum", skip_serializing_if = "Option::is_none")]
        enum_values: Option<Vec<String>>,
        #[serde(rename = "enumNames", skip_serializing_if = "Option::is_none")]
        enum_names: Option<Vec<String>>,
    },
    Number {
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        default: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        minimum: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        maximum: Option<f64>,
    },
    Integer {
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        default: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        minimum: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        maximum: Option<i64>,
    },
    Boolean {
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        default: Option<bool>,
    },
}

/// Request schema for elicitation (flat object with primitive properties)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestSchema {
    #[serde(rename = "type")]
    pub schema_type: String,
    pub properties: HashMap<String, PrimitiveSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<Vec<String>>,
}

impl Default for RequestSchema {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestSchema {
    pub fn new() -> Self {
        Self {
            schema_type: "object".to_string(),
            properties: HashMap::new(),
            required: None,
        }
    }

    pub fn with_string(mut self, name: &str, title: Option<&str>, description: Option<&str>) -> Self {
        self.properties.insert(
            name.to_string(),
            PrimitiveSchema::String {
                title: title.map(String::from),
                description: description.map(String::from),
                default: None,
                format: None,
                enum_values: None,
                enum_names: None,
            },
        );
        self
    }

    pub fn with_string_default(mut self, name: &str, title: Option<&str>, default: &str) -> Self {
        self.properties.insert(
            name.to_string(),
            PrimitiveSchema::String {
                title: title.map(String::from),
                description: None,
                default: Some(default.to_string()),
                format: None,
                enum_values: None,
                enum_names: None,
            },
        );
        self
    }

    pub fn with_enum(mut self, name: &str, title: Option<&str>, options: Vec<String>) -> Self {
        self.properties.insert(
            name.to_string(),
            PrimitiveSchema::String {
                title: title.map(String::from),
                description: None,
                default: None,
                format: None,
                enum_values: Some(options),
                enum_names: None,
            },
        );
        self
    }

    #[allow(dead_code)]
    pub fn with_enum_named(mut self, name: &str, title: Option<&str>, options: Vec<String>, names: Vec<String>) -> Self {
        self.properties.insert(
            name.to_string(),
            PrimitiveSchema::String {
                title: title.map(String::from),
                description: None,
                default: None,
                format: None,
                enum_values: Some(options),
                enum_names: Some(names),
            },
        );
        self
    }

    pub fn with_boolean(mut self, name: &str, title: Option<&str>, default: Option<bool>) -> Self {
        self.properties.insert(
            name.to_string(),
            PrimitiveSchema::Boolean {
                title: title.map(String::from),
                description: None,
                default,
            },
        );
        self
    }

    #[allow(dead_code)]
    pub fn with_integer(mut self, name: &str, title: Option<&str>, min: Option<i64>, max: Option<i64>) -> Self {
        self.properties.insert(
            name.to_string(),
            PrimitiveSchema::Integer {
                title: title.map(String::from),
                description: None,
                default: None,
                minimum: min,
                maximum: max,
            },
        );
        self
    }

    pub fn required_fields(mut self, fields: Vec<&str>) -> Self {
        self.required = Some(fields.into_iter().map(String::from).collect());
        self
    }
}

/// Elicitation response result
#[derive(Debug, Clone, Deserialize)]
pub struct ElicitResult {
    pub action: String,
    #[serde(default)]
    pub content: Option<HashMap<String, Value>>,
}

/// Parsed elicitation action
#[derive(Debug, Clone)]
pub enum ElicitAction {
    Accept(HashMap<String, Value>),
    Decline,
    Cancel,
}

impl ElicitResult {
    pub fn into_action(self) -> ElicitAction {
        match self.action.as_str() {
            "accept" => ElicitAction::Accept(self.content.unwrap_or_default()),
            "decline" => ElicitAction::Decline,
            _ => ElicitAction::Cancel,
        }
    }
}

/// Manager for elicitation requests (handles bidirectional JSON-RPC)
pub struct ElicitationManager {
    request_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ElicitResult>>>>,
}

impl Default for ElicitationManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ElicitationManager {
    pub fn new() -> Self {
        Self {
            request_id: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Create an elicitation request JSON-RPC message
    pub fn create_request(&self, message: &str, schema: RequestSchema) -> (u64, Value) {
        let id = self.request_id.fetch_add(1, Ordering::SeqCst);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "elicitation/create",
            "params": {
                "message": message,
                "requestedSchema": schema
            }
        });
        (id, request)
    }

    /// Register a pending request and get a receiver for the response
    pub async fn register_pending(&self, id: u64) -> oneshot::Receiver<ElicitResult> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        rx
    }

    /// Handle an incoming response to a pending elicitation request
    pub async fn handle_response(&self, id: u64, result: ElicitResult) -> bool {
        if let Some(tx) = self.pending.lock().await.remove(&id) {
            let _ = tx.send(result);
            true
        } else {
            false
        }
    }

    /// Check if an incoming message is an elicitation response
    pub fn is_elicitation_response(msg: &Value) -> Option<(u64, ElicitResult)> {
        if msg.get("method").is_some() {
            return None;
        }
        
        let id = msg.get("id")?.as_u64()?;
        let result = msg.get("result")?;
        
        let elicit_result: ElicitResult = serde_json::from_value(result.clone()).ok()?;
        Some((id, elicit_result))
    }
}

/// Helper to build common elicitation schemas
pub mod schemas {
    use super::*;

    /// Simple text input
    pub fn text_input(field_name: &str, title: &str) -> RequestSchema {
        RequestSchema::new()
            .with_string(field_name, Some(title), None)
            .required_fields(vec![field_name])
    }

    /// Yes/No confirmation
    pub fn confirm(field_name: &str, title: &str) -> RequestSchema {
        RequestSchema::new()
            .with_boolean(field_name, Some(title), Some(false))
            .required_fields(vec![field_name])
    }

    /// Single selection from options
    pub fn select(field_name: &str, title: &str, options: Vec<String>) -> RequestSchema {
        RequestSchema::new()
            .with_enum(field_name, Some(title), options)
            .required_fields(vec![field_name])
    }

    /// Multiple selection hint (MCP doesn't support true multiselect)
    pub fn multiselect_hint(field_name: &str, title: &str, options: Vec<String>) -> RequestSchema {
        let options_str = options.join(", ");
        RequestSchema::new()
            .with_string(
                field_name, 
                Some(title), 
                Some(&format!("Choose from: {}. Separate multiple with commas.", options_str))
            )
            .required_fields(vec![field_name])
    }
}
