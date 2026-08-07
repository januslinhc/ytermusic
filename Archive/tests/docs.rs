use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value as JsonValue;
use ytermusic::{
    app::{
        Action, AppState, Effect, PlayerCommand, SearchFilter, SearchItem, SearchPage, reduce,
        stable_queue_item_id,
    },
    config::Config,
    domain::{MediaId, MediaItem, MediaKind, PlaybackStatus},
    ui::{
        controller::{UiController, reduce_key},
        input::{InputMode, TextEntryContext, palette_entries},
    },
};

const RELEASE_ARTIFACTS: [&str; 11] = [
    "README.md",
    "LICENSE",
    "Makefile",
    "config.example.toml",
    "deny.toml",
    ".github/workflows/ci.yml",
    "packaging/homebrew/ytermusic.rb",
    "packaging/scoop/ytermusic.json",
    "packaging/winget/Ytermusic.Ytermusic.yaml",
    "packaging/winget/Ytermusic.Ytermusic.installer.yaml",
    "packaging/winget/Ytermusic.Ytermusic.locale.en-US.yaml",
];

#[derive(Debug, Eq, PartialEq)]
struct PackageMetadata {
    version: String,
    license: String,
}

struct LicenseExpressionParser<'a> {
    tokens: &'a [String],
    offset: usize,
    allowed: &'a BTreeSet<String>,
}

impl LicenseExpressionParser<'_> {
    fn parse(mut self) -> Result<bool, String> {
        let covered = self.parse_or()?;
        if self.offset == self.tokens.len() {
            Ok(covered)
        } else {
            Err(format!(
                "unexpected license token {:?}",
                self.tokens[self.offset]
            ))
        }
    }

    fn parse_or(&mut self) -> Result<bool, String> {
        let mut covered = self.parse_and()?;
        while self.consume("OR") {
            let right = self.parse_and()?;
            covered |= right;
        }
        Ok(covered)
    }

    fn parse_and(&mut self) -> Result<bool, String> {
        let mut covered = self.parse_primary()?;
        while self.consume("AND") {
            let right = self.parse_primary()?;
            covered &= right;
        }
        Ok(covered)
    }

    fn parse_primary(&mut self) -> Result<bool, String> {
        if self.consume("(") {
            let covered = self.parse_or()?;
            if !self.consume(")") {
                return Err("unclosed license-expression parenthesis".to_owned());
            }
            return Ok(covered);
        }

        let license = self
            .tokens
            .get(self.offset)
            .ok_or_else(|| "license expression ended unexpectedly".to_owned())?;
        if matches!(license.as_str(), ")" | "AND" | "OR" | "WITH") {
            return Err(format!("expected a license identifier, found {license:?}"));
        }
        self.offset += 1;

        if self.consume("WITH") {
            let exception = self
                .tokens
                .get(self.offset)
                .ok_or_else(|| "WITH must be followed by an exception identifier".to_owned())?;
            if matches!(exception.as_str(), "(" | ")" | "AND" | "OR" | "WITH") {
                return Err(format!(
                    "expected a license exception identifier, found {exception:?}"
                ));
            }
            self.offset += 1;
            Ok(self
                .allowed
                .contains(&format!("{license} WITH {exception}")))
        } else {
            Ok(self.allowed.contains(license))
        }
    }

    fn consume(&mut self, expected: &str) -> bool {
        if self
            .tokens
            .get(self.offset)
            .is_some_and(|token| token == expected)
        {
            self.offset += 1;
            true
        } else {
            false
        }
    }
}

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_artifact(relative_path: &str) -> String {
    let path = repository_root().join(relative_path);
    fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "required repository artifact {} is missing or unreadable: {error}",
            path.display()
        )
    })
}

fn markdown_section<'a>(document: &'a str, heading: &str) -> &'a str {
    let Some(heading_start) = document.find(heading) else {
        panic!("document must contain section heading {heading:?}");
    };
    let section_start = heading_start + heading.len();
    let remainder = &document[section_start..];
    let section_end = remainder.find("\n## ").unwrap_or(remainder.len());
    &remainder[..section_end]
}

fn markdown_between<'a>(document: &'a str, start: &str, end: &str) -> &'a str {
    let Some(start_index) = document.find(start) else {
        panic!("document must contain start heading {start:?}");
    };
    let remainder = &document[start_index + start.len()..];
    let Some(end_index) = remainder.find(end) else {
        panic!("document must contain end heading {end:?} after {start:?}");
    };
    &remainder[..end_index]
}

fn assert_fragments_in_order(contents: &str, fragments: &[&str]) {
    let mut offset = 0;
    for fragment in fragments {
        let remainder = &contents[offset..];
        let Some(relative_index) = remainder.find(fragment) else {
            panic!("section must contain {fragment:?} after byte offset {offset}");
        };
        offset += relative_index + fragment.len();
    }
}

fn plain_key(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn documented_song() -> MediaItem {
    MediaItem {
        id: MediaId {
            provider: "youtube-music".to_owned(),
            video_id: "docs-quick-start".to_owned(),
        },
        kind: MediaKind::Song,
        title: "Documented Song".to_owned(),
        creators: vec!["Documented Artist".to_owned()],
        collection: None,
        duration_ms: Some(180_000),
        artwork_url: None,
        explicit: false,
    }
}

fn apply_actions(mut state: AppState, actions: Vec<Action>) -> (AppState, Vec<Effect>) {
    let mut all_effects = Vec::new();
    for action in actions {
        let (next_state, effects) = reduce(state, action);
        state = next_state;
        all_effects.extend(effects);
    }
    (state, all_effects)
}

fn package_metadata() -> PackageMetadata {
    let manifest = read_artifact("Cargo.toml");
    let parsed: toml::Value = toml::from_str(&manifest)
        .unwrap_or_else(|error| panic!("Cargo.toml must remain valid TOML: {error}"));
    let package = parsed
        .get("package")
        .and_then(toml::Value::as_table)
        .unwrap_or_else(|| panic!("Cargo.toml must contain a package table"));
    let string_field = |field: &str| {
        package
            .get(field)
            .and_then(toml::Value::as_str)
            .unwrap_or_else(|| panic!("Cargo.toml package.{field} must be a string"))
            .to_owned()
    };

    PackageMetadata {
        version: string_field("version"),
        license: string_field("license"),
    }
}

fn cargo_host_target() -> String {
    let output = Command::new("rustc")
        .arg("-vV")
        .output()
        .unwrap_or_else(|error| panic!("rustc -vV must run for metadata validation: {error}"));
    assert!(
        output.status.success(),
        "rustc -vV must succeed for metadata validation: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let version = String::from_utf8(output.stdout)
        .unwrap_or_else(|error| panic!("rustc -vV output must be UTF-8: {error}"));
    version
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map_or_else(
            || panic!("rustc -vV output must contain a host target"),
            str::to_owned,
        )
}

fn offline_locked_metadata() -> JsonValue {
    let target = cargo_host_target();
    // Filtering to the host keeps this check offline on a fresh test build;
    // target-only packages are audited against Cargo.lock below.
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--offline",
            "--all-features",
            "--filter-platform",
            &target,
        ])
        .current_dir(repository_root())
        .output()
        .unwrap_or_else(|error| panic!("offline cargo metadata must run: {error}"));
    assert!(
        output.status.success(),
        "offline cargo metadata must succeed for {target}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("cargo metadata must emit valid JSON: {error}"))
}

fn allowed_licenses_for_package(
    policy: &toml::Value,
    package_name: &str,
    package_version: &str,
) -> BTreeSet<String> {
    let licenses = policy
        .get("licenses")
        .and_then(toml::Value::as_table)
        .unwrap_or_else(|| panic!("deny.toml must contain a licenses table"));
    let mut allowed = licenses
        .get("allow")
        .and_then(toml::Value::as_array)
        .unwrap_or_else(|| panic!("licenses.allow must be an array"))
        .iter()
        .map(|license| {
            license
                .as_str()
                .unwrap_or_else(|| panic!("licenses.allow entries must be strings"))
                .to_owned()
        })
        .collect::<BTreeSet<_>>();
    let exact_package = format!("{package_name}@{package_version}");
    for exception in licenses
        .get("exceptions")
        .and_then(toml::Value::as_array)
        .unwrap_or_else(|| panic!("licenses.exceptions must be an array"))
    {
        let applies = exception
            .get("crate")
            .and_then(toml::Value::as_str)
            .is_some_and(|package| package == package_name || package == exact_package);
        if applies {
            allowed.extend(
                exception
                    .get("allow")
                    .and_then(toml::Value::as_array)
                    .unwrap_or_else(|| panic!("license exception allow must be an array"))
                    .iter()
                    .map(|license| {
                        license
                            .as_str()
                            .unwrap_or_else(|| {
                                panic!("license exception allow entries must be strings")
                            })
                            .to_owned()
                    }),
            );
        }
    }
    allowed
}

fn license_expression_is_covered(
    expression: &str,
    allowed: &BTreeSet<String>,
) -> Result<bool, String> {
    let normalized = expression.replace('/', " OR ");
    let tokens = normalized
        .replace('(', " ( ")
        .replace(')', " ) ")
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return Err("license expression must not be empty".to_owned());
    }
    LicenseExpressionParser {
        tokens: &tokens,
        offset: 0,
        allowed,
    }
    .parse()
}

