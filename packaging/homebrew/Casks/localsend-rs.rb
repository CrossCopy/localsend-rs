# The Homebrew cask for the LocalSend RS desktop app.
#
# This file lives in the LocalSend RS repository as the source of truth; each
# release renders it from the real archives (`scripts/homebrew.py`) and attaches
# the result to the GitHub release. Copying it into `HuakunShen/homebrew-tap` as
# `Casks/localsend-rs.rb` is the owner's action — this repository never pushes
# to the tap.
#
# Verify locally with:
#   brew audit --cask HuakunShen/tap/localsend-rs
#   brew install --cask HuakunShen/tap/localsend-rs
#
# `auto_updates true` is load-bearing: the app updates itself from the release
# feed (Apple Developer ID verified, see docs/release/macos.md), so
# `brew upgrade` must not fight it.
cask "localsend-rs" do
  arch arm: "aarch64", intel: "x86_64"

  version "0.2.1"
  sha256 arm:   "c2007cd2485b394f8c8fbbe36462408a85a1fe1c877002b56a97df13605accfa",
         intel: "126e6c7deb4f40df565b42732c7923ecbe969c377d49b3100adaf1d1e4244b58"

  url "https://github.com/CrossCopy/localsend-rs/releases/download/v#{version}/localsend-gpui-#{arch}-apple-darwin-v#{version}.tar.gz"
  name "LocalSend RS"
  desc "LocalSend v2 client and desktop app written in Rust"
  homepage "https://github.com/CrossCopy/localsend-rs"

  livecheck do
    url "https://github.com/CrossCopy/localsend-rs/releases/latest"
    strategy :github_latest do |json|
      json["tag_name"]&.sub(/^v/, "")
    end
  end

  auto_updates true

  app "LocalSend RS.app"

  zap trash: [
    "~/Library/Application Support/LocalSendGpui",
  ]
end
