#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow;
use bytes::Buf;
use enum_map::{Enum, EnumMap};
use futures::future::{try_join_all};
use futures::{try_join};
use futures::{prelude::*};
// #[cfg(windows)]
// use once_cell::sync::Lazy;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Child;
use tokio::sync::oneshot;
// use tokio_metrics;
use tracing::{trace};
use std::convert::Infallible;
use std::ffi::OsString;
use std::iter::once;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::{Arc};
use std::task::Poll;
// use std::time::Duration;
use std::{process::Stdio};
use std::result::Result;
use clap::{self, Parser};
use tokio::{net::{TcpStream, TcpListener}};
use serde;
use tokio_serde::{SymmetricallyFramed, formats::SymmetricalMessagePack};
use tokio_util::codec::{FramedRead, LengthDelimitedCodec};
use strum_macros;
use strum::IntoEnumIterator;

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

#[cfg(windows)]
use tokio_util::codec::FramedWrite;

type Error = anyhow::Error;

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone, Copy, strum_macros::EnumIter, Enum)]
enum StreamDir {
   Stdin,
   Stdout,
   Stderr,
}

#[derive(Debug, clap::Subcommand)]
enum PeerConfig {
   #[cfg(windows)]
   Server{listener_port: u16},
   Client{server_path: PathBuf},
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
   #[command(subcommand)]
   peer_config: PeerConfig,
   #[arg(global = true)]
   inferior_cmd_line: Vec<OsString>,
}

#[derive(Default)]
struct ProvidedStreams {
   stdin: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>,
   stdout: Option<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>,
   stderr: Option<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>,
}

#[derive(Default)]
struct ControlledStreams {
   stdin: Option<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>,
   stdout: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>,
   stderr: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>,
}

#[derive(Debug, Default)]
struct ProxyStreams(EnumMap<StreamDir, Option<Box<TcpStream>>>);

#[cfg(windows)]
#[tracing::instrument]
async fn connect_to_client(stream_target: StreamDir, port: u16)
   -> Result<TcpStream, Error>
{
   trace!("Opening unmarked connection for stream dir: {:#?}", stream_target);
   let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
   trace!("Opened unmarked connection for stream dir: {:#?}", stream_target);
   let length_delimited = FramedWrite::new(&mut stream, LengthDelimitedCodec::new());
   let mut serialized =
      SymmetricallyFramed::new(length_delimited, SymmetricalMessagePack::<StreamDir>::default());
   serialized.send(stream_target).await?;
   trace!("Marked connection for stream dir: {:#?}", stream_target);
   Ok(stream)
}

fn flatten<T, E, F>(res: Result<Result<T, E>, F>) -> Result<T, Error>
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

async fn proxy<R, W>(mut reader: Box<R>, mut writer: Box<W>) -> Result<(), std::io::Error>
where
   R: tokio::io::AsyncRead + Send + 'static + ?Sized + Unpin,
   W: tokio::io::AsyncWrite + Send + 'static + ?Sized + Unpin,
{
   let mut buf = vec![0; 1usize << 20];
   loop {
      match reader.read(&mut buf).await? {
         0 => return writer.shutdown().await,
         n => {
            let mut read = std::io::Cursor::new(&mut buf[..n]);
            loop {
               if !read.has_remaining() {
                  break;
               }
               match writer.write_buf(&mut read).await? {
                  0 => return Ok(()),
                  _ => (),
               }
            }
         }
      }
   }
}

async fn proxy_child(mut child: Child, mut provided_streams: ProvidedStreams, mut controlled_streams: ControlledStreams) -> anyhow::Result<ExitStatus> {
   let streams_fut = tokio::spawn(async move {
      try_join! (
         proxy(provided_streams.stdin.take().unwrap(), controlled_streams.stdin.take().unwrap()),
         proxy(controlled_streams.stdout.take().unwrap(), provided_streams.stdout.take().unwrap()),
         proxy(controlled_streams.stderr.take().unwrap(), provided_streams.stderr.take().unwrap()),
      )
   }.fuse());
   let res = child.wait().await?;
   if let Poll::Ready(r) = futures::poll!(streams_fut) {
      let _ = flatten(r)?;
   }
   Ok(res)
}

