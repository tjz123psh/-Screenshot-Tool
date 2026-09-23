//! Inspect and safely manage vellum bindings inside a user's Niri KDL config.
//!
//! Two rules drive this module, both learned from the predecessor:
//!
//! * Never touch a file that Niri's config does not actually include. Some
//!   setups ship a generated `binds.kdl` owned by a desktop shell; writing
//!   there is silently overwritten at best.
//! * A partial hotkey set is worse than none. If any default key is already
//!   taken, nothing is written and the caller reports the conflict.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use vellum_core::proc::{self, which};

use super::{Binding, DEFAULT_SHORTCUTS, InstallResult, Status, launcher_path};

/// `niri validate` parses the whole config tree; 10s is the predecessor's
/// budget and is far more than a healthy run needs.
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(10);

/// A live compositor answers `load-config-file` immediately or not at all.
const RELOAD_TIMEOUT: Duration = Duration::from_secs(5);

/// Marker comments delimiting the region this tool owns.
pub const MANAGED_BEGIN: &str = "// >>> vellum managed shortcuts";
pub const MANAGED_END: &str = "// <<< vellum managed shortcuts";

static BIND_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*([^\s{]+)(?:\s+[^{}]+)?\s*\{(.*)\}\s*$").unwrap());
static SPAWN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"\bspawn\s+"(?:[^"]*/)?vellum(?:ctl)?"\s+"({})""#,
        super::action_alternation()
    ))
    .unwrap()
});
static SPAWN_SH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"\bspawn-sh\s+"[^"]*(?:^|/)vellum(?:ctl)?\s+({})(?:\s|;|")"#,
        super::action_alternation()
    ))
    .unwrap()
});
static INCLUDE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^\s*include\s+"([^"]+)""#).unwrap());
static KEY_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*([A-Za-z0-9_+\-]+)(?:\s+[^{}]*)?\s*\{").unwrap());
static BINDS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bbinds\s*\{").unwrap());

/// `$XDG_CONFIG_HOME/niri` or `~/.config/niri`.
pub fn config_dir() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(base) if !base.is_empty() => PathBuf::from(base).join("niri"),
        _ => vellum_core::paths::home().join(".config").join("niri"),
    }
}

/// `config.kdl` plus every existing file it pulls in, recursively.
///
/// Only these files matter: a `*.kdl` lying around unincluded has no effect on
/// the running compositor, so it must not be edited or trusted for conflicts.
pub fn active_config_files(directory: Option<&Path>) -> Vec<PathBuf> {
    let root = directory.map(Path::to_path_buf).unwrap_or_else(config_dir);
    let entry = root.join("config.kdl");
    if !entry.exists() {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    let mut pending = vec![entry.canonicalize().unwrap_or(entry)];
    while let Some(path) = pending.pop() {
        if !path.exists() || !seen.insert(path.clone()) {
            continue;
        }
        found.push(path.clone());
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let parent = path.parent().unwrap_or(Path::new("."));
        let mut includes = Vec::new();
        for capture in INCLUDE_RE.captures_iter(&text) {
            let candidate = parent.join(&capture[1]);
            let candidate = candidate.canonicalize().unwrap_or(candidate);
            if candidate.exists() {
                includes.push(candidate);
            }
        }
        // Reverse so the traversal order matches the include order.
        includes.reverse();
        pending.extend(includes);
    }
    found
}

/// The user-owned keybind file, i.e. an included file named `keybinds.kdl`.
fn included_keybinds_path(root: &Path) -> Option<PathBuf> {
    active_config_files(Some(root))
        .into_iter()
        .find(|path| path.file_name().is_some_and(|name| name == "keybinds.kdl"))
}

/// Blank out strings and comments so brace counting cannot be fooled.
fn mask_code(text: &str) -> Vec<char> {
    let chars: Vec<char> = text.chars().collect();
    let mut masked = chars.clone();
    let mut quote = false;
    let mut escaped = false;
    let mut line_comment = false;
    let mut block_comment = false;
    let mut i = 0;
    while i < chars.len() {
        let char = chars[i];
        let next = chars.get(i + 1).copied().unwrap_or('\0');
        if line_comment {
            if char == '\n' {
                line_comment = false;
            } else {
                masked[i] = ' ';
            }
        } else if block_comment {
            masked[i] = ' ';
            if char == '*' && next == '/' {
                masked[i + 1] = ' ';
                block_comment = false;
                i += 1;
            }
        } else if quote {
            masked[i] = ' ';
            if escaped {
                escaped = false;
            } else if char == '\\' {
                escaped = true;
            } else if char == '"' {
                quote = false;
            }
        } else if char == '/' && next == '/' {
            masked[i] = ' ';
            masked[i + 1] = ' ';
            line_comment = true;
            i += 1;
        } else if char == '/' && next == '*' {
            masked[i] = ' ';
            masked[i + 1] = ' ';
            block_comment = true;
            i += 1;
        } else if char == '"' {
            masked[i] = ' ';
            quote = true;
        }
        i += 1;
    }
    masked
}

/// Byte offsets of the opening and closing brace of the first `binds` block.
fn find_binds_span(text: &str) -> Option<(usize, usize)> {
    let masked = mask_code(text);
    let code: String = masked.iter().collect();
    let found = BINDS_RE.find(&code)?;
    let opening = code[found.start()..found.end()].find('{')? + found.start();
    let mut depth = 0usize;
    for (index, char) in code[opening..].char_indices() {
        match char {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((opening, opening + index));
                }
            }
            _ => {}
        }
    }
    None
}

