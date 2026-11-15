fn main() {
    cfg_aliases::cfg_aliases! {
        codecs: { any(feature = "lz4", feature = "lzma", feature = "deflate", feature = "zstd") },
    }
}
