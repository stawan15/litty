cask "litty" do
  version "@VERSION@"
  sha256 "@DMG_SHA@"

  url "https://github.com/stawan15/litty/releases/download/v#{version}/litty-#{version}-macos-universal.dmg"
  name "litty"
  desc "Tiny, fast, zero-config terminal emulator"
  homepage "https://github.com/stawan15/litty"

  depends_on macos: :big_sur

  app "litty.app"
  binary "#{appdir}/litty.app/Contents/MacOS/litty"

  # The app is ad-hoc signed, not notarized: clear the download quarantine so it opens normally.
  postflight do
    system_command "/usr/bin/xattr", args: ["-dr", "com.apple.quarantine", "#{appdir}/litty.app"]
  end

  zap trash: "~/.cache/litty"
end
