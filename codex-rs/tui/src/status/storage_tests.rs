use super::*;
use pretty_assertions::assert_eq;
use ratatui::style::Modifier;

#[test]
fn local_home_storage_snapshot() {
    let storage = StatusStorageDisplay {
        local_codex_home: "/opt/codex/home".to_string(),
    };
    let rendered = storage
        .lines(/*width*/ 80)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @"Codex home (local TUI): /opt/codex/home");
}

#[test]
fn long_home_is_preserved_at_narrow_widths() {
    for home in [
        "/srv/local/configuration/codex/profiles/engineering/long-profile-name",
        r"C:\Users\developer\AppData\Local\codex\profiles\engineering",
    ] {
        let storage = StatusStorageDisplay {
            local_codex_home: home.to_string(),
        };
        for width in [1, 2, 8, 16, 40, 80] {
            let lines = storage.lines(width);
            assert!(lines.iter().all(|line| line.width() <= width));
            let value: String = lines
                .iter()
                .flat_map(|line| &line.spans)
                .filter(|span| !span.style.add_modifier.contains(Modifier::DIM))
                .map(|span| span.content.as_ref())
                .collect();
            assert_eq!(value, home, "width {width}");
        }
    }
}