fn assert_package_license_is_covered(policy: &toml::Value, package: &JsonValue) {
    let package_name = package
        .get("name")
        .and_then(JsonValue::as_str)
        .unwrap_or_else(|| panic!("metadata package must have a string name"));
    let package_version = package
        .get("version")
        .and_then(JsonValue::as_str)
        .unwrap_or_else(|| panic!("metadata package must have a string version"));
    let expression = package
        .get("license")
        .and_then(JsonValue::as_str)
        .unwrap_or_else(|| {
            panic!("{package_name} {package_version} must declare a license expression")
        });
    let allowed = allowed_licenses_for_package(policy, package_name, package_version);
    let covered = license_expression_is_covered(expression, &allowed).unwrap_or_else(|error| {
        panic!("{package_name} {package_version} has invalid license expression {expression:?}: {error}")
    });
    assert!(
        covered,
        "{package_name} {package_version} requires uncovered license expression {expression:?}"
    );
}

fn yaml_scalar<'a>(document: &'a str, key: &str) -> Option<&'a str> {
    document.lines().map(str::trim).find_map(|line| {
        let (candidate, value) = line.split_once(':')?;
        (candidate == key).then(|| value.trim().trim_matches('"'))
    })
}

fn contains_sha256_digest(contents: &str) -> bool {
    contents
        .split(|character: char| !character.is_ascii_hexdigit())
        .any(|candidate| {
            candidate.len() == 64 && candidate.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

fn assert_release_placeholders(path: &str, contents: &str) {
    assert!(
        contents.contains("TEMPLATE ONLY"),
        "{path} must be visibly marked TEMPLATE ONLY"
    );
    assert!(
        contents.contains("__RELEASE_URL_"),
        "{path} must retain a release URL placeholder"
    );
    assert!(
        contents.contains("__SHA256_"),
        "{path} must retain a checksum placeholder"
    );
    assert!(
        !contains_sha256_digest(contents),
        "{path} must not look like a publishable release manifest"
    );
}

fn assert_no_literal_secrets(path: &str, contents: &str) {
    let lowercase = contents.to_ascii_lowercase();
    for forbidden in [
        "-----begin private key-----",
        "aiza",
        "ya29.",
        "bearer eyj",
        "password = \"",
        "password=\"",
        "api_key = \"",
        "api_key=\"",
        "client_secret = \"",
        "client_secret=\"",
        "cookie = \"",
        "cookie=\"",
    ] {
        assert!(
            !lowercase.contains(forbidden),
            "{path} contains secret-looking literal material matching {forbidden:?}"
        );
    }
}

#[test]
fn readme_documents_supported_install_and_usage() {
    let readme = read_artifact("README.md");
    let lowercase = readme.to_ascii_lowercase();

    for required_heading in [
        "## anonymous quick start",
        "## install",
        "### macos",
        "### linux",
        "### windows",
        "## keyboard reference",
        "## country charts",
        "## podcasts and resume",
        "## queue, radio, and fades",
        "## browser session privacy",
        "## files and paths",
        "## troubleshooting",
    ] {
        assert!(
            lowercase.contains(required_heading),
            "README must contain the section {required_heading:?}"
        );
    }

    for required_content in [
        "mpv",
        "yt-dlp",
        "ffmpeg",
        "ytermusic doctor",
        "country-specific",
        "trending",
        "podcast",
        "resume",
        "shuffle",
        "sequential",
        "repeat",
        "endless radio",
        "fade-in",
        "fade-out",
        "browser session",
        "temporary cookie",
        "config.toml",
        "ytermusic.db",
        "ytermusic.log",
        "unofficial",
        "not affiliated with youtube or google",
        "streams only and never downloads media",
    ] {
        assert!(
            lowercase.contains(required_content),
            "README must explain {required_content:?}"
        );
    }
}

#[test]
fn readme_documents_country_podcast_discovery_contract() {
    let readme = read_artifact("README.md");
    let discovery = markdown_between(&readme, "## Country charts", "## Queue, radio, and fades")
        .to_ascii_lowercase();
    let normalized = discovery.split_whitespace().collect::<Vec<_>>().join(" ");

    for required in [
        "podcasts opens with country top shows when no show is open",
        "`c` refreshes both charts and podcasts",
        "`zz` detects the os locale and falls back to `us`",
        "apple's public top shows metadata",
        "apple provides discovery metadata only, not playback links",
        "direct unauthenticated request to apple",
        "selected or detected country",
        "youtube music remains responsible for search, show details, and playback",
        "press `enter` to lazily match the selected show",
        "`/` remains available for manual search",
        "`esc` returns from an opened show to the rankings",
        "cached in process memory only for the current session",
        "fresh for about one hour",
        "if rankings cannot be loaded, podcasts remains usable and manual search remains available",
    ] {
        assert!(
            normalized.contains(required),
            "README country and podcast sections must document {required:?}"
        );
    }
}

#[test]
fn anonymous_quick_start_submits_then_activates_with_enter() {
    let readme = read_artifact("README.md");
    let quick_start = markdown_section(&readme, "## Anonymous quick start").to_ascii_lowercase();

    assert_fragments_in_order(
        &quick_start,
        &[
            "ytermusic\n",
            "press `/` to open search",
            "type a query",
            "press `enter` to submit the query",
            "select a result",
            "press `enter` again to replace the queue and play the selected result",
        ],
    );
    let normalized_quick_start = quick_start.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        normalized_quick_start.contains(
            "`space` pauses or resumes an already-active item; it cannot enqueue or start a fresh search result."
        ),
        "anonymous quick start must state Space's exact post-activation pause/resume boundary"
    );
    for incorrect_activation in ["press `space` to play", "press `space` to activate"] {
        assert!(
            !quick_start.contains(incorrect_activation),
            "anonymous quick start must reject incorrect activation step {incorrect_activation:?}"
        );
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one linear scenario proves every documented quick-start key at production seams"
)]
fn documented_anonymous_sequence_executes_production_controller_and_reducer() {
    let mut controller = UiController::default();
    let mut state = AppState::default();

    (controller, _) = reduce_key(controller, &state, plain_key('/'));
    assert_eq!(
        controller.input_mode(),
        InputMode::TextEntry(TextEntryContext::Search)
    );
    for character in "midnight".chars() {
        let (next_controller, actions) = reduce_key(controller, &state, plain_key(character));
        controller = next_controller;
        assert!(actions.is_empty());
    }
    assert_eq!(controller.input_text(), "midnight");

    let (next_controller, submit_actions) = reduce_key(controller, &state, key(KeyCode::Enter));
    controller = next_controller;
    assert_eq!(
        submit_actions,
        vec![Action::SearchSubmitted {
            query: "midnight".to_owned(),
            filter: SearchFilter::All,
        }]
    );
    let (next_state, search_effects) = apply_actions(state, submit_actions);
    state = next_state;
    let [
        Effect::Search {
            generation: search_generation,
            query,
            filter,
        },
    ] = search_effects.as_slice()
    else {
        panic!("the first Enter must execute the production search effect");
    };
    let search_generation = *search_generation;
    assert_eq!(query, "midnight");
    assert_eq!(*filter, SearchFilter::All);
    assert_eq!(state.search().query(), "midnight");
    assert!(state.search().loading());
    assert_eq!(state.search().active_generation(), Some(search_generation));

    let item = documented_song();
    let selected_id = SearchItem::Playable(item.clone()).stable_id();
    (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: search_generation,
            result: Ok(SearchPage::new(vec![SearchItem::Playable(item.clone())])),
        },
    );
    assert_eq!(state.search().selected_id(), Some(&selected_id));

    let (next_controller, premature_space_actions) = reduce_key(controller, &state, plain_key(' '));
    controller = next_controller;
    assert_eq!(premature_space_actions, vec![Action::TogglePlayback]);
    let (next_state, premature_space_effects) = apply_actions(state, premature_space_actions);
    state = next_state;
    assert!(premature_space_effects.is_empty());
    assert!(state.queue().items().is_empty());
    assert!(state.playback().current.is_none());

    let queue_id = stable_queue_item_id(&item.id);
    let (next_controller, activate_actions) = reduce_key(controller, &state, key(KeyCode::Enter));
    controller = next_controller;
    assert_eq!(
        activate_actions,
        vec![Action::PlayMediaList {
            items: vec![item.clone()],
            selected_id: item.id.clone(),
            shuffle_seed: None,
        }]
    );
    let (next_state, activation_effects) = apply_actions(state, activate_actions);
    state = next_state;
    assert_eq!(state.queue().items().len(), 1);
    assert_eq!(
        state.queue().current().map(ytermusic::queue::QueueItem::id),
        Some(&queue_id)
    );
    assert_eq!(state.playback().current.as_ref(), Some(&item.id));
    assert_eq!(state.playback().status, PlaybackStatus::Resolving);
    let Some(resolve_generation) = activation_effects.iter().find_map(|effect| match effect {
        Effect::Resolve {
            generation,
            item: resolving,
            ..
        } if resolving == &item => Some(*generation),
        _ => None,
    }) else {
        panic!("the second Enter must execute the production resolve effect");
    };

    let (next_controller, resolving_space_actions) = reduce_key(controller, &state, plain_key(' '));
    controller = next_controller;
    assert_eq!(resolving_space_actions, vec![Action::TogglePlayback]);
    let (next_state, resolving_space_effects) = apply_actions(state, resolving_space_actions);
    state = next_state;
    assert!(resolving_space_effects.is_empty());
    assert_eq!(state.playback().status, PlaybackStatus::Resolving);

    (state, _) = reduce(
        state,
        Action::ResolveSucceeded {
            generation: resolve_generation,
        },
    );
    (state, _) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation: resolve_generation,
            status: PlaybackStatus::Playing,
        },
    );
    let (next_controller, pause_actions) = reduce_key(controller, &state, plain_key(' '));
    controller = next_controller;
    assert_eq!(pause_actions, vec![Action::TogglePlayback]);
    let (next_state, pause_effects) = apply_actions(state, pause_actions);
    state = next_state;
    assert_eq!(pause_effects, vec![Effect::Player(PlayerCommand::Pause)]);

    (state, _) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation: resolve_generation,
            status: PlaybackStatus::Paused,
        },
    );
    let (_, resume_actions) = reduce_key(controller, &state, plain_key(' '));
    assert_eq!(resume_actions, vec![Action::TogglePlayback]);
    let (_, resume_effects) = apply_actions(state, resume_actions);
    assert_eq!(resume_effects, vec![Effect::Player(PlayerCommand::Resume)]);
}

