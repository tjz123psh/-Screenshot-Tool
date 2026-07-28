//! Hyprland backend for the shortcut commands.
//!
//! Hyprland has two config dialects in the wild and vellum has to treat them
//! differently, because the risk of a bad edit is not the same:
//!
//! * Classic `hyprland.conf` (hyprlang). Line-oriented, so a managed block
//!   between marker comments is as safe here as in niri's KDL, and a live
//!   compositor can be asked to check the result: `hyprctl reload` followed by
//!   `hyprctl configerrors`. vellum writes these.
//! * Lua config (`hyprland.lua`, Hyprland 0.5x). Bindings are *code*, produced
//!   by arbitrary expressions with local variables such as `mainMod`. Splicing
//!   generated statements into a program is a different class of operation from
//!   inserting config lines, and this build additionally refuses
//!   `hyprctl keyword` with "keyword can't work with non-legacy parsers", so
//!   there is no cheap validation to catch a mistake. vellum never writes here;
//!   `install` returns a snippet for the user to paste.
//!
//! Discovery reads config text in both cases. Asking the compositor is not an
//! option: `hyprctl binds` reports every Lua binding as `dispatcher: __lua`
//! with an opaque numeric `arg`, so it cannot say which chord runs vellum.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use super::{Binding, DEFAULT_SHORTCUTS, InstallResult, Status, launcher_path};
use vellum_core::proc::{self, which};

/// A reload either applies or fails immediately; this only guards a hang.
const RELOAD_TIMEOUT: Duration = Duration::from_secs(5);

/// Marker comments delimiting the region this tool owns. `#` because that is
/// hyprlang's comment character.
pub const MANAGED_BEGIN: &str = "# >>> vellum managed shortcuts";
pub const MANAGED_END: &str = "# <<< vellum managed shortcuts";

/// Matches vellum in a `bind = ...` line or a Lua `exec_cmd("... vellumctl x")`.
///
/// Accepts both `vellum` and `vellumctl`, with or without a leading path, so a
/// hand-written binding counts as "already present" and is not duplicated.
static SPAWN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[\s/\x22'])vellum(?:ctl)?\s+(region|long|pin-last)\b").unwrap()
});

/// `bind = MODS, KEY, dispatcher, args`. Captures the mods+key half so a chord
/// can be compared against the defaults.
static BIND_LINE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*bind[lrenmtsdpio]*\s*=\s*([^,]*),\s*([^,]+),(.*)$").unwrap()
});

/// `hl.bind("...", ...)` or `hl.bind(mainMod .. " + Print", ...)`.
static LUA_BIND_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*hl\.bind\s*\(\s*(.+?)\s*,\s*hl\.").unwrap());

/// `local mainMod = "SUPER"` - a string constant used to build chords.
///
/// A trailing comment is allowed and must be: real configs annotate these, and
/// anchoring at the closing quote silently matched nothing on the machine this
/// was written for.
static LUA_CONST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*local\s+([A-Za-z_]\w*)\s*=\s*"([^"]*)"\s*(?:--.*)?$"#).unwrap()
});

/// `$XDG_CONFIG_HOME/hypr` or `~/.config/hypr`.
pub fn config_dir() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(base) if !base.is_empty() => PathBuf::from(base).join("hypr"),
        _ => vellum_core::paths::home().join(".config").join("hypr"),
    }
}

/// Which dialect a config directory uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    /// `hyprland.conf` exists: writable.
    Conf,
    /// Only `hyprland.lua` exists: read-only, snippet instead.
    Lua,
    /// Neither entry point is present.
    Missing,
}

fn dialect(root: &Path) -> Dialect {
    if root.join("hyprland.conf").exists() {
        Dialect::Conf
    } else if root.join("hyprland.lua").exists() {
        Dialect::Lua
    } else {
        Dialect::Missing
    }
}

fn root_of(directory: Option<&Path>) -> PathBuf {
    directory.map(Path::to_path_buf).unwrap_or_else(config_dir)
}