#[cfg(windows)]
#[tracing::instrument]
async fn start_inferior(command_line: &Vec<OsString>, ProxyStreams(mut streams): ProxyStreams) -> anyhow::Result<ExitStatus>
{
   #[cfg(create_desktop)]
   {
      use windows::Win32::System::Threading::{GetCurrentThreadId};
      use windows::{
         core::*, Win32::{System::{StationsAndDesktops::*, SystemServices::*}, Security::SECURITY_ATTRIBUTES},
      };
      use windows::Win32::Foundation::GetLastError;
      use windows::Win32::Foundation::BOOL;
      #[macro_use(defer)]
      extern crate scopeguard;

      let old_desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) }?;

      let desktop = unsafe
      {
         let mut sa = SECURITY_ATTRIBUTES::default();
         sa.bInheritHandle = BOOL(1);
         CreateDesktopW(
            w!("hiddenDesktop"),
            PCWSTR::null(),
            None,
            Default::default(),
            DESKTOP_CREATEWINDOW.0,
            Some(&sa),
         )?
      };
      match desktop {
         HDESK(ptr) if ptr == 0 => anyhow::bail!(anyhow::Error::new(windows::core::Error::from(unsafe{ GetLastError() })).context("Failed to open desktop!!!")),
         HDESK(_) => ()
      }
      defer! {
         unsafe { CloseDesktop(desktop); }
      }
      match unsafe { SetThreadDesktop(desktop) } {
         BOOL(0) => anyhow::bail!(anyhow::Error::new(windows::core::Error::from(unsafe{ GetLastError() })).context("Failed to assign desktop!!!")),
         _ => ()
      }
      match unsafe { CloseDesktop(old_desktop) } {
         BOOL(0) => anyhow::bail!(anyhow::Error::new(windows::core::Error::from(unsafe{ GetLastError() })).context("Failed to close old desktop!!!")),
         _ => ()
      }
   }

   use windows::Win32::System::Threading::CREATE_NO_WINDOW;
   use std::os::windows::process::CommandExt;

   let mut inferior_cmd = setup_process(command_line, || Stdio::piped())?;
   inferior_cmd.creation_flags(CREATE_NO_WINDOW.0);
   let mut inferior = tokio::process::Command::from(inferior_cmd).spawn()?;
   let controlled_streams = ControlledStreams{
      stdin: Some(Box::new(inferior.stdin.take().unwrap())),
      stdout: Some(Box::new(inferior.stdout.take().unwrap())),
      stderr: Some(Box::new(inferior.stderr.take().unwrap())),
   };
   proxy_child(
      inferior,
      ProvidedStreams{
         stdin: Some(Box::new(streams[StreamDir::Stdin].take().unwrap())),
         stdout: Some(Box::new(streams[StreamDir::Stdout].take().unwrap())),
         stderr: Some(Box::new(streams[StreamDir::Stderr].take().unwrap())),
      },
      controlled_streams,
   ).await
}

fn setup_process(cmd_line: &Vec<OsString>, io: fn() -> Stdio) -> Result<std::process::Command, Error>
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

#[tracing::instrument]
async fn provide_server_streams(listener: TcpListener) -> Result<ProxyStreams, Error> {
   let mut chans = EnumMap::<StreamDir, Option<oneshot::Sender<TcpStream>>>::default();

   let stop_accept_token = tokio_util::sync::CancellationToken::new();
   let accepted_all = stop_accept_token.clone();

   let awaiters_fut =
   {
      let accept_drop_guard = Arc::new(stop_accept_token.drop_guard());
      let res = try_join_all(StreamDir::iter()
         .map(|dir| {
            let (sender, receiver) = oneshot::channel();
            chans[dir] = Some(sender);

            let accept_guard = accept_drop_guard.clone();
            async move {
               let stream = receiver.await
                  .or_else(|_|{anyhow::bail!("Unable to receive socket for reading!!!")})?;
               trace!("Got incoming stream: {:#?}", stream);
               drop(accept_guard);
               Ok((dir, stream))
            }
         }));
      res
   };

   let acceptor = async move {
      loop {
         trace!("Accepting connection...");

         let (mut str, addr) = listener.accept().await?;

         trace!("Accepted connection {:?}", addr);

         let length_delimited = FramedRead::new(&mut str, LengthDelimitedCodec::new());
         let mut deserialized = SymmetricallyFramed::new(length_delimited, SymmetricalMessagePack::<StreamDir>::default());
         trace!("Before getting stream ID");
         match { let res = deserialized.try_next().await?; trace!("Got stream ID: {:#?}", res); res } {
            None => break Err::<(), Error>(anyhow::anyhow!("Server sends jibberish!!!")),
            Some(stream_dir) => match chans[stream_dir].take() {
               None => break Err(anyhow::anyhow!("Channel to send to is not created yet!!!")),
               Some(chan) =>
               {
                  trace!("Before sending stream to awaiter");
                  let send_res = chan.send(str);
                  trace!("After sending stream to awaiter");
                  match send_res {
                     Ok(()) => (),
                     Err(_) => break Err(anyhow::anyhow!("Channel to send to was already attached!")),
                  }
               }
            }
         }
      }
   };

   let join_acceptor = tokio::spawn(async move {
      tokio::select! {
         _ = accepted_all.cancelled() => Ok::<(), Error>(()),
         Err(err) = acceptor => Err::<(), Error>(err),
      }
   });

   match try_join! (join_acceptor.map(flatten), awaiters_fut) {
      Err(err) => Err(err),
      Ok(((), kv)) => Ok(ProxyStreams(
         kv.into_iter().map(|(dir, str)| (dir, Some(Box::new(str))))
         .collect::<EnumMap<_, _>>()))
   }
}