#[test]
fn readme_keyboard_reference_matches_the_controller() {
    let readme = read_artifact("README.md");
    let keyboard = readme
        .split("## Keyboard reference")
        .nth(1)
        .and_then(|section| section.split("\n## ").next())
        .unwrap_or_else(|| panic!("README must contain its keyboard reference"));

    for entry in palette_entries() {
        if entry.shortcut == "Enter" {
            continue;
        }
        let expected_shortcut = format!("`{}`", entry.shortcut);
        assert!(
            keyboard.lines().any(|row| {
                let cells = row.split('|').map(str::trim).collect::<Vec<_>>();
                cells.get(1).is_some_and(|cell| *cell == expected_shortcut)
                    && cells.get(2).is_some_and(|cell| *cell == entry.label)
            }),
            "README keyboard reference is missing shortcut/action row {:?} / {:?}",
            entry.shortcut,
            entry.label,
        );
    }
    assert!(
        keyboard.contains("Ctrl-C"),
        "README must document the global Ctrl-C quit binding"
    );
    assert!(
        keyboard.lines().any(|row| {
            let cells = row.split('|').map(str::trim).collect::<Vec<_>>();
            cells.get(1) == Some(&"`Enter`")
                && cells.get(2) == Some(&"Activate selected row / submit text")
        }),
        "README Enter row must cover both row activation and text submission"
    );
}

#[test]
fn readme_documents_explicit_list_playback_and_favorite_targeting() {
    let readme = read_artifact("README.md");
    let favorites =
        markdown_section(&readme, "## Favorites and list playback").to_ascii_lowercase();
    let normalized = favorites.split_whitespace().collect::<Vec<_>>().join(" ");

    for required in [
        "top-level destination after library",
        "normal-mode `f`",
        "search",
        "charts",
        "opened podcast episode list",
        "library song lists",
        "history",
        "queue focus",
        "player focus",
        "metadata",
        "text entry",
        "atomically replaces the queue",
        "currently loaded playable rows",
        "full media ids",
        "repeat mode",
        "shuffle enabled",
        "selected item stays current",
        "remaining items are randomized",
        "disables endless radio",
    ] {
        assert!(
            normalized.contains(required),
            "README list-playback and Favorites guidance must explain {required:?}"
        );
    }
}

#[test]
fn readme_documents_exact_explicit_list_capacity_boundary() {
    let readme = read_artifact("README.md");
    let favorites =
        markdown_section(&readme, "## Favorites and list playback").to_ascii_lowercase();
    let normalized = favorites.split_whitespace().collect::<Vec<_>>().join(" ");

    for required in [
        "at most 1,024 unique playable rows are accepted",
        "1,025 or more are rejected",
        "old queue, playback, and modes are preserved",
    ] {
        assert!(
            normalized.contains(required),
            "README explicit-list capacity guidance must explain {required:?}"
        );
    }
}

