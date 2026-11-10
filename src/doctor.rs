//! The `abortWatch` key bridges add to their `doctor` output.

use serde::Serialize;
use serde_json::Value;

use crate::protocol::Mechanism;

/// `{"mechanism": "sway"|null, "available": bool, "reason": "..."|null}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AbortWatchDoctor {
    pub mechanism: Option<Mechanism>,
    pub available: bool,
    pub reason: Option<String>,
}

impl AbortWatchDoctor {
    /// `abort-watch` would arm with `mechanism`.
    pub fn available(mechanism: Mechanism) -> Self {
        Self {
            mechanism: Some(mechanism),
            available: true,
            reason: None,
        }
    }

    /// `abort-watch` would fail; `mechanism` is the one that was probed, or
    /// `None` when the desktop has no mechanism at all (GNOME, niri, ...).
    pub fn unavailable(mechanism: Option<Mechanism>, reason: impl Into<String>) -> Self {
        Self {
            mechanism,
            available: false,
            reason: Some(reason.into()),
        }
    }

    /// The inner object.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// `{"abortWatch": {...}}`, to merge into a doctor object.
    pub fn fragment(&self) -> Value {
        serde_json::json!({ "abortWatch": self.to_value() })
    }
}

/// Shorthand for [`AbortWatchDoctor::fragment`].
pub fn doctor_fragment(
    mechanism: Option<Mechanism>,
    available: bool,
    reason: Option<&str>,
) -> Value {
    AbortWatchDoctor {
        mechanism,
        available,
        reason: reason.map(str::to_owned),
    }
    .fragment()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        assert_eq!(
            AbortWatchDoctor::available(Mechanism::Sway)
                .fragment()
                .to_string(),
            r#"{"abortWatch":{"available":true,"mechanism":"sway","reason":null}}"#
        );
        assert_eq!(
            AbortWatchDoctor::unavailable(None, "no global Escape on GNOME").to_value(),
            serde_json::json!({"mechanism": null, "available": false, "reason": "no global Escape on GNOME"})
        );
        assert_eq!(
            doctor_fragment(Some(Mechanism::Xi2), false, Some("XInput2 missing")),
            serde_json::json!({"abortWatch": {"mechanism": "xi2", "available": false, "reason": "XInput2 missing"}})
        );
        // As a serde field (bridges with a typed doctor struct).
        #[derive(Serialize)]
        struct Report {
            #[serde(rename = "abortWatch")]
            abort_watch: AbortWatchDoctor,
        }
        let r = Report {
            abort_watch: AbortWatchDoctor::available(Mechanism::Kglobalaccel),
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"abortWatch":{"mechanism":"kglobalaccel","available":true,"reason":null}}"#
        );
    }
}
