//! Intent - User intention parsing and representation
//!
//! Intents represent what the user wants to accomplish, extracted from
//! their natural language input.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// The type of action an intent requires
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ActionType {
    /// Just needs a text response (questions, explanations)
    SimpleResponse,
    /// Needs to generate and execute code
    GenerateCode,
    /// Needs to create a UI surface
    GenerateUi,
    /// Needs to perform a system action (file ops, settings)
    SystemAction,
    /// Local model decided this needs cloud AI
    CloudEscalate,
}

/// A parsed user intent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intent {
    /// Brief description of what to do
    pub action: String,
    /// What type of action is needed
    pub action_type: ActionType,
    /// Confidence in the parsing (0.0 - 1.0)
    pub confidence: f32,
    /// Extracted parameters
    pub parameters: serde_json::Value,
    /// Whether this should be routed to cloud AI
    pub requires_cloud: bool,
}

impl Intent {
    /// Create a simple response intent
    pub fn simple_response(action: &str) -> Self {
        Self {
            action: action.to_string(),
            action_type: ActionType::SimpleResponse,
            confidence: 1.0,
            parameters: serde_json::Value::Null,
            requires_cloud: false,
        }
    }

    /// Create a code generation intent
    pub fn generate_code(action: &str, params: serde_json::Value) -> Self {
        Self {
            action: action.to_string(),
            action_type: ActionType::GenerateCode,
            confidence: 1.0,
            parameters: params,
            requires_cloud: false,
        }
    }

    /// Check if this intent should be handled locally
    pub fn is_local(&self) -> bool {
        !self.requires_cloud && self.confidence > 0.7
    }
}

/// Categories of intents for routing
#[derive(Debug, Clone)]
pub enum IntentCategory {
    /// Information retrieval (what, who, when, where, why, how)
    Information,
    /// Creation (make, create, generate, write)
    Creation,
    /// Transformation (convert, change, modify, edit)
    Transformation,
    /// Analysis (analyze, compare, summarize, explain)
    Analysis,
    /// Action (open, run, execute, send, save)
    Action,
    /// Navigation (go to, find, show, display)
    Navigation,
    /// Configuration (set, configure, change setting)
    Configuration,
    /// Unknown / ambiguous
    Unknown,
}

impl IntentCategory {
    /// Categorize based on keywords in the action
    pub fn from_action(action: &str) -> Self {
        let action_lower = action.to_lowercase();

        if action_lower.contains("what")
            || action_lower.contains("who")
            || action_lower.contains("when")
            || action_lower.contains("where")
            || action_lower.contains("tell me")
        {
            return Self::Information;
        }

        if action_lower.contains("create")
            || action_lower.contains("make")
            || action_lower.contains("generate")
            || action_lower.contains("write")
        {
            return Self::Creation;
        }

        if action_lower.contains("convert")
            || action_lower.contains("transform")
            || action_lower.contains("change")
            || action_lower.contains("modify")
        {
            return Self::Transformation;
        }

        if action_lower.contains("analyze")
            || action_lower.contains("compare")
            || action_lower.contains("summarize")
            || action_lower.contains("explain")
        {
            return Self::Analysis;
        }

        if action_lower.contains("open")
            || action_lower.contains("run")
            || action_lower.contains("execute")
            || action_lower.contains("send")
            || action_lower.contains("save")
        {
            return Self::Action;
        }

        if action_lower.contains("find")
            || action_lower.contains("show")
            || action_lower.contains("display")
            || action_lower.contains("go to")
        {
            return Self::Navigation;
        }

        if action_lower.contains("set")
            || action_lower.contains("configure")
            || action_lower.contains("setting")
        {
            return Self::Configuration;
        }

        Self::Unknown
    }
}
