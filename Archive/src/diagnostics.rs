use std::{ffi::OsString, fmt::Write as _, path::Path};

use crate::process::{CommandSpec, ExecutableLocator, ProcessOutput, ProcessRunner};

const MPV: &str = "mpv";
const YT_DLP: &str = "yt-dlp";
const FFMPEG: &str = "ffmpeg";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    MacOs,
    Linux,
    Windows,
    Other,
}

impl Platform {
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Other
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticStatus {
    Healthy,
    Degraded,
    Unhealthy,
}

impl DiagnosticStatus {
    const fn label(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Unhealthy => "unhealthy",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticRow {
    component: String,
    status: DiagnosticStatus,
    detail: String,
}

impl DiagnosticRow {
    #[must_use]
    pub fn new(component: &str, status: DiagnosticStatus, detail: &str) -> Self {
        Self {
            component: sanitize(component),
            status,
            detail: sanitize(detail),
        }
    }

    #[must_use]
    pub fn component(&self) -> &str {
        &self.component
    }

    #[must_use]
    pub const fn status(&self) -> DiagnosticStatus {
        self.status
    }

    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorReport {
    rows: Vec<DiagnosticRow>,
}

impl DoctorReport {
    #[must_use]
    pub fn new(rows: Vec<DiagnosticRow>) -> Self {
        Self { rows }
    }

    #[must_use]
    pub fn rows(&self) -> &[DiagnosticRow] {
        &self.rows
    }

    #[must_use]
    pub fn row(&self, component: &str) -> Option<&DiagnosticRow> {
        self.rows.iter().find(|row| row.component() == component)
    }

    #[must_use]
    pub fn browsing_available(&self) -> bool {
        self.row("browsing")
            .is_some_and(|row| row.status != DiagnosticStatus::Unhealthy)
    }

    #[must_use]
    pub fn playback_available(&self) -> bool {
        self.row("playback")
            .is_some_and(|row| row.status != DiagnosticStatus::Unhealthy)
    }

    #[must_use]
    pub fn exit_code(&self) -> u8 {
        u8::from(!self.playback_available())
    }

    #[must_use]
    pub fn render(&self) -> String {
        let mut rendered = String::from("COMPONENT  STATUS     DETAILS\n");
        for row in &self.rows {
            let component = compact(&sanitize(row.component()), 10);
            let detail = compact(&sanitize(row.detail()), 190);
            let _ = writeln!(
                rendered,
                "{component:<10} {status:<10} {detail}",
                status = row.status.label()
            );
        }
        rendered
    }
}

pub struct DependencyChecker<'a> {
    locator: &'a dyn ExecutableLocator,
    runner: &'a dyn ProcessRunner,
    platform: Platform,
}

impl<'a> DependencyChecker<'a> {
    #[must_use]
    pub const fn new(
        locator: &'a dyn ExecutableLocator,
        runner: &'a dyn ProcessRunner,
        platform: Platform,
    ) -> Self {
        Self {
            locator,
            runner,
            platform,
        }
    }

    pub async fn check(&self) -> DoctorReport {
        let mut rows = vec![DiagnosticRow::new(
            "browsing",
            DiagnosticStatus::Healthy,
            "metadata browsing available",
        )];

        rows.push(self.check_mpv().await);
        rows.push(self.check_yt_dlp().await);
        rows.push(self.check_ffmpeg().await);

        let dependency_rows = &rows[1..];
        let playback_status = if dependency_rows
            .iter()
            .any(|row| row.status == DiagnosticStatus::Unhealthy)
        {
            DiagnosticStatus::Unhealthy
        } else if dependency_rows
            .iter()
            .any(|row| row.status == DiagnosticStatus::Degraded)
        {
            DiagnosticStatus::Degraded
        } else {
            DiagnosticStatus::Healthy
        };
        let playback_detail = match playback_status {
            DiagnosticStatus::Healthy => "ready",
            DiagnosticStatus::Degraded => "ready; some versions are unknown",
            DiagnosticStatus::Unhealthy => "unavailable; browsing still works",
        };
        rows.push(DiagnosticRow::new(
            "playback",
            playback_status,
            playback_detail,
        ));

        DoctorReport { rows }
    }

    async fn check_mpv(&self) -> DiagnosticRow {
        let path = match self.locate(MPV) {
            Ok(path) => path,
            Err(row) => return row,
        };

        let version = self
            .probe(
                &path,
                [OsString::from("--no-config"), OsString::from("--version")],
                "version",
            )
            .await;
        let capability = self
            .probe(
                &path,
                [
                    OsString::from("--no-config"),
                    OsString::from("--list-options"),
                ],
                "JSON IPC capability",
            )
            .await;

        self.capability_row(
            MPV,
            &path,
            version,
            capability,
            |output| contains_ascii_case_insensitive(output, "input-ipc-server"),
            "JSON IPC support is absent",
        )
    }

    async fn check_yt_dlp(&self) -> DiagnosticRow {
        let path = match self.locate(YT_DLP) {
            Ok(path) => path,
            Err(row) => return row,
        };

        let version = self
            .probe(
                &path,
                [
                    OsString::from("--ignore-config"),
                    OsString::from("--version"),
                ],
                "version",
            )
            .await;
        let capability = self
            .probe(
                &path,
                [OsString::from("--ignore-config"), OsString::from("--help")],
                "JSON output capability",
            )
            .await;

        self.capability_row(
            YT_DLP,
            &path,
            version,
            capability,
            supports_yt_dlp_json,
            "JSON output support is absent",
        )
    }

    async fn check_ffmpeg(&self) -> DiagnosticRow {
        let path = match self.locate(FFMPEG) {
            Ok(path) => path,
            Err(row) => return row,
        };

        match self
            .probe(&path, [OsString::from("-version")], "version")
            .await
        {
            Probe::Failed(reason) => self.unhealthy_row(FFMPEG, &reason),
            Probe::Succeeded(output) => match first_version(&output) {
                Some(version) => healthy_row(FFMPEG, &path, &version),
                None => degraded_row(FFMPEG, &path),
            },
        }
    }

    fn capability_row(
        &self,
        component: &str,
        path: &Path,
        version: Probe,
        capability: Probe,
        supports_capability: impl FnOnce(&str) -> bool,
        absent_reason: &str,
    ) -> DiagnosticRow {
        let version_output = match version {
            Probe::Succeeded(output) => output,
            Probe::Failed(reason) => return self.unhealthy_row(component, &reason),
        };
        let capability_output = match capability {
            Probe::Succeeded(output) => output,
            Probe::Failed(reason) => return self.unhealthy_row(component, &reason),
        };
        if !supports_capability(&capability_output) {
            return self.unhealthy_row(component, absent_reason);
        }

        match first_version(&version_output) {
            Some(version) => healthy_row(component, path, &version),
            None => degraded_row(component, path),
        }
    }

    fn locate(&self, component: &str) -> Result<std::path::PathBuf, DiagnosticRow> {
        match self.locator.find(component) {
            Ok(Some(path)) => Ok(path),
            Ok(None) => Err(self.unavailable_row(component)),
            Err(error) => Err(self.unhealthy_row(component, &format!("lookup failed: {error}"))),
        }
    }

    fn unavailable_row(&self, component: &str) -> DiagnosticRow {
        self.unhealthy_row(component, "not found")
    }

    fn unhealthy_row(&self, component: &str, reason: &str) -> DiagnosticRow {
        DiagnosticRow::new(
            component,
            DiagnosticStatus::Unhealthy,
            &format!("{reason} | {}", install_hint(self.platform, component)),
        )
    }

    async fn probe<const N: usize>(
        &self,
        path: &Path,
        args: [OsString; N],
        purpose: &str,
    ) -> Probe {
        let spec = CommandSpec::new(path, args);
        match self.runner.output(spec).await {
            Ok(output) if output.status.success() => Probe::Succeeded(output_text(&output)),
            Ok(output) => Probe::Failed(format!(
                "{purpose} probe exited with {}",
                output
                    .status
                    .code()
                    .map_or_else(|| "a signal".to_owned(), |code| code.to_string())
            )),
            Err(error) => Probe::Failed(format!("{purpose} probe failed: {error}")),
        }
    }
}

enum Probe {
    Succeeded(String),
    Failed(String),
}

fn output_text(output: &ProcessOutput) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!("{stdout}\n{stderr}")
}

fn healthy_row(component: &str, path: &Path, version: &str) -> DiagnosticRow {
    DiagnosticRow::new(
        component,
        DiagnosticStatus::Healthy,
        &format!("{version} | {}", path.to_string_lossy()),
    )
}

fn degraded_row(component: &str, path: &Path) -> DiagnosticRow {
    DiagnosticRow::new(
        component,
        DiagnosticStatus::Degraded,
        &format!("version unknown | {}", path.to_string_lossy()),
    )
}

fn install_hint(platform: Platform, dependency: &str) -> String {
    match platform {
        Platform::MacOs => format!("brew install {dependency}"),
        Platform::Linux => {
            format!("install {dependency} with your distribution's package manager")
        }
        Platform::Windows => match dependency {
            MPV => "winget install --id=shinchiro.mpv".to_owned(),
            YT_DLP => "winget install --id=yt-dlp.yt-dlp".to_owned(),
            FFMPEG => "winget install --id=Gyan.FFmpeg".to_owned(),
            _ => format!("winget search {dependency}"),
        },
        Platform::Other => format!("install {dependency} and add it to PATH"),
    }
}

fn supports_yt_dlp_json(output: &str) -> bool {
    contains_ascii_case_insensitive(output, "--dump-single-json")
        || output.split_ascii_whitespace().any(|word| {
            word.trim_matches(|character: char| {
                matches!(character, ',' | ';' | '[' | ']' | '(' | ')')
            }) == "-J"
        })
}

fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    haystack.to_ascii_lowercase().contains(needle)
}

fn first_version(output: &str) -> Option<String> {
    let bytes = output.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        if !bytes[start].is_ascii_digit() {
            start += 1;
            continue;
        }

        let mut end = start;
        while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b'.') {
            end += 1;
        }
        let candidate = output[start..end].trim_end_matches('.');
        let parts = candidate.split('.').collect::<Vec<_>>();
        let semantic = matches!(parts.len(), 2 | 3)
            && parts
                .iter()
                .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
        if semantic {
            return Some(candidate.trim_end_matches('.').to_owned());
        }
        start = end.max(start + 1);
    }
    None
}

