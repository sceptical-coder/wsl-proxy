pub(crate) use crate::essentials::*;
pub(crate) use crate::wsl_proxy::*;
use futures::FutureExt;
use futures::SinkExt;
pub(crate) use futures::future::try_join_all;
pub(crate) use tokio::net::TcpStream;
pub(crate) use std::process::Stdio;
pub(crate) use tokio::process::Child;
pub(crate) use futures::try_join;
pub(crate) use std::task::Poll;
pub(crate) use std::process::ExitStatus;
pub(crate) use strum::IntoEnumIterator;

#[derive(Default)]
pub(crate) struct ProvidedStreams {
   pub(crate) stdin: Option<Box<dyn ResidualStream>>,
   pub(crate) stdout: Option<Box<dyn AsyncWrite>>,
   pub(crate) stderr: Option<Box<dyn AsyncWrite>>,
}

#[derive(Default)]
pub(crate) struct ControlledStreams {
   pub(crate) stdin: Option<Box<dyn AsyncWrite>>,
   pub(crate) stdout: Option<Box<dyn ResidualStream>>,
   pub(crate) stderr: Option<Box<dyn ResidualStream>>,
}

pub(crate) fn flatten<T, E, F>(res: Result<Result<T, E>, F>) -> Result<T, Error>
where
   E: Into<Error>,
   F: Into<Error>,
{
   match res {
      Ok(Ok(result)) => Ok::<T, Error>(result),
      Ok(Err(err)) => Err(err.into()),
      Err(err) => Err(err.into()),
  }
}

#[cfg(not(unix))]
fn get_exit_code(e: ExitStatus) -> i32 {
   e.code().unwrap()
}

#[cfg(unix)]
fn get_exit_code(e: ExitStatus) -> i32 {
   use std::os::unix::process::ExitStatusExt;
   ExitStatusExt::into_raw(e)
}

pub(crate) fn exit(e: ExitStatus) -> ! {
   std::process::exit(get_exit_code(e))
}

pub(crate) async fn proxy_child(mut child: Child, mut provided_streams: ProvidedStreams, mut controlled_streams: ControlledStreams)
 -> Result<ExitStatus> {
    let streams_fut = tokio::spawn(async move {
       try_join! (
          proxy_residual(provided_streams.stdin.take().unwrap(), controlled_streams.stdin.take().unwrap()),
          proxy_residual(controlled_streams.stdout.take().unwrap(), provided_streams.stdout.take().unwrap()),
          proxy_residual(controlled_streams.stderr.take().unwrap(), provided_streams.stderr.take().unwrap()),
       )
    }.fuse());
    let res = child.wait().await?;
    if let Poll::Ready(r) = futures::poll!(streams_fut) {
       let _ = flatten(r)?;
    }
    Ok(res)
 }

pub(crate) fn setup_process(cmd_line: &Vec<OsString>, io: fn() -> Stdio) -> Result<std::process::Command, Error>
{
   let mut res = std::process::Command
      ::new(cmd_line.first().ok_or(anyhow::anyhow!("Expected to have non-empty command-line for inferior"))?);
   res
      .args(&cmd_line[1..])
      .stdin(io())
      .stdout(io())
      .stderr(io());
   Ok(res)
}

pub(crate) async fn connect_to_daemon(port: u16, peer: CommunicationPeer) -> anyhow::Result<TcpStream> {
    trace!("Opening unmarked connection to daemon for peer: {:#?}", peer);
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    trace!("Opened unmarked connection for stream dir: {:#?}", peer);
    write_to_wire(&mut stream, &peer).await?;
    trace!("Marked connection to daemon for peer: {:#?}", peer);
    Ok(stream)
 }

pub(crate) async fn write_to_wire<W, T>(w: &mut W, t: &T) -> Result<()> 
where
   W: tokio::io::AsyncWrite + Unpin,
   T: serde::Serialize,
{
   use tokio_util::codec::LengthDelimitedCodec;
   use tokio_util::codec::FramedWrite;
   let mut length_delimited = FramedWrite::new(w, LengthDelimitedCodec::new());
   let msg = wire_format::to_vec(t)?;
   Ok(length_delimited.send(msg.into()).await?)
}