#[test]
fn readme_documents_two_click_favorites_and_list_row_mouse_behavior() {
    let readme = read_artifact("README.md");
    let keyboard = markdown_section(&readme, "## Keyboard reference").to_ascii_lowercase();
    let normalized = keyboard.split_whitespace().collect::<Vec<_>>().join(" ");

    for required in [
        "for favorites and other visible lists",
        "the first click selects an unselected row",
        "clicking the already-selected row again activates it",
    ] {
        assert!(
            normalized.contains(required),
            "README Favorites/list mouse guidance must explain {required:?}"
        );
    }
}

#[test]
fn readme_documents_local_favorites_persistence_and_limits() {
    let readme = read_artifact("README.md");
    let favorites =
        markdown_section(&readme, "## Favorites and list playback").to_ascii_lowercase();
    let normalized = favorites.split_whitespace().collect::<Vec<_>>().join(" ");

    for required in [
        "local to this machine and database",
        "`ytermusic.db`",
        "at startup",
        "newest first",
        "capped at 1,024",
        "without eviction",
        "does not stop playback",
        "remove it from the queue",
        "persist across app restarts",
        "remain independent of session and queue resets",
    ] {
        assert!(
            normalized.contains(required),
            "README local Favorites guidance must explain {required:?}"
        );
    }
}

#[test]
fn readme_explains_tab_navigation_and_the_audio_reactive_spectrum() {
    let readme = read_artifact("README.md").to_ascii_lowercase();
    let readme = readme.split_whitespace().collect::<Vec<_>>().join(" ");

    for required in [
        "tab",
        "shift-tab",
        "navigation, content, and player",
        "`left`/`right`",
        "wrap",
        "wide layout",
        "three rows",
        "compact layout",
        "one row",
        "tiny layout",
        "ffmpeg",
        "separate low-bandwidth audio decode",
        "visualizer.enabled",
        "visualizer.max_fps",
        "`visualizer.max_fps` accepts `1-30`",
        "caps spectrum publication and redraw cadence",
        "mono 8 khz decode and fft cadence remain fixed",
        "process runtime and i/o remain bounded separately",
        "default is 15",
        "stream urls are never logged",
        "quiet baseline",
        "never interrupts audio",
    ] {
        assert!(
            readme.contains(required),
            "README navigation/visualizer guidance must explain {required:?}"
        );
    }
    assert!(
        !readme.contains("bounds redraw and analysis work"),
        "README must not imply visualizer.max_fps throttles decode or FFT work"
    );
}

#[test]
fn readme_explains_synchronized_lyrics_controls_sources_and_privacy() {
    let readme = read_artifact("README.md").to_ascii_lowercase();
    let readme = readme.split_whitespace().collect::<Vec<_>>().join(" ");

    for required in [
        "time-synchronized lyrics appear automatically",
        "`l` opens and closes the full lyrics overlay",
        "`j` / `k` or the arrow keys",
        "`enter` resumes follow mode",
        "`esc` closes the overlay",
        "plain lyrics",
        "youtube music",
        "lrclib",
        "track title",
        "artist",
        "strict full-title, artist, and album search first",
        "if that returns no match",
        "one bounded complete-title request without artist or album metadata",
        "before up to three bounded exact title segments",
        "up to three bounded exact title segments",
        "without artist or album metadata",
        "duration is used locally",
        "unique conservative match",
        "`lyrics.external_sync = false` disables all lrclib requests",
        "duration",
        "lyrics.external_sync = false",
        "lyrics.enabled = false",
        "memory only",
    ] {
        assert!(
            readme.contains(required),
            "README lyrics guidance must explain {required:?}"
        );
    }
}

#[test]
fn readme_explains_genuine_animated_artwork_requirements_and_caps() {
    let readme = read_artifact("README.md").to_ascii_lowercase();
    let readme = readme.split_whitespace().collect::<Vec<_>>().join(" ");

    for required in [
        "genuine low-resolution video frames",
        "wide layout only",
        "ffmpeg",
        "static artwork",
        "artwork.animated = false",
        "artwork.max_fps",
        "1-15",
        "default is 8",
        "bounded",
        "memory only",
    ] {
        assert!(
            readme.contains(required),
            "README animated-artwork guidance must explain {required:?}"
        );
    }
}

#[test]
fn example_config_deserializes_and_documents_real_defaults_and_bounds() {
    let example = read_artifact("config.example.toml");
    let parsed: Config = toml::from_str(&example)
        .unwrap_or_else(|error| panic!("config.example.toml must deserialize as Config: {error}"));
    parsed
        .validate()
        .unwrap_or_else(|error| panic!("config.example.toml must pass Config::validate: {error}"));

    assert_eq!(
        parsed,
        Config::default(),
        "example values must match production defaults"
    );
    assert_fragments_in_order(&example, &["[notifications]", "enabled = true"]);
    for documented in [
        "Default: ZZ",
        "Default: 80",
        "Default: 250",
        "Default: 1.0",
        "Default: 15",
        "Default: 30",
        "Default: 60",
        "Default: true",
        "Default: false",
        "Valid range: 0-100",
        "Valid range: 0-10000",
        "Valid range: 0.5-3.0",
        "Valid range: 1-600",
        "Valid range: 0-300",
    ] {
        assert!(
            example.contains(documented),
            "example config must document {documented:?}"
        );
    }
    assert_no_literal_secrets("config.example.toml", &example);
}

#[test]
fn license_matches_cargo_metadata() {
    let metadata = package_metadata();
    let license = read_artifact("LICENSE");

    assert_eq!(metadata.license, "MIT");
    assert!(license.starts_with("MIT License\n"));
    assert!(license.contains("Permission is hereby granted, free of charge"));
    assert!(license.contains("THE SOFTWARE IS PROVIDED \"AS IS\""));
    assert!(license.contains("Copyright (c) 2026 Ytermusic contributors"));
}

