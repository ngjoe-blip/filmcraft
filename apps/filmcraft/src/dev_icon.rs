//! Dev-time Linux desktop entry: the shell icon fallback.
//!
//! The Linux shell (Wayland *and* X11) resolves the window to a `.desktop` file by app id (set in
//! `main.rs` via `eframe`'s `with_app_id`) and takes the taskbar / window-list icon from its
//! `Icon=` entry in the hicolor theme. Packaging installs both
//! (`packaging/linux/ai.storyteller.filmcraft.desktop` and the hicolor PNGs) — but a dev machine
//! running `cargo run` has neither, so KDE/GNOME fall back to the generic Wayland icon. The fix is
//! to ensure a user-level entry at startup: when no entry for the app id exists in any XDG data
//! dir, install one (plus the hicolor icon PNGs, embedded so it works whatever the working
//! directory is) into `$XDG_DATA_HOME`/`~/.local/share`. An existing packaged entry is never
//! overridden; the user-owned dev copy is rewritten when it points at another binary (a dev build
//! moves between checkouts). The shell picks the icon up at the *next* app start;
//! `update-desktop-database`/`kbuildsycoca` is not needed for icon lookup.
//!
//! Every failure is reported and skipped — a missing dev icon must never stop the app
//! ([`AGENTS.md`](/AGENTS.md) §0 "never crash").

/// Install the user-level desktop entry and icons when the app id has no entry anywhere (Linux).
pub fn ensure_dev_desktop_entry() {
    if let Err(e) = ensure() {
        eprintln!("filmcraft: desktop entry (taskbar icon) not installed: {e}");
    }
}

/// Install missing files; `Ok` whether anything was written or an entry already existed.
fn ensure() -> Result<(), DevIconError> {
    let file_name = format!("{}.desktop", crate::APP_ID);
    let user_dir = user_data_dir()?;
    let exe = std::env::current_exe().map_err(DevIconError::Io)?;
    let exe = exe.to_string_lossy();
    for dir in search_dirs(&user_dir) {
        let candidate = dir.join("applications").join(&file_name);
        if !candidate.is_file() {
            continue;
        }
        // A packaged entry (any system dir) already gives the shell its icon; only our own
        // user-level copy is kept fresh.
        if dir != user_dir {
            return Ok(());
        }
        let body = std::fs::read_to_string(&candidate).map_err(DevIconError::Io)?;
        if desktop_file_exec_is_ours(&body, &exe) {
            return Ok(());
        }
        break; // a dev entry from another checkout: fall through and rewrite it
    }
    for (size, png) in ICONS {
        let dest = user_dir.join("icons").join("hicolor").join(format!("{size}x{size}")).join("apps").join(format!("{}.png", crate::APP_ID));
        if !dest.is_file()
            && let Some(parent) = dest.parent()
        {
            std::fs::create_dir_all(parent).map_err(DevIconError::Io)?;
            std::fs::write(&dest, png).map_err(DevIconError::Io)?;
        }
    }
    let applications = user_dir.join("applications");
    std::fs::create_dir_all(&applications).map_err(DevIconError::Io)?;
    let body = desktop_file_content(crate::APP_ID, "FilmCraft", &exe);
    std::fs::write(applications.join(&file_name), body).map_err(DevIconError::Io)?;
    Ok(())
}

/// The hicolor icon sizes installed for the taskbar (a subset large enough for panel and topbar;
/// the shell picks the nearest size, so 256 and 512 cover both).
const ICONS: &[(u16, &[u8])] = &[
    (256, include_bytes!("../../../assets/app-icon/hicolor/256x256/apps/ai.storyteller.filmcraft.png")),
    (512, include_bytes!("../../../assets/app-icon/hicolor/512x512/apps/ai.storyteller.filmcraft.png")),
];

/// The user data dir we install into: `$XDG_DATA_HOME`, else `$HOME/.local/share` (XDG base
/// directory spec). Untrusted environment: an empty value means the default, and a relative or
/// missing one is an error — never a guess that would write into the working directory.
fn user_data_dir() -> Result<std::path::PathBuf, DevIconError> {
    if let Some(v) = std::env::var_os("XDG_DATA_HOME")
        && !v.is_empty()
        && std::path::Path::new(&v).is_absolute()
    {
        return Ok(std::path::PathBuf::from(v));
    }
    match std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        Some(home) => Ok(std::path::PathBuf::from(home).join(".local").join("share")),
        None => Err(DevIconError::NoHome),
    }
}

/// The XDG data dirs to search for an existing entry, user dir first, then `$XDG_DATA_DIRS` (or
/// the spec default `/usr/local/share:/usr/share`). Only absolute paths are taken; the spec
/// requires them and a relative one would read the working directory.
fn search_dirs(user: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut dirs = vec![user.to_path_buf()];
    let system = std::env::var_os("XDG_DATA_DIRS")
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    for dir in system.split(':') {
        if std::path::Path::new(dir).is_absolute() {
            dirs.push(std::path::PathBuf::from(dir));
        }
    }
    dirs
}

