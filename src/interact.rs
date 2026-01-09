use anyhow::{anyhow, Result};
use inquire::{Confirm, Text, MultiSelect, Select};
use serde_json::{json, Value};

pub fn handle_interact(args: &Value) -> Result<Value> {
    // Check for TTY
    if !atty::is(atty::Stream::Stdin) {
        return Err(anyhow!(
            "Interaction requires a TTY. This tool cannot be used in non-interactive environments (piped stdin)."
        ));
    }

    let kind = args
        .get("kind")
        .and_then(|k| k.as_str())
        .ok_or_else(|| anyhow!("Missing kind"))?;
    
    let message = args
        .get("message")
        .and_then(|m| m.as_str())
        .ok_or_else(|| anyhow!("Missing message"))?;

    match kind {
        "input" => {
            let default = args.get("default").and_then(|d| d.as_str());
            let mut prompt = Text::new(message);
            if let Some(d) = default {
                prompt = prompt.with_default(d);
            }
            let result = prompt.prompt()?;
            Ok(json!(result))
        }
        "confirm" => {
            let default = args.get("default").and_then(|d| d.as_bool()).unwrap_or(false);
            let result = Confirm::new(message)
                .with_default(default)
                .prompt()?;
            Ok(json!(result))
        }
        "select" => {
            let options = args
                .get("options")
                .and_then(|o| o.as_array())
                .ok_or_else(|| anyhow!("Missing options for select"))?;
            
            let items: Vec<String> = options
                .iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .collect();
            
            let result = Select::new(message, items).prompt()?;
            Ok(json!(result))
        }
        "multiselect" => {
            let options = args
                .get("options")
                .and_then(|o| o.as_array())
                .ok_or_else(|| anyhow!("Missing options for multiselect"))?;
            
            let items: Vec<String> = options
                .iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .collect();
            
            let result = MultiSelect::new(message, items).prompt()?;
            Ok(json!(result))
        }
        _ => Err(anyhow!("Unknown interaction kind: {}", kind)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Note: In test environment, stdin is NOT a TTY, so the TTY check fails first.
    // We can only meaningfully test that all calls fail (due to no TTY).
    // Full interactive testing would require integration tests with a PTY.
    
    #[test]
    fn test_input_fails_without_tty() {
        let args = json!({
            "kind": "input",
            "message": "Enter something"
        });
        let result = handle_interact(&args);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("TTY"));
    }

    #[test]
    fn test_confirm_fails_without_tty() {
        let args = json!({
            "kind": "confirm",
            "message": "Are you sure?"
        });
        let result = handle_interact(&args);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("TTY"));
    }

    #[test]
    fn test_select_fails_without_tty() {
        let args = json!({
            "kind": "select",
            "message": "Choose one",
            "options": ["a", "b", "c"]
        });
        let result = handle_interact(&args);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("TTY"));
    }

    #[test]
    fn test_multiselect_fails_without_tty() {
        let args = json!({
            "kind": "multiselect", 
            "message": "Choose many",
            "options": ["x", "y", "z"]
        });
        let result = handle_interact(&args);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("TTY"));
    }

    #[test]
    fn test_graceful_tty_error_message() {
        let args = json!({
            "kind": "input",
            "message": "Test"
        });
        let result = handle_interact(&args);
        let err = result.unwrap_err().to_string();
        // Should have a clear message about TTY requirement
        assert!(err.contains("TTY") || err.contains("non-interactive"));
    }
}