#[test]
fn ci_covers_cross_platform_quality_release_and_security_policy() {
    let workflow = read_artifact(".github/workflows/ci.yml");

    for required in [
        "fail-fast: false",
        "os: [ubuntu-latest, macos-latest, windows-latest]",
        "runs-on: ${{ matrix.os }}",
        "cargo fmt --check",
        "cargo clippy --all-targets --all-features -- -D warnings",
        "cargo test --all-targets",
        "cargo build --release",
        "runner.os == 'Linux'",
        "cargo test --doc --all-features",
        "dependency-security:",
        "taiki-e/install-action@cargo-deny",
        "cargo deny --config deny.toml check",
        "rustsec/audit-check@v2.0.0",
        "actions/cache@v4",
        ".cargo/registry",
        ".cargo/git",
        "target",
        "hashFiles('**/Cargo.lock')",
    ] {
        assert!(
            workflow.contains(required),
            "CI workflow must contain {required:?}"
        );
    }

    for unix_only_matrix_command in ["sudo ", "apt-get ", "rm -rf ", "shell: bash", "/bin/sh"] {
        assert!(
            !workflow.contains(unix_only_matrix_command),
            "cross-platform CI must avoid Unix-only command {unix_only_matrix_command:?}"
        );
    }
    assert!(
        !workflow.contains("cargo-deny-action"),
        "CI must invoke cargo-deny with the repository policy explicitly"
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one policy contract keeps the four cargo-deny sections reviewed together"
)]
fn cargo_deny_policy_is_explicit_and_restrictive() {
    let policy_text = read_artifact("deny.toml");
    let policy: toml::Value = toml::from_str(&policy_text)
        .unwrap_or_else(|error| panic!("deny.toml must be valid TOML: {error}"));

    let licenses = policy
        .get("licenses")
        .and_then(toml::Value::as_table)
        .unwrap_or_else(|| panic!("deny.toml must contain a licenses table"));
    let allowed_licenses = licenses
        .get("allow")
        .and_then(toml::Value::as_array)
        .unwrap_or_else(|| panic!("licenses.allow must be an explicit array"));
    let allowed_licenses = allowed_licenses
        .iter()
        .map(|license| {
            license
                .as_str()
                .unwrap_or_else(|| panic!("licenses.allow entries must be strings"))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        allowed_licenses,
        [
            "Apache-2.0",
            "Apache-2.0 WITH LLVM-exception",
            "BSD-2-Clause",
            "BSD-3-Clause",
            "CDLA-Permissive-2.0",
            "ISC",
            "MIT",
            "MPL-2.0",
            "Unicode-3.0",
            "Unicode-DFS-2016",
            "Zlib",
        ],
        "licenses.allow must equal the reviewed SPDX allowlist for the locked graph"
    );
    assert_eq!(
        licenses.get("include-dev").and_then(toml::Value::as_bool),
        Some(true),
        "license policy must cover development dependencies"
    );
    assert!(
        licenses
            .get("confidence-threshold")
            .and_then(toml::Value::as_float)
            .is_some_and(|threshold| threshold >= 0.8),
        "license detection must use an explicit confidence threshold"
    );
    let exceptions = licenses
        .get("exceptions")
        .and_then(toml::Value::as_array)
        .unwrap_or_else(|| panic!("licenses.exceptions must be an explicit array"));
    assert!(
        exceptions.iter().any(|exception| {
            exception.get("crate").and_then(toml::Value::as_str) == Some("terminfo@0.9.0")
                && exception
                    .get("allow")
                    .and_then(toml::Value::as_array)
                    .is_some_and(|allow| {
                        allow.as_slice() == [toml::Value::String("WTFPL".to_owned())]
                    })
        }),
        "the locked terminfo crate's WTFPL license must be a narrow reviewed exception"
    );

    let advisories = policy
        .get("advisories")
        .and_then(toml::Value::as_table)
        .unwrap_or_else(|| panic!("deny.toml must contain an advisories table"));
    assert_eq!(
        advisories
            .get("ignore")
            .and_then(toml::Value::as_array)
            .map(Vec::len),
        Some(0),
        "advisory and yanked-package exemptions must be explicit and empty"
    );
    assert_eq!(
        advisories.get("yanked").and_then(toml::Value::as_str),
        Some("deny"),
        "yanked dependencies must fail the policy check"
    );

    let bans = policy
        .get("bans")
        .and_then(toml::Value::as_table)
        .unwrap_or_else(|| panic!("deny.toml must contain a bans table"));
    assert!(
        matches!(
            bans.get("multiple-versions").and_then(toml::Value::as_str),
            Some("warn" | "deny")
        ),
        "bans.multiple-versions must be reviewed explicitly"
    );
    assert_eq!(
        bans.get("wildcards").and_then(toml::Value::as_str),
        Some("deny"),
        "wildcard dependency requirements must be denied"
    );
    for field in ["allow", "deny", "skip", "skip-tree"] {
        assert!(
            bans.get(field).and_then(toml::Value::as_array).is_some(),
            "bans.{field} must be an explicit array"
        );
    }

    let sources = policy
        .get("sources")
        .and_then(toml::Value::as_table)
        .unwrap_or_else(|| panic!("deny.toml must contain a sources table"));
    assert_eq!(
        sources
            .get("unknown-registry")
            .and_then(toml::Value::as_str),
        Some("deny")
    );
    assert_eq!(
        sources.get("unknown-git").and_then(toml::Value::as_str),
        Some("deny")
    );
    assert_eq!(
        sources
            .get("allow-registry")
            .and_then(toml::Value::as_array)
            .map(Vec::as_slice),
        Some(
            [toml::Value::String(
                "https://github.com/rust-lang/crates.io-index".to_owned()
            )]
            .as_slice()
        ),
        "only crates.io may supply registry dependencies"
    );
    assert_eq!(
        sources
            .get("allow-git")
            .and_then(toml::Value::as_array)
            .map(Vec::len),
        Some(0),
        "git dependencies must not be allowlisted"
    );
}

#[test]
fn cargo_deny_allowlist_covers_locked_license_metadata() {
    let policy_text = read_artifact("deny.toml");
    let policy: toml::Value = toml::from_str(&policy_text)
        .unwrap_or_else(|error| panic!("deny.toml must be valid TOML: {error}"));
    let metadata = offline_locked_metadata();
    let host_packages = metadata
        .get("packages")
        .and_then(JsonValue::as_array)
        .unwrap_or_else(|| panic!("cargo metadata must contain a packages array"));

    let lock: toml::Value = toml::from_str(&read_artifact("Cargo.lock"))
        .unwrap_or_else(|error| panic!("Cargo.lock must be valid TOML: {error}"));
    let locked_webpki = lock
        .get("package")
        .and_then(toml::Value::as_array)
        .unwrap_or_else(|| panic!("Cargo.lock must contain packages"))
        .iter()
        .find(|package| {
            package.get("name").and_then(toml::Value::as_str) == Some("webpki-root-certs")
                && package.get("version").and_then(toml::Value::as_str) == Some("1.0.9")
        })
        .unwrap_or_else(|| panic!("Cargo.lock must resolve webpki-root-certs 1.0.9"));
    assert_eq!(
        locked_webpki.get("source").and_then(toml::Value::as_str),
        Some("registry+https://github.com/rust-lang/crates.io-index")
    );
    assert_eq!(
        locked_webpki.get("checksum").and_then(toml::Value::as_str),
        Some("b96554aa2acc8ccdb7e1c9a58a7a68dd5d13bccc69cd124cb09406db612a1c9b")
    );

    // cargo-deny's unfiltered graph includes this wasm-only edge. Pin its
    // reviewed metadata to the exact registry checksum in Cargo.lock.
    let target_specific_webpki: JsonValue = serde_json::from_str(
        r#"{
            "name": "webpki-root-certs",
            "version": "1.0.9",
            "license": "CDLA-Permissive-2.0",
            "source": "registry+https://github.com/rust-lang/crates.io-index",
            "checksum": "b96554aa2acc8ccdb7e1c9a58a7a68dd5d13bccc69cd124cb09406db612a1c9b"
        }"#,
    )
    .unwrap_or_else(|error| panic!("target-specific metadata witness must be valid JSON: {error}"));
    assert_eq!(
        target_specific_webpki
            .get("source")
            .and_then(JsonValue::as_str),
        locked_webpki.get("source").and_then(toml::Value::as_str)
    );
    assert_eq!(
        target_specific_webpki
            .get("checksum")
            .and_then(JsonValue::as_str),
        locked_webpki.get("checksum").and_then(toml::Value::as_str)
    );

    for package in host_packages
        .iter()
        .chain(std::iter::once(&target_specific_webpki))
    {
        assert_package_license_is_covered(&policy, package);
    }
}