#[must_use]
pub fn sanitize(input: &str) -> String {
    let flattened = input
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else if character == char::REPLACEMENT_CHARACTER {
                '?'
            } else {
                character
            }
        })
        .collect::<String>();
    redact_credentials(&redact_url_queries(&flattened))
}

fn redact_url_queries(input: &str) -> String {
    let mut rendered = String::with_capacity(input.len());
    let mut cursor = 0;

    while let Some(relative_start) = find_url(&input[cursor..]) {
        let url_start = cursor + relative_start;
        rendered.push_str(&input[cursor..url_start]);
        let url_end = find_url_end(input, url_start);
        let url = &input[url_start..url_end];
        rendered.push_str(&redact_url(url));
        cursor = url_end;
    }
    rendered.push_str(&input[cursor..]);
    rendered
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QuoteStyle {
    Plain(char),
    BackslashEscaped(char),
}

fn find_url_end(input: &str, url_start: usize) -> usize {
    let enclosing_quote = quote_style_before(input, url_start);
    input[url_start..]
        .char_indices()
        .find(|(offset, character)| {
            let index = url_start + offset;
            character.is_whitespace()
                || *character == '>'
                || enclosing_quote
                    .is_some_and(|style| quote_width_at(input, index, style).is_some())
        })
        .map_or(input.len(), |(offset, _)| url_start + offset)
}

fn redact_url(url: &str) -> String {
    let query = url.find('?');
    let fragment = url.find('#');
    let sensitive_start = [query, fragment].into_iter().flatten().min();
    let public_url = sensitive_start.map_or(url, |start| &url[..start]);
    let mut redacted = redact_url_userinfo(public_url);

    match (query, fragment) {
        (Some(query_start), Some(fragment_start)) if query_start < fragment_start => {
            redacted.push_str("?[REDACTED]#[REDACTED]");
        }
        (Some(_) | None, Some(_)) => redacted.push_str("#[REDACTED]"),
        (Some(_), None) => redacted.push_str("?[REDACTED]"),
        (None, None) => {}
    }
    redacted
}

fn redact_url_userinfo(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_owned();
    };
    let authority_start = scheme_end + 3;
    let authority_end = url[authority_start..]
        .find('/')
        .map_or(url.len(), |offset| authority_start + offset);
    let Some(userinfo_end) = url[authority_start..authority_end].rfind('@') else {
        return url.to_owned();
    };
    let at = authority_start + userinfo_end;
    format!("{}[REDACTED]{}", &url[..authority_start], &url[at..])
}

