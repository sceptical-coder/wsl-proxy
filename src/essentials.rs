pub(crate) use anyhow::Error;
pub(crate) use anyhow::Result;
pub(crate) use rmp_serde as wire_format;

pub(crate) trait AsyncRead : tokio::io::AsyncRead + Send + Unpin + std::fmt::Debug {}

impl<T> AsyncRead for T
where
   T: tokio::io::AsyncRead + Send + Unpin + std::fmt::Debug
{}

pub(crate) trait AsyncWrite : tokio::io::AsyncWrite + Send + Unpin + std::fmt::Debug {}

impl<T> AsyncWrite for T
where
   T: tokio::io::AsyncWrite + Send + Unpin + std::fmt::Debug
{}