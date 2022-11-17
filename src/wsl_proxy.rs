pub(crate) use crate::essentials::*;
use futures::{TryStreamExt, Stream};
pub(crate) use tracing::trace;
pub(crate) use std::ffi::OsString;
pub(crate) use std::convert::Infallible;
pub(crate) use enum_map::EnumMap;
pub(crate) use strum_macros;

use enum_map::Enum;
use anyhow;
use bytes::{Buf, BytesMut};
use tokio::{io::AsyncWriteExt};
use clap::{self, Parser};
use serde;

pub(crate) trait ResidualStream: Stream<Item = Result<BytesMut, std::io::Error>> + std::fmt::Debug + Send + Unpin {}
impl<RS> ResidualStream for RS
where
   RS: Stream<Item = Result<BytesMut, std::io::Error>> + std::fmt::Debug + Send + Unpin
{}
pub(crate) type Residual = (Box<dyn ResidualStream>, Box<dyn AsyncWrite>);

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone, Copy, strum_macros::EnumIter, Enum)]
pub(crate) enum StreamDir {
   Stdin,
   Stdout,
   Stderr,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq)]
pub(crate) enum CommunicationSide {
   Client,
   Server,
}

pub(crate) type CommmunicationId = u32;

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone, Copy)]
pub(crate) struct CommunicationPeer {
   pub(crate) side: CommunicationSide,
   pub(crate) id: CommmunicationId,
}

#[derive(Debug, clap::Subcommand)]
enum PeerConfig {
   Server{listener_port: u16, client_pid: CommmunicationId, inferior: Vec<OsString>},
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
   #[command(subcommand)]
   peer_config: PeerConfig,
}

#[derive(Debug, Default)]
pub(crate) struct ProxyStreams(pub(crate) EnumMap<StreamDir, Option<Residual>>);

#[tracing::instrument]
pub(crate) async fn proxy_residual<W>(mut reader: impl ResidualStream, mut writer: Box<W>) -> Result<(), std::io::Error>
where
   W: tokio::io::AsyncWrite + Send + 'static + ?Sized + Unpin + std::fmt::Debug,
{
   loop {
      let frame = reader.try_next().await?;
      match frame {
         None => {
            trace!("Read everything from reader {:#?}, shutting down writer {:#?}", reader, writer);
            return writer.shutdown().await
         }
         Some(mut frame) => {
            trace!("Got chunk {:x?} from reader {:#?}, writing...", frame, reader);
            loop {
               if !frame.has_remaining() {
                  trace!("Finished writing chunk to writer {:#?}", writer);
                  break;
               }
               match writer.write_buf(&mut frame).await? {
                  0 => {
                     trace!("Writer {:#?} seems to be closed, giving up.", writer);
                     return Ok(())
                  }
                  n => trace!("Written {} bytes to writer {:#?}, continuing.", n, writer)
               }
            }
         }
      }
   }
}

pub(crate) fn get_args<T>() -> anyhow::Result<T>
where
   T: clap::Parser + std::fmt::Debug
{
    let args = T::try_parse()?;
    trace!("args: {:#?}", args);
    Ok(args)
}

pub(crate) fn install_tracer(_log_file: String)
{
   #[cfg(debug_assertions)]
   {
      let log_out = std::fs::File::create(_log_file).ok().unwrap();
      let builder =
         tracing_subscriber::FmtSubscriber::builder()
         .with_ansi(false)
         .with_file(true)
         .with_line_number(true)
         .with_writer(std::sync::Mutex::new(log_out));
      let builder = builder.with_max_level(tracing::Level::TRACE);
      tracing::subscriber::set_global_default(builder.finish()).ok();
   }
}

pub(crate) fn pid_log(kind: &str) -> String {
   // format!("trace-{}-{}.log", kind, std::process::id())
   format!("trace-{}.log", kind)
}

pub(crate) fn log_error(fun: fn() -> anyhow::Result<Infallible>)
{
   match fun() {
      Err(err) => { trace!("{:#}", err); },
      _ => unreachable!()
   };
}

pub(crate) fn master(log_file: String, try_main: fn() -> anyhow::Result<Infallible>)
{
   install_tracer(log_file);
   log_error(try_main);
}