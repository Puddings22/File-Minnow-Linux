//! "Search Here with File Minnow" in file managers' folder menus, added or
//! removed for the current user from Options. The packages install it for
//! Nemo and Dolphin system-wide; Nautilus (GNOME Files), Caja and Thunar keep
//! such entries per user, so they are only added when the user asks.
use anyhow::{Context, Result, bail};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Manager {
    Nautilus,
    Nemo,
    Caja,
    Thunar,
    Dolphin,
}
pub const MANAGERS: [Manager; 5] = [
    Manager::Nautilus,
    Manager::Nemo,
    Manager::Caja,
    Manager::Thunar,
    Manager::Dolphin,
];
const NAME: &str = "Search Here with File Minnow";
/// Marks File Minnow's entry among the user's Thunar actions.
const THUNAR_ID: &str = "file-minnow-search-here";

/// Where per-user and system-wide files live (the XDG base directories).
#[derive(Clone, Debug)]
pub struct Locations {
    pub data_home: PathBuf,
    pub config_home: PathBuf,
    pub data_dirs: Vec<PathBuf>,
    pub config_dirs: Vec<PathBuf>,
    /// The command written into the menu entries.
    pub program: PathBuf,
}
impl Locations {
    pub fn from_env() -> Self {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let base = |name: &str, fallback: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.join(fallback))
        };
        let list = |name: &str, fallback: &str| -> Vec<PathBuf> {
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| fallback.to_owned())
                .split(':')
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .collect()
        };
        Self {
            data_home: base("XDG_DATA_HOME", ".local/share"),
            config_home: base("XDG_CONFIG_HOME", ".config"),
            data_dirs: list("XDG_DATA_DIRS", "/usr/local/share:/usr/share"),
            config_dirs: list("XDG_CONFIG_DIRS", "/etc/xdg"),
            program: program(),
        }
    }
}
/// `file-minnow` when that name on the PATH is this program, else the full
/// path of this program (a portable install outside the PATH).
fn program() -> PathBuf {
    let Ok(current) = std::env::current_exe().and_then(fs::canonicalize) else {
        return PathBuf::from("file-minnow");
    };
    let on_path = std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path)
            .any(|dir| fs::canonicalize(dir.join("file-minnow")).is_ok_and(|p| p == current))
    });
    if on_path {
        PathBuf::from("file-minnow")
    } else {
        current
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub manager: Manager,
    /// The file manager is installed (its program is on the PATH).
    pub available: bool,
    /// Installed system-wide by the package.
    pub system: bool,
    /// Added for this user.
    pub user: bool,
}
impl Manager {
    pub fn label(self) -> &'static str {
        match self {
            Self::Nautilus => "GNOME Files (Nautilus)",
            Self::Nemo => "Nemo",
            Self::Caja => "Caja",
            Self::Thunar => "Thunar",
            Self::Dolphin => "Dolphin",
        }
    }
    fn program(self) -> &'static str {
        match self {
            Self::Nautilus => "nautilus",
            Self::Nemo => "nemo",
            Self::Caja => "caja",
            Self::Thunar => "thunar",
            Self::Dolphin => "dolphin",
        }
    }
    /// The file holding the entry, relative to a data or config directory.
    fn relative(self) -> PathBuf {
        PathBuf::from(match self {
            Self::Nautilus => "nautilus/scripts/Search Here with File Minnow",
            Self::Caja => "caja/scripts/Search Here with File Minnow",
            Self::Nemo => "nemo/actions/file-minnow-search-here.nemo_action",
            Self::Dolphin => "kio/servicemenus/file-minnow-search-here.desktop",
            Self::Thunar => "Thunar/uca.xml",
        })
    }
    /// Caja and Thunar keep these under the config directory, the others
    /// under the data directory.
    fn in_config(self) -> bool {
        matches!(self, Self::Caja | Self::Thunar)
    }
    fn user_file(self, at: &Locations) -> PathBuf {
        let base = if self.in_config() {
            &at.config_home
        } else {
            &at.data_home
        };
        base.join(self.relative())
    }
    pub fn state(self, at: &Locations) -> State {
        let available = std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| dir.join(self.program()).is_file())
        });
        self.state_with(at, available)
    }
    fn state_with(self, at: &Locations, available: bool) -> State {
        let system = matches!(self, Self::Nemo | Self::Dolphin)
            && at
                .data_dirs
                .iter()
                .any(|dir| dir.join(self.relative()).is_file());
        let file = self.user_file(at);
        let user = if self == Self::Thunar {
            fs::read_to_string(&file).is_ok_and(|text| text.contains(THUNAR_ID))
        } else {
            file.is_file()
        };
        State {
            manager: self,
            available,
            system,
            user,
        }
    }
    pub fn install(self, at: &Locations) -> Result<()> {
        let file = self.user_file(at);
        // Desktop entries quote with double quotes; the others are parsed
        // like a shell command line.
        let program = quoted(&at.program, self == Self::Dolphin)?;
        let (text, executable) = match self {
            Self::Nautilus | Self::Caja => {
                let selected = if self == Self::Nautilus {
                    "NAUTILUS_SCRIPT_SELECTED_FILE_PATHS"
                } else {
                    "CAJA_SCRIPT_SELECTED_FILE_PATHS"
                };
                (
                    format!(
                        "#!/bin/sh\n# Added by File Minnow (Options > Interface); remove it there.\n\
                         # Searches the selected folder, or else the folder that is open.\n\
                         folder=$(printf '%s\\n' \"${selected}\" | head -n 1)\n\
                         [ -d \"$folder\" ] || folder=$(pwd)\n\
                         exec {program} show --in \"$folder\"\n"
                    ),
                    true,
                )
            }
            Self::Nemo => (
                format!(
                    "[Nemo Action]\nName={NAME}\nComment=Find files inside this folder instantly\n\
                     Exec={program} show --in %F\nIcon-Name=file-minnow\nSelection=s\n\
                     Extensions=dir;\nQuote=double\n"
                ),
                false,
            ),
            // Recent KDE versions only read executable service menus here.
            Self::Dolphin => (
                format!(
                    "[Desktop Entry]\nType=Service\nMimeType=inode/directory;\nActions=search;\n\
                     X-KDE-Priority=TopLevel\nX-KDE-StartupNotify=false\n\n\
                     [Desktop Action search]\nName={NAME}\nIcon=file-minnow\n\
                     Exec={program} show --in %f\n"
                ),
                true,
            ),
            Self::Thunar => (thunar_with_action(at, &file, &program)?, false),
        };
        let parent = file.parent().context("No folder for the menu entry")?;
        fs::create_dir_all(parent)?;
        crate::config::atomic_write(&file, text.trim_end().as_bytes())?;
        if executable {
            fs::set_permissions(&file, fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    }
    pub fn uninstall(self, at: &Locations) -> Result<()> {
        let file = self.user_file(at);
        if self == Self::Thunar {
            let Ok(text) = fs::read_to_string(&file) else {
                return Ok(());
            };
            if let Some(rest) = without_thunar_action(&text) {
                crate::config::atomic_write(&file, rest.trim_end().as_bytes())?;
            }
            return Ok(());
        }
        match fs::remove_file(&file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}
/// The program as one word of a command line: single-quoted for a shell,
/// double-quoted for a desktop entry.
fn quoted(program: &Path, desktop_entry: bool) -> Result<String> {
    let text = program
        .to_str()
        .context("The program path is not valid text")?;
    if text.contains(['\n', '\'', '"', '%', '$', '`', '\\']) {
        bail!("The program path cannot be written into a menu entry");
    }
    Ok(
        if text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+".contains(&b))
        {
            text.to_owned()
        } else if desktop_entry {
            format!("\"{text}\"")
        } else {
            format!("'{text}'")
        },
    )
}
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
/// The user's Thunar actions with File Minnow's added. Without a file of
/// their own, Thunar uses the system's default actions (such as "Open
/// Terminal Here"); those are copied first so they stay.
fn thunar_with_action(at: &Locations, file: &Path, program: &str) -> Result<String> {
    let current = fs::read_to_string(file).ok().or_else(|| {
        at.config_dirs
            .iter()
            .find_map(|dir| fs::read_to_string(dir.join("Thunar/uca.xml")).ok())
    });
    let text = current
        .filter(|t| t.contains("</actions>"))
        .unwrap_or_else(|| {
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<actions>\n</actions>\n".into()
        });
    let text = without_thunar_action(&text).unwrap_or(text);
    let action = format!(
        "<action>\n\t<icon>file-minnow</icon>\n\t<name>{NAME}</name>\n\t<submenu></submenu>\n\
         \t<unique-id>{THUNAR_ID}</unique-id>\n\t<command>{} show --in %f</command>\n\
         \t<description>Find files inside this folder instantly</description>\n\
         \t<range></range>\n\t<patterns>*</patterns>\n\t<directories/>\n</action>\n",
        xml_escape(program)
    );
    let end = text
        .rfind("</actions>")
        .context("Unreadable Thunar actions")?;
    Ok(format!("{}{action}{}", &text[..end], &text[end..]))
}
/// The Thunar actions without File Minnow's, if it was there.
fn without_thunar_action(text: &str) -> Option<String> {
    let id = text.find(THUNAR_ID)?;
    let start = text[..id].rfind("<action>")?;
    let end = id + text[id..].find("</action>")? + "</action>".len();
    let end = end + text[end..].len() - text[end..].trim_start_matches(['\n', '\r']).len();
    Some(format!("{}{}", &text[..start], &text[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn locations(temp: &Path) -> Locations {
        Locations {
            data_home: temp.join("data"),
            config_home: temp.join("config"),
            data_dirs: vec![temp.join("system-data")],
            config_dirs: vec![temp.join("system-config")],
            program: PathBuf::from("/opt/File Minnow/file-minnow"),
        }
    }
    #[test]
    fn add_and_remove_for_each_file_manager() {
        let temp = tempfile::tempdir().unwrap();
        let at = locations(temp.path());
        for manager in MANAGERS {
            assert!(!manager.state_with(&at, true).user, "{manager:?}");
            manager.install(&at).unwrap();
            let file = manager.user_file(&at);
            let text = fs::read_to_string(&file).unwrap();
            let program = if manager == Manager::Dolphin {
                "\"/opt/File Minnow/file-minnow\""
            } else {
                "'/opt/File Minnow/file-minnow'"
            };
            assert!(text.contains(&format!("{program} show --in")), "{text}");
            assert!(manager.state_with(&at, true).user, "{manager:?}");
            let executable = fs::metadata(&file).unwrap().permissions().mode() & 0o111 != 0;
            assert_eq!(
                executable,
                matches!(
                    manager,
                    Manager::Nautilus | Manager::Caja | Manager::Dolphin
                )
            );
            manager.install(&at).unwrap();
            assert_eq!(
                fs::read_to_string(&file).unwrap(),
                text,
                "adding twice changes nothing"
            );
            manager.uninstall(&at).unwrap();
            assert!(!manager.state_with(&at, true).user, "{manager:?}");
        }
    }
    #[test]
    fn thunar_keeps_other_actions() {
        let temp = tempfile::tempdir().unwrap();
        let at = locations(temp.path());
        // No user file yet: the system defaults are kept.
        let system = temp.path().join("system-config/Thunar");
        fs::create_dir_all(&system).unwrap();
        fs::write(
            system.join("uca.xml"),
            "<?xml version=\"1.0\"?>\n<actions>\n<action>\n\t<name>Open Terminal Here</name>\n\t<unique-id>1-1</unique-id>\n\t<command>exo-open --launch TerminalEmulator</command>\n</action>\n</actions>\n",
        )
        .unwrap();
        Manager::Thunar.install(&at).unwrap();
        let file = Manager::Thunar.user_file(&at);
        let text = fs::read_to_string(&file).unwrap();
        assert!(
            text.contains("Open Terminal Here") && text.contains(THUNAR_ID),
            "{text}"
        );
        assert!(text.find(THUNAR_ID) < text.rfind("</actions>"));
        Manager::Thunar.uninstall(&at).unwrap();
        let text = fs::read_to_string(&file).unwrap();
        assert!(
            text.contains("Open Terminal Here") && !text.contains(THUNAR_ID),
            "{text}"
        );
        assert!(text.trim_end().ends_with("</action>\n</actions>"), "{text}");
    }
    #[test]
    fn system_entries_and_program_quoting() {
        let temp = tempfile::tempdir().unwrap();
        let at = locations(temp.path());
        let nemo = temp
            .path()
            .join("system-data/nemo/actions/file-minnow-search-here.nemo_action");
        fs::create_dir_all(nemo.parent().unwrap()).unwrap();
        fs::write(&nemo, "x").unwrap();
        assert!(Manager::Nemo.state_with(&at, true).system);
        assert!(!Manager::Dolphin.state_with(&at, true).system);
        assert_eq!(
            quoted(Path::new("/usr/bin/file-minnow"), false).unwrap(),
            "/usr/bin/file-minnow"
        );
        assert!(quoted(Path::new("/tmp/it's/file-minnow"), false).is_err());
        assert!(quoted(Path::new("/tmp/$x/file-minnow"), true).is_err());
    }
}