/// Whether the `.desktop` body's `Exec` line points at our binary (a stale dev entry from another
/// checkout does not, and is rewritten; a packaged `Exec=filmcraft` never matches a dev path).
fn desktop_file_exec_is_ours(body: &str, exe: &str) -> bool {
    body.lines().any(|l| l.strip_prefix("Exec=").is_some_and(|rest| rest.contains(exe)))
}

/// The `.desktop` body: same fields as `packaging/linux/ai.storyteller.filmcraft.desktop`, with
/// `Exec` pointing at this exact binary (a dev build isn't on `$PATH`, so `TryExec` is left out)
/// and the icon by theme name. `exe` is shell-quoted to survive spaces in the path.
fn desktop_file_content(app_id: &str, name: &str, exe: &str) -> String {
    let mut out = String::new();
    out.push_str("[Desktop Entry]\n");
    out.push_str("Type=Application\n");
    out.push_str(&format!("Name={name}\n"));
    out.push_str("GenericName=Video Editor\n");
    out.push_str("Comment=Edit video, color and sound\n");
    out.push_str(&format!("Exec={} %F\n", quote(exe)));
    out.push_str(&format!("Icon={app_id}\n"));
    out.push_str("Terminal=false\n");
    out.push_str("StartupNotify=true\n");
    out.push_str(&format!("StartupWMClass={app_id}\n"));
    out.push_str("Categories=AudioVideo;Video;AudioVideoEditing;\n");
    out.push_str("Keywords=video;editor;film;timeline;color;grading;nle;\n");
    out
}

/// Quote `s` for a shell `Exec=` line (Desktop Entry spec §"Exec string": an argument with
/// reserved characters is wrapped in double quotes, escaping `\`, `"`, `` ` `` and `$`).
fn quote(s: &str) -> String {
    let needs = s.is_empty() || s.bytes().any(|b| b" \"'\\`$<>&~|;()*?[]#".contains(&b));
    if !needs {
        return s.into();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if matches!(c, '\\' | '"' | '`' | '$') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Everything that can keep the dev icon out of the taskbar; printed, never fatal.
#[derive(Debug)]
enum DevIconError {
    Io(std::io::Error),
    NoHome,
}

impl std::fmt::Display for DevIconError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DevIconError::Io(e) => write!(f, "{e}"),
            DevIconError::NoHome => write!(f, "no home directory for the user data dir"),
        }
    }
}

impl From<std::io::Error> for DevIconError {
    fn from(e: std::io::Error) -> Self {
        DevIconError::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The entry mirrors the packaged one (minus `TryExec`, plus the dev `Exec` path).
    #[test]
    fn desktop_file_matches_the_packaged_fields() {
        let body = desktop_file_content("ai.storyteller.filmcraft", "FilmCraft", "/target/debug/filmcraft");
        assert_eq!(
            body,
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=FilmCraft\n\
             GenericName=Video Editor\n\
             Comment=Edit video, color and sound\n\
             Exec=/target/debug/filmcraft %F\n\
             Icon=ai.storyteller.filmcraft\n\
             Terminal=false\n\
             StartupNotify=true\n\
             StartupWMClass=ai.storyteller.filmcraft\n\
             Categories=AudioVideo;Video;AudioVideoEditing;\n\
             Keywords=video;editor;film;timeline;color;grading;nle;\n"
        );
    }

    /// A path with shell-reserved characters stays one `Exec` argument.
    #[test]
    fn exec_quotes_shell_reserved_paths() {
        assert_eq!(quote("/opt/film craft/filmcraft"), "\"/opt/film craft/filmcraft\"");
        assert_eq!(quote("/bin/filmcraft"), "/bin/filmcraft");
        assert_eq!(quote("/a$b\"c\\d"), "\"/a\\$b\\\"c\\\\d\"");
    }

    /// Only an `Exec` line that contains our binary marks the entry as ours — a packaged
    /// `Exec=filmcraft` or another checkout's path does not.
    #[test]
    fn desktop_file_exec_is_ours_matches_only_our_binary() {
        let body = desktop_file_content("ai.storyteller.filmcraft", "FilmCraft", "/a/filmcraft");
        assert!(desktop_file_exec_is_ours(&body, "/a/filmcraft"));
        assert!(!desktop_file_exec_is_ours(&body, "/b/filmcraft"));
        assert!(!desktop_file_exec_is_ours("Exec=filmcraft %F\n", "/a/filmcraft"));
        assert!(!desktop_file_exec_is_ours("Name=filmcraft\n", "/a/filmcraft"));
    }
}
