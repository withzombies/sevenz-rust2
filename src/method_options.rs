#[cfg(feature = "compress")]
use lzma_rust2::LZMA2Options;

#[cfg(feature = "aes256")]
use crate::aes256sha256::AesEncoderOptions;
use std::fmt::Debug;

#[cfg(feature = "bzip2")]
#[derive(Debug, Copy, Clone)]
pub struct Bzip2Options(pub(crate) u32);

#[cfg(feature = "bzip2")]
impl Bzip2Options {
    pub const fn from_level(level: u32) -> Self {
        Self(level)
    }
}

#[cfg(feature = "deflate")]
#[derive(Debug, Copy, Clone)]
pub struct DeflateOptions(pub(crate) u32);

#[cfg(feature = "deflate")]
impl DeflateOptions {
    pub const fn from_level(level: u32) -> Self {
        Self(level)
    }
}

#[cfg(feature = "zstd")]
#[derive(Debug, Copy, Clone)]
pub struct ZStandardOptions(pub(crate) i32);

#[cfg(feature = "zstd")]
impl ZStandardOptions {
    pub const fn from_level(level: i32) -> Self {
        Self(level)
    }
}

#[derive(Debug, Clone)]
pub enum MethodOptions {
    Num(u32),
    #[cfg(feature = "compress")]
    LZMA2(LZMA2Options),
    #[cfg(feature = "bzip2")]
    BZIP2(Bzip2Options),
    #[cfg(feature = "deflate")]
    DEFLATE(DeflateOptions),
    #[cfg(feature = "zstd")]
    ZSTD(ZStandardOptions),
    #[cfg(feature = "aes256")]
    Aes(AesEncoderOptions),
}

#[cfg(feature = "aes256")]
impl From<AesEncoderOptions> for MethodOptions {
    fn from(value: AesEncoderOptions) -> Self {
        Self::Aes(value)
    }
}

#[cfg(feature = "aes256")]
impl From<AesEncoderOptions> for crate::SevenZMethodConfiguration {
    fn from(value: AesEncoderOptions) -> Self {
        Self::new(crate::SevenZMethod::AES256SHA256).with_options(MethodOptions::Aes(value))
    }
}

#[cfg(feature = "compress")]
impl From<LZMA2Options> for crate::SevenZMethodConfiguration {
    fn from(value: LZMA2Options) -> Self {
        Self::new(crate::SevenZMethod::LZMA2).with_options(MethodOptions::LZMA2(value))
    }
}

impl From<u32> for MethodOptions {
    fn from(n: u32) -> Self {
        Self::Num(n)
    }
}

#[cfg(feature = "compress")]
impl From<LZMA2Options> for MethodOptions {
    fn from(o: LZMA2Options) -> Self {
        Self::LZMA2(o)
    }
}

#[cfg(feature = "bzip2")]
impl From<Bzip2Options> for MethodOptions {
    fn from(o: Bzip2Options) -> Self {
        Self::BZIP2(o)
    }
}

#[cfg(feature = "deflate")]
impl From<DeflateOptions> for MethodOptions {
    fn from(o: DeflateOptions) -> Self {
        Self::DEFLATE(o)
    }
}

#[cfg(feature = "zstd")]
impl From<ZStandardOptions> for MethodOptions {
    fn from(o: ZStandardOptions) -> Self {
        Self::ZSTD(o)
    }
}

impl MethodOptions {
    pub fn get_lzma2_dict_size(&self) -> u32 {
        match self {
            MethodOptions::Num(n) => *n,
            #[cfg(feature = "compress")]
            MethodOptions::LZMA2(o) => o.dict_size,
            #[allow(unused)]
            _ => 0,
        }
    }
}