fn find_url(input: &str) -> Option<usize> {
    let lower = input.to_ascii_lowercase();
    [lower.find("https://"), lower.find("http://")]
        .into_iter()
        .flatten()
        .min()
}

fn redact_credentials(input: &str) -> String {
    let mut value = input.to_owned();
    let mut search_from = 0;
    while let Some((key_start, key_end)) = find_sensitive_key(&value, search_from) {
        let Some((value_start, value_end)) = credential_value_range(&value, key_start, key_end)
        else {
            search_from = key_end;
            continue;
        };
        value.replace_range(value_start..value_end, "[REDACTED]");
        search_from = value_start + "[REDACTED]".len();
    }
    value
}

fn credential_value_range(value: &str, key_start: usize, key_end: usize) -> Option<(usize, usize)> {
    let key_quote = quote_style_before(value, key_start);
    let mut cursor = key_end;
    let mut wrapper_quote = None;

    if let Some(style) = key_quote {
        if let Some(width) = quote_width_at(value, cursor, style) {
            cursor += width;
        } else {
            wrapper_quote = Some(style);
        }
    } else if let Some((_, width)) = quote_style_at(value, cursor) {
        cursor += width;
    }

    let before_spacing = cursor;
    cursor = skip_whitespace(value, cursor);
    let had_spacing = cursor > before_spacing;
    match next_character(value, cursor) {
        Some((':' | '=', width)) => {
            cursor += width;
            cursor = skip_whitespace(value, cursor);
        }
        Some(_) if had_spacing && key_quote.is_none() => {}
        Some(_) | None => return None,
    }

    if let Some(style) = wrapper_quote {
        return Some((cursor, find_closing_quote(value, cursor, style)));
    }

    if let Some((style, width)) = quote_style_at(value, cursor) {
        let value_start = cursor + width;
        return Some((value_start, find_closing_quote(value, value_start, style)));
    }

    Some((cursor, credential_value_end(value, cursor)))
}