#[test]
fn readme_installs_and_verifies_deno_for_youtube_resolution() {
    let readme = read_artifact("README.md");
    let install = markdown_section(&readme, "## Install").to_ascii_lowercase();
    let macos = markdown_between(&readme, "### macOS", "### Linux").to_ascii_lowercase();
    let linux = markdown_between(&readme, "### Linux", "### Windows").to_ascii_lowercase();
    let windows =
        markdown_between(&readme, "### Windows", "## Keyboard reference").to_ascii_lowercase();
    let troubleshooting = markdown_section(&readme, "## Troubleshooting").to_ascii_lowercase();

    assert!(
        install.contains("javascript runtime") && install.contains("youtube"),
        "install guidance must explain that yt-dlp uses Deno for YouTube JavaScript challenges"
    );
    assert!(
        macos.contains("brew install mpv yt-dlp ffmpeg deno"),
        "macOS dependencies must install Deno with the playback tools"
    );
    assert!(
        linux.contains("deno") && linux.contains("deno.land/install.sh"),
        "Linux guidance must include the official Deno installer"
    );
    assert!(
        windows.contains("winget install --exact --id denoland.deno"),
        "WinGet guidance must install Deno"
    );
    assert_fragments_in_order(
        &windows,
        &["scoop bucket add extras", "scoop install extras/mpv"],
    );
    for command in ["deno --version", "yt-dlp --ignore-config --version"] {
        assert!(
            troubleshooting.contains(command),
            "troubleshooting must include manual verification command {command:?}"
        );
    }
    assert!(
        troubleshooting.contains("does not check `deno`"),
        "README must state the current doctor limitation instead of claiming it verifies Deno"
    );
}

#[test]
fn homebrew_and_scoop_templates_match_package_metadata() {
    let metadata = package_metadata();
    let formula = read_artifact("packaging/homebrew/ytermusic.rb");
    let scoop_text = read_artifact("packaging/scoop/ytermusic.json");
    let scoop: JsonValue = serde_json::from_str(&scoop_text)
        .unwrap_or_else(|error| panic!("Scoop template must be valid JSON: {error}"));

    assert!(formula.contains(&format!("version \"{}\"", metadata.version)));
    assert!(formula.contains(&format!("license \"{}\"", metadata.license)));
    for dependency in ["mpv", "yt-dlp", "ffmpeg", "deno"] {
        assert!(
            formula.contains(&format!("depends_on \"{dependency}\"")),
            "Homebrew formula must declare {dependency}"
        );
    }
    assert_release_placeholders("packaging/homebrew/ytermusic.rb", &formula);

    assert_eq!(
        scoop.get("version").and_then(JsonValue::as_str),
        Some(metadata.version.as_str())
    );
    assert_eq!(
        scoop.get("license").and_then(JsonValue::as_str),
        Some(metadata.license.as_str())
    );
    assert!(
        scoop
            .get("##")
            .and_then(JsonValue::as_str)
            .is_some_and(|comment| comment.contains("TEMPLATE ONLY")),
        "Scoop template must use the supported ## comment property"
    );
    assert!(
        scoop.get("_comment").is_none(),
        "Scoop template must not use the deprecated _comment property"
    );
    let scoop_dependencies = scoop
        .get("depends")
        .and_then(JsonValue::as_array)
        .unwrap_or_else(|| panic!("Scoop template must contain a depends array"));
    for dependency in ["main/yt-dlp", "main/ffmpeg", "main/deno"] {
        assert!(
            scoop_dependencies
                .iter()
                .any(|value| value.as_str() == Some(dependency)),
            "Scoop template must declare {dependency}"
        );
    }
    assert!(
        scoop_dependencies
            .iter()
            .all(|dependency| dependency.as_str() != Some("extras/mpv")),
        "extras/mpv cannot be a hard dependency before the extras bucket exists"
    );
    let scoop_notes = scoop
        .get("notes")
        .and_then(JsonValue::as_array)
        .unwrap_or_else(|| panic!("Scoop template must contain installation notes"))
        .iter()
        .filter_map(JsonValue::as_str)
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    assert_fragments_in_order(
        &scoop_notes,
        &["scoop bucket add extras", "scoop install extras/mpv"],
    );
    assert_release_placeholders("packaging/scoop/ytermusic.json", &scoop_text);
}

#[test]
fn local_release_makefile_is_debug_capable_dependency_aware_and_nonpublishing() {
    let makefile = read_artifact("Makefile");

    for target in [
        "help:",
        "build:",
        "build-debug:",
        "run:",
        "build-release:",
        "check:",
        "universal:",
        "package:",
        "checksum:",
        "formula:",
        "release-local:",
        "clean:",
    ] {
        assert!(makefile.contains(target), "Makefile must define {target}");
    }

    for required in [
        "cargo build --locked",
        "cargo build --release --locked",
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "cargo fmt --all -- --check",
        "cargo clippy --all-targets --all-features -- -D warnings",
        "cargo test --all-targets --all-features --quiet",
        "GITHUB_OWNER",
        "packaging/homebrew/ytermusic.rb",
        "rustup show active-toolchain",
        "rustup component add --toolchain \"$$toolchain\" cargo",
        "rustup target add --toolchain \"$$toolchain\"",
        "rustup which rustc --toolchain \"$$toolchain\"",
        "lipo",
        "shasum",
    ] {
        assert!(
            makefile.contains(required),
            "Makefile must contain {required:?}"
        );
    }

    let gitignore = read_artifact(".gitignore");
    assert!(
        gitignore.lines().any(|line| line == "/dist/"),
        ".gitignore must contain an explicit /dist/ line"
    );

    for forbidden in ["gh release create", "git push", "git tag"] {
        assert!(
            !makefile.contains(forbidden),
            "Makefile must not publish through {forbidden:?}"
        );
    }

    let universal_recipe = makefile
        .split_once("universal: targets")
        .unwrap_or_else(|| panic!("Makefile must define universal with targets as a prerequisite"))
        .1
        .split_once("\npackage:")
        .unwrap_or_else(|| panic!("package must follow the universal recipe"))
        .0;
    let target_builds = universal_recipe
        .lines()
        .filter(|line| line.contains("cargo build --release --locked --target"))
        .collect::<Vec<_>>();
    assert_eq!(
        target_builds.len(),
        2,
        "universal must build exactly two target architectures"
    );
    assert!(
        target_builds
            .iter()
            .all(|line| line.contains("RUSTC=\"$$rustc\" rustup run \"$$toolchain\" cargo build")),
        "universal target builds must use Cargo and Rustc from the resolved Rustup toolchain"
    );
}

#[test]
fn readme_documents_local_makefile_workflow() {
    let readme = read_artifact("README.md");
    let macos = markdown_between(&readme, "### macOS", "### Linux");
    let workflow = markdown_section(macos, "#### Makefile workflow").to_ascii_lowercase();
    let normalized = workflow.split_whitespace().collect::<Vec<_>>().join(" ");

    for command in [
        "make build",
        "make run",
        "make build-release",
        "make check",
        "make release-local github_owner=your-github-name",
    ] {
        assert!(
            normalized.contains(command),
            "README Makefile workflow must mention {command:?}"
        );
    }

    for required in [
        "`make build` (an alias for `make build-debug`) creates a native debug build",
        "`make build-release` creates an optimized native build",
        "optimized arm64 and intel builds into a universal macos archive under `dist/`",
        "prepares local artifacts under `dist/`",
        "does not create git tags or upload github releases",
        "tagging and upload remain manual",
    ] {
        assert!(
            normalized.contains(required),
            "README Makefile workflow must explain {required:?}"
        );
    }
    assert!(
        !workflow.contains(['<', '>']),
        "README Makefile workflow must not use shell-redirection characters as placeholders"
    );
}

#[test]
fn local_release_design_describes_archive_stability_without_reproducibility_overclaim() {
    let design = read_artifact("docs/plans/2026-08-07-local-release-makefile-design.md")
        .to_ascii_lowercase();
    let normalized = design.split_whitespace().collect::<Vec<_>>().join(" ");

    for required in [
        "fixed, version-derived name",
        "contains only the top-level `ytermusic` executable",
        "does not promise byte-for-byte reproducible gzip output",
        "formula hashes the archive produced by the packaging run",
    ] {
        assert!(
            normalized.contains(required),
            "local release design must explain {required:?}"
        );
    }
    assert!(
        !normalized.contains("deterministic"),
        "local release design must not call gzip archive bytes deterministic"
    );
}

