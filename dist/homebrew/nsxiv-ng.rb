# The Homebrew formula of nsxiv-ng: builds the viewer from source with cargo, so Gatekeeper
# never quarantines it. It lives here, in no tap; the sha256 is a placeholder until the
# release's source tarball exists.
class NsxivNg < Formula
  desc "Keyboard-driven image viewer with nsxiv's keys"
  homepage "https://github.com/hawkff/nsxiv-ng"
  url "https://github.com/hawkff/nsxiv-ng/archive/refs/tags/v0.0.1.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "MIT"
  head "https://github.com/hawkff/nsxiv-ng.git", branch: "main"

  depends_on "rust" => :build
  depends_on :macos

  def install
    system "cargo", "install", *std_cargo_args(path: "crates/app")
    (pkgshare/"examples").install "etc/key-handler.example", "etc/nsxiv-rifle"
  end

  test do
    name = shell_output("#{bin}/nsxiv-ng -v").lines.first.chomp
    # a HEAD build has Cargo.toml's version, which the formula's does not name
    if version.head?
      assert_match(/\Ansxiv-ng \d+\.\d+\.\d+\z/, name)
    else
      assert_equal "nsxiv-ng #{version}", name
    end
  end
end
