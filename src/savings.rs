//! Backend-independent measures that keep the tokens sent to Ollama or Anthropic down.

use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Refuses a chat request whose body arrives more than `max` times within `window`. A client that
/// resends the same conversation over and over (the Mendix chat did this after an empty answer)
/// would otherwise pay for the whole prompt on every call.
pub struct LoopGuard {
    max: usize,
    window: Duration,
    seen: Mutex<HashMap<u64, Vec<Instant>>>,
}

impl LoopGuard {
    /// `max = 0` turns the guard off.
    pub fn new(max: usize, window: Duration) -> Self {
        Self {
            max,
            window,
            seen: Mutex::new(HashMap::new()),
        }
    }

    pub fn max(&self) -> usize {
        self.max
    }

    /// Records the request and tells whether it has now been seen too often.
    pub fn repeated_too_often(&self, path: &str, body: &[u8]) -> bool {
        if self.max == 0 {
            return false;
        }
        let mut hasher = DefaultHasher::new();
        (path, body).hash(&mut hasher);
        let key = hasher.finish();

        let now = Instant::now();
        let mut seen = self.seen.lock().expect("loop guard lock");
        seen.retain(|_, times| {
            times.retain(|t| now.duration_since(*t) < self.window);
            !times.is_empty()
        });
        let times = seen.entry(key).or_default();
        times.push(now);
        times.len() > self.max
    }
}

/// What [`tidy_tools`] did to a request's tools.
#[derive(Debug, Default, PartialEq)]
pub struct ToolsTidy {
    /// Number of tools the caller sent.
    pub before: usize,
    /// Number of tools left after `--tools-deny`.
    pub after: usize,
    /// Whether the remaining tools had to be sorted.
    pub reordered: bool,
}

impl ToolsTidy {
    /// Whether the request changed.
    pub fn changed(&self) -> bool {
        self.before != self.after || self.reordered
    }
}

/// Drops the tools whose name matches `deny` and sorts the rest by name, so the tool list (the
/// start of the prompt) is the same bytes on every call and the backend's prompt cache can be
/// reused: the Mendix GenAI connector sends its tools in a different order each time. Without
/// tools left, `tools` and `tool_choice` are removed.
pub fn tidy_tools(chat: &mut Value, deny: Option<&Regex>) -> ToolsTidy {
    let Some(tools) = chat.get_mut("tools").and_then(Value::as_array_mut) else {
        return ToolsTidy::default();
    };
    let name = |tool: &Value| {
        tool["function"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    let before = tools.len();
    if let Some(deny) = deny {
        tools.retain(|tool| !deny.is_match(&name(tool)));
    }
    let kept: Vec<String> = tools.iter().map(name).collect();
    tools.sort_by_key(name);
    let tidy = ToolsTidy {
        before,
        after: tools.len(),
        reordered: tools.iter().map(name).ne(kept),
    };

    if tools.is_empty() {
        let map = chat.as_object_mut().expect("tools came from an object");
        map.remove("tools");
        map.remove("tool_choice");
        map.remove("parallel_tool_calls");
    }
    tidy
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> Value {
        json!({"type": "function", "function": {"name": name}})
    }

    #[test]
    fn sorts_and_filters_tools() {
        let mut chat = json!({"tools": [tool("b"), tool("vc_x"), tool("a")]});
        let deny = Regex::new("^vc_").unwrap();
        let tidy = tidy_tools(&mut chat, Some(&deny));
        assert_eq!(
            tidy,
            ToolsTidy {
                before: 3,
                after: 2,
                reordered: true
            }
        );
        assert!(tidy.changed());
        assert_eq!(chat["tools"], json!([tool("a"), tool("b")]));
        // Already tidy: nothing changes.
        assert!(!tidy_tools(&mut chat, Some(&deny)).changed());
    }

    #[test]
    fn removes_tool_choice_without_tools() {
        let mut chat = json!({"tools": [tool("vc_x")], "tool_choice": "auto"});
        assert!(tidy_tools(&mut chat, Regex::new("^vc_").ok().as_ref()).changed());
        assert_eq!(chat, json!({}));
        assert_eq!(
            tidy_tools(&mut json!({"messages": []}), None),
            ToolsTidy::default()
        );
    }

    #[test]
    fn loop_guard_refuses_repeats() {
        let guard = LoopGuard::new(2, Duration::from_secs(60));
        assert!(!guard.repeated_too_often("/chat", b"a"));
        assert!(!guard.repeated_too_often("/chat", b"a"));
        assert!(!guard.repeated_too_often("/chat", b"b"));
        assert!(guard.repeated_too_often("/chat", b"a"));
        assert!(!LoopGuard::new(0, Duration::from_secs(60)).repeated_too_often("/chat", b"a"));
    }
}