fn next_character(value: &str, index: usize) -> Option<(char, usize)> {
    value[index..]
        .chars()
        .next()
        .map(|character| (character, character.len_utf8()))
}

fn skip_whitespace(value: &str, mut cursor: usize) -> usize {
    while let Some((character, width)) = next_character(value, cursor) {
        if !character.is_whitespace() {
            break;
        }
        cursor += width;
    }
    cursor
}

fn find_closing_quote(value: &str, value_start: usize, style: QuoteStyle) -> usize {
    value[value_start..]
        .char_indices()
        .find(|(offset, _)| quote_width_at(value, value_start + offset, style).is_some())
        .map_or(value.len(), |(offset, _)| value_start + offset)
}

fn quote_style_before(value: &str, index: usize) -> Option<QuoteStyle> {
    let (quote_index, quote) = value[..index].char_indices().next_back()?;
    if !matches!(quote, '"' | '\'') {
        return None;
    }

    match backslash_count_before(value, quote_index) {
        1 => Some(QuoteStyle::BackslashEscaped(quote)),
        count if count % 2 == 0 => Some(QuoteStyle::Plain(quote)),
        _ => None,
    }
}

fn quote_style_at(value: &str, index: usize) -> Option<(QuoteStyle, usize)> {
    let (first, first_width) = next_character(value, index)?;
    if matches!(first, '"' | '\'') && !is_escaped(value, index) {
        return Some((QuoteStyle::Plain(first), first_width));
    }
    if first != '\\' {
        return None;
    }

    let quote_index = index + first_width;
    let (quote, quote_width) = next_character(value, quote_index)?;
    if matches!(quote, '"' | '\'') && backslash_count_before(value, quote_index) == 1 {
        return Some((
            QuoteStyle::BackslashEscaped(quote),
            first_width + quote_width,
        ));
    }
    None
}

fn quote_width_at(value: &str, index: usize, style: QuoteStyle) -> Option<usize> {
    let (found, width) = quote_style_at(value, index)?;
    (found == style).then_some(width)
}

fn is_escaped(value: &str, index: usize) -> bool {
    backslash_count_before(value, index) % 2 == 1
}

fn backslash_count_before(value: &str, index: usize) -> usize {
    value[..index]
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'\\')
        .count()
}

fn find_sensitive_key(value: &str, search_from: usize) -> Option<(usize, usize)> {
    let bytes = value.as_bytes();
    let mut start = search_from;
    while start < bytes.len() {
        if !is_key_byte(bytes[start]) || (start > 0 && is_key_byte(bytes[start - 1])) {
            start += 1;
            continue;
        }

        let mut end = start + 1;
        while end < bytes.len() && is_key_byte(bytes[end]) {
            end += 1;
        }
        let key = value[start..end].to_ascii_lowercase();
        if key.contains("cookie") || key.contains("authorization") {
            return Some((start, end));
        }
        start = end;
    }
    None
}

const fn is_key_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

fn credential_value_end(value: &str, value_start: usize) -> usize {
    let tail = &value[value_start..];
    for (offset, character) in tail.char_indices() {
        if !character.is_whitespace() {
            continue;
        }
        let remainder = tail[offset..].trim_start();
        let lower = remainder.to_ascii_lowercase();
        if lower.starts_with("http://")
            || lower.starts_with("https://")
            || find_sensitive_key(remainder, 0).is_some_and(|(start, _)| start == 0)
        {
            return value_start + offset;
        }
    }
    value.len()
}

fn compact(input: &str, max_chars: usize) -> String {
    let mut characters = input.chars();
    let compact = characters.by_ref().take(max_chars).collect::<String>();
    if characters.next().is_some() {
        format!("{compact}…")
    } else {
        compact
    }
}