/// Translates a niri chord such as `Mod+Shift+Print` into Hyprland's
/// `SUPER SHIFT, Print` (mods and key, comma separated).
fn chord(niri_chord: &str) -> (String, String) {
    let mut mods = Vec::new();
    let mut key = "";
    for part in niri_chord.split('+') {
        match part {
            "Mod" | "Super" => mods.push("SUPER"),
            "Ctrl" | "Control" => mods.push("CTRL"),
            "Shift" => mods.push("SHIFT"),
            "Alt" => mods.push("ALT"),
            other => key = other,
        }
    }
    (mods.join(" "), key.to_string())
}

/// The chord as Hyprland's own config spells it, for conflict comparison and
/// for display: `SUPER SHIFT, Print`.
fn chord_label(niri_chord: &str) -> String {
    let (mods, key) = chord(niri_chord);
    if mods.is_empty() {
        key
    } else {
        format!("{mods}, {key}")
    }
}

/// Normalises a chord for comparison: uppercase, sorted mods, no spaces.
///
/// Users write `SUPER SHIFT`, `SHIFT SUPER` and `SUPERSHIFT` interchangeably,
/// and a missed match here would mean silently installing a duplicate binding
/// on a key the user already uses.
fn normalise(mods: &str, key: &str) -> String {
    let mut parts: Vec<String> = mods
        .split(|c: char| c.is_whitespace() || c == '+')
        .filter(|s| !s.is_empty())
        .map(|s| s.trim().to_uppercase())
        .map(|s| match s.as_str() {
            "MOD" | "WIN" | "LOGO" | "MOD4" => "SUPER".to_string(),
            "CONTROL" => "CTRL".to_string(),
            other => other.to_string(),
        })
        .collect();
    parts.sort();
    parts.push(key.trim().to_uppercase());
    parts.join("+")
}

/// Keys already bound anywhere in the config text.
fn taken_chords(text: &str) -> HashSet<String> {
    let mut taken = HashSet::new();
    for capture in BIND_LINE_RE.captures_iter(text) {
        taken.insert(normalise(&capture[1], &capture[2]));
    }
    taken
}

/// Renders the managed block for `hyprland.conf`.
///
/// `exec` runs the command through a shell, so `$HOME` is expanded there; the
/// absolute path is required because compositors do not spawn with the user's
/// login PATH.
fn render_conf(items: &[(&str, &str, &str)]) -> String {
    let launcher = launcher_path();
    let mut lines = vec![MANAGED_BEGIN.to_string()];
    for (niri_chord, action, title) in items {
        let (mods, key) = chord(niri_chord);
        lines.push(format!(
            "bind = {mods}, {key}, exec, {launcher} {action} # {title}"
        ));
    }
    lines.push(MANAGED_END.to_string());
    lines.join("\n")
}

/// Renders a Lua snippet in the same shape the user's own config uses.
fn render_lua(items: &[(&str, &str, &str)]) -> String {
    let launcher = launcher_path();
    let mut lines = vec![
        "-- vellum 快捷键（粘贴到 ~/.config/hypr/conf/keybinds.lua）".to_string(),
        "-- 用绝对路径：~/.local/bin 不在合成器 spawn 的 PATH 中。".to_string(),
    ];
    for (niri_chord, action, title) in items {
        let (mods, key) = chord(niri_chord);
        let chord_lua = if mods.is_empty() {
            format!("\"{key}\"")
        } else {
            format!("\"{} + {key}\"", mods.replace(' ', " + "))
        };
        lines.push(format!(
            "hl.bind({chord_lua}, hl.dsp.exec_cmd(\"{launcher} {action}\")) -- {title}"
        ));
    }
    lines.join("\n")
}

fn write_backup(path: &Path, text: &str) -> std::io::Result<PathBuf> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let backup = path.with_file_name(format!("{name}.vellum-backup"));
    std::fs::write(&backup, text)?;
    Ok(backup)
}