/// Keys already bound anywhere in the file, used for conflict detection.
fn binding_keys(text: &str) -> HashSet<String> {
    KEY_LINE_RE
        .captures_iter(text)
        .map(|capture| capture[1].to_string())
        .filter(|key| key != "binds")
        .collect()
}

fn render_bindings(items: &[(&str, &str, &str)], indent: &str) -> String {
    let launcher = launcher_path();
    let mut lines = vec![MANAGED_BEGIN.to_string()];
    for (key, action, title) in items {
        // `spawn-sh` rather than `spawn`: it runs through a shell, so `$HOME`
        // expands. Compositors do not spawn with the user's PATH, which is why
        // the path is absolute rather than a bare `vellumctl`.
        lines.push(format!(
            "{key} hotkey-overlay-title=\"{title}\" {{ spawn-sh \"{launcher} {action}\"; }}"
        ));
    }
    lines.push(MANAGED_END.to_string());
    lines
        .into_iter()
        .map(|line| format!("{indent}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn write_backup(path: &Path, text: &str) -> std::io::Result<PathBuf> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let backup = path.with_file_name(format!("{name}.vellum-backup"));
    std::fs::write(&backup, text)?;
    Ok(backup)
}

/// Ask niri itself whether the edit is valid, then reload if it is live.
///
/// Reporting is deliberately verbose: "written but not yet active" and
/// "written and active" are very different states for someone pressing keys.
fn validate_and_reload(root: &Path) -> (bool, String) {
    let config = root.join("config.kdl");
    let Some(niri) = which("niri") else {
        return (
            true,
            "配置已写入；未检测到 niri，登录图形会话后生效".to_string(),
        );
    };
    if !config.exists() {
        return (
            true,
            "配置已写入；未找到 config.kdl，下次 Niri 启动后生效".to_string(),
        );
    }
    let args = [
        std::ffi::OsStr::new("validate"),
        std::ffi::OsStr::new("--config"),
        config.as_os_str(),
    ];
    let Some(checked) = proc::run(&niri, &args, VALIDATE_TIMEOUT) else {
        // Treated as a failure so the caller restores the original file: an
        // unverified config is not something to leave in place.
        return (
            false,
            "无法验证 Niri 配置：niri validate 未返回".to_string(),
        );
    };
    if !checked.success {
        // stderr first, stdout as the fallback: niri has used both.
        let detail = if checked.stderr.trim().is_empty() {
            checked.stdout.trim().to_string()
        } else {
            checked.stderr.trim().to_string()
        };
        return (
            false,
            if detail.is_empty() {
                "Niri 配置验证失败".to_string()
            } else {
                format!("Niri 配置验证失败：{detail}")
            },
        );
    }
    if std::env::var_os("NIRI_SOCKET").is_none() {
        return (true, "配置验证通过；下次 Niri reload 后生效".to_string());
    }
    match proc::run(
        &niri,
        &["msg", "action", "load-config-file"],
        RELOAD_TIMEOUT,
    ) {
        Some(output) if output.success => (true, "配置验证通过并已重新加载".to_string()),
        // The edit is valid and on disk either way, so this is not an error.
        _ => (
            true,
            "配置验证通过；自动重新加载失败，请稍后手动 reload".to_string(),
        ),
    }
}

/// Install the defaults into the active user keybind file.
///
/// `directory` is for tests: passing it skips `niri validate` and the reload,
/// which would otherwise operate on the real session.
pub fn install(directory: Option<&Path>) -> InstallResult {
    let root = directory.map(Path::to_path_buf).unwrap_or_else(config_dir);
    let Some(target) = included_keybinds_path(&root) else {
        return InstallResult::new(
            Status::Unavailable,
            None,
            "未找到已被 config.kdl include 的 keybinds.kdl",
        );
    };
    let text = match std::fs::read_to_string(&target) {
        Ok(text) => text,
        Err(error) => {
            return InstallResult::new(
                Status::Error,
                Some(target),
                format!("无法读取配置：{error}"),
            );
        }
    };
    let Some((_, closing)) = find_binds_span(&text) else {
        return InstallResult::new(
            Status::Unavailable,
            Some(target),
            "配置文件中未找到 binds { ... } 块",
        );
    };

    let existing: HashSet<String> = discover_active(Some(&root))
        .into_iter()
        .map(|binding| binding.action)
        .collect();
    let occupied = binding_keys(&text);
    let mut additions = Vec::new();
    let mut conflicts = Vec::new();
    for entry in DEFAULT_SHORTCUTS {
        let (key, action, _) = entry;
        if existing.contains(*action) {
            continue;
        }
        if occupied.contains(*key) {
            conflicts.push(format!("{key}（需要 {action}）"));
            continue;
        }
        additions.push(*entry);
    }

    if !conflicts.is_empty() {
        let mut result =
            InstallResult::new(Status::Conflict, Some(target), "快捷键存在冲突，未修改配置");
        result.conflicts = conflicts;
        return result;
    }
    if additions.is_empty() {
        return InstallResult::new(Status::Ok, Some(target), "快捷键已存在");
    }

    // Insert just above the closing brace, keeping whatever the user wrote.
    let line_start = text[..closing].rfind('\n').map_or(0, |index| index + 1);
    let closing_prefix = &text[line_start..closing];
    let block = render_bindings(&additions, "    ");
    let new_text = if closing_prefix.trim().is_empty() {
        format!("{}{block}\n{}", &text[..line_start], &text[line_start..])
    } else {
        format!("{}\n{block}\n{}", &text[..closing], &text[closing..])
    };

    let backup = match write_backup(&target, &text).and_then(|backup| {
        std::fs::write(&target, &new_text)?;
        Ok(backup)
    }) {
        Ok(backup) => backup,
        Err(error) => {
            return InstallResult::new(
                Status::Error,
                Some(target),
                format!("无法写入配置：{error}"),
            );
        }
    };

    let mut detail = String::new();
    if directory.is_none() {
        let (valid, message) = validate_and_reload(&root);
        detail = message;
        if !valid {
            match std::fs::write(&target, &text) {
                Ok(()) => detail.push_str("；已自动恢复原配置"),
                Err(error) => detail.push_str(&format!(
                    "；恢复备份失败：{error}（备份：{}）",
                    backup.display()
                )),
            }
            return InstallResult::new(Status::Error, Some(target), detail);
        }
    }
    let mut result = InstallResult::new(Status::Installed, Some(target), detail);
    result.added = additions
        .iter()
        .map(|(key, _, _)| (*key).to_string())
        .collect();
    result
}

/// Remove only the managed block; hand-written bindings are left untouched.
pub fn remove(directory: Option<&Path>) -> InstallResult {
    let root = directory.map(Path::to_path_buf).unwrap_or_else(config_dir);
    let Some(target) = included_keybinds_path(&root) else {
        return InstallResult::new(Status::Unavailable, None, "未找到用户 keybinds.kdl");
    };
    let text = match std::fs::read_to_string(&target) {
        Ok(text) => text,
        Err(error) => {
            return InstallResult::new(
                Status::Error,
                Some(target),
                format!("无法读取配置：{error}"),
            );
        }
    };
    let pattern = Regex::new(&format!(
        r"(?ms)^[ \t]*{}\n.*?^[ \t]*{}\n?",
        regex::escape(MANAGED_BEGIN),
        regex::escape(MANAGED_END)
    ))
    .expect("static pattern");
    let new_text = pattern.replace(&text, "");
    if new_text == text {
        return InstallResult::new(Status::Ok, Some(target), "没有 vellum 托管区域");
    }
    let backup = match write_backup(&target, &text).and_then(|backup| {
        std::fs::write(&target, new_text.as_ref())?;
        Ok(backup)
    }) {
        Ok(backup) => backup,
        Err(error) => {
            return InstallResult::new(
                Status::Error,
                Some(target),
                format!("无法写入配置：{error}"),
            );
        }
    };
    let mut detail = String::new();
    if directory.is_none() {
        let (valid, message) = validate_and_reload(&root);
        detail = message;
        if !valid {
            match std::fs::write(&target, &text) {
                Ok(()) => detail.push_str("；已自动恢复原配置"),
                Err(error) => detail.push_str(&format!(
                    "；恢复备份失败：{error}（备份：{}）",
                    backup.display()
                )),
            }
            return InstallResult::new(Status::Error, Some(target), detail);
        }
    }
    InstallResult::new(Status::Removed, Some(target), detail)
}

fn scan(path: &Path, text: &str, into: &mut Vec<Binding>) {
    for (number, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        let Some(binding) = BIND_LINE_RE.captures(line) else {
            continue;
        };
        let body = &binding[2];
        let action = SPAWN_RE
            .captures(body)
            .or_else(|| SPAWN_SH_RE.captures(body));
        if let Some(action) = action {
            into.push(Binding {
                key: binding[1].to_string(),
                action: action[1].to_string(),
                path: path.to_path_buf(),
                line: number + 1,
            });
        }
    }
}

/// Every vellum binding anywhere under the niri config tree.
///
/// Used by `doctor`/`shortcuts list` so a binding in an unincluded file still
/// shows up as an explanation for "my hotkey does nothing".
pub fn discover(directory: Option<&Path>) -> Vec<Binding> {
    let root = directory.map(Path::to_path_buf).unwrap_or_else(config_dir);
    if !root.exists() {
        return Vec::new();
    }
    let mut files = Vec::new();
    collect_kdl(&root, &mut files);
    files.sort();
    let mut bindings = Vec::new();
    for path in files {
        if let Ok(text) = std::fs::read_to_string(&path) {
            scan(&path, &text, &mut bindings);
        }
    }
    bindings
}

fn collect_kdl(dir: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_kdl(&path, into);
        } else if path.extension().is_some_and(|ext| ext == "kdl") {
            into.push(path);
        }
    }
}

