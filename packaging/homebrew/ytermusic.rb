# TEMPLATE ONLY: release automation must replace every __...__ token before publication.
class Ytermusic < Formula
  desc "Keyboard-first YouTube Music terminal player"
  homepage "https://github.com/ytermusic/ytermusic"
  url "__RELEASE_URL_MACOS_UNIVERSAL__"
  version "0.1.0"
  sha256 "__SHA256_MACOS_UNIVERSAL__"
  license "MIT"

  depends_on "mpv"
  depends_on "yt-dlp"
  depends_on "ffmpeg"
  depends_on "deno"

  def install
    bin.install "ytermusic"
  end

  test do
    system "#{bin}/ytermusic", "--help"
  end
end