/// Asks a live Hyprland to reload and report parse errors.
///
/// Returns `(accepted, detail)`. When Hyprland is not running the edit is still
/// valid on disk, so this is reported as success with a different message —
/// "written but not yet active" and "written and active" are very different
/// states for someone about to press a key.
fn reload_and_check() -> (bool, String) {
    let Some(hyprctl) = which("hyprctl") else {
        return (
            true,
            "配置已写入；未检测到 hyprctl，登录 Hyprland 会话后生效".to_string(),
        );
    };
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return (
            true,
            "配置已写入；Hyprland 未在运行，下次登录后生效".to_string(),
        );
    }
    if proc::run(&hyprctl, &["reload"], RELOAD_TIMEOUT).is_none() {
        return (false, "无法让 Hyprland 重新加载配置".to_string());
    }
    match proc::run(&hyprctl, &["configerrors"], RELOAD_TIMEOUT) {
        // `configerrors` prints nothing (or "no errors") on a clean config.
        Some(output) => {
            let text = output.combined().trim().to_string();
            if text.is_empty() || text.contains("no errors") {
                (true, "配置已写入并重新加载".to_string())
            } else {
                (false, format!("Hyprland 配置报错：{text}"))
            }
        }
        None => (true, "配置已写入并重新加载".to_string()),
    }
}

pub fn install(directory: Option<&Path>) -> InstallResult {
    let root = root_of(directory);
    match dialect(&root) {
        Dialect::Missing => InstallResult::new(
            Status::Unavailable,
            None,
            "未找到 hyprland.conf 或 hyprland.lua",
        ),
        // Lua config is code, not settings: hand the user a snippet instead of
        // generating statements into their program.
        Dialect::Lua => {
            let mut result = InstallResult::new(
                Status::Unavailable,
                Some(root.join("hyprland.lua")),
                "Hyprland 使用 Lua 配置，vellum 不会自动改写；请粘贴下面的片段",
            );
            result.snippet = Some(render_lua(DEFAULT_SHORTCUTS));
            result
        }
        Dialect::Conf => install_conf(&root, directory.is_some()),
    }
}

fn install_conf(root: &Path, testing: bool) -> InstallResult {
    let target = root.join("hyprland.conf");
    let Ok(original) = std::fs::read_to_string(&target) else {
        return InstallResult::new(Status::Error, Some(target), "无法读取 hyprland.conf");
    };

    let present: HashSet<String> = discover_in(&target, &original)
        .into_iter()
        .map(|b| b.action)
        .collect();
    let pending: Vec<(&str, &str, &str)> = DEFAULT_SHORTCUTS
        .iter()
        .copied()
        .filter(|(_, action, _)| !present.contains(*action))
        .collect();
    if pending.is_empty() {
        return InstallResult::new(Status::Ok, Some(target), "快捷键已存在");
    }

    // A partial hotkey set is worse than none: if any chord is taken, write
    // nothing and let the user decide.
    let taken = taken_chords(&original);
    let conflicts: Vec<String> = pending
        .iter()
        .filter(|(niri_chord, _, _)| {
            let (mods, key) = chord(niri_chord);
            taken.contains(&normalise(&mods, &key))
        })
        .map(|(niri_chord, _, _)| chord_label(niri_chord))
        .collect();
    if !conflicts.is_empty() {
        let mut result =
            InstallResult::new(Status::Conflict, Some(target), "快捷键存在冲突，未修改配置");
        result.conflicts = conflicts;
        return result;
    }

    let mut updated = original.clone();
    if !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&render_conf(&pending));
    updated.push('\n');

    let backup = match write_backup(&target, &original) {
        Ok(path) => path,
        Err(err) => {
            return InstallResult::new(Status::Error, Some(target), format!("无法备份配置：{err}"));
        }
    };
    if let Err(err) = std::fs::write(&target, &updated) {
        return InstallResult::new(Status::Error, Some(target), format!("无法写入配置：{err}"));
    }

    let mut result = InstallResult::new(Status::Installed, Some(target.clone()), String::new());
    result.added = pending
        .iter()
        .map(|(chord, _, _)| chord_label(chord))
        .collect();

    // Only touch the live compositor for a real install, never in tests.
    if testing {
        result.detail = "快捷键已写入".to_string();
        return result;
    }
    let (accepted, detail) = reload_and_check();
    if accepted {
        result.detail = detail;
        return result;
    }
    // Never leave an unvalidated config in place.
    let mut failure = InstallResult::new(Status::Error, Some(target.clone()), detail);
    match std::fs::write(&target, &original) {
        Ok(()) => {
            let _ = reload_and_check();
        }
        Err(err) => {
            failure.detail = format!(
                "{}；恢复原配置失败：{err}，备份在 {}",
                failure.detail,
                backup.display()
            );
        }
    }
    failure
}