#[cfg(not(windows))]
#[test]
fn local_release_makefile_does_not_expand_untrusted_values_as_make_expressions() {
    for variable in ["VERSION", "GITHUB_OWNER", "REPO"] {
        for (source, from_environment, environment_override) in [
            ("command-line", false, false),
            ("environment", true, false),
            ("make -e environment", true, true),
        ] {
            let marker = format!("YTERMUSIC_MAKE_EXPANSION_MARKER_{variable}_{source}");
            let payload = format!("$(info {marker})");
            let mut command = Command::new("make");
            command.current_dir(repository_root());
            if environment_override {
                command.arg("-e");
            }
            command.arg("help");
            if from_environment {
                command.env(variable, payload);
            } else {
                command.arg(format!("{variable}={payload}"));
            }

            let output = command.output().unwrap_or_else(|error| {
                panic!("make help must run for {variable} from {source}: {error}")
            });
            assert!(
                output.status.success(),
                "make help failed for {variable} from {source}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                !String::from_utf8_lossy(&output.stdout).contains(&marker)
                    && !String::from_utf8_lossy(&output.stderr).contains(&marker),
                "Make expanded an untrusted {variable} value from {source} as an expression"
            );
        }
    }
}

#[test]
fn winget_templates_are_consistent_dependency_aware_and_nonpublishable() {
    let metadata = package_metadata();
    let version_manifest = read_artifact("packaging/winget/Ytermusic.Ytermusic.yaml");
    let installer_manifest = read_artifact("packaging/winget/Ytermusic.Ytermusic.installer.yaml");
    let locale_manifest = read_artifact("packaging/winget/Ytermusic.Ytermusic.locale.en-US.yaml");

    for (path, manifest) in [
        (
            "packaging/winget/Ytermusic.Ytermusic.yaml",
            version_manifest.as_str(),
        ),
        (
            "packaging/winget/Ytermusic.Ytermusic.installer.yaml",
            installer_manifest.as_str(),
        ),
        (
            "packaging/winget/Ytermusic.Ytermusic.locale.en-US.yaml",
            locale_manifest.as_str(),
        ),
    ] {
        assert!(
            manifest.contains("TEMPLATE ONLY"),
            "{path} must be visibly marked TEMPLATE ONLY"
        );
        assert!(
            manifest.contains("https://aka.ms/winget-manifest.")
                && manifest.contains(".1.12.0.schema.json"),
            "{path} must identify its authoritative winget 1.12 schema"
        );
        assert_eq!(
            yaml_scalar(manifest, "PackageIdentifier"),
            Some("Ytermusic.Ytermusic"),
            "{path} must use the shared package identifier"
        );
        assert_eq!(
            yaml_scalar(manifest, "PackageVersion"),
            Some(metadata.version.as_str()),
            "{path} version must match Cargo.toml"
        );
        assert_eq!(
            yaml_scalar(manifest, "ManifestVersion"),
            Some("1.12.0"),
            "{path} must target the same winget schema"
        );
    }

    assert_eq!(
        yaml_scalar(&version_manifest, "ManifestType"),
        Some("version")
    );
    assert_eq!(
        yaml_scalar(&installer_manifest, "ManifestType"),
        Some("installer")
    );
    assert_eq!(
        yaml_scalar(&locale_manifest, "ManifestType"),
        Some("defaultLocale")
    );
    assert_eq!(yaml_scalar(&locale_manifest, "License"), Some("MIT"));
    for dependency in [
        "shinchiro.mpv",
        "yt-dlp.yt-dlp",
        "Gyan.FFmpeg",
        "DenoLand.Deno",
    ] {
        assert!(
            installer_manifest.contains(&format!("PackageIdentifier: {dependency}")),
            "winget installer must declare {dependency}"
        );
    }
    assert_release_placeholders(
        "packaging/winget/Ytermusic.Ytermusic.installer.yaml",
        &installer_manifest,
    );
}

#[test]
fn readme_describes_the_actual_cache_persistence_boundaries() {
    let readme = read_artifact("README.md");
    let files = markdown_section(&readme, "## Files and paths").to_ascii_lowercase();
    let normalized = files.split_whitespace().collect::<Vec<_>>().join(" ");

    for required in [
        "chart metadata cache is stored in `ytermusic.db`",
        "player artwork and stream-resolver caches are memory-only",
        "notification artwork cache retains at most two private png files",
        "removes notification artwork leftovers at startup",
        "do not delete or move `ytermusic.db` while ytermusic is running",
        "session, playback history, podcast progress, and cached chart metadata",
    ] {
        assert!(
            normalized.contains(required),
            "files-and-paths guidance must explain {required:?}"
        );
    }
    assert!(
        !normalized.contains("cache contains disposable artwork and resolver data"),
        "README must not claim memory-only caches persist in the OS cache directory"
    );
}

#[test]
fn release_artifacts_contain_no_literal_secrets() {
    for path in RELEASE_ARTIFACTS {
        if path == "Makefile"
            && !repository_root()
                .join(path)
                .try_exists()
                .unwrap_or_else(|error| panic!("cannot inspect Makefile: {error}"))
        {
            continue;
        }

        let contents = read_artifact(path);
        assert_no_literal_secrets(path, &contents);
        assert!(
            Path::new(path).extension().is_some() || matches!(path, "LICENSE" | "Makefile"),
            "artifact list must only contain explicit files"
        );
    }
}

#[test]
fn readme_documents_richer_player_keyboard_mouse_and_visual_contract() {
    let readme = read_artifact("README.md").to_ascii_lowercase();
    let normalized = readme.split_whitespace().collect::<Vec<_>>().join(" ");

    for required in [
        "visible button-style labels are keyboard shortcuts",
        "`f7` / `f8` / `f9`",
        "native media previous, play/pause, and next keys",
        "music seeks by a fixed 10 seconds",
        "podcasts use the configured skip interval",
        "mouse clicks work on navigation, visible list rows, player controls, and the progress bar",
        "the mouse wheel moves lists and scrollable overlays",
        "wide uses animated artwork when available and otherwise static artwork",
        "compact uses static artwork only",
        "tiny omits artwork",
        "theme-derived spectrum gradient",
        "accent through foreground to brighter foreground",
        "terminal's color capability",
        "`no_color`",
        "non-terminal or dumb output uses monochrome",
        "ordinary ansi terminals use the basic 16-color palette",
        "256-color terminals use ansi 256",
        "24-bit terminals use true color",
        "timestamp-derived fade",
        "full lyrics overlay",
        "highlights the active synchronized line",
        "tabs and timed-line breaks are normalized to spaces, while other control characters are removed",
        "charts keeps its list viewport stable",
    ] {
        assert!(
            normalized.contains(required),
            "README richer-interaction guidance must explain {required:?}"
        );
    }
    assert!(
        !normalized.contains("accent through success to warning"),
        "README must describe the implemented spectrum interpolation, not an invented palette"
    );
}

