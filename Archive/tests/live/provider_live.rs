#![cfg(feature = "live-tests")]

use std::{
    error::Error,
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use secrecy::SecretString;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use ytermusic::{
    app::Generation,
    domain::{MediaKind, RegionCode, SearchFilter},
    lyrics::{
        LrclibClient, LyricsSource, LyricsSourceService, MAX_LYRICS_TEXT_BYTES,
        MAX_TIMED_LYRIC_LINE_BYTES, MAX_TIMED_LYRIC_LINES,
    },
    podcast_rankings::{
        ApplePodcastRankingSource, MAX_PODCAST_RECOMMENDATIONS, PodcastRankingSource,
    },
    process::TokioProcessRunner,
    provider::{LibrarySection, MusicProvider, SearchItem, YtMusicProvider},
    resolver::{Resolver, SystemResolverClock, YtDlpResolver},
    ui::{
        animation::{AnimationDecoder, AnimationKey, AnimationRequest, FfmpegAnimationDecoder},
        artwork::CellSize,
        spectrum::{
            FfmpegSpectrumDecoder, MAX_SPECTRUM_LEVEL, SpectrumDecoder, SpectrumError,
            SpectrumFrame, SpectrumFrameOutput, SpectrumKey, SpectrumRequest, SpectrumTarget,
        },
    },
};

const LIVE_GATE: &str = "YTERMUSIC_LIVE_TESTS";
const LIVE_COOKIE: &str = "YTERMUSIC_LIVE_COOKIE";
const MAX_PODCAST_METADATA_BYTES: usize = 512;

fn live_tests_enabled() -> bool {
    std::env::var_os(LIVE_GATE).is_some()
}

fn require_explicit_live_gate() -> Result<(), io::Error> {
    if std::env::var(LIVE_GATE).as_deref() == Ok("1") {
        Ok(())
    } else {
        Err(io::Error::other(
            "live smoke requires YTERMUSIC_LIVE_TESTS=1",
        ))
    }
}

async fn observe_bounded_spectrum_and_reap(
    frames: &mut watch::Receiver<Option<SpectrumFrameOutput>>,
    cancel: CancellationToken,
    decode: tokio::task::JoinHandle<Result<(), SpectrumError>>,
    expected_band_count: usize,
    observation_timeout: Duration,
    cleanup_timeout: Duration,
) -> Result<(), io::Error> {
    let observation = async {
        tokio::time::timeout(observation_timeout, frames.changed())
            .await
            .map_err(|_| io::Error::other("timed out waiting for one live spectrum frame"))?
            .map_err(|_| io::Error::other("live spectrum decoder closed without a frame"))?;
        let published = frames.borrow();
        let frame = published
            .as_ref()
            .ok_or_else(|| io::Error::other("live spectrum decoder published no frame"))?
            .as_ref()
            .map_err(|_| io::Error::other("live spectrum decoder is unavailable"))?;
        if frame.levels().len() != expected_band_count {
            return Err(io::Error::other(
                "live spectrum frame violated the band-count bound",
            ));
        }
        if !frame
            .levels()
            .iter()
            .all(|level| *level <= MAX_SPECTRUM_LEVEL)
        {
            return Err(io::Error::other(
                "live spectrum frame violated the level bound",
            ));
        }
        Ok(())
    }
    .await;

    cancel.cancel();
    let mut decode = decode;
    let cleanup = match tokio::time::timeout(cleanup_timeout, &mut decode).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(_))) => Err(io::Error::other(
            "live spectrum decoder failed after observation",
        )),
        Ok(Err(_)) => Err(io::Error::other("live spectrum decoder task failed")),
        Err(_) => {
            decode.abort();
            let _ = decode.await;
            Err(io::Error::other(
                "live spectrum decoder did not stop after cancellation",
            ))
        }
    };

    observation?;
    cleanup
}