pub fn remove(directory: Option<&Path>) -> InstallResult {
    let root = root_of(directory);
    match dialect(&root) {
        Dialect::Missing => InstallResult::new(
            Status::Unavailable,
            None,
            "未找到 hyprland.conf 或 hyprland.lua",
        ),
        Dialect::Lua => InstallResult::new(
            Status::Ok,
            Some(root.join("hyprland.lua")),
            "Hyprland 使用 Lua 配置，vellum 从未写入；请手动删除相关 hl.bind 行",
        ),
        Dialect::Conf => remove_conf(&root, directory.is_some()),
    }
}

fn remove_conf(root: &Path, testing: bool) -> InstallResult {
    let target = root.join("hyprland.conf");
    let Ok(original) = std::fs::read_to_string(&target) else {
        return InstallResult::new(Status::Error, Some(target), "无法读取 hyprland.conf");
    };
    let Some(start) = original.find(MANAGED_BEGIN) else {
        return InstallResult::new(Status::Ok, Some(target), "没有 vellum 托管区域");
    };
    let Some(end) = original[start..].find(MANAGED_END) else {
        return InstallResult::new(Status::Error, Some(target), "托管区域缺少结束标记");
    };
    let mut cut_to = start + end + MANAGED_END.len();
    if original[cut_to..].starts_with('\n') {
        cut_to += 1;
    }
    let updated = format!("{}{}", &original[..start], &original[cut_to..]);

    if let Err(err) = write_backup(&target, &original) {
        return InstallResult::new(Status::Error, Some(target), format!("无法备份配置：{err}"));
    }
    if let Err(err) = std::fs::write(&target, &updated) {
        return InstallResult::new(Status::Error, Some(target), format!("无法写入配置：{err}"));
    }
    let detail = if testing {
        "已移除 vellum 自动管理的快捷键；手动配置未修改".to_string()
    } else {
        let (_, detail) = reload_and_check();
        format!("已移除 vellum 自动管理的快捷键；{detail}")
    };
    InstallResult::new(Status::Removed, Some(target), detail)
}

/// Renders a Lua chord expression as the key combination it produces.
///
/// Chords are built by concatenation, typically `mainMod .. " + Print"`. Showing
/// that source text to someone asking "which key takes a screenshot?" answers a
/// different question than the one they asked, so string literals and any
/// `local NAME = "..."` constants from the same file are substituted.
///
/// Anything that is not a literal or a known constant is left as written: a
/// chord assembled by a function call cannot be resolved without a Lua
/// interpreter, and inventing a plausible key would be worse than showing the
/// expression.
fn render_lua_chord(expression: &str, constants: &HashMap<String, String>) -> String {
    let mut parts = Vec::new();
    for piece in expression.split("..") {
        let piece = piece.trim();
        if let Some(literal) = piece
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
        {
            parts.push(literal.to_string());
        } else if let Some(value) = constants.get(piece) {
            parts.push(value.clone());
        } else {
            return expression.to_string();
        }
    }
    // Collapse the spacing the concatenation produced: "SUPER" + " + Print"
    // arrives as "SUPER + Print" with the separators already embedded.
    parts
        .join("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Collects `local NAME = "value"` string constants declared in one file.
fn lua_constants(text: &str) -> HashMap<String, String> {
    LUA_CONST_RE
        .captures_iter(text)
        .map(|capture| (capture[1].to_string(), capture[2].to_string()))
        .collect()
}

/// Finds vellum bindings in one config file's text.
///
/// Handles both dialects in one pass: a `bind =` line yields its real chord,
/// while a Lua `hl.bind` line has its chord expression resolved against the
/// string constants declared in the same file.
fn discover_in(path: &Path, text: &str) -> Vec<Binding> {
    let mut found = Vec::new();
    let constants = lua_constants(text);
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.starts_with("--") {
            continue;
        }
        let Some(action) = SPAWN_RE.captures(line).map(|c| c[1].to_string()) else {
            continue;
        };
        let key = if let Some(capture) = BIND_LINE_RE.captures(line) {
            format!("{}, {}", capture[1].trim(), capture[2].trim())
        } else if let Some(capture) = LUA_BIND_RE.captures(line) {
            render_lua_chord(capture[1].trim(), &constants)
        } else {
            continue;
        };
        found.push(Binding {
            key,
            action,
            path: path.to_path_buf(),
            line: index + 1,
        });
    }
    found
}

fn collect_config_files(dir: &Path, into: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_config_files(&path, into);
        } else if path
            .extension()
            .is_some_and(|ext| ext == "conf" || ext == "lua")
        {
            into.push(path);
        }
    }
}

