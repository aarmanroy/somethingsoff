//! Log format definitions
//!
//! Defines all supported log formats for parsing

use serde::{Deserialize, Serialize};

/// Supported log formats
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogFormat {
    /// JSON Lines format (one JSON object per line)
    Json,
    /// logfmt (key=value pairs)
    Logfmt,
    /// Python logging module format
    PythonLogging,
    /// Go logrus text format
    GoLogrus,
    /// Apache/Nginx Combined Log Format
    ApacheCombined,
    /// Java Log4j/Log4j2 format
    Log4j,
    /// Syslog (RFC3164 / RFC5424)
    Syslog,
    /// Unknown/unsupported format
    Unknown,
}

impl LogFormat {
    /// Get the format name as a string
    pub fn name(&self) -> &str {
        match self {
            LogFormat::Json => "json",
            LogFormat::Logfmt => "logfmt",
            LogFormat::PythonLogging => "python-logging",
            LogFormat::GoLogrus => "go-logrus",
            LogFormat::ApacheCombined => "apache-combined",
            LogFormat::Log4j => "log4j",
            LogFormat::Syslog => "syslog",
            LogFormat::Unknown => "unknown",
        }
    }

    /// Parse format from string
    pub fn from_format_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "json" | "jsonl" => Some(LogFormat::Json),
            "logfmt" => Some(LogFormat::Logfmt),
            "python" | "python-logging" => Some(LogFormat::PythonLogging),
            "go" | "logrus" | "go-logrus" => Some(LogFormat::GoLogrus),
            "apache" | "nginx" | "apache-combined" => Some(LogFormat::ApacheCombined),
            "log4j" | "log4j2" => Some(LogFormat::Log4j),
            "syslog" => Some(LogFormat::Syslog),
            _ => None,
        }
    }
}

impl std::fmt::Display for LogFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_names() {
        assert_eq!(LogFormat::Json.name(), "json");
        assert_eq!(LogFormat::PythonLogging.name(), "python-logging");
        assert_eq!(LogFormat::GoLogrus.name(), "go-logrus");
        assert_eq!(LogFormat::ApacheCombined.name(), "apache-combined");
        assert_eq!(LogFormat::Log4j.name(), "log4j");
        assert_eq!(LogFormat::Unknown.name(), "unknown");
    }

    #[test]
    fn test_format_from_str() {
        assert_eq!(LogFormat::from_format_str("json"), Some(LogFormat::Json));
        assert_eq!(LogFormat::from_format_str("JSONL"), Some(LogFormat::Json));
        assert_eq!(
            LogFormat::from_format_str("python"),
            Some(LogFormat::PythonLogging)
        );
        assert_eq!(
            LogFormat::from_format_str("go-logrus"),
            Some(LogFormat::GoLogrus)
        );
        assert_eq!(
            LogFormat::from_format_str("apache"),
            Some(LogFormat::ApacheCombined)
        );
        assert_eq!(LogFormat::from_format_str("log4j"), Some(LogFormat::Log4j));
        assert_eq!(LogFormat::from_format_str("unknown"), None);
    }

    #[test]
    fn test_format_display() {
        assert_eq!(format!("{}", LogFormat::Json), "json");
        assert_eq!(format!("{}", LogFormat::PythonLogging), "python-logging");
    }
}