#[tokio::test]
async fn spectrum_smoke_reaps_decoder_on_every_observation_failure() {
    enum Failure {
        Timeout,
        Closed,
        DecoderError,
        MissingFrame,
        WrongBandCount,
    }

    struct Reaped(Arc<AtomicBool>);

    impl Drop for Reaped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    for failure in [
        Failure::Timeout,
        Failure::Closed,
        Failure::DecoderError,
        Failure::MissingFrame,
        Failure::WrongBandCount,
    ] {
        let (output, mut frames) = watch::channel(None);
        let keep_output = match failure {
            Failure::Timeout => Some(output),
            Failure::Closed => None,
            Failure::DecoderError => {
                output.send_replace(Some(Err(SpectrumError::Unavailable)));
                Some(output)
            }
            Failure::MissingFrame => {
                output.send_replace(None);
                Some(output)
            }
            Failure::WrongBandCount => {
                let frame = SpectrumFrame::new(vec![1].into_boxed_slice())
                    .unwrap_or_else(|| panic!("valid bounded fixture"));
                output.send_replace(Some(Ok(Arc::new(frame))));
                Some(output)
            }
        };
        let cancel = CancellationToken::new();
        let decode_cancel = cancel.clone();
        let reaped = Arc::new(AtomicBool::new(false));
        let task_reaped = Arc::clone(&reaped);
        let decode = tokio::spawn(async move {
            let _reaped = Reaped(task_reaped);
            decode_cancel.cancelled().await;
            Ok(())
        });

        let result = observe_bounded_spectrum_and_reap(
            &mut frames,
            cancel,
            decode,
            16,
            Duration::from_millis(1),
            Duration::from_millis(25),
        )
        .await;

        drop(keep_output);
        assert!(result.is_err(), "fixture must exercise an error path");
        assert!(reaped.load(Ordering::SeqCst), "decoder task was not reaped");
    }
}

