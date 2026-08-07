use url::Url;
use ytermusic::{
    app::{Action, AppState, Effect, reduce},
    domain::ArtworkUrl,
    ui::artwork::ArtworkIdentity,
};

fn url(value: &str) -> Url {
    match Url::parse(value) {
        Ok(url) => url,
        Err(error) => panic!("test URL must be valid: {error}"),
    }
}

fn artwork_url(value: &str) -> ArtworkUrl {
    match ArtworkUrl::try_from(url(value)) {
        Ok(url) => url,
        Err(error) => panic!("test artwork URL must be accepted: {error}"),
    }
}

fn assert_sentinels_absent(rendered: &str) {
    for sentinel in [
        "sentinel-user",
        "sentinel-password",
        "sentinel-host.example",
        "sentinel-path",
        "sentinel-query",
        "sentinel-fragment",
    ] {
        assert!(
            !rendered.contains(sentinel),
            "rendered value leaked {sentinel:?}: {rendered}"
        );
    }
}

#[test]
fn artwork_url_redacts_debug_and_display_without_losing_identity() {
    let first = artwork_url(
        "https://sentinel-user:sentinel-password@sentinel-host.example/sentinel-path/cover.jpg?sentinel-query=secret#sentinel-fragment",
    );
    let second = artwork_url("https://other.example.test/other-cover.jpg");

    assert_ne!(first, second);
    assert_eq!(format!("{first:?}"), "ArtworkUrl([REDACTED])");
    assert_eq!(format!("{second:?}"), "ArtworkUrl([REDACTED])");
    assert_eq!(format!("{first}"), "[REDACTED artwork URL]");
    assert_eq!(format!("{second}"), "[REDACTED artwork URL]");
    assert_sentinels_absent(&format!("{first:?} {first}"));

    let first_identity = ArtworkIdentity::from_url(first.as_url());
    let second_identity = ArtworkIdentity::from_url(second.as_url());
    assert_ne!(first_identity, second_identity);
}

#[test]
fn artwork_url_serde_is_transparent_and_validated() {
    let raw = url("https://images.example.test/private/cover.jpg?token=secret");
    let artwork = match ArtworkUrl::try_from(raw.clone()) {
        Ok(url) => url,
        Err(error) => panic!("HTTP artwork URL must be accepted: {error}"),
    };

    let encoded = match serde_json::to_string(&artwork) {
        Ok(encoded) => encoded,
        Err(error) => panic!("artwork URL must serialize: {error}"),
    };
    let raw_encoded = match serde_json::to_string(&raw) {
        Ok(encoded) => encoded,
        Err(error) => panic!("raw URL must serialize: {error}"),
    };
    assert_eq!(encoded, raw_encoded);

    let decoded: ArtworkUrl = match serde_json::from_str(&encoded) {
        Ok(decoded) => decoded,
        Err(error) => panic!("artwork URL must deserialize: {error}"),
    };
    assert_eq!(decoded, artwork);
    assert_eq!(decoded.as_url(), &raw);

    assert!(ArtworkUrl::try_from(url("file:///tmp/cover.jpg")).is_err());
    assert!(ArtworkUrl::try_from(url("ftp://images.example.test/cover.jpg")).is_err());
    assert!(
        serde_json::from_str::<ArtworkUrl>("\"file:///tmp/private-cover.jpg\"").is_err(),
        "deserialization must enforce the same HTTP(S) boundary"
    );
}

#[test]
fn artwork_url_is_redacted_across_action_effect_and_app_state_debug() {
    let artwork = artwork_url(
        "https://sentinel-user:sentinel-password@sentinel-host.example/sentinel-path/cover.jpg?sentinel-query=secret#sentinel-fragment",
    );
    let action = Action::ArtworkRequested {
        url: artwork.clone(),
    };
    assert_sentinels_absent(&format!("{action:?}"));

    let (state, effects) = reduce(AppState::default(), action);
    let [Effect::FetchArtwork { url, .. }] = effects.as_slice() else {
        panic!("artwork request must emit one fetch effect");
    };
    assert_eq!(url, &artwork);
    assert_sentinels_absent(&format!("{effects:?}"));
    assert_sentinels_absent(&format!("{state:?}"));
}
