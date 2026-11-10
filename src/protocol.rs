//! Wire format of `abort-watch` (contract v2): JSON lines on stdout, plain
//! command lines on stdin.

use serde::Serialize;

/// The compositor-specific mechanism that delivers the user's Escape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Mechanism {
    /// XInput2 raw key events on the X root window (x11-bridge).
    Xi2,
    /// sway IPC: per-device `bindsym ... nop` + `binding` events (wlroots-bridge).
    Sway,
    /// Hyprland IPC: per-device bind + socket2 events (wlroots-bridge).
    Hyprland,
    /// KDE kglobalaccel global shortcut on the session bus (kwin-portal-bridge).
    Kglobalaccel,
}

impl Mechanism {
    /// The wire name (`"xi2"`, `"sway"`, `"hyprland"`, `"kglobalaccel"`),
    /// also used as the shared-binding directory name.
    pub fn as_str(self) -> &'static str {
        match self {
            Mechanism::Xi2 => "xi2",
            Mechanism::Sway => "sway",
            Mechanism::Hyprland => "hyprland",
            Mechanism::Kglobalaccel => "kglobalaccel",
        }
    }
}

impl std::fmt::Display for Mechanism {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One stdout line. Field order is the declared order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub(crate) enum Event {
    Ready {
        mechanism: Mechanism,
        #[serde(rename = "syntheticSafe")]
        synthetic_safe: bool,
    },
    Abort,
    Error {
        message: String,
    },
    Ack {
        cmd: &'static str,
    },
}

impl Event {
    pub(crate) fn line(&self) -> String {
        // Serializing these plain enums cannot fail.
        let mut s = serde_json::to_string(self)
            .unwrap_or_else(|_| r#"{"event":"error","message":"serialization failed"}"#.to_owned());
        s.push('\n');
        s
    }
}

/// One stdin line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Suspend,
    Resume,
    /// Blank line: ignored without any reply.
    Empty,
    /// Anything else: answered with an `error` event, no exit.
    Unknown(String),
}

/// Longest unknown command echoed back in the error message.
const ECHO_MAX: usize = 64;

pub(crate) fn parse_command(raw: &[u8]) -> Command {
    let text = String::from_utf8_lossy(raw);
    let t = text.trim();
    match t {
        "" => Command::Empty,
        "suspend" => Command::Suspend,
        "resume" => Command::Resume,
        other => Command::Unknown(other.chars().take(ECHO_MAX).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_lines_are_exact() {
        assert_eq!(
            Event::Ready {
                mechanism: Mechanism::Kglobalaccel,
                synthetic_safe: false
            }
            .line(),
            "{\"event\":\"ready\",\"mechanism\":\"kglobalaccel\",\"syntheticSafe\":false}\n"
        );
        assert_eq!(
            Event::Ready {
                mechanism: Mechanism::Xi2,
                synthetic_safe: true
            }
            .line(),
            "{\"event\":\"ready\",\"mechanism\":\"xi2\",\"syntheticSafe\":true}\n"
        );
        assert_eq!(Event::Abort.line(), "{\"event\":\"abort\"}\n");
        assert_eq!(
            Event::Error {
                message: "a \"b\"\nc".into()
            }
            .line(),
            "{\"event\":\"error\",\"message\":\"a \\\"b\\\"\\nc\"}\n"
        );
        assert_eq!(
            Event::Ack { cmd: "suspend" }.line(),
            "{\"event\":\"ack\",\"cmd\":\"suspend\"}\n"
        );
    }

    #[test]
    fn every_line_is_one_line() {
        let ev = Event::Error {
            message: "multi\nline\r\nmessage".into(),
        };
        let line = ev.line();
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn mechanism_names() {
        for (m, s) in [
            (Mechanism::Xi2, "xi2"),
            (Mechanism::Sway, "sway"),
            (Mechanism::Hyprland, "hyprland"),
            (Mechanism::Kglobalaccel, "kglobalaccel"),
        ] {
            assert_eq!(m.as_str(), s);
            assert_eq!(serde_json::to_string(&m).unwrap(), format!("\"{s}\""));
        }
    }

    #[test]
    fn commands() {
        assert_eq!(parse_command(b"suspend"), Command::Suspend);
        assert_eq!(parse_command(b"  resume\r"), Command::Resume);
        assert_eq!(parse_command(b""), Command::Empty);
        assert_eq!(parse_command(b" \t"), Command::Empty);
        assert_eq!(
            parse_command(b"SUSPEND"),
            Command::Unknown("SUSPEND".into())
        );
        assert_eq!(
            parse_command(b"{\"cmd\":\"suspend\"}"),
            Command::Unknown("{\"cmd\":\"suspend\"}".into())
        );
        let long = vec![b'x'; 500];
        match parse_command(&long) {
            Command::Unknown(s) => assert_eq!(s.len(), ECHO_MAX),
            other => panic!("{other:?}"),
        }
    }
}
