# Zorg Homebrew Formula Template

When it is time to publish `zorg` to a Homebrew tap, use the following Ruby formula template. 
This template ensures that Homebrew automatically downloads the 50MB Sapling parameters and installs them into Homebrew's `pkgshare` directory (`/opt/homebrew/share/zorg/params/`), exactly where our Rust code in `send.rs` expects to find them as a fallback.

```ruby
class Zorg < Formula
  desc "A modern Zcash CLI wallet"
  homepage "https://github.com/your-repo/zorg"
  url "https://github.com/your-repo/zorg/archive/v0.1.0.tar.gz"
  sha256 "..." # The hash of your source code tarball
  
  depends_on "rust" => :build

  # 1. Tell Homebrew to auto-download the Spend params
  resource "sapling-spend" do
    url "https://download.z.cash/downloads/sapling-spend.params"
    sha256 "8e48ffd23abb3a5fd9c5589204f32d9c31285a04b78096ba40a79b75677efc13"
  end

  # 2. Tell Homebrew to auto-download the Output params
  resource "sapling-output" do
    url "https://download.z.cash/downloads/sapling-output.params"
    sha256 "2f0ebbcbb9bb0bcffe95a397e7eba89c29eb4dde6191c339db88570e3f3fb0e4"
  end

  def install
    # Build your Rust CLI binary
    system "cargo", "install", *std_cargo_args

    # 3. Create a 'params' directory in Homebrew's shared folder for this package
    (pkgshare/"params").mkpath
    
    # 4. Move the downloaded resources into that folder
    resource("sapling-spend").stage { (pkgshare/"params").install "sapling-spend.params" }
    resource("sapling-output").stage { (pkgshare/"params").install "sapling-output.params" }
  end
end
```

### How the Integration Works
1. **The `resource` blocks**: Homebrew uses these to fetch the files using its built-in downloader (with native progress bars) safely outside the build sandbox. Note that Homebrew uses SHA-256 for its `sha256` declarations because it natively enforces SHA-256 for all downloads.
2. **The `pkgshare/"params"` destination**: In Homebrew, `pkgshare` translates to `/opt/homebrew/share/zorg/` (on Apple Silicon). This perfectly matches the `brew_paths` fallback list hardcoded into `send.rs`.
3. **The Runtime Hand-off (BLAKE2b)**: When the user installs `zorg`, Homebrew puts the files in `pkgshare`. The very first time the user runs a transaction, your Rust code finds them there, verifies their authentic **BLAKE2b** hashes (using the lightning-fast `blake2b_simd` crate), and securely copies them to `~/Library/Application Support/ZcashParams` for global Zcash ecosystem use.
