class Zorg < Formula
  desc "A modern Zcash CLI wallet"
  homepage "https://github.com/your-org/zorg"
  url "https://github.com/your-org/zorg/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "REPLACE_WITH_REAL_HASH"

  depends_on "rust" => :build

  resource "sapling-spend" do
    url "https://download.z.cash/downloads/sapling-spend.params"
    sha256 "8e48ffd23abb3a5fd9c5589204f32d9c31285a04b78096ba40a79b75677efc13"
  end

  resource "sapling-output" do
    url "https://download.z.cash/downloads/sapling-output.params"
    sha256 "2f0ebbcbb9bb0bcffe95a397e7eba89c29eb4dde6191c339db88570e3f3fb0e4"
  end

  def install
    system "cargo", "install", *std_cargo_args

    (pkgshare/"params").mkpath
    resource("sapling-spend").stage { (pkgshare/"params").install "sapling-spend.params" }
    resource("sapling-output").stage { (pkgshare/"params").install "sapling-output.params" }
  end

  test do
    assert_match "zorg", shell_output("#{bin}/zorg --help")
  end
end