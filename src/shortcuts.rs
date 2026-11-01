pub fn format(text: &str) -> String {
    format_for(text, cfg!(target_os = "macos"))
}

fn format_for(text: &str, mac: bool) -> String {
    if mac {
        return text.to_owned();
    }
    text.replace('⌘', "Ctrl+")
        .replace('⌥', "Alt+")
        .replace('⇧', "Shift+")
        .replace('↩', "Enter")
        .replace('⌫', "Backspace")
}

#[cfg(test)]
mod tests {
    #[test]
    fn shortcut_labels_follow_platform() {
        assert_eq!(
            super::format_for("Run (⇧⌘↩)", false),
            "Run (Shift+Ctrl+Enter)"
        );
        assert_eq!(super::format_for("⌥⌘L", false), "Alt+Ctrl+L");
        assert_eq!(super::format_for("⇧⌘↩", true), "⇧⌘↩");
    }
}
