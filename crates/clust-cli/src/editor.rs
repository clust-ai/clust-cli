use std::process::Command;

/// Categories for sorting editors in the picker modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EditorCategory {
    Generic = 0,
    JetBrains = 1,
    Terminal = 2,
}

/// A detected editor on the system.
#[derive(Debug, Clone)]
pub struct DetectedEditor {
    pub name: String,
    pub binary: String,
    pub category: EditorCategory,
}

/// Known editors to scan for, ordered by category.
const KNOWN_EDITORS: &[(&str, &str, EditorCategory)] = &[
    // Generic
    ("VS Code", "code", EditorCategory::Generic),
    ("Cursor", "cursor", EditorCategory::Generic),
    ("Zed", "zed", EditorCategory::Generic),
    ("Sublime Text", "subl", EditorCategory::Generic),
    // JetBrains
    ("IntelliJ IDEA", "idea", EditorCategory::JetBrains),
    ("WebStorm", "webstorm", EditorCategory::JetBrains),
    ("PyCharm", "pycharm", EditorCategory::JetBrains),
    ("GoLand", "goland", EditorCategory::JetBrains),
    ("RustRover", "rustrover", EditorCategory::JetBrains),
    ("CLion", "clion", EditorCategory::JetBrains),
    ("PHPStorm", "phpstorm", EditorCategory::JetBrains),
    ("Rider", "rider", EditorCategory::JetBrains),
    ("Fleet", "fleet", EditorCategory::JetBrains),
    // Terminal
    ("Neovim", "nvim", EditorCategory::Terminal),
    ("Vim", "vim", EditorCategory::Terminal),
    ("Emacs", "emacs", EditorCategory::Terminal),
    ("Helix", "hx", EditorCategory::Terminal),
];

/// Detect installed editors by checking for known binaries in PATH.
/// Returns editors sorted by category (Generic, JetBrains, Terminal).
pub fn detect_installed_editors() -> Vec<DetectedEditor> {
    let mut editors: Vec<DetectedEditor> = KNOWN_EDITORS
        .iter()
        .filter(|(_, binary, _)| which::which(binary).is_ok())
        .map(|(name, binary, category)| DetectedEditor {
            name: name.to_string(),
            binary: binary.to_string(),
            category: *category,
        })
        .collect();
    editors.sort_by_key(|e| e.category);
    editors
}

/// Look up a detected editor by its binary name.
pub fn find_editor_by_binary<'a>(
    editors: &'a [DetectedEditor],
    binary: &str,
) -> Option<&'a DetectedEditor> {
    editors.iter().find(|e| e.binary == binary)
}

/// Open a directory in the given editor.
pub fn open_in_editor(editor: &DetectedEditor, path: &str) {
    if editor.category == EditorCategory::Terminal {
        open_terminal_editor(editor, path);
    } else {
        let _ = Command::new(&editor.binary).arg(path).spawn();
    }
}

/// Open a terminal editor in a new terminal window.
fn open_terminal_editor(editor: &DetectedEditor, path: &str) {
    #[cfg(target_os = "macos")]
    {
        // Use osascript to open a new Terminal.app window with the editor
        let script = format!(
            "tell app \"Terminal\" to do script \"cd {} && {} .\"",
            shell_escape(path),
            editor.binary,
        );
        let _ = Command::new("osascript").args(["-e", &script]).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let terminals: &[(&str, &[&str])] = &[
            ("x-terminal-emulator", &["-e"]),
            ("gnome-terminal", &["--"]),
            ("konsole", &["-e"]),
            ("xfce4-terminal", &["-e"]),
        ];
        for &(bin, args) in terminals {
            let mut cmd = Command::new(bin);
            for arg in args {
                cmd.arg(arg);
            }
            cmd.arg(&editor.binary).arg(path);
            if cmd.spawn().is_ok() {
                return;
            }
        }
    }
}

/// Simple shell escaping for paths (wraps in single quotes, escapes inner quotes).
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