pub fn discover(directory: Option<&Path>) -> Vec<Binding> {
    let root = root_of(directory);
    let mut files = Vec::new();
    collect_config_files(&root, &mut files);
    let mut bindings = Vec::new();
    for path in files {
        if let Ok(text) = std::fs::read_to_string(&path) {
            bindings.extend(discover_in(&path, &text));
        }
    }
    bindings
}

/// Hyprland pulls in config through `source =` (conf) or `require` (Lua), and
/// the Lua form resolves module paths at runtime. Rather than reimplement two
/// loaders, treat every config file under the directory as active: the worst
/// case is reporting a binding from a file the user has commented out of the
/// require list, which is far better than missing a real conflict.
pub fn discover_active(directory: Option<&Path>) -> Vec<Binding> {
    discover(directory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shortcuts::tempdir::TempDir;

    #[test]
    fn niri_chords_translate_to_hyprland_syntax() {
        assert_eq!(chord_label("Mod+Print"), "SUPER, Print");
        assert_eq!(chord_label("Mod+Shift+Print"), "SUPER SHIFT, Print");
        assert_eq!(chord_label("Mod+Ctrl+Print"), "SUPER CTRL, Print");
    }

    #[test]
    fn chord_comparison_ignores_order_and_spelling() {
        // A user writing SHIFT SUPER must not get a duplicate binding.
        assert_eq!(
            normalise("SUPER SHIFT", "Print"),
            normalise("SHIFT SUPER", "print")
        );
        assert_eq!(normalise("SUPER", "Print"), normalise("MOD", "Print"));
        assert_eq!(normalise("CTRL", "Print"), normalise("CONTROL", "Print"));
    }

    #[test]
    fn a_lua_config_is_never_written() {
        let dir = TempDir::new();
        std::fs::write(dir.path().join("hyprland.lua"), "-- user config\n").unwrap();
        let before = std::fs::read_to_string(dir.path().join("hyprland.lua")).unwrap();

        let result = install(Some(dir.path()));

        assert_eq!(result.status, Status::Unavailable);
        assert!(result.snippet.is_some(), "must offer a snippet to paste");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("hyprland.lua")).unwrap(),
            before,
            "the user's Lua must be untouched"
        );
    }

    #[test]
    fn the_lua_snippet_uses_an_absolute_launcher_path() {
        let snippet = render_lua(DEFAULT_SHORTCUTS);
        // A bare `vellumctl` does not resolve: compositors do not spawn with
        // the user's login PATH.
        assert!(snippet.contains("$HOME/.local/bin/vellumctl region"));
        assert!(snippet.contains("hl.bind(\"SUPER + SHIFT + Print\""));
    }

    #[test]
    fn a_conf_config_gets_a_managed_block() {
        let dir = TempDir::new();
        let path = dir.path().join("hyprland.conf");
        std::fs::write(&path, "bind = SUPER, Return, exec, kitty\n").unwrap();

        let result = install(Some(dir.path()));

        assert_eq!(result.status, Status::Installed, "{}", result.detail);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(MANAGED_BEGIN) && text.contains(MANAGED_END));
        assert!(text.contains("bind = SUPER, Print, exec, $HOME/.local/bin/vellumctl region"));
        // The user's own binding survives.
        assert!(text.contains("bind = SUPER, Return, exec, kitty"));
    }

    #[test]
    fn installing_twice_changes_nothing() {
        let dir = TempDir::new();
        let path = dir.path().join("hyprland.conf");
        std::fs::write(&path, "").unwrap();

        install(Some(dir.path()));
        let once = std::fs::read_to_string(&path).unwrap();
        let second = install(Some(dir.path()));

        assert_eq!(second.status, Status::Ok);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), once);
    }

    #[test]
    fn one_taken_chord_blocks_the_whole_group() {
        let dir = TempDir::new();
        let path = dir.path().join("hyprland.conf");
        // Same chord, different spelling order: must still be detected.
        std::fs::write(&path, "bind = SHIFT SUPER, Print, exec, grim\n").unwrap();

        let result = install(Some(dir.path()));

        assert_eq!(result.status, Status::Conflict);
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("vellumctl")
        );
    }

    #[test]
    fn a_backup_precedes_any_edit() {
        let dir = TempDir::new();
        let path = dir.path().join("hyprland.conf");
        std::fs::write(&path, "bind = SUPER, Return, exec, kitty\n").unwrap();

        install(Some(dir.path()));

        let backup = std::fs::read_to_string(dir.path().join("hyprland.conf.vellum-backup"))
            .expect("backup must exist");
        assert!(!backup.contains("vellumctl"), "backup holds the original");
    }

    #[test]
    fn remove_deletes_only_the_managed_block() {
        let dir = TempDir::new();
        let path = dir.path().join("hyprland.conf");
        std::fs::write(&path, "bind = SUPER, Return, exec, kitty\n").unwrap();
        install(Some(dir.path()));

        let result = remove(Some(dir.path()));

        assert_eq!(result.status, Status::Removed);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("vellumctl"));
        assert!(!text.contains(MANAGED_BEGIN));
        assert!(text.contains("bind = SUPER, Return, exec, kitty"));
    }

    #[test]
    fn discovery_reads_both_dialects() {
        let dir = TempDir::new();
        std::fs::write(
            dir.path().join("hyprland.conf"),
            "bind = SUPER, Print, exec, /home/u/.local/bin/vellumctl region\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("keybinds.lua"),
            "hl.bind(mainMod .. \" + SHIFT + Print\", hl.dsp.exec_cmd(\"$HOME/.local/bin/vellumctl long\"))\n",
        )
        .unwrap();

        let found = discover(Some(dir.path()));

        let actions: HashSet<&str> = found.iter().map(|b| b.action.as_str()).collect();
        assert!(actions.contains("region"), "conf binding not found");
        assert!(actions.contains("long"), "lua binding not found");
    }

    #[test]
    fn a_lua_chord_resolves_its_local_constants() {
        // Shape taken from a real config: the chord is built from a `mainMod`
        // local, so the raw expression answers a different question than "which
        // key is it".
        //
        // The trailing comment is load bearing. A first version of the constant
        // pattern anchored the closing quote to end-of-line, which matched a
        // stripped-down fixture but not the real declaration, so discovery kept
        // printing `mainMod .. " + Print"` while this test passed.
        let dir = TempDir::new();
        std::fs::write(
            dir.path().join("keybinds.lua"),
            "local mainMod = \"SUPER\" -- Sets \"Windows\" key as main modifier\n\
             hl.bind(mainMod .. \" + Print\", hl.dsp.exec_cmd(\"$HOME/.local/bin/vellumctl region\"))\n",
        )
        .unwrap();

        let found = discover(Some(dir.path()));

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "SUPER + Print");
    }

    #[test]
    fn an_unresolvable_chord_is_shown_as_written() {
        // No `mainMod` declaration anywhere, so there is nothing to substitute.
        // Inventing a key would be worse than showing the source.
        let dir = TempDir::new();
        std::fs::write(
            dir.path().join("keybinds.lua"),
            "hl.bind(pick_mod() .. \" + Print\", hl.dsp.exec_cmd(\"vellumctl long\"))\n",
        )
        .unwrap();

        let found = discover(Some(dir.path()));

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "pick_mod() .. \" + Print\"");
    }

    #[test]
    fn commented_out_bindings_are_ignored() {
        let dir = TempDir::new();
        std::fs::write(
            dir.path().join("hyprland.conf"),
            "# bind = SUPER, Print, exec, vellumctl region\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("keybinds.lua"),
            "-- hl.bind(\"SUPER + Print\", hl.dsp.exec_cmd(\"vellumctl long\"))\n",
        )
        .unwrap();

        assert!(discover(Some(dir.path())).is_empty());
    }
}
