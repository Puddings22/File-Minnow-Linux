use anyhow::{Result, bail};
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_key: bool,
    pub key: String,
}
impl Chord {
    pub fn parse(input: &str) -> Result<Self> {
        let mut chord = Self {
            ctrl: false,
            alt: false,
            shift: false,
            super_key: false,
            key: String::new(),
        };
        for part in input.split('+').map(str::trim) {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => {
                    if chord.ctrl {
                        bail!("Duplicate Ctrl modifier");
                    }
                    chord.ctrl = true;
                }
                "alt" => {
                    if chord.alt {
                        bail!("Duplicate Alt modifier");
                    }
                    chord.alt = true;
                }
                "shift" => {
                    if chord.shift {
                        bail!("Duplicate Shift modifier");
                    }
                    chord.shift = true;
                }
                "super" | "meta" | "win" => {
                    if chord.super_key {
                        bail!("Duplicate Super modifier");
                    }
                    chord.super_key = true;
                }
                key => {
                    if !chord.key.is_empty() {
                        bail!("Use one key per shortcut");
                    }
                    chord.key = match key {
                        "space" => "Space".into(),
                        "enter" | "return" => "Enter".into(),
                        "esc" | "escape" => "Escape".into(),
                        "tab" => "Tab".into(),
                        "backspace" => "Backspace".into(),
                        "delete" | "del" => "Delete".into(),
                        "insert" => "Insert".into(),
                        "home" => "Home".into(),
                        "end" => "End".into(),
                        "pageup" => "PageUp".into(),
                        "pagedown" => "PageDown".into(),
                        "up" | "arrowup" => "ArrowUp".into(),
                        "down" | "arrowdown" => "ArrowDown".into(),
                        "left" | "arrowleft" => "ArrowLeft".into(),
                        "right" | "arrowright" => "ArrowRight".into(),
                        "," | "comma" => "Comma".into(),
                        "." | "period" => "Period".into(),
                        "-" | "minus" => "Minus".into(),
                        "=" | "equals" => "Equals".into(),
                        _ if keypad(key).is_some() => keypad(key).unwrap().0.into(),
                        _ if key.len() == 1 && key.as_bytes()[0].is_ascii_alphanumeric() => {
                            key.to_ascii_uppercase()
                        }
                        _ if key.starts_with('f')
                            && key[1..].parse::<u32>().is_ok_and(|n| (1..=24).contains(&n)) =>
                        {
                            key.to_ascii_uppercase()
                        }
                        _ => bail!("Unsupported shortcut key: {part}"),
                    };
                }
            }
        }
        if chord.key.is_empty() {
            bail!("Shortcut is missing a key");
        }
        Ok(chord)
    }
    pub fn keysym(&self) -> u32 {
        match self.key.as_str() {
            "Space" => 0x20,
            "Enter" => 0xff0d,
            "Escape" => 0xff1b,
            "Tab" => 0xff09,
            "Backspace" => 0xff08,
            "Delete" => 0xffff,
            "Insert" => 0xff63,
            "Home" => 0xff50,
            "End" => 0xff57,
            "PageUp" => 0xff55,
            "PageDown" => 0xff56,
            "ArrowLeft" => 0xff51,
            "ArrowUp" => 0xff52,
            "ArrowRight" => 0xff53,
            "ArrowDown" => 0xff54,
            "Comma" => 44,
            "Period" => 46,
            "Minus" => 45,
            "Equals" => 61,
            s if keypad(s).is_some() => keypad(s).unwrap().1,
            s if s.len() == 1 => s.as_bytes()[0].to_ascii_lowercase() as u32,
            s => 0xffbd + s[1..].parse::<u32>().unwrap_or(1),
        }
    }
    pub fn portal_trigger(&self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("CTRL".to_owned());
        }
        if self.alt {
            parts.push("ALT".to_owned());
        }
        if self.shift {
            parts.push("SHIFT".to_owned());
        }
        if self.super_key {
            parts.push("LOGO".to_owned());
        }
        parts.push(
            match self.key.as_str() {
                "ArrowLeft" => "Left",
                "ArrowRight" => "Right",
                "ArrowUp" => "Up",
                "ArrowDown" => "Down",
                "Enter" => "Return",
                "Space" => "space",
                "Comma" => "comma",
                s => s,
            }
            .to_owned(),
        );
        parts.join("+")
    }
}
/// Numeric keypad keys: canonical name (the X keysym name) and keysym.
/// Accepts the name in any letter case, as GTK reports it when recording.
fn keypad(key: &str) -> Option<(&'static str, u32)> {
    const KEYS: [(&str, u32); 16] = [
        ("KP_Multiply", 0xffaa),
        ("KP_Add", 0xffab),
        ("KP_Subtract", 0xffad),
        ("KP_Decimal", 0xffae),
        ("KP_Divide", 0xffaf),
        ("KP_Enter", 0xff8d),
        ("KP_0", 0xffb0),
        ("KP_1", 0xffb1),
        ("KP_2", 0xffb2),
        ("KP_3", 0xffb3),
        ("KP_4", 0xffb4),
        ("KP_5", 0xffb5),
        ("KP_6", 0xffb6),
        ("KP_7", 0xffb7),
        ("KP_8", 0xffb8),
        ("KP_9", 0xffb9),
    ];
    KEYS.into_iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_keys() {
        assert_eq!(Chord::parse("Ctrl+Alt+Space").unwrap().keysym(), 32);
        assert_eq!(
            Chord::parse("Ctrl+Alt+Space").unwrap().portal_trigger(),
            "CTRL+ALT+space"
        );
        assert_eq!(Chord::parse("F12").unwrap().keysym(), 0xffc9);
        let keypad = Chord::parse("Ctrl+KP_Multiply").unwrap();
        assert_eq!(
            (keypad.key.as_str(), keypad.keysym()),
            ("KP_Multiply", 0xffaa)
        );
        assert_eq!(keypad.portal_trigger(), "CTRL+KP_Multiply");
        for bad in ["", "Ctrl", "Ctrl+Ctrl+A", "Ctrl+A+B", "SomeKey", "Ctrl+"] {
            assert!(Chord::parse(bad).is_err(), "{bad}");
        }
    }
}
