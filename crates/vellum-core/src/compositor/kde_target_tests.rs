use super::*;

fn arguments(script: &str) -> serde_json::Value {
    let (_, arguments) = script.rsplit_once(".apply(null, ").unwrap();
    serde_json::from_str(arguments.strip_suffix(");").unwrap()).unwrap()
}

#[test]
fn float_and_resize_keep_the_same_exact_pid_and_title() {
    let title = "vellum 钉图 [vellum-pin:one]";
    let float = script_for(4242, Some(title), None, 0, 0, true);
    let resize = script_for(4242, Some(title), None, 320, 180, false);
    assert_eq!(
        arguments(&float),
        serde_json::json!([4242, title, null, 0, 0, true])
    );
    assert_eq!(
        arguments(&resize),
        serde_json::json!([4242, title, null, 320, 180, false])
    );
    for script in [&float, &resize] {
        assert!(script.contains("!w.deleted && w.pid === pid"));
        assert!(script.contains("w.caption === title"));
        assert!(script.contains("if (candidates.length === 1)"));
        assert!(!script.contains("stackingOrder"));
        assert!(!script.contains("activeWindow"));
    }
}

#[test]
fn extended_captions_veto_rather_than_relax_the_exact_target() {
    let script = script_for(4242, Some("pin"), None, 320, 180, true);
    assert!(script.contains("return !w.deleted && w.pid === pid;"));
    assert!(script.contains("return title === null || w.caption === title;"));
    assert!(script.contains("if (title !== null && windows.some(function (w) {"));
    assert!(
        script.contains(
            "w.caption !== title && w.caption.indexOf(title) === 0;\n  })) {\n    return;"
        )
    );
    assert!(script.find("windows.some").unwrap() < script.find("w.keepAbove =").unwrap());
}

#[test]
fn titles_are_data_even_with_quotes_code_and_unicode_line_separators() {
    let title = "钉图 \"\\\n'); workspace.activeWindow.closeWindow(); //\u{2028}\u{2029}";
    let script = script_for(42, Some(title), None, 120, 80, false);
    assert_eq!(arguments(&script)[1], title);
    assert!(!script.contains('\u{2028}'));
    assert!(!script.contains('\u{2029}'));
    assert!(script.contains("\\u2028"));
    assert!(script.contains("\\u2029"));
}

#[test]
fn foreign_pid_is_refused_before_any_script_or_dbus_call() {
    let foreign_pid = if std::process::id() == 1 { 2 } else { 1 };
    assert!(!place_window(foreign_pid, 0, 0, 200, 100, true));
    assert!(!apply_script(
        foreign_pid,
        Some("pin"),
        None,
        200,
        100,
        true
    ));
}

#[test]
fn empty_explicit_targets_are_refused_without_downgrading_to_pid() {
    for title in ["", " \n\t"] {
        assert!(!apply_script(
            std::process::id(),
            Some(title),
            None,
            200,
            100,
            true
        ));
    }
}