/// Bindings that the running compositor actually has.
pub fn discover_active(directory: Option<&Path>) -> Vec<Binding> {
    let mut bindings = Vec::new();
    for path in active_config_files(directory) {
        if let Ok(text) = std::fs::read_to_string(&path) {
            scan(&path, &text, &mut bindings);
        }
    }
    bindings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shortcuts::tempdir;

    fn workspace(binds: &str) -> tempdir::TempDir {
        let dir = tempdir::TempDir::new();
        std::fs::create_dir_all(dir.path().join("dms")).unwrap();
        std::fs::write(
            dir.path().join("config.kdl"),
            "include \"dms/keybinds.kdl\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("dms/keybinds.kdl"), binds).unwrap();
        dir
    }

    const PLAIN: &str = "binds {\n    Mod+T { spawn \"foot\"; }\n}\n";

    #[test]
    fn only_included_files_are_editable() {
        let dir = tempdir::TempDir::new();
        std::fs::write(dir.path().join("config.kdl"), "// nothing included\n").unwrap();
        std::fs::write(dir.path().join("keybinds.kdl"), PLAIN).unwrap();
        let result = install(Some(dir.path()));
        assert_eq!(result.status, Status::Unavailable);
    }

    #[test]
    fn install_adds_a_managed_block() {
        let dir = workspace(PLAIN);
        let result = install(Some(dir.path()));
        assert_eq!(result.status, Status::Installed);
        // Derived, not a magic number. A hardcoded count here has to be bumped
        // every time a default is added, and a stale one is exactly how the `full`
        // addition first showed up — as a failing count rather than as a statement
        // about what install is supposed to do.
        assert_eq!(result.added.len(), DEFAULT_SHORTCUTS.len());
        let text = std::fs::read_to_string(dir.path().join("dms/keybinds.kdl")).unwrap();
        assert!(text.contains(MANAGED_BEGIN));
        assert!(text.contains("vellumctl region"));
        // The user's own binding survives, and the block stays inside binds.
        assert!(text.contains("Mod+T { spawn \"foot\"; }"));
        assert!(text.find(MANAGED_END).unwrap() < text.rfind('}').unwrap());
    }

    #[test]
    fn install_is_idempotent() {
        let dir = workspace(PLAIN);
        assert_eq!(install(Some(dir.path())).status, Status::Installed);
        let once = std::fs::read_to_string(dir.path().join("dms/keybinds.kdl")).unwrap();
        assert_eq!(install(Some(dir.path())).status, Status::Ok);
        let twice = std::fs::read_to_string(dir.path().join("dms/keybinds.kdl")).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn a_single_conflict_blocks_the_whole_group() {
        let dir = workspace("binds {\n    Mod+Shift+Print { spawn \"other\"; }\n}\n");
        let result = install(Some(dir.path()));
        assert_eq!(result.status, Status::Conflict);
        assert_eq!(result.conflicts.len(), 1);
        let text = std::fs::read_to_string(dir.path().join("dms/keybinds.kdl")).unwrap();
        assert!(!text.contains(MANAGED_BEGIN));
        assert!(!text.contains("vellumctl"));
    }

    #[test]
    fn a_backup_is_written_before_editing() {
        let dir = workspace(PLAIN);
        install(Some(dir.path()));
        let backup =
            std::fs::read_to_string(dir.path().join("dms/keybinds.kdl.vellum-backup")).unwrap();
        assert_eq!(backup, PLAIN);
    }

    #[test]
    fn remove_only_deletes_the_managed_block() {
        let dir = workspace(PLAIN);
        install(Some(dir.path()));
        let result = remove(Some(dir.path()));
        assert_eq!(result.status, Status::Removed);
        let text = std::fs::read_to_string(dir.path().join("dms/keybinds.kdl")).unwrap();
        assert!(!text.contains(MANAGED_BEGIN));
        assert!(text.contains("Mod+T { spawn \"foot\"; }"));
        assert_eq!(remove(Some(dir.path())).status, Status::Ok);
    }

    #[test]
    fn braces_inside_strings_and_comments_do_not_confuse_the_parser() {
        let dir = workspace(
            "// binds { commented out }\nbinds {\n    Mod+T { spawn-sh \"echo }{\"; }\n}\n",
        );
        let result = install(Some(dir.path()));
        assert_eq!(result.status, Status::Installed);
        let text = std::fs::read_to_string(dir.path().join("dms/keybinds.kdl")).unwrap();
        // Inserted into the real block, not the commented one.
        let managed = text.find(MANAGED_BEGIN).unwrap();
        assert!(managed > text.find("binds {\n").unwrap());
    }

    #[test]
    fn discovery_finds_both_spawn_forms() {
        let dir = workspace(concat!(
            "binds {\n",
            "    Mod+Print { spawn \"/home/u/.local/bin/vellumctl\" \"region\"; }\n",
            "    Mod+Shift+Print { spawn-sh \"$HOME/.local/bin/vellum long\"; }\n",
            "    // Mod+Ctrl+Print { spawn-sh \"vellumctl pin-last\"; }\n",
            "}\n",
        ));
        let found = discover_active(Some(dir.path()));
        let actions: Vec<&str> = found.iter().map(|b| b.action.as_str()).collect();
        assert_eq!(actions, ["region", "long"]);
        assert_eq!(found[0].key, "Mod+Print");
    }

    #[test]
    fn existing_bindings_are_not_duplicated_under_a_different_key() {
        let dir = workspace(
            "binds {\n    Mod+Alt+P { spawn-sh \"$HOME/.local/bin/vellumctl region\"; }\n}\n",
        );
        let result = install(Some(dir.path()));
        assert_eq!(result.status, Status::Installed);
        // Every default except the chord the file already binds.
        let expected: Vec<&str> = DEFAULT_SHORTCUTS
            .iter()
            .map(|(chord, _, _)| *chord)
            .filter(|chord| *chord != "Mod+Print")
            .collect();
        assert_eq!(result.added, expected);
    }

    #[test]
    fn nested_includes_are_followed() {
        let dir = tempdir::TempDir::new();
        std::fs::create_dir_all(dir.path().join("dms")).unwrap();
        std::fs::write(dir.path().join("config.kdl"), "include \"outer.kdl\"\n").unwrap();
        std::fs::write(
            dir.path().join("outer.kdl"),
            "include \"dms/keybinds.kdl\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("dms/keybinds.kdl"), PLAIN).unwrap();
        assert_eq!(install(Some(dir.path())).status, Status::Installed);
    }
}