#[test]
fn readme_and_example_config_document_native_notification_boundaries() {
    let readme = read_artifact("README.md").to_ascii_lowercase();
    let normalized = readme.split_whitespace().collect::<Vec<_>>().join(" ");
    for required in [
        "native now-playing notifications are enabled by default",
        "never block playback",
        "macos and linux can attach artwork",
        "windows notifications require an optional, already-registered `notifications.windows_aum_id`",
        "windows notifications are text-only",
        "no registry or powershell changes",
        "at most two artwork files",
        "every tui startup, even when notifications are disabled or unavailable",
        "dedicated os thread with a 100 ms startup bound",
        "continues with text-only notifications when possible",
        "artwork urls and provider ids are never logged",
    ] {
        assert!(
            normalized.contains(required),
            "README notification guidance must explain {required:?}"
        );
    }

    let example = read_artifact("config.example.toml").to_ascii_lowercase();
    for required in [
        "# show native now-playing notifications. default: true",
        "# failures never block playback",
        "# windows notifications are text-only",
        "# ytermusic does not register or mutate an appusermodelid",
    ] {
        assert!(
            example.contains(required),
            "example notification config must document {required:?}"
        );
    }
}

#[test]
fn release_checklist_records_richer_interaction_audit_and_unrun_manual_smoke() {
    let checklist = read_artifact("docs/release-checklist.md");
    let audit = markdown_between(
        &checklist,
        "## Richer player interaction verification",
        "## Favorites and explicit-list playback verification",
    )
    .to_ascii_lowercase();
    let normalized = audit.split_whitespace().collect::<Vec<_>>().join(" ");

    for required in [
        "supplemental audit date: 2026-08-05",
        "reviewed implementation revision: `c13747ad3c37ffce0ea9ffa35d2f214ce8322240`",
        "privacy and resource audit",
        "windows cross-compile",
        "target not installed",
        "notification center entry and artwork",
        "f7/f8/f9",
        "all player mouse controls and progress seeking",
        "charts section scrolling",
        "not run",
        "no interactive environment or user consent",
    ] {
        assert!(
            normalized.contains(required),
            "release checklist must record {required:?}"
        );
    }
    assert!(!normalized.contains("favorites startup load"));
    assert!(!normalized.contains("explicit-list queue replacement"));
}

#[test]
fn release_checklist_includes_favorites_and_explicit_list_manual_smoke() {
    let checklist = read_artifact("docs/release-checklist.md");
    let audit = markdown_between(
        &checklist,
        "## Favorites and explicit-list playback verification",
        "## Offline gate",
    );
    let normalized = audit
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    for required in [
        "supplemental audit date: 2026-08-06",
        "reviewed implementation revision: `d35ed465cfb7764fa93d680b06efb02f91da8400`",
        "### automated contract evidence",
        "explicit_list_accepts_1024_unique_items_and_rejects_1025",
        "favorite_capacity_is_transactional_and_never_evicts",
        "favorites_persist_across_reopen_and_session_replacement",
        "### manual macos smoke",
        "newest-first",
        "content, queue, and player focus",
        "overflow rejection without eviction",
        "removing the playing favorite",
        "selected/current shuffle behavior",
        "endless radio disablement",
        "1,024 unique playable rows",
        "1,025-row rejection preserves the old queue, playback, and modes",
    ] {
        assert!(
            normalized.contains(required),
            "release checklist must include manual coverage for {required:?}"
        );
    }

    for scope in [
        "local favorites storage",
        "favorites runtime and reducer behavior",
        "explicit-list queue behavior",
        "favorites and list input",
    ] {
        let row = audit
            .lines()
            .find(|line| line.to_ascii_lowercase().contains(scope))
            .unwrap_or_else(|| panic!("automated audit must contain scope {scope:?}"));
        let cells = row.split('|').map(str::trim).collect::<Vec<_>>();
        assert_eq!(cells.get(1), Some(&"PASS"), "{scope:?} must record PASS");
    }

    for scenario in [
        "favorites startup load",
        "favorite toggling",
        "explicit-list queue replacement",
        "explicit-list playback accepts 1,024",
    ] {
        let row = audit
            .lines()
            .find(|line| line.to_ascii_lowercase().contains(scenario))
            .unwrap_or_else(|| panic!("manual audit must contain scenario {scenario:?}"));
        let cells = row.split('|').map(str::trim).collect::<Vec<_>>();
        assert_eq!(cells.get(1), Some(&"NOT RUN"), "{scenario:?} was not run");
    }
}

#[test]
fn production_tui_uses_one_detected_color_capability_for_artwork_and_theme() {
    let cli = read_artifact("src/cli.rs");
    let enter_tui = markdown_between(
        &cli,
        "    async fn enter_tui(",
        "}\n\n/// Parses the command-line arguments",
    );

    for required in [
        "detected_color_capability()",
        "color_capability",
        "ArtworkRuntimeComponents::new(",
        "Theme::for_capability(color_capability)",
    ] {
        assert!(
            enter_tui.contains(required),
            "production TUI must use {required:?}"
        );
    }
    assert!(!enter_tui.contains("ColorCapability::TrueColor"));
    assert!(!enter_tui.contains("Theme::default()"));
}

#[test]
fn production_tui_prepares_notification_cache_before_enabled_and_platform_gates() {
    let cli = read_artifact("src/cli.rs");
    let enter_tui = markdown_between(
        &cli,
        "    async fn enter_tui(",
        "}\n\n/// Parses the command-line arguments",
    );

    for required in [
        "initialize_notification_service(",
        "config.notifications.enabled",
        "NotificationArtworkCache::new(&cache_root)",
        "NativeNotifier::from_prepared_cache(cache, windows_aum_id)",
    ] {
        assert!(
            enter_tui.contains(required),
            "production TUI must use {required:?}"
        );
    }
    let initialization = enter_tui
        .find("initialize_notification_service(")
        .unwrap_or_else(|| panic!("notification initializer missing"));
    let services = enter_tui
        .find("RuntimeServices::new(")
        .unwrap_or_else(|| panic!("runtime services composition missing"));
    assert!(
        initialization < services,
        "cache preparation must precede runtime services"
    );
    assert!(
        !enter_tui.contains("NativeNotifier::new("),
        "async TUI startup must not perform synchronous notification cache filesystem work"
    );
}

#[test]
fn readme_documents_bubble_tea_ui_motion_contract() {
    let readme = read_artifact("README.md");
    let ui = markdown_section(&readme, "## Keyboard reference");
    let normalized = ui
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    for required in [
        "theme-aware borderless animated progress bar",
        "pausing freezes progress fill and shimmer",
        "list animation is presentation-only",
        "logical selection changes immediately",
        "visible loading states use a braille spinner",
        "motion clock idles when nothing visible needs animation",
        "mouse seeking remains supported across the complete progress bar",
    ] {
        assert!(
            normalized.contains(required),
            "README UI section must state {required:?}"
        );
    }
}

#[test]
fn release_checklist_records_ui_motion_audit_and_unrun_manual_scenarios() {
    let checklist = read_artifact("docs/release-checklist.md");
    let audit = markdown_between(
        &checklist,
        "## Bubble Tea UI motion verification",
        "## Offline gate",
    );
    let normalized = audit
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for required in [
        "supplemental audit date: 2026-08-07",
        "theme-aware borderless progress",
        "presentation-only selection motion",
        "braille loading spinner",
        "bounded coalesced motion clock",
        "### manual macos smoke",
    ] {
        assert!(
            normalized.contains(required),
            "motion audit must contain {required:?}"
        );
    }

    for scenario in [
        "play, pause, resume, and seek",
        "rapid keyboard and mouse selection across every list",
        "visible loading start and completion",
        "resize wide, compact, and tiny layouts",
        "idle cpu and redraw behavior",
    ] {
        let row = audit
            .lines()
            .find(|line| line.to_ascii_lowercase().contains(scenario))
            .unwrap_or_else(|| panic!("motion audit must contain scenario {scenario:?}"));
        let cells = row.split('|').map(str::trim).collect::<Vec<_>>();
        assert_eq!(cells.get(1), Some(&"NOT RUN"), "{scenario:?} was not run");
    }
}