#[tracing::instrument]
async fn client_main(server_path: PathBuf, inferior_cmd_line: Vec<OsString>) -> Result<ExitStatus, Error> {
   trace!("inferior_command_line: {:?}", inferior_cmd_line);
   let (server, ProxyStreams(mut streams)) = {
      let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
      trace!("Bound listener at: {:#?}", listener.local_addr());
      let port = listener.local_addr()?.port();

      let server_streams_fut = tokio::spawn(
         provide_server_streams(listener)).map(flatten);
      let server_cmd_line =
         once(server_path.as_os_str().to_owned())
         .chain(once("server".to_owned().into()))
         .chain(once(port.to_string().into()))
         .chain(once("--".into()))
         .chain(inferior_cmd_line.into_iter())
         .collect::<Vec<_>>();
      trace!("Starting server: {:?}", server_cmd_line);
      let server = tokio::process::Command::from(setup_process(&server_cmd_line, || Stdio::inherit())?).spawn()?;
      trace!("Server started: {:#?}", server);
      let server_streams = server_streams_fut.await?;
      trace!("Got server streams: {:#?}", server_streams);
      (server, server_streams)
   };

   proxy_child(
      server,
      ProvidedStreams {
         stdin: Some(Box::new(tokio::io::stdin())),
         stdout: Some(Box::new(tokio::io::stdout())),
         stderr: Some(Box::new(tokio::io::stderr())),
      },
      ControlledStreams {
         stdin: Some(Box::new(streams[StreamDir::Stdin].take().unwrap())),
         stdout: Some(Box::new(streams[StreamDir::Stdout].take().unwrap())),
         stderr: Some(Box::new(streams[StreamDir::Stderr].take().unwrap())),
      },
   ).await
}

#[cfg(windows)]
#[tracing::instrument]
async fn server_main(listener_port: u16, inferior_cmd_line: Vec<OsString>) -> Result<ExitStatus, Error> {
   trace!("Before loop that connects to client");
   let cs = ProxyStreams(
      try_join_all(StreamDir::iter().map(
         |dir| {
            // let task_monitor = TASK_MONITOR.clone();
            tokio::spawn(connect_to_client(dir, listener_port))
               .map(flatten)
               .map(move |res|
                  res.map(|str| (dir, str)))
         }))
      .await?
      .into_iter()
      .map(|(k, v)| (k, Some(Box::new(v))))
      .collect::<EnumMap<_, _>>()
   );
   trace!("Connected to client, streams: {:#?}", cs);
   start_inferior(&inferior_cmd_line, cs).await
}

#[cfg(not(unix))]
fn get_exit_code(e: ExitStatus) -> i32
{
   e.code().unwrap()
}

#[cfg(unix)]
fn get_exit_code(e: ExitStatus) -> i32
{
   ExitStatusExt::into_raw(e)
}

// #[cfg(windows)]
// static TASK_MONITOR: Lazy<tokio_metrics::TaskMonitor> = Lazy::new(|| {
//    tokio_metrics::TaskMonitor::new()
// });

#[tokio::main]
async fn entry_point() -> Result<Infallible, Error> {
   // print task metrics every 500ms
   // let task_monitor = TASK_MONITOR.clone();
   {
      // let task_monitor = task_monitor.clone();
      // tokio::spawn(async move {
      //    for interval in task_monitor.intervals() {
      //       // pretty-print the metric interval
      //       trace!("{:?}", interval);
      //       // wait 500ms
      //       tokio::time::sleep(Duration::from_millis(50)).await;
      //    }
      // });

   //    let handle = tokio::runtime::Handle::current();
   //    let runtime_monitor = tokio_metrics::RuntimeMonitor::new(&handle);
   //    tokio::spawn(async move {
   //       for interval in runtime_monitor.intervals() {
   //           // pretty-print the metric interval
   //           trace!("{:?}", interval);
   //           // wait 500ms
   //           tokio::time::sleep(Duration::from_millis(500)).await;
   //       }
   //   });
   }

   let args = Cli::try_parse()?;
   trace!("args: {:#?}", args);
   std::process::exit(get_exit_code(async {
         Ok::<_, Error>(match args.peer_config {
            PeerConfig::Client{server_path} => client_main(server_path, args.inferior_cmd_line).await,
            #[cfg(windows)]
            PeerConfig::Server{listener_port} => server_main(listener_port, args.inferior_cmd_line).await,
         }?)
      }.await?))
}
fn main()
{
   #[cfg(debug_assertions)]
   {
      let log_file = format!("trace-{}.log", std::process::id());
      let log_out = std::fs::File::create(log_file).ok().unwrap();
      let builder =
         tracing_subscriber::FmtSubscriber::builder()
         .with_ansi(false)
         .with_writer(std::sync::Mutex::new(log_out));
      let builder = builder.with_max_level(tracing::Level::TRACE);
      tracing::subscriber::set_global_default(builder.finish()).ok();
   }
   match entry_point() {
      Err(err) => { trace!("{}", err); },
      _ => panic!("Should not have come to be here!")
   };
}