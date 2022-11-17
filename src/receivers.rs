use std::path::PathBuf;

pub(crate) use fs2::FileExt;
use tokio_util::codec::{BytesCodec, FramedRead};
use crate::essentials::*;
use tracing;

#[tracing::instrument]
pub(crate) async fn fork_to_value<R, T>(r: R) -> Result<(Option<T>, FramedRead<R, BytesCodec>)>
where
   R: AsyncRead,
   T: for<'a> serde::Deserialize<'a>,
{
   use tokio_util::codec::LengthDelimitedCodec;
   use futures::TryStreamExt;
   let mut length_delimited = FramedRead::new(r, LengthDelimitedCodec::new());
   let r = length_delimited.try_next().await?;
   let res_stream = length_delimited.map_decoder(|_| BytesCodec::new());
   let r = r.map(
      |r| { 
         tracing::trace!("Read frame: {:#?}", r);
         wire_format::from_slice(&r)
      }).transpose()?;
   Ok((r, res_stream))
}

pub(crate) fn read_port_file(port_file: &std::fs::File) -> anyhow::Result<u16> {
    port_file.try_lock_shared()?;
    use std::io::BufRead;
    let current_port = std::io::BufReader::new(port_file)
       .lines()
       .map(|r| r.map_err(anyhow::Error::new))
       .map(|r|
          r.and_then(|l| Ok(l.parse::<u16>()?)))
       .next();
    match current_port {
       None => anyhow::bail!("Port file seems to be somehow empty"),
       Some(r) => Ok(r?)
    }
 }

 pub(crate) fn canonicalize_port_file(mut port_file_path: PathBuf) -> anyhow::Result<PathBuf> {
   let filename = match port_file_path.file_name() {
      None => anyhow::bail!("Path to file was expected instead of \"{:?}\"", port_file_path),
      Some(filename) => PathBuf::from(filename)
   };
   let mut parent = { port_file_path.pop(); std::fs::canonicalize(port_file_path)? };
   let port_file_path = { parent.push(filename); parent };
   Ok(port_file_path)
 }