#[tokio::test]
#[ignore = "requires explicit live-test gate and network access"]
async fn anonymous_youtube_plain_lyrics_are_bounded() -> Result<(), Box<dyn Error>> {
    require_explicit_live_gate()?;
    let provider = YtMusicProvider::new_unauthenticated().await?;
    let page = provider
        .search("Massive Attack Teardrop", SearchFilter::Songs)
        .await?;
    let item = page
        .items
        .iter()
        .find_map(|item| match item {
            SearchItem::Playable(item) if item.kind == MediaKind::Song => Some(item),
            _ => None,
        })
        .ok_or_else(|| io::Error::other("live song search returned no playable result"))?;
    let lyrics = provider.lyrics(&item.id).await?;
    assert!(
        !lyrics.text().trim().is_empty() && lyrics.text().len() <= MAX_LYRICS_TEXT_BYTES,
        "YouTube Music lyrics violated the normalized text bounds"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit live-test gate and network access"]
async fn anonymous_lrclib_match_returns_bounded_synchronized_lyrics() -> Result<(), Box<dyn Error>>
{
    require_explicit_live_gate()?;
    let provider = Arc::new(YtMusicProvider::new_unauthenticated().await?);
    let page = provider
        .search("Massive Attack Teardrop", SearchFilter::Songs)
        .await?;
    let item = page
        .items
        .iter()
        .find_map(|item| match item {
            SearchItem::Playable(item) if item.kind == MediaKind::Song => Some(item),
            _ => None,
        })
        .ok_or_else(|| io::Error::other("live song search returned no playable result"))?;
    let source = LyricsSourceService::new(provider, Arc::new(LrclibClient::new()?), true);
    let document = source
        .load(item)
        .await?
        .ok_or_else(|| io::Error::other("live sources returned no lyrics document"))?;
    assert_eq!(
        document.source(),
        LyricsSource::Lrclib,
        "live lyrics did not produce a conservative LRCLIB match"
    );
    assert!(
        !document.timed().is_empty() && document.timed().len() <= MAX_TIMED_LYRIC_LINES,
        "LRCLIB synchronized lyrics violated the normalized line bounds"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit live-test gate and network access"]
async fn anonymous_lrclib_multilingual_fallback_returns_synchronized_lyrics()
-> Result<(), Box<dyn Error>> {
    require_explicit_live_gate()?;
    let provider = Arc::new(YtMusicProvider::new_unauthenticated().await?);
    let page = provider
        .search("MC Cheung Tinfu Not My Problem", SearchFilter::Songs)
        .await?;
    let item = page
        .items
        .iter()
        .filter_map(|item| match item {
            SearchItem::Playable(item) if item.kind == MediaKind::Song => Some(item),
            _ => None,
        })
        .filter(|item| {
            let title = item.title.to_lowercase();
            let creator_matches = item.creators.iter().any(|creator| {
                let creator = creator.to_lowercase();
                creator.contains("mc")
                    && (creator.contains("cheung tinfu") || creator.contains("張天賦"))
            });
            let bilingual_title_matches = title.contains("not my problem")
                && title.contains("與我無關")
                && [" - ", " – ", " — "]
                    .iter()
                    .any(|separator| title.contains(separator));
            let duration_matches = item
                .duration_ms
                .is_some_and(|duration_ms| duration_ms.abs_diff(205_000) <= 5_000);
            bilingual_title_matches && creator_matches && duration_matches
        })
        .min_by_key(|item| {
            item.duration_ms
                .map_or(u64::MAX, |duration_ms| duration_ms.abs_diff(205_000))
        })
        .ok_or_else(|| {
            io::Error::other(
                "live song search did not return the playable MC Cheung Tinfu 'Not My Problem' target near 205 seconds",
            )
        })?;
    let source = LyricsSourceService::new(provider, Arc::new(LrclibClient::new()?), true);
    let document = source
        .load(item)
        .await?
        .ok_or_else(|| io::Error::other("live sources returned no lyrics document"))?;

    assert_eq!(
        document.source(),
        LyricsSource::Lrclib,
        "multilingual fallback did not produce a conservative LRCLIB match"
    );
    assert!(
        !document.timed().is_empty() && document.timed().len() <= MAX_TIMED_LYRIC_LINES,
        "LRCLIB synchronized lyrics violated the normalized line-count bounds"
    );
    assert!(
        document.timed().iter().all(|line| {
            !line.text().trim().is_empty()
                && line.text().len() <= MAX_TIMED_LYRIC_LINE_BYTES
                && line.end_ms().is_none_or(|end_ms| end_ms >= line.start_ms())
        }),
        "LRCLIB synchronized lyrics violated the normalized text or timestamp bounds"
    );
    assert!(
        document
            .timed()
            .windows(2)
            .all(|lines| lines[0].start_ms() < lines[1].start_ms()),
        "LRCLIB synchronized lyric timestamps were not strictly increasing"
    );
    let retained_text_bytes = document.plain().map_or(0, str::len).saturating_add(
        document
            .timed()
            .iter()
            .map(|line| line.text().len())
            .fold(0_usize, usize::saturating_add),
    );
    assert!(
        retained_text_bytes <= MAX_LYRICS_TEXT_BYTES,
        "LRCLIB lyrics violated the normalized total-text bound"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit live-test gate, network access, yt-dlp, and FFmpeg"]
async fn anonymous_video_preview_decodes_one_bounded_frame() -> Result<(), Box<dyn Error>> {
    require_explicit_live_gate()?;
    let provider = YtMusicProvider::new_unauthenticated().await?;
    let page = provider
        .search("Massive Attack Teardrop", SearchFilter::Songs)
        .await?;
    let mut item = page
        .items
        .iter()
        .find_map(|item| match item {
            SearchItem::Playable(item) if item.kind == MediaKind::Song => Some(item.clone()),
            _ => None,
        })
        .ok_or_else(|| io::Error::other("live search returned no playable media"))?;
    // Search currently has no Videos filter. The live resolver smoke only needs a
    // real playable YouTube media ID with preview eligibility enabled.
    item.kind = MediaKind::Video;
    let extractor = YtDlpResolver::new(
        "yt-dlp",
        Arc::new(TokioProcessRunner),
        Arc::new(SystemResolverClock),
        Duration::from_mins(1),
    );
    let stream = extractor
        .resolve(&item, None, CancellationToken::new())
        .await?;
    let preview = stream
        .preview_url
        .ok_or_else(|| io::Error::other("resolved live video did not include a preview stream"))?;
    let size = CellSize::new(4, 2);
    let request = AnimationRequest::new(
        AnimationKey::new(Generation::new(1), item.id, size),
        preview,
    );
    let decoder =
        FfmpegAnimationDecoder::new("ffmpeg").with_process_timeout(Duration::from_secs(45));
    let (output, mut frames) = watch::channel(None);
    let cancel = CancellationToken::new();
    let decode_cancel = cancel.clone();
    let decode = tokio::spawn(async move { decoder.decode(request, output, decode_cancel).await });

    tokio::time::timeout(Duration::from_secs(30), frames.changed())
        .await
        .map_err(|_| io::Error::other("timed out waiting for one live preview frame"))?
        .map_err(|_| io::Error::other("live preview decoder closed without a frame"))?;
    {
        let published = frames.borrow();
        let frame = published
            .as_ref()
            .ok_or_else(|| io::Error::other("live preview decoder published no frame"))?
            .as_ref()
            .map_err(|_| io::Error::other("live preview decoder returned a safe failure"))?;
        assert_eq!(frame.width(), size.width, "decoded frame width changed");
        assert_eq!(frame.height(), size.height, "decoded frame height changed");
    }
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), decode)
        .await
        .map_err(|_| io::Error::other("live preview decoder did not stop after cancellation"))??
        .map_err(|_| io::Error::other("live preview decoder failed after publishing a frame"))?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit live-test gate, network access, yt-dlp, and FFmpeg"]
async fn anonymous_audio_stream_produces_one_bounded_spectrum_frame() -> Result<(), Box<dyn Error>>
{
    const BAND_COUNT: u16 = 16;

    require_explicit_live_gate()?;
    let provider = YtMusicProvider::new_unauthenticated()
        .await
        .map_err(|_| io::Error::other("live music provider is unavailable"))?;
    let page = provider
        .search("Massive Attack Teardrop", SearchFilter::Songs)
        .await
        .map_err(|_| io::Error::other("live song search is unavailable"))?;
    let item = page
        .items
        .iter()
        .find_map(|item| match item {
            SearchItem::Playable(item) if item.kind == MediaKind::Song => Some(item.clone()),
            _ => None,
        })
        .ok_or_else(|| io::Error::other("live song search returned no playable result"))?;
    let extractor = YtDlpResolver::new(
        "yt-dlp",
        Arc::new(TokioProcessRunner),
        Arc::new(SystemResolverClock),
        Duration::from_mins(1),
    );
    let stream = extractor
        .resolve(&item, None, CancellationToken::new())
        .await
        .map_err(|_| io::Error::other("live audio resolver is unavailable"))?;
    let analysis_url = stream
        .analysis_stream_url()
        .ok_or_else(|| io::Error::other("resolved audio is not eligible for bounded analysis"))?;

    let target = SpectrumTarget::new(BAND_COUNT, 3)
        .ok_or_else(|| io::Error::other("live spectrum target is invalid"))?;
    let request = SpectrumRequest::new(
        SpectrumKey::new(Generation::new(1), item.id, target),
        analysis_url,
    );
    let decoder =
        FfmpegSpectrumDecoder::new("ffmpeg").with_process_timeout(Duration::from_secs(45));
    let (output, mut frames) = watch::channel(None);
    let cancel = CancellationToken::new();
    let decode_cancel = cancel.clone();
    let decode = tokio::spawn(async move { decoder.decode(request, output, decode_cancel).await });

    observe_bounded_spectrum_and_reap(
        &mut frames,
        cancel,
        decode,
        usize::from(BAND_COUNT),
        Duration::from_secs(30),
        Duration::from_secs(5),
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit live-test gate and network access"]
async fn anonymous_song_search_returns_normalized_results() -> Result<(), Box<dyn Error>> {
    if !live_tests_enabled() {
        return Ok(());
    }
    let provider = YtMusicProvider::new_unauthenticated().await?;
    let page = provider
        .search("Massive Attack Teardrop", SearchFilter::Songs)
        .await?;
    assert!(
        !page.items.is_empty(),
        "anonymous song query returned no normalized items"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit live-test gate and network access"]
async fn anonymous_podcast_search_returns_normalized_results() -> Result<(), Box<dyn Error>> {
    if !live_tests_enabled() {
        return Ok(());
    }
    let provider = YtMusicProvider::new_unauthenticated().await?;
    let page = provider
        .search("Rustacean Station", SearchFilter::Podcasts)
        .await?;
    assert!(
        !page.items.is_empty(),
        "anonymous podcast query returned no normalized items"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit live-test gate and network access"]
async fn anonymous_regional_charts_return_normalized_sections() -> Result<(), Box<dyn Error>> {
    if !live_tests_enabled() {
        return Ok(());
    }
    let provider = YtMusicProvider::new_unauthenticated().await?;
    for region in [
        RegionCode::parse("HK")?,
        RegionCode::parse("JP")?,
        RegionCode::parse("US")?,
    ] {
        let sections = provider.charts(&region).await?;
        assert!(
            !sections.is_empty(),
            "regional chart query returned no normalized sections"
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit live-test gate and network access"]
async fn anonymous_country_podcast_rankings_are_bounded() -> Result<(), Box<dyn Error>> {
    if !live_tests_enabled() {
        return Ok(());
    }
    let source = ApplePodcastRankingSource::new()?;
    for requested in [
        RegionCode::parse("US")?,
        RegionCode::parse("JP")?,
        RegionCode::parse("HK")?,
    ] {
        let page = source.top_shows(&requested).await?;
        assert_eq!(page.region(), &requested, "ranking region changed");
        assert!(
            (1..=MAX_PODCAST_RECOMMENDATIONS).contains(&page.items().len()),
            "ranking item count was outside the supported bounds"
        );
        for (expected_rank, item) in (1..).zip(page.items()) {
            assert_eq!(
                item.rank(),
                expected_rank,
                "ranking order was not contiguous"
            );
            assert!(
                !item.title().trim().is_empty() && item.title().len() <= MAX_PODCAST_METADATA_BYTES,
                "ranking title was empty or exceeded the metadata bound"
            );
            assert!(
                item.publisher().len() <= MAX_PODCAST_METADATA_BYTES,
                "ranking publisher exceeded the metadata bound"
            );
            if let Some(artwork) = item.artwork_url() {
                assert!(
                    artwork.as_url().as_str().len() <= MAX_PODCAST_METADATA_BYTES,
                    "ranking artwork metadata exceeded the metadata bound"
                );
                assert!(
                    artwork.as_url().scheme() == "https" && artwork.as_url().host_str().is_some(),
                    "ranking artwork metadata was invalid"
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit live-test gate, injected credential, and network access"]
async fn authenticated_library_returns_normalized_page() -> Result<(), Box<dyn Error>> {
    if !live_tests_enabled() {
        return Ok(());
    }
    let Some(cookie) = std::env::var_os(LIVE_COOKIE) else {
        return Ok(());
    };
    let cookie = cookie
        .into_string()
        .map_err(|_| io::Error::other("live credential is not valid Unicode"))?;
    let provider = YtMusicProvider::from_cookie(SecretString::from(cookie)).await?;
    let _page = provider.library(LibrarySection::Songs).await?;
    Ok(())
}
