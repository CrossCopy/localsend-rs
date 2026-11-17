# The Homebrew formula for the `localsend-rs` command-line interface.
#
# This file lives in the LocalSend RS repository as the source of truth; each
# release renders it from the real archives (`scripts/homebrew.py`) and attaches
# the result to the GitHub release. Copying it into `HuakunShen/homebrew-tap` as
# `Formula/localsend-rs-cli.rb` is the owner's action — this repository never
# pushes to the tap.
#
# The token is `localsend-rs-cli` rather than `localsend-rs` so it cannot be
# confused with the cask for the desktop app (or with Homebrew core's
# `localsend-cli`, which installs the official LocalSend terminal client).
# The installed executable is still `localsend-rs`.
#
# Verify locally with:
#   brew audit HuakunShen/tap/localsend-rs-cli
#   brew install HuakunShen/tap/localsend-rs-cli
#
# There is deliberately no `version` stanza: Homebrew 7 rejects one that
# repeats the version in the URLs, and derives it from the URL instead.
class LocalsendRsCli < Formula
  desc "LocalSend v2 client and CLI written in Rust"
  homepage "https://github.com/CrossCopy/localsend-rs"
  license "MIT"

  livecheck do
    url :stable
    strategy :github_latest do |json|
      json["tag_name"]&.sub(/^v/, "")
    end
  end

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/CrossCopy/localsend-rs/releases/download/v0.2.1/localsend-rs-aarch64-apple-darwin-v0.2.1.tar.gz"
      sha256 "92fb40e6fd41521c705f1a09508e39353582ba84a0e8a14c168e45201dd87832"
    else
      url "https://github.com/CrossCopy/localsend-rs/releases/download/v0.2.1/localsend-rs-x86_64-apple-darwin-v0.2.1.tar.gz"
      sha256 "3e9af057de50832260cb7cf4b089dead49c457363ed39ce29de08483b6135b57"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/CrossCopy/localsend-rs/releases/download/v0.2.1/localsend-rs-aarch64-unknown-linux-musl-v0.2.1.tar.gz"
      sha256 "41d9a00475d19871d22d06b69eec866d357bc069c7316c7007a1faaf18871b79"
    else
      url "https://github.com/CrossCopy/localsend-rs/releases/download/v0.2.1/localsend-rs-x86_64-unknown-linux-musl-v0.2.1.tar.gz"
      sha256 "8f08219c75b64ea2d947bec42f14e5bd9e74144ee5299be2d5af124eec44b661"
    end
  end

  def install
    arch = Hardware::CPU.intel? ? "x86_64" : "aarch64"
    os = OS.mac? ? "apple-darwin" : "unknown-linux-musl"
    # Homebrew strips the archive's single root directory when unpacking;
    # accept both layouts so older brews keep working.
    staged = "localsend-rs-#{arch}-#{os}-v#{version}/localsend-rs"
    bin.install File.exist?(staged) ? staged : "localsend-rs"
  end

  test do
    # The CLI has no `--version` flag; its banner is the stable identity.
    assert_match "LocalSend", shell_output("#{bin}/localsend-rs --help")
  end
